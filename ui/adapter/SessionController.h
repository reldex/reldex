#pragma once

// `SessionController` -- ADR-0003 D1's session-level object.
//
// It sequences calls and owns handles, and that is all it does: open, execute,
// fetch with a bounded number of fetches in flight, close, and surface state
// and errors as Qt properties and signals. No business rule lives here and
// none lives in QML (`AGENTS.md`).
//
// M3.3 adds the worksheet's connect flow on top of that machinery -- a second,
// coarser state (`connectState`) beside the statement-level `state`:
//
//   NotConnected/ConnectFailed/TimedOut/Cancelled/Lost --connectProfile()--> Preparing
//   Preparing --parameters ready--> Connecting (open submitted, timer armed)
//   Preparing --password needed--> AwaitingPassword --submitPassword()--> Preparing
//   Connecting --OPENED--> Connected | ConnectFailed | TimedOut (limit reached)
//   Connected --disconnectSession()--> Disconnecting --TERMINAL--> NotConnected
//   Connected --TERMINAL (lost)--> Lost
//   Preparing/AwaitingPassword/Connecting --cancelConnect()--> Cancelled, at once
//
// The workspace half (connect parameters joined to `resolve_password` on the
// service thread, saving a typed password after success, the "stored
// password was refused" memory) is `ConnectionManager`'s. A cancelled or
// timed-out connect is abandoned (`reldex_session_abandon`, M2.15): whatever
// that session still reports is consumed here and never adopted. See
// ui/README.md "Connect flow (M3.3)".
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

#include <QElapsedTimer>
#include <QHash>
#include <QObject>
#include <QPointer>
#include <QSet>
#include <QString>
#include <QtQml/qqmlregistration.h>

class Bridge;
class ConnectionManager;
class QTimer;

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
    /// The error's rendered cause chain (`ReldexErrorView::cause`), or empty.
    Q_PROPERTY(QString errorCause READ errorCause NOTIFY errorChanged)
    /// A short, stable key for the error's kind (`error.authentication`,
    /// `error.timeout`, ...), the same table `ConnectionManager` uses; QML
    /// turns it into a translated label.
    Q_PROPERTY(QString errorKey READ errorKey NOTIFY errorChanged)

    // --- M3.3: the connect flow ---------------------------------------------
    Q_PROPERTY(ConnectState connectState READ connectState NOTIFY connectStateChanged)
    /// The profile being connected, or last connected (hex id, as
    /// `ProfileModel`'s `profileId` role spells it).
    Q_PROPERTY(QString connectProfileId READ connectProfileId NOTIFY connectStateChanged)
    Q_PROPERTY(QString connectProfileName READ connectProfileName NOTIFY connectStateChanged)
    /// The connect limit this attempt runs under, from the settings registry
    /// (`CONNECT_TIMEOUT`, application then profile level), in seconds; 0
    /// means no limit was set.
    Q_PROPERTY(int connectTimeoutSeconds READ connectTimeoutSeconds NOTIFY connectStateChanged)
    /// Why the user is being asked for a password (`AwaitingPassword`).
    Q_PROPERTY(PasswordPromptReason passwordPromptReason READ passwordPromptReason NOTIFY
                       passwordPromptChanged)
    /// For `StoreFailed`: the credential store's own, value-free message
    /// (e.g. that the saved entry is not one Reldex wrote).
    Q_PROPERTY(QString passwordPromptDetail READ passwordPromptDetail NOTIFY passwordPromptChanged)
    /// Whether the prompt may offer to save the password (a store exists and
    /// can hold it). Saving happens only after the connect succeeds.
    Q_PROPERTY(bool offerSavePassword READ offerSavePassword NOTIFY passwordPromptChanged)
    /// The last `TRANSACTION_STATE`; the disconnect confirmation reads it.
    Q_PROPERTY(bool transactionPossiblyActive READ transactionPossiblyActive NOTIFY
                       transactionStateChanged)
    /// What the UI may offer now -- decided here, not in QML.
    Q_PROPERTY(bool canConnect READ canConnect NOTIFY canConnectChanged)
    Q_PROPERTY(bool canCancelConnect READ canCancelConnect NOTIFY connectStateChanged)
    Q_PROPERTY(bool canDisconnect READ canDisconnect NOTIFY connectStateChanged)

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

    /// M3.4: whether the profile this worksheet's session is bound to should
    /// show the persistent production indicator (SPEC.md §17; ADR-0006 P3).
    /// This is `Profile::treat_as_production()` itself, carried across the
    /// adapter boundary as a plain bool -- QML never sees, and never derives
    /// this from, `ReldexEnvironmentKind`. Since M3.3 the connect flow sets
    /// it from the profile's `treatAsProduction` when the session opens, and
    /// clears it when the session closes or is lost. Writable only so a test
    /// can drive the indicator directly.
    Q_PROPERTY(bool activeProfileIsProduction READ activeProfileIsProduction WRITE
                       setActiveProfileIsProduction NOTIFY activeProfileIsProductionChanged)

    // --- the mock scenario that open() uses (connectProfile() opens Oracle) ---
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

    /// M3.3: where the worksheet's connection is. See the class comment for
    /// the transitions.
    enum ConnectState {
        NotConnected,
        /// The workspace service thread is preparing the parameters.
        Preparing,
        /// The user must type a password; `passwordPromptReason` says why.
        AwaitingPassword,
        /// The session is connecting: cancellable, and bounded by
        /// `connectTimeoutSeconds`.
        Connecting,
        Connected,
        Disconnecting,
        /// The connect failed; the error properties say how.
        ConnectFailed,
        /// The connect limit passed first. The attempt was abandoned.
        TimedOut,
        /// The user cancelled. Nothing arriving later is adopted.
        Cancelled,
        /// The connected session was lost; `transactionPossiblyLost` says
        /// whether work went with it.
        Lost,
    };
    Q_ENUM(ConnectState)

    /// M3.3: why a password is asked for, by name (ADR-0007 S3's
    /// `PromptReason`, plus the adapter's own refused-password case).
    enum PasswordPromptReason {
        NoPrompt,
        /// The profile asks at every connect.
        PromptEachTime,
        /// Nothing is saved for this profile.
        NotStored,
        /// No credential store on this platform: every connect prompts.
        StoreUnavailable,
        /// The store could not be read; `passwordPromptDetail` says why.
        StoreFailed,
        /// The database refused the saved password earlier this run. It is
        /// never retried automatically; typing one may replace it.
        StoredPasswordRefused,
    };
    Q_ENUM(PasswordPromptReason)

    /// `disconnectSession()`'s argument, named for QML (`ReldexCloseDisposition`).
    enum DisconnectChoice {
        /// Refused by the core while a transaction may be open.
        DisconnectOnly = RELDEX_CLOSE_DISPOSITION_NONE,
        CommitAndDisconnect = RELDEX_CLOSE_DISPOSITION_COMMIT,
        RollbackAndDisconnect = RELDEX_CLOSE_DISPOSITION_ROLLBACK,
    };
    Q_ENUM(DisconnectChoice)

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
    /// False once the session has ended (`isTerminated()`): whether what was
    /// open was lost is `transactionPossiblyLost()`.
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
    [[nodiscard]] QString errorCause() const { return m_errorCause; }
    [[nodiscard]] QString errorKey() const;

    [[nodiscard]] bool canConnect() const noexcept;
    [[nodiscard]] bool canCancelConnect() const noexcept
    {
        return m_connectState == Preparing || m_connectState == AwaitingPassword
                || m_connectState == Connecting;
    }
    [[nodiscard]] bool canDisconnect() const noexcept { return m_connectState == Connected; }

    [[nodiscard]] ConnectState connectState() const noexcept { return m_connectState; }
    [[nodiscard]] QString connectProfileId() const
    {
        return QString::fromLatin1(m_connectProfileId.toHex());
    }
    [[nodiscard]] QString connectProfileName() const { return m_connectProfileName; }
    [[nodiscard]] int connectTimeoutSeconds() const noexcept { return m_connectTimeoutSeconds; }
    [[nodiscard]] PasswordPromptReason passwordPromptReason() const noexcept
    {
        return m_passwordPromptReason;
    }
    [[nodiscard]] QString passwordPromptDetail() const { return m_passwordPromptDetail; }
    [[nodiscard]] bool offerSavePassword() const noexcept { return m_offerSavePassword; }
    /// Wall-clock milliseconds from submitting the last successful open to
    /// its `OPENED` (M3.3's measurement), or -1.
    [[nodiscard]] qint64 lastConnectMs() const noexcept { return m_lastConnectMs; }
    /// Whether the last connect used a password the store handed out.
    [[nodiscard]] bool usedStoredPassword() const noexcept { return m_usedStoredPassword; }

    /// Test-only: which driver `connectProfile()` opens once the parameters
    /// are prepared. `RELDEX_DRIVER_KIND_ORACLE` (the default) opens what
    /// was prepared; `RELDEX_DRIVER_KIND_MOCK` opens the mock world below
    /// instead, so the adapter's connect flow can be driven with no database
    /// -- the prepare step, the prompt, the settings-resolved limit and the
    /// save still run for real.
    void setConnectDriverForTesting(int driverKind) { m_connectDriver = driverKind; }
    /// Test-only, with the mock connect driver: a `ReldexMockFailure` every
    /// connect fails with.
    void setMockConnectFailure(int failure) { m_mockConnectFailure = failure; }
    /// Test-only, with the mock connect driver: park every connect until
    /// `Bridge::releaseMockBlock()`.
    void setMockBlockConnect(bool block) { m_mockBlockConnect = block; }

    [[nodiscard]] int maxFetchesInFlight() const noexcept { return m_maxFetchesInFlight; }
    void setMaxFetchesInFlight(int fetches);
    [[nodiscard]] int fetchRows() const noexcept { return m_fetchRows; }
    void setFetchRows(int rows);
    [[nodiscard]] bool autoFetch() const noexcept { return m_autoFetch; }
    void setAutoFetch(bool automatic);
    [[nodiscard]] bool runOnOpen() const noexcept { return m_runOnOpen; }
    void setRunOnOpen(bool run);

    [[nodiscard]] bool activeProfileIsProduction() const noexcept
    {
        return m_activeProfileIsProduction;
    }
    void setActiveProfileIsProduction(bool production);

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

    // --- M3.3: the connect flow. None of these blocks: each submits and
    // returns; the outcome arrives as `connectState` changes. ---------------

    /// Connects this worksheet to the profile `profileIdHex`. Refused (false)
    /// while a session is open or a connect is in flight: a worksheet's
    /// session is never silently replaced -- disconnect first.
    Q_INVOKABLE bool connectProfile(const QString &profileIdHex);
    /// Answers `AwaitingPassword`. The text crosses into Reldex once, as a
    /// zeroizing `ReldexSecret` (`reldex_secret_from_utf8`); it is never
    /// logged or kept in a property. With `savePassword` (and
    /// `offerSavePassword`), it is saved -- after the connect succeeds, and
    /// only then (ADR-0007).
    Q_INVOKABLE bool submitPassword(const QString &password, bool savePassword);
    /// Cancels a connect in flight and returns at once. A connecting session
    /// is abandoned; whatever it reports later is consumed, never adopted.
    Q_INVOKABLE bool cancelConnect();
    /// Closes the connected session with `disposition` (a
    /// `ReldexCloseDisposition`). With a transaction possibly open, `NONE`
    /// is refused by the core -- the session stays open and the error says
    /// why -- so the UI asks first (`SPEC.md` §10).
    Q_INVOKABLE bool disconnectSession(int disposition = RELDEX_CLOSE_DISPOSITION_NONE);

    /// `ConnectionManager`'s drain hands over the `CONNECT_PREPARED` reply
    /// here; the summary it carries is released after this returns.
    void handleConnectPrepared(const ReldexWorkspaceReply &reply);

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
    void activeProfileIsProductionChanged();
    void mockConfigChanged();
    void connectStateChanged();
    void passwordPromptChanged();
    void canConnectChanged();

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

    // --- M3.3 ---------------------------------------------------------------
    [[nodiscard]] ConnectionManager *connections() const;
    void setConnectState(ConnectState state);
    void setPasswordPrompt(PasswordPromptReason reason, const QString &detail, bool offerSave);
    /// Adopts the connection manager's last error as this controller's.
    void adoptManagerError(const ConnectionManager *manager);
    /// Sets an error the adapter itself raised (no `ReldexError` exists).
    void setAdapterError(int kind, const QString &message);
    /// Opens the prepared session (or, for a test, the mock).
    void openPrepared(const ReldexConnectSummary *summary, int limitSeconds);
    /// The `OPENED` of the connect in flight.
    void finishConnect(bool ok);
    void onConnectTimeout();
    /// Abandons the connecting session; its later events are consumed.
    void abandonConnectingSession();
    /// Stops treating the current session as this worksheet's: it has ended
    /// (or will), and its remaining events are consumed as it retires.
    void retireCurrentSession();
    /// Forgets a session whose `TERMINAL` has been drained, so the next
    /// connect starts clean.
    void recycleEndedSession();

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
    QString m_errorCause;

    // --- M3.3 connect flow ---------------------------------------------------
    ConnectState m_connectState = NotConnected;
    QByteArray m_connectProfileId;
    QString m_connectProfileName;
    /// `Profile::treat_as_production()` of the profile being connected;
    /// becomes `activeProfileIsProduction` once the session opens (M3.4).
    bool m_connectProfileIsProduction = false;
    int m_connectTimeoutSeconds = 0;
    PasswordPromptReason m_passwordPromptReason = NoPrompt;
    QString m_passwordPromptDetail;
    bool m_offerSavePassword = false;
    bool m_usedStoredPassword = false;
    /// The typed password the user asked to save, held (zeroizing) only until
    /// the connect it was typed for succeeds or fails.
    reldex::SecretHandle m_passwordToSave;
    QTimer *m_connectTimer = nullptr;
    QElapsedTimer m_connectClock;
    qint64 m_lastConnectMs = -1;
    quint64 m_connectOpenRequest = 0;
    /// Connect-flow sessions that already ended for this controller -- an
    /// abandoned connect, or an open that failed -- whose `TERMINAL` has not
    /// been drained yet: their events are consumed and ignored, then the id
    /// is unregistered. The worksheet can connect again meanwhile.
    QSet<quint64> m_retiringSessions;
    int m_connectDriver = RELDEX_DRIVER_KIND_ORACLE;
    int m_mockConnectFailure = RELDEX_MOCK_FAILURE_NONE;
    bool m_mockBlockConnect = false;

    int m_maxFetchesInFlight = 2;
    int m_fetchRows = 1000;
    bool m_autoFetch = true;
    bool m_runOnOpen = false;
    bool m_activeProfileIsProduction = false;

    qint64 m_mockRows = 0;
    qint64 m_mockSeed = 0;
    qint64 m_mockPerFetchLatencyUs = 0;
    qint64 m_mockFirstBatchLatencyUs = 0;
    qint64 m_mockBlockDurationMs = 0;
};
