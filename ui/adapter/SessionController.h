#pragma once

// `SessionController` -- ADR-0003 D1's session-level object.
//
// It sequences calls and owns handles, and that is all it does: open, execute,
// fetch with a bounded number of fetches in flight, close, and surface state
// and errors as Qt properties and signals. No business rule lives here and
// none lives in QML (`AGENTS.md`).
//
// Back-pressure lives here too, because it lives nowhere else: the hub's event
// queue is unbounded and a FETCHED event carries a batch that becomes the
// adapter's memory (A16). `maxFetchesInFlight` is the bound, and it is a bound
// on *outstanding requests*, not on requests per result: a re-execute leaves
// the replaced result's fetches in flight (they still hold memory and each
// still owes exactly one reply), so `m_fetchesInFlight` deliberately survives
// it and each stale reply hands its slot to the current result.

#include "ReldexHandles.h"
#include "ResultTableModel.h"

#include <QHash>
#include <QObject>
#include <QPointer>
#include <QString>
#include <QtQml/qqmlregistration.h>

class Bridge;

class SessionController : public QObject, public ResultFetchSource
{
    Q_OBJECT
    QML_ELEMENT
    QML_UNCREATABLE("SessionController is created by Bridge and reached as bridge.session")

    Q_PROPERTY(State state READ state NOTIFY stateChanged)
    Q_PROPERTY(ResultTableModel *model READ model CONSTANT)
    Q_PROPERTY(quint64 sessionId READ sessionId NOTIFY sessionIdChanged)
    Q_PROPERTY(qint64 rowsFetched READ rowsFetched NOTIFY rowsFetchedChanged)
    Q_PROPERTY(bool hasError READ hasError NOTIFY errorChanged)
    Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY errorChanged)
    Q_PROPERTY(QString errorNativeMessage READ errorNativeMessage NOTIFY errorChanged)
    Q_PROPERTY(int errorKind READ errorKind NOTIFY errorChanged)
    Q_PROPERTY(int errorNativeCode READ errorNativeCode NOTIFY errorChanged)
    Q_PROPERTY(int errorLine READ errorLine NOTIFY errorChanged)
    Q_PROPERTY(int errorColumn READ errorColumn NOTIFY errorChanged)
    Q_PROPERTY(int errorCharOffset READ errorCharOffset NOTIFY errorChanged)

    /// How many fetches may be outstanding at once. This is the adapter's
    /// back-pressure (A16); raising it trades memory for pipelining.
    Q_PROPERTY(int maxFetchesInFlight READ maxFetchesInFlight WRITE setMaxFetchesInFlight NOTIFY
                       maxFetchesInFlightChanged)
    /// `max_rows` per fetch.
    Q_PROPERTY(int fetchRows READ fetchRows WRITE setFetchRows NOTIFY fetchRowsChanged)
    /// Keep fetching until the result is exhausted, rather than waiting for
    /// `fetchMore()`.
    Q_PROPERTY(bool autoFetch READ autoFetch WRITE setAutoFetch NOTIFY autoFetchChanged)
    /// Run the mock generated query as soon as the session opens.
    Q_PROPERTY(bool runOnOpen READ runOnOpen WRITE setRunOnOpen NOTIFY runOnOpenChanged)

    // --- the mock scenario, which is the only driver this build can open ---
    Q_PROPERTY(qint64 mockRows READ mockRows WRITE setMockRows NOTIFY mockConfigChanged)
    Q_PROPERTY(qint64 mockSeed READ mockSeed WRITE setMockSeed NOTIFY mockConfigChanged)
    Q_PROPERTY(qint64 mockPerFetchLatencyUs READ mockPerFetchLatencyUs WRITE
                       setMockPerFetchLatencyUs NOTIFY mockConfigChanged)
    Q_PROPERTY(qint64 mockFirstBatchLatencyUs READ mockFirstBatchLatencyUs WRITE
                       setMockFirstBatchLatencyUs NOTIFY mockConfigChanged)
    Q_PROPERTY(qint64 mockBlockDurationMs READ mockBlockDurationMs WRITE setMockBlockDurationMs
                       NOTIFY mockConfigChanged)

public:
    enum State {
        Idle,
        Opening,
        Ready,
        Executing,
        Fetching,
        ResultComplete,
        Closing,
        Closed,
        Failed,
    };
    Q_ENUM(State)

    explicit SessionController(Bridge *bridge, QObject *parent = nullptr);
    ~SessionController() override;

    [[nodiscard]] State state() const noexcept { return m_state; }
    [[nodiscard]] ResultTableModel *model() const noexcept { return m_model; }
    [[nodiscard]] quint64 sessionId() const noexcept { return m_sessionId; }
    [[nodiscard]] quint64 resultId() const noexcept { return m_resultId; }
    [[nodiscard]] qint64 rowsFetched() const noexcept { return m_rowsFetched; }
    [[nodiscard]] int fetchesInFlight() const noexcept { return m_fetchesInFlight; }
    [[nodiscard]] bool isExhausted() const noexcept { return m_exhausted; }
    [[nodiscard]] int cancelKind() const noexcept { return m_cancelKind; }
    [[nodiscard]] int closeOutcome() const noexcept { return m_closeOutcome; }
    [[nodiscard]] qint64 rowsAffected() const noexcept { return m_rowsAffected; }
    /// Requests submitted and not yet answered.
    [[nodiscard]] int outstandingRequests() const noexcept
    {
        return static_cast<int>(m_outstanding.size());
    }
    /// The last `TRANSACTION_STATE` (ABI 3.2): a transaction may be open.
    [[nodiscard]] bool transactionPossiblyActive() const noexcept
    {
        return m_transactionPossiblyActive;
    }
    /// The session's `TERMINAL` has arrived; it is gone for good.
    [[nodiscard]] bool isTerminated() const noexcept { return m_terminated; }
    /// What that `TERMINAL` said about the transaction (`SPEC.md` §10).
    [[nodiscard]] bool transactionPossiblyLost() const noexcept { return m_transactionPossiblyLost; }

    [[nodiscard]] bool hasError() const noexcept { return m_hasError; }
    [[nodiscard]] QString errorMessage() const { return m_errorMessage; }
    [[nodiscard]] QString errorNativeMessage() const { return m_errorNativeMessage; }
    [[nodiscard]] int errorKind() const noexcept { return m_errorKind; }
    [[nodiscard]] int errorNativeCode() const noexcept { return m_errorNativeCode; }
    [[nodiscard]] int errorLine() const noexcept { return m_errorLine; }
    [[nodiscard]] int errorColumn() const noexcept { return m_errorColumn; }
    [[nodiscard]] int errorCharOffset() const noexcept { return m_errorCharOffset; }

    [[nodiscard]] int maxFetchesInFlight() const noexcept { return m_maxFetchesInFlight; }
    void setMaxFetchesInFlight(int fetches);
    [[nodiscard]] int fetchRows() const noexcept { return m_fetchRows; }
    void setFetchRows(int rows);
    [[nodiscard]] bool autoFetch() const noexcept { return m_autoFetch; }
    void setAutoFetch(bool automatic);
    [[nodiscard]] bool runOnOpen() const noexcept { return m_runOnOpen; }
    void setRunOnOpen(bool run);

    [[nodiscard]] qint64 mockRows() const noexcept { return m_mockRows; }
    void setMockRows(qint64 rows);
    [[nodiscard]] qint64 mockSeed() const noexcept { return m_mockSeed; }
    void setMockSeed(qint64 seed);
    [[nodiscard]] qint64 mockPerFetchLatencyUs() const noexcept { return m_mockPerFetchLatencyUs; }
    void setMockPerFetchLatencyUs(qint64 microseconds);
    [[nodiscard]] qint64 mockFirstBatchLatencyUs() const noexcept
    {
        return m_mockFirstBatchLatencyUs;
    }
    void setMockFirstBatchLatencyUs(qint64 microseconds);
    [[nodiscard]] qint64 mockBlockDurationMs() const noexcept { return m_mockBlockDurationMs; }
    void setMockBlockDurationMs(qint64 milliseconds);

    /// Opens a mock session. Never blocks: the answer arrives as an OPENED
    /// event.
    Q_INVOKABLE bool open();
    /// Submits one of `ReldexMockStatement`'s texts.
    Q_INVOKABLE bool executeMockStatement(int statement);
    Q_INVOKABLE bool execute(const QString &sql, qint64 deadlineMs = 0);
    Q_INVOKABLE bool closeResult();
    Q_INVOKABLE bool closeSession(int disposition = RELDEX_CLOSE_DISPOSITION_NONE);

    // --- ResultFetchSource -------------------------------------------------
    [[nodiscard]] bool canFetchMoreRows() const override;
    void fetchMoreRows() override;

    /// Called by `Bridge::drain()` with ownership of whatever the event
    /// carried.
    void handleEvent(const ReldexEvent &raw, reldex::BatchHandle batch, reldex::ErrorHandle error);

Q_SIGNALS:
    void stateChanged();
    void sessionIdChanged();
    void rowsFetchedChanged();
    void errorChanged();
    void maxFetchesInFlightChanged();
    void fetchRowsChanged();
    void autoFetchChanged();
    void runOnOpenChanged();
    void mockConfigChanged();

    void opened();
    void executed();
    /// Every row of the current result has been fetched.
    void resultComplete();
    void sessionClosed(int outcome, bool stillOpen);
    void failed();
    /// `TRANSACTION_STATE` flipped (ABI 3.2).
    void transactionStateChanged(bool possiblyActive);
    /// The session ended, however it ended (ABI 3.2 `TERMINAL`). The UI must
    /// surface `transactionPossiblyLost` (`SPEC.md` §10).
    void terminated(bool transactionPossiblyLost, bool abandoned);

private:
    void setState(State state);
    void clearError();
    void adoptError(const ReldexError *error);
    void takeThreadLocalError();
    void submitFetches();
    void handleTerminal(const ReldexEvent &raw, const reldex::ErrorHandle &error);
    /// Submits `close_result` for `result`. Used both by `closeResult()` and
    /// by `execute()`, which must not leave the result it replaces open.
    bool submitCloseResult(quint64 result);
    /// Reads the current result's column descriptions straight from the
    /// result (ABI 3), with no batch involved.
    [[nodiscard]] QList<ResultTableModel::ColumnDescription> readResultColumns() const;
    [[nodiscard]] quint64 nextRequest(int expectedEventKind);
    [[nodiscard]] bool checkThread() const;

    // Ownership/teardown rule (found by an ASan use-after-free on PR #47,
    // CI job qt-asan: `SessionController::closeSession` dereferencing an
    // already-freed `Bridge`): a `SessionController` does NOT own its
    // `Bridge` and must never assume it outlives the controller. In the real
    // app and in `tst_coreinfo`'s QML tests alike, `Bridge` is a QML-owned
    // sibling of whatever owns this controller (e.g. `Main.qml`'s `Bridge`
    // versus `ObjectBrowserModel`'s owned `SessionController`, both children
    // of the same window) -- nothing makes one a parent of the other, so
    // `QQmlApplicationEngine`'s teardown can (and, per the ASan report, does)
    // destroy `Bridge` before this controller, in whichever order the QML
    // object tree happens to list them. `QPointer`, not a raw pointer: every
    // existing `m_bridge == nullptr`/`!= nullptr` guard in this file already
    // assumed a raw pointer stays valid or is explicitly cleared, which is
    // false the instant `Bridge` is destroyed out from under a still-live
    // `SessionController` -- `QPointer` is what makes those guards actually
    // catch that instead of dereferencing freed memory (`Bridge::isValid()`
    // at `closeSession()`'s own guard was the crash site). A `SessionController`
    // whose `Bridge` has gone this way still needs to behave safely: every
    // `reldex_*` call this class makes already routes through a `m_bridge ==
    // nullptr` check first, so once that check starts telling the truth,
    // `open()`/`execute()`/`closeSession()`/etc. simply report failure the
    // same way they already do for "no bridge was ever set" -- no new
    // failure mode, just a guard that used to lie.
    QPointer<Bridge> m_bridge;
    ResultTableModel *m_model = nullptr;

    State m_state = Idle;
    quint64 m_sessionId = 0;
    quint64 m_resultId = 0;
    quint64 m_nextRequestId = 1;
    /// Request id -> the `ReldexEventKind` its one reply must carry. The
    /// library guarantees exactly one reply per accepted request, so that part
    /// is asserted in debug rather than defended against.
    ///
    /// It no longer records which result a fetch was issued against: ABI 3 has
    /// `FETCHED` carry `result`/`has_result` itself, so the event says what a
    /// side map used to have to remember.
    QHash<quint64, int> m_outstanding;

    qint64 m_rowsFetched = 0;
    qint64 m_rowsAffected = -1;
    int m_fetchesInFlight = 0;
    int m_pendingFetchRequests = 0;
    bool m_exhausted = false;
    bool m_hasResult = false;
    int m_cancelKind = RELDEX_CANCEL_KIND_UNKNOWN;
    int m_closeOutcome = RELDEX_CLOSE_OUTCOME_UNKNOWN;
    bool m_transactionPossiblyActive = false;
    bool m_terminated = false;
    bool m_transactionPossiblyLost = false;

    bool m_hasError = false;
    QString m_errorMessage;
    QString m_errorNativeMessage;
    int m_errorKind = RELDEX_ERROR_KIND_UNKNOWN;
    int m_errorNativeCode = 0;
    int m_errorLine = 0;
    int m_errorColumn = 0;
    int m_errorCharOffset = -1;

    int m_maxFetchesInFlight = 2;
    int m_fetchRows = 1000;
    bool m_autoFetch = true;
    bool m_runOnOpen = false;

    qint64 m_mockRows = 0;
    qint64 m_mockSeed = 0;
    qint64 m_mockPerFetchLatencyUs = 0;
    qint64 m_mockFirstBatchLatencyUs = 0;
    qint64 m_mockBlockDurationMs = 0;
};
