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
    m_outstanding.insert(request, Outstanding { expectedEventKind, m_resultId });
    return request;
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
    m_model->beginResult(0);
    m_resultId = 0;
    m_hasResult = false;
    m_exhausted = false;
    m_rowsFetched = 0;
    m_rowsAffected = -1;
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

bool SessionController::closeResult()
{
    if (!checkThread() || m_bridge == nullptr || !m_bridge->isValid() || m_sessionId == 0
        || m_resultId == 0) {
        return false;
    }
    const quint64 request = nextRequest(RELDEX_EVENT_KIND_RESULT_CLOSED);
    const ReldexStatus status =
            reldex_session_close_result(m_bridge->hub(), m_sessionId, request, m_resultId);
    if (status != RELDEX_STATUS_OK) {
        m_outstanding.remove(request);
        takeThreadLocalError();
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
    return m_hasResult && !m_exhausted && m_sessionId != 0 && m_resultId != 0;
}

void SessionController::fetchMoreRows()
{
    if (!canFetchMoreRows()) {
        return;
    }
    ++m_pendingFetchRequests;
    submitFetches();
}

void SessionController::submitFetches()
{
    if (!checkThread() || m_bridge == nullptr || !m_bridge->isValid() || !m_hasResult
        || m_exhausted) {
        return;
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
    // "Exactly one reply per accepted request" is a library guarantee, so it
    // is asserted in debug rather than defended against (ADR-0003 A5 rule 5).
    const auto entry = m_outstanding.find(raw.request);
    const bool known = entry != m_outstanding.end();
    Q_ASSERT_X(known, "SessionController::handleEvent",
               "an event arrived for a request that was never accepted, or was already replied to");
    Outstanding outstanding;
    if (known) {
        outstanding = entry.value();
        Q_ASSERT_X(outstanding.kind == raw.kind, "SessionController::handleEvent",
                   "the reply's kind does not match the request that was submitted");
        m_outstanding.erase(entry);
    }

    if (error) {
        adoptError(error.get());
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
            m_model->beginResult(static_cast<int>(raw.column_count));
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
        if (!m_hasResult || outstanding.result != m_resultId) {
            // The result this fetch was issued against has been closed or
            // replaced. Exactly one reply still arrives for it (A5 rule 5);
            // the batch it carries is released as this handle goes away.
            return;
        }
        if (error) {
            setState(Failed);
            Q_EMIT failed();
            return;
        }
        const auto rows = static_cast<int>(raw.row_count);
        if (rows > 0) {
            m_model->applyBatch(std::move(batch), rows);
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
        // D7: an event kind this build predates is unknown, never an assert.
        return;
    }
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
