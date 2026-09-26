#include "SessionController.h"

#include "Bridge.h"
#include "Metrics.h"

#include <QByteArray>
#include <QThread>
#include <QtGlobal>

#include <algorithm>

namespace {

qint64 envNumber(const char *name, qint64 fallback)
{
    const QByteArray value = qgetenv(name);
    if (value.isEmpty()) {
        return fallback;
    }
    bool ok = false;
    const qint64 parsed = value.toLongLong(&ok);
    return ok ? parsed : fallback;
}

} // namespace

SessionController::SessionController(Bridge *bridge, QObject *parent)
    : QObject(parent)
    , m_bridge(bridge)
    , m_model(new ResultTableModel(this))
{
    m_model->setFetchSource(this);

    // Defaults M1.8 can set without rebuilding. Every one of them is also a
    // writable property, so a test sets them directly.
    // 1,000,000 by default because that is the shape M1's exit gate asks for
    // (a TableView scrolling a million mock rows); a test sets `mockRows` to
    // something it can check exhaustively instead.
    m_mockRows = envNumber("RELDEX_S15_ROWS", 1000000);
    m_mockSeed = envNumber("RELDEX_S15_SEED", 0);
    m_mockPerFetchLatencyUs = envNumber("RELDEX_S15_PER_FETCH_LATENCY_US", 0);
    m_mockFirstBatchLatencyUs = envNumber("RELDEX_S15_FIRST_BATCH_LATENCY_US", 0);
    m_fetchRows = static_cast<int>(std::clamp<qint64>(envNumber("RELDEX_S15_FETCH_ROWS", 1000), 1,
                                                     1000000));
    m_maxFetchesInFlight = static_cast<int>(
            std::clamp<qint64>(envNumber("RELDEX_S15_FETCHES_IN_FLIGHT", 2), 1, 1024));
    // Spike S15's K2 measures execute -> first row painted, and only the first
    // batch is part of that. Turning auto-fetch off keeps the rest of the
    // stream out of the measurement instead of subtracting it afterwards; the
    // view can still drive its own `fetchMore()`.
    m_autoFetch = envNumber("RELDEX_S15_AUTOFETCH", 1) != 0;
}

SessionController::~SessionController()
{
    // The model releases every batch it holds here, which is what
    // reldex_hub_destroy requires of us (ADR-0003 D3). The session itself is
    // not closed from a destructor: ~Bridge destroys the hub straight after,
    // and that never commits (`SPEC.md` §10).
    if (m_bridge != nullptr && m_sessionId != 0) {
        m_bridge->unregisterSession(m_sessionId);
    }
}

bool SessionController::checkThread() const
{
    // Every reldex_* call this adapter makes is on this object's thread.
    // reldex.h "THREADS": all calls but request_cancel come from one thread,
    // and this adapter does not use the cancel-from-any-thread allowance
    // either, which is what makes ~Bridge's teardown order sufficient (A10).
    Q_ASSERT(thread() == QThread::currentThread());
    return thread() == QThread::currentThread();
}

void SessionController::setState(State state)
{
    if (m_state == state) {
        return;
    }
    m_state = state;
    Q_EMIT stateChanged();
}

void SessionController::clearError()
{
    if (!m_hasError) {
        return;
    }
    m_hasError = false;
    m_errorMessage.clear();
    m_errorNativeMessage.clear();
    m_errorKind = RELDEX_ERROR_KIND_UNKNOWN;
    m_errorNativeCode = 0;
    m_errorLine = 0;
    m_errorColumn = 0;
    m_errorCharOffset = -1;
    Q_EMIT errorChanged();
}

void SessionController::adoptError(const ReldexError *error)
{
    if (error == nullptr) {
        return;
    }
    ReldexErrorView view = reldex::makeErrorView();
    if (reldex_error_view(error, &view) != RELDEX_STATUS_OK) {
        return;
    }
    m_hasError = true;
    // D6: an unmapped kind is "unknown", never an assertion.
    m_errorKind = view.kind;
    m_errorMessage = QString::fromUtf8(reinterpret_cast<const char *>(view.message.ptr),
                                       static_cast<qsizetype>(view.message.len));
    m_errorNativeMessage =
            view.has_native ? QString::fromUtf8(reinterpret_cast<const char *>(
                                                        view.native_message.ptr),
                                                static_cast<qsizetype>(view.native_message.len))
                            : QString();
    m_errorNativeCode = view.has_native ? view.native_code : 0;
    m_errorLine = view.has_line_column ? static_cast<int>(view.line) : 0;
    m_errorColumn = view.has_line_column ? static_cast<int>(view.column) : 0;
    m_errorCharOffset = view.has_char_offset ? static_cast<int>(view.char_offset) : -1;
    Q_EMIT errorChanged();
}

void SessionController::takeThreadLocalError()
{
    // reldex.h: every non-OK return records the failure on *this* thread
    // first, so a take straight after one always describes that failure.
    const reldex::ErrorHandle error(reldex_last_error_take());
    adoptError(error.get());
}

quint64 SessionController::nextRequest(int expectedEventKind)
{
    const quint64 request = m_nextRequestId++;
    m_outstanding.insert(request, expectedEventKind);
    return request;
}

QList<ResultTableModel::ColumnDescription> SessionController::readResultColumns() const
{
    // ABI 3: the whole header, from the result itself, the moment EXECUTED is
    // drained -- no batch, no allocation, and an answer even for a result that
    // has columns and no rows.
    QList<ResultTableModel::ColumnDescription> columns;
    if (m_bridge == nullptr || m_sessionId == 0 || m_resultId == 0) {
        return columns;
    }
    const std::size_t count =
            reldex_session_result_column_count(m_bridge->hub(), m_sessionId, m_resultId);
    if (count == 0) {
        // Zero is the only way this call reports a failure, and it records a
        // thread-local error when it does. Take it so it cannot be mistaken
        // for the next call's failure -- a result with no columns is not a
        // case that reaches here (no columns means no result id at all).
        const reldex::ErrorHandle stale(reldex_last_error_take());
        Q_UNUSED(stale);
        return columns;
    }
    columns.reserve(static_cast<qsizetype>(count));
    for (std::size_t column = 0; column < count; ++column) {
        ReldexColumnInfo info = reldex::makeColumnInfo();
        ResultTableModel::ColumnDescription described;
        if (reldex_session_result_column(m_bridge->hub(), m_sessionId, m_resultId, column, &info)
            == RELDEX_STATUS_OK) {
            // A13: NUL-terminated, but `len` is authoritative. Deep-copied
            // here, which is what makes the "valid until you submit a close"
            // lifetime rule a non-issue for this adapter.
            described.name = QString::fromUtf8(reinterpret_cast<const char *>(info.name.ptr),
                                               static_cast<qsizetype>(info.name.len));
            described.kind = info.kind;
        }
        columns.append(described);
    }
    return columns;
}

bool SessionController::open()
{
    if (!checkThread() || m_bridge == nullptr || !m_bridge->isValid() || m_sessionId != 0) {
        return false;
    }
    clearError();

    ReldexOpenOptions options = reldex::makeOpenOptions();
    options.driver = RELDEX_DRIVER_KIND_MOCK;
    options.mock.scenario = RELDEX_MOCK_SCENARIO_S14;
    options.mock.rows = static_cast<quint64>(std::max<qint64>(0, m_mockRows));
    options.mock.seed = static_cast<quint64>(std::max<qint64>(0, m_mockSeed));
    options.mock.per_fetch_latency_us =
            static_cast<quint64>(std::max<qint64>(0, m_mockPerFetchLatencyUs));
    options.mock.first_batch_latency_us =
            static_cast<quint64>(std::max<qint64>(0, m_mockFirstBatchLatencyUs));
    options.mock.block_duration_ms =
            static_cast<quint64>(std::max<qint64>(0, m_mockBlockDurationMs));

    const quint64 request = nextRequest(RELDEX_EVENT_KIND_OPENED);
    ReldexSessionId session = 0;
    const ReldexStatus status =
            reldex_hub_open_session(m_bridge->hub(), &options, request, &session);
    if (status != RELDEX_STATUS_OK) {
        m_outstanding.remove(request);
        takeThreadLocalError();
        setState(Failed);
        Q_EMIT failed();
        return false;
    }
    m_sessionId = session;
    // Registering after the call is race-free: events are only ever dispatched
    // from this thread's event loop, and we are in it.
    m_bridge->registerSession(m_sessionId, this);
    Q_EMIT sessionIdChanged();
    setState(Opening);
    return true;
}

bool SessionController::executeMockStatement(int statement)
{
    const ReldexStr text = reldex_mock_statement(statement);
    if (text.ptr == nullptr || text.len == 0) {
        return false;
    }
    return execute(QString::fromUtf8(reinterpret_cast<const char *>(text.ptr),
                                     static_cast<qsizetype>(text.len)));
}

bool SessionController::execute(const QString &sql, qint64 deadlineMs)
{
    if (!checkThread() || m_bridge == nullptr || !m_bridge->isValid() || m_sessionId == 0) {
        return false;
    }
    clearError();

    // ABI 3 is explicit that a second execute does NOT close the first result:
    // it stays open, holding its server-side cursor and its rows, until the
    // caller closes it or the session ends. A worksheet that re-runs a query
    // would otherwise accumulate one open cursor per run, which on a real
    // database is a resource leak with a server-side limit attached. So the
    // result being replaced is closed here, explicitly.
    if (m_resultId != 0) {
        submitCloseResult(m_resultId);
    }

    m_model->beginResult({});
    m_resultId = 0;
    m_hasResult = false;
    m_exhausted = false;
    m_rowsFetched = 0;
    m_rowsAffected = -1;
    // Pending requests are the *view's* asks against the result being replaced,
    // so they go. `m_fetchesInFlight` deliberately does not: those fetches are
    // genuinely still outstanding, they still hold memory, and each will still
    // produce exactly one reply (A5). Counting them is what keeps the in-flight
    // bound a bound across a re-execute; the stale-reply path in `handleEvent`
    // is what hands each freed slot to the new result.
    m_pendingFetchRequests = 0;
    Q_EMIT rowsFetchedChanged();

    const QByteArray utf8 = sql.toUtf8();
    const ReldexStr text {
        reinterpret_cast<const std::uint8_t *>(utf8.constData()),
        static_cast<std::size_t>(utf8.size()),
    };
    const quint64 request = nextRequest(RELDEX_EVENT_KIND_EXECUTED);
    m_bridge->metrics()->markExecuteSubmitted();
    const ReldexStatus status =
            reldex_session_execute(m_bridge->hub(), m_sessionId, request, text,
                                   static_cast<quint64>(std::max<qint64>(0, deadlineMs)));
    if (status != RELDEX_STATUS_OK) {
        m_outstanding.remove(request);
        takeThreadLocalError();
        setState(Failed);
        Q_EMIT failed();
        return false;
    }
    setState(Executing);
    return true;
}

bool SessionController::submitCloseResult(quint64 result)
{
    if (!checkThread() || m_bridge == nullptr || !m_bridge->isValid() || m_sessionId == 0
        || result == 0) {
        return false;
    }
    const quint64 request = nextRequest(RELDEX_EVENT_KIND_RESULT_CLOSED);
    const ReldexStatus status =
            reldex_session_close_result(m_bridge->hub(), m_sessionId, request, result);
    if (status != RELDEX_STATUS_OK) {
        m_outstanding.remove(request);
        takeThreadLocalError();
        return false;
    }
    return true;
}

bool SessionController::closeResult()
{
    if (m_resultId == 0 || !submitCloseResult(m_resultId)) {
        return false;
    }
    m_resultId = 0;
    m_hasResult = false;
    m_exhausted = true;
    m_model->reset();
    return true;
}

bool SessionController::closeSession(int disposition)
{
    if (!checkThread() || m_bridge == nullptr || !m_bridge->isValid() || m_sessionId == 0) {
        return false;
    }
    const quint64 request = nextRequest(RELDEX_EVENT_KIND_SESSION_CLOSED);
    const ReldexStatus status =
            reldex_session_close(m_bridge->hub(), m_sessionId, request, disposition);
    if (status != RELDEX_STATUS_OK) {
        m_outstanding.remove(request);
        takeThreadLocalError();
        return false;
    }
    setState(Closing);
    return true;
}

bool SessionController::canFetchMoreRows() const
{
    // The question `QAbstractItemModel::canFetchMore()` is really asking is
    // "can more rows still appear for the result being shown".
    //
    // In-flight fetches are deliberately *not* a reason to answer no: rows are
    // on their way, and a view that was told no would stop asking. The bound on
    // how many requests may be outstanding belongs in `fetchMoreRows()` and
    // `submitFetches()`, which is where it is.
    if (!m_hasResult || m_exhausted || m_sessionId == 0 || m_resultId == 0) {
        return false;
    }
    // Past the model's ceiling every further batch would be refused on arrival,
    // so fetching it would cost a round trip and its memory for nothing.
    return m_model == nullptr || !m_model->rowLimitReached();
}

void SessionController::fetchMoreRows()
{
    if (!canFetchMoreRows()) {
        return;
    }
    if (m_autoFetch) {
        // The stream already keeps `maxFetchesInFlight` requests outstanding,
        // so a view's `fetchMore()` asks for nothing new. It must *not* queue a
        // pending request either: `submitFetches()` only consumes the pending
        // count when auto-fetch is off, so the counter would grow once per
        // `fetchMore()` for the life of the result and never come back down.
        submitFetches();
        return;
    }
    // Bounded by the same number as the in-flight limit: a view can ask many
    // times before the first reply lands, and a queue of requests that outlives
    // what the pipe can hold is just a number growing in a member.
    m_pendingFetchRequests = std::min(m_pendingFetchRequests + 1, m_maxFetchesInFlight);
    submitFetches();
}

void SessionController::submitFetches()
{
    if (!checkThread() || m_bridge == nullptr || !m_bridge->isValid() || !m_hasResult
        || m_exhausted) {
        return;
    }
    if (m_model != nullptr && m_model->rowLimitReached()) {
        return; // the model would refuse whatever came back
    }
    while (m_fetchesInFlight < m_maxFetchesInFlight
           && (m_autoFetch || m_pendingFetchRequests > 0)) {
        const quint64 request = nextRequest(RELDEX_EVENT_KIND_FETCHED);
        const ReldexStatus status =
                reldex_session_fetch(m_bridge->hub(), m_sessionId, request, m_resultId,
                                     static_cast<std::uint32_t>(std::max(1, m_fetchRows)));
        if (status != RELDEX_STATUS_OK) {
            m_outstanding.remove(request);
            takeThreadLocalError();
            setState(Failed);
            Q_EMIT failed();
            return;
        }
        ++m_fetchesInFlight;
        if (!m_autoFetch) {
            --m_pendingFetchRequests;
        }
    }
}

void SessionController::handleEvent(const ReldexEvent &raw, reldex::BatchHandle batch,
                                    reldex::ErrorHandle error)
{
    // Only a reply may touch the request bookkeeping. Everything else is
    // handled -- or ignored -- first (ABI 3.2): `EXECUTING`, `TERMINAL`,
    // `SERVER_OUTPUT` and `TRANSACTION_STATE` answer no request (`request ==
    // 0`; `EXECUTING` names its statement in `executing_request`), and a kind
    // this build does not know -- or `FETCHED_SEGMENT`, which it never asks
    // for -- is ignored (D7). The library already guarantees that only a
    // reply carries a request id; switching on the kind first keeps this
    // class from depending on that for anything but its assert.
    switch (raw.kind) {
    case RELDEX_EVENT_KIND_EXECUTING:
        return;
    case RELDEX_EVENT_KIND_TRANSACTION_STATE:
        if (m_transactionPossiblyActive != raw.transaction_possibly_active) {
            m_transactionPossiblyActive = raw.transaction_possibly_active;
            Q_EMIT transactionStateChanged(m_transactionPossiblyActive);
        }
        return;
    case RELDEX_EVENT_KIND_TERMINAL:
        handleTerminal(raw, error);
        return;
    case RELDEX_EVENT_KIND_OPENED:
    case RELDEX_EVENT_KIND_EXECUTED:
    case RELDEX_EVENT_KIND_FETCHED:
    case RELDEX_EVENT_KIND_RESULT_CLOSED:
    case RELDEX_EVENT_KIND_SESSION_CLOSED:
    case RELDEX_EVENT_KIND_COMPLETED:
    case RELDEX_EVENT_KIND_SERVER_OUTPUT_CONFIGURED:
        break;
    default:
        // SERVER_OUTPUT (its lines are released by the Bridge; the output
        // pane is M3.x's), FETCHED_SEGMENT, and every kind this build
        // predates.
        return;
    }

    // "Exactly one reply per accepted request" is a library guarantee, so it
    // is asserted in debug rather than defended against (ADR-0003 A5 rule 5).
    // `Q_ASSERT_X` compiles to nothing under this project's default
    // RelWithDebInfo build (ui/build.sh).
    const auto entry = m_outstanding.find(raw.request);
    const bool known = entry != m_outstanding.end();
    Q_ASSERT_X(known, "SessionController::handleEvent",
               "a reply arrived for a request that was never accepted, or was already replied to");
    if (known) {
        Q_ASSERT_X(entry.value() == raw.kind, "SessionController::handleEvent",
                   "the reply's kind does not match the request that was submitted");
        m_outstanding.erase(entry);
    }

    if (error) {
        adoptError(error.get());
    }

    if (m_terminated) {
        // The session's TERMINAL has been drained, so the state it set --
        // Failed on a loss, Closed otherwise -- is final. A reply can still
        // arrive after it, for a request submitted after the session ended
        // but before its TERMINAL was drained (always a failure, or a close
        // settled as FAILED): it may clear bookkeeping and report its error,
        // never move the state. A close still says it was answered, because
        // whoever submitted it is waiting for exactly that.
        if (raw.kind == RELDEX_EVENT_KIND_SESSION_CLOSED) {
            m_closeOutcome = raw.close_outcome;
            Q_EMIT sessionClosed(raw.close_outcome, false);
        }
        return;
    }

    switch (raw.kind) {
    case RELDEX_EVENT_KIND_OPENED:
        m_cancelKind = raw.cancel_kind;
        if (error) {
            setState(Failed);
            Q_EMIT failed();
            return;
        }
        setState(Ready);
        Q_EMIT opened();
        if (m_runOnOpen) {
            executeMockStatement(RELDEX_MOCK_STATEMENT_GENERATED_QUERY);
        }
        return;

    case RELDEX_EVENT_KIND_EXECUTED:
        if (error) {
            setState(Failed);
            Q_EMIT failed();
            return;
        }
        m_rowsAffected = raw.has_rows_affected ? static_cast<qint64>(raw.rows_affected) : -1;
        if (raw.has_result) {
            m_resultId = raw.result;
            m_hasResult = true;
            // Headers first, complete, before a single row exists -- ABI 3's
            // result description rather than the first batch's.
            m_model->beginResult(readResultColumns());
            Q_ASSERT_X(m_model->columnCount() == static_cast<int>(raw.column_count),
                       "SessionController::handleEvent",
                       "the result description disagrees with the EXECUTED event's column_count");
            setState(Fetching);
            Q_EMIT executed();
            submitFetches();
            return;
        }
        setState(Ready);
        Q_EMIT executed();
        return;

    case RELDEX_EVENT_KIND_FETCHED: {
        m_fetchesInFlight = std::max(0, m_fetchesInFlight - 1);
        // ABI 3: the event names the result the fetch was issued against, so
        // there is nothing to look up and nothing that can get out of step.
        const quint64 fetchedResult = raw.has_result ? raw.result : 0;
        if (!m_hasResult || fetchedResult != m_resultId) {
            // The result this fetch was issued against has been closed or
            // replaced. Exactly one reply still arrives for it (A5 rule 5);
            // the batch it carries is released as this handle goes away.
            //
            // The slot it just freed belongs to the *current* result, though.
            // Without this the new result would run permanently short of
            // in-flight fetches -- or, if every slot was held by the old one,
            // stall outright with nothing left to wake it.
            submitFetches();
            return;
        }
        if (error) {
            setState(Failed);
            Q_EMIT failed();
            return;
        }
        const auto rows = static_cast<int>(raw.row_count);
        if (rows > 0) {
            // Spike S15's K4 is exactly this call: the batch is "described and
            // its columns viewable" when `applyBatch` returns, because that is
            // where `reldex_batch_column` is called once per column. Both
            // clock reads are behind the same off-by-default flag as the
            // recorder, so a normal build pays one relaxed atomic load here.
            Metrics *const metrics = m_bridge->metrics();
            const bool timing = metrics->isEnabled();
            const qint64 applyStartNs = timing ? metrics->nowNs() : 0;
            m_model->applyBatch(std::move(batch), rows);
            if (timing) {
                metrics->recordApplyBatch(metrics->nowNs() - applyStartNs);
            }
            m_rowsFetched += rows;
            Q_EMIT rowsFetchedChanged();
            m_bridge->metrics()->markFirstRowsInserted();
        } else {
            // A batch with no rows means the result is exhausted; the model
            // still takes it, to read column names if it has none yet.
            m_model->applyBatch(std::move(batch), 0);
            m_exhausted = true;
        }
        if (m_exhausted && m_fetchesInFlight == 0) {
            m_bridge->metrics()->markResultComplete(m_rowsFetched);
            setState(ResultComplete);
            Q_EMIT resultComplete();
            return;
        }
        submitFetches();
        return;
    }

    case RELDEX_EVENT_KIND_RESULT_CLOSED:
        return;

    case RELDEX_EVENT_KIND_SESSION_CLOSED:
        m_closeOutcome = raw.close_outcome;
        setState(raw.session_still_open ? Ready : Closed);
        Q_EMIT sessionClosed(raw.close_outcome, raw.session_still_open);
        return;

    default:
        return;
    }
}

void SessionController::handleTerminal(const ReldexEvent &raw, const reldex::ErrorHandle &error)
{
    // Since ABI 3.2 this also arrives the moment a session is lost
    // mid-statement, with nobody having asked to close it. Replies to requests
    // accepted before then may still follow, so `m_outstanding` is left alone.
    m_terminated = true;
    m_transactionPossiblyLost = raw.transaction_possibly_lost;
    if (error) {
        adoptError(error.get());
    }
    // TERMINAL is authoritative (`SPEC.md` §10). A lost session is Failed
    // even if a close settled first and said Closed; an ended one is Closed
    // unless something already failed it.
    if (raw.session_state == RELDEX_SESSION_STATE_LOST) {
        if (m_state != Failed) {
            setState(Failed);
            Q_EMIT failed();
        }
    } else if (m_state != Closed && m_state != Failed) {
        setState(Closed);
    }
    // Whatever was open went with the session; whether it was lost is
    // `transactionPossiblyLost`, not this flag.
    if (m_transactionPossiblyActive) {
        m_transactionPossiblyActive = false;
        Q_EMIT transactionStateChanged(false);
    }
    Q_EMIT terminated(raw.transaction_possibly_lost, raw.abandoned);
}

void SessionController::setMaxFetchesInFlight(int fetches)
{
    const int clamped = std::max(1, fetches);
    if (clamped == m_maxFetchesInFlight) {
        return;
    }
    m_maxFetchesInFlight = clamped;
    Q_EMIT maxFetchesInFlightChanged();
}

void SessionController::setFetchRows(int rows)
{
    const int clamped = std::max(1, rows);
    if (clamped == m_fetchRows) {
        return;
    }
    m_fetchRows = clamped;
    Q_EMIT fetchRowsChanged();
}

void SessionController::setAutoFetch(bool automatic)
{
    if (automatic == m_autoFetch) {
        return;
    }
    m_autoFetch = automatic;
    Q_EMIT autoFetchChanged();
}

void SessionController::setRunOnOpen(bool run)
{
    if (run == m_runOnOpen) {
        return;
    }
    m_runOnOpen = run;
    Q_EMIT runOnOpenChanged();
}

void SessionController::setMockRows(qint64 rows)
{
    if (rows == m_mockRows) {
        return;
    }
    m_mockRows = rows;
    Q_EMIT mockConfigChanged();
}

void SessionController::setMockSeed(qint64 seed)
{
    if (seed == m_mockSeed) {
        return;
    }
    m_mockSeed = seed;
    Q_EMIT mockConfigChanged();
}

void SessionController::setMockPerFetchLatencyUs(qint64 microseconds)
{
    if (microseconds == m_mockPerFetchLatencyUs) {
        return;
    }
    m_mockPerFetchLatencyUs = microseconds;
    Q_EMIT mockConfigChanged();
}

void SessionController::setMockFirstBatchLatencyUs(qint64 microseconds)
{
    if (microseconds == m_mockFirstBatchLatencyUs) {
        return;
    }
    m_mockFirstBatchLatencyUs = microseconds;
    Q_EMIT mockConfigChanged();
}

void SessionController::setMockBlockDurationMs(qint64 milliseconds)
{
    if (milliseconds == m_mockBlockDurationMs) {
        return;
    }
    m_mockBlockDurationMs = milliseconds;
    Q_EMIT mockConfigChanged();
}
