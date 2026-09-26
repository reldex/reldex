#pragma once

// `Bridge` -- ADR-0003 D1/D5: the one object that owns the `ReldexHub*`, holds
// the waker registration, and turns the wake into a budgeted drain on the Qt
// event loop.
//
// The three rules it exists to enforce, stated where they are implemented:
//
//  * the waker callback runs on a Reldex worker thread and does **exactly one**
//    thing -- a coalesced queued `invokeMethod` back to this object. It calls
//    no `reldex_*` function (the library answers `RELDEX_STATUS_REENTRANT`,
//    A3/A18) and lets no C++ exception escape into Rust (A18);
//  * `drain()` stops at a budget and **re-posts itself** when it stops with
//    the queue non-empty, because the waker is edge-triggered on empty ->
//    non-empty and no further wake is guaranteed (A16);
//  * teardown is D5 rule 2 in order: unregister the waker (which blocks until
//    an in-flight wake returns), let every batch go, then destroy the hub --
//    with no other thread anywhere inside a `reldex_*` call (A10).
//
// Two rules for anyone writing a slot that a drain can reach:
//
//  * `drain()` is never re-entered. A nested event loop (a modal dialog, a
//    `QEventLoop` spun inside a handler) can deliver a posted drain while an
//    outer one is still running; that delivery bails out and re-posts instead
//    of nesting, so no event is dispatched twice and none is lost;
//  * **destroy the `Bridge` with `deleteLater()`, never `delete`, from inside
//    anything a drain calls.** `~Bridge` tears the hub down underneath the
//    loop that is still walking it; `deleteLater()` defers that to the next
//    return to the event loop, which is after the drain has finished.
//
// Teardown order for every dependent this Bridge owns (M3.2 round-2 fix,
// 2026-09-26): a dependent that is BOTH a QObject child of this Bridge (e.g.
// `new ConnectionManager(this, this)`) AND reaches back into one of this
// Bridge's own data members from its own destructor (`m_hubSinks`, via
// `unregisterHubSink()`) must be deleted explicitly, in `~Bridge()`'s body,
// before that data member's destructor runs. C++ destroys a derived class's
// own data members (in reverse declaration order) only *after* that class's
// destructor body finishes, and only *then* does the `QObject` base
// destructor run `deleteChildren()` -- so a QObject child left for that
// automatic cleanup is destroyed after `m_hubSinks` (a plain `QHash` member,
// not a QObject) has already been destructed, and any callback into it from
// that child's destructor is a use-after-destruction. `m_session` and
// `m_connections` both follow this rule below; anything added later that
// shares this shape (a QObject child + a callback into a Bridge member on
// teardown) must too.

#include <QAtomicInt>
#include <QHash>
#include <QObject>
#include <QPointer>
#include <QtQml/qqmlregistration.h>

#include "Metrics.h"
#include "ReldexHandles.h"
// Included rather than forward-declared: all three are Q_PROPERTY types, and
// moc needs a complete type to register a pointer property.
#include "ScrollDriver.h"
#include "SessionController.h"

// `ConnectionManager` is a Q_PROPERTY type too (`connections`), for the same
// reason as the three above.
#include "ConnectionManager.h"
// M4.7: `ServerOutputController` (a Q_PROPERTY type, `serverOutput`) and its
// `SettingsController` (not QML-facing -- see that class's own doc comment).
#include "ServerOutputController.h"
#include "SettingsController.h"

class Bridge : public QObject
{
    Q_OBJECT
    QML_ELEMENT

    /// False when the library's ABI version cannot serve the header this was
    /// built against (`startError()` says why), when the hub could not be
    /// created, or when the waker could not be registered. ADR-0003 D7: the
    /// adapter refuses to start rather than guessing -- and a `Bridge` that
    /// could not register its waker would never drain anything, which shows
    /// up as a spinner that never stops (the failure `SPEC.md` §2 ranks
    /// worst), so it reports itself invalid instead.
    Q_PROPERTY(bool valid READ isValid CONSTANT)
    Q_PROPERTY(StartError startError READ startError CONSTANT)
    Q_PROPERTY(SessionController *session READ session CONSTANT)
    Q_PROPERTY(Metrics *metrics READ metrics CONSTANT)
    /// M3.2's connection manager: profile list, create/edit/delete,
    /// test-connect. Owns its own workspace service thread, independent of
    /// this Bridge's hub (ADR-0006 P6/M2.11 README "Threads").
    Q_PROPERTY(ConnectionManager *connections READ connections CONSTANT)
    /// M4.7: the DBMS_OUTPUT pane's adapter object. See that class's own
    /// documentation for what "the active worksheet's pane" means today.
    Q_PROPERTY(ServerOutputController *serverOutput READ serverOutput CONSTANT)
    /// Spike S15's measurement driver (M1.8). Inert unless the environment
    /// asks for a measurement run; see `ui/adapter/ScrollDriver.h`.
    Q_PROPERTY(ScrollDriver *scrollDriver READ scrollDriver CONSTANT)
    /// ADR-0003 D5's budget: at most this many events per drain. 0 removes
    /// the limit. Writable so a test can force 1 and prove the re-post.
    Q_PROPERTY(int drainEventBudget READ drainEventBudget WRITE setDrainEventBudget NOTIFY
                       drainEventBudgetChanged)
    /// The other half of the budget, in milliseconds. 0 removes the limit.
    Q_PROPERTY(int drainTimeBudgetMs READ drainTimeBudgetMs WRITE setDrainTimeBudgetMs NOTIFY
                       drainTimeBudgetMsChanged)

public:
    /// Why a `Bridge` refused to start. `None` on a valid one.
    enum class StartError {
        None,
        /// The library's ABI major differs from the header's: nothing can be
        /// assumed about any struct or function (D7).
        AbiMajorMismatch,
        /// Same major, but the library's minor is **older** than the header's.
        /// Fields and kinds this adapter was built to read may not exist in
        /// it, and a field an older library never fills can sit inside its
        /// struct's tail padding, where `struct_size` cannot say it is absent
        /// (ADR-0003 A38). A newer minor is fine: it only adds.
        AbiMinorTooOld,
        HubCreateFailed,
        WakerRegistrationFailed,
    };
    Q_ENUM(StartError)

    /// The ABI check alone, for a library reporting `libraryVersion`
    /// (`reldex_abi_version()`'s encoding) against this build's header.
    [[nodiscard]] static StartError checkAbiVersion(quint32 libraryVersion) noexcept;

    explicit Bridge(QObject *parent = nullptr);
    ~Bridge() override;

    [[nodiscard]] bool isValid() const noexcept { return m_hub != nullptr; }
    [[nodiscard]] StartError startError() const noexcept { return m_startError; }
    [[nodiscard]] ReldexHub *hub() const noexcept { return m_hub.get(); }
    [[nodiscard]] SessionController *session() const noexcept { return m_session; }
    [[nodiscard]] Metrics *metrics() const noexcept { return m_metrics; }
    [[nodiscard]] ScrollDriver *scrollDriver() const noexcept { return m_scrollDriver; }
    [[nodiscard]] ConnectionManager *connections() const noexcept { return m_connections; }
    [[nodiscard]] ServerOutputController *serverOutput() const noexcept { return m_serverOutput; }
    /// C++-only (tests, and `ServerOutputController`'s own construction):
    /// not exposed to QML (`SettingsController`'s own doc comment explains
    /// why -- it is a generic settings gateway, not this pane's alone).
    [[nodiscard]] SettingsController *settings() const noexcept { return m_settings; }

    [[nodiscard]] int drainEventBudget() const noexcept { return m_drainEventBudget; }
    void setDrainEventBudget(int events);
    [[nodiscard]] int drainTimeBudgetMs() const noexcept { return m_drainTimeBudgetMs; }
    void setDrainTimeBudgetMs(int milliseconds);

    /// Routes this hub's events for `id` to `controller` until it is
    /// unregistered or destroyed.
    void registerSession(quint64 id, SessionController *controller);
    void unregisterSession(quint64 id);

    /// As `registerSession`, for `ConnectionManager`'s test-connect probe --
    /// a second, concrete consumer of hub events rather than a general
    /// interface, matching this class's existing direct coupling to
    /// `SessionController` (no `HubEventSink` abstraction exists yet; adding
    /// one for exactly one more caller would be speculative).
    void registerHubSink(quint64 id, ConnectionManager *sink);
    void unregisterHubSink(quint64 id);

    /// The hub's queue depth. Diagnostic only (A4): `drain()` loops on
    /// `reldex_hub_next_event` instead, which is the same information without
    /// a race.
    [[nodiscard]] Q_INVOKABLE qint64 pendingEvents() const;

    /// Mock-only: releases a statement parked by `ReldexMockStatement::Block`.
    Q_INVOKABLE bool releaseMockBlock(quint64 session);

    /// Opens the session if it is not open yet, then runs the mock generated
    /// query. The whole sequence lives here so QML has none of it
    /// (`AGENTS.md`: business rules do not belong in QML).
    Q_INVOKABLE bool run();

    /// `run()`, but only when the `RELDEX_S15_AUTORUN` environment variable
    /// asks for it. Called from QML's `Component.onCompleted`.
    Q_INVOKABLE bool autoStart();

    // --- diagnostics, for tests and for M1.8 ------------------------------
    [[nodiscard]] qint64 drainCount() const noexcept { return m_drainCount; }
    [[nodiscard]] qint64 drainedEvents() const noexcept { return m_drainedEvents; }
    /// Events whose session had no (live) owner. They are released, not lost
    /// silently, and counted here so a routing bug is visible.
    [[nodiscard]] qint64 orphanEvents() const noexcept { return m_orphanEvents; }

public Q_SLOTS:
    /// Takes events until the queue is empty or the budget is spent, then
    /// re-posts itself if it stopped early.
    void drain();

Q_SIGNALS:
    void drainEventBudgetChanged();
    void drainTimeBudgetMsChanged();
    /// Emitted at the end of every drain. `budgetHit` is true when the drain
    /// stopped early and re-posted itself.
    void drained(int events, bool budgetHit);

private:
    friend void reldexBridgeWakeImpl(void *userData) noexcept;

    void postDrain();
    void dispatch(ReldexEvent &raw);
    void drainAndRelease();
    [[nodiscard]] bool checkThread(const char *what) const;

    /// Set last, and only when the hub exists *and* its waker was registered:
    /// `isValid()` is exactly "this Bridge can deliver events".
    reldex::HubHandle m_hub;
    StartError m_startError = StartError::None;
    SessionController *m_session = nullptr;
    Metrics *m_metrics = nullptr;
    ScrollDriver *m_scrollDriver = nullptr;
    ConnectionManager *m_connections = nullptr;
    SettingsController *m_settings = nullptr;
    ServerOutputController *m_serverOutput = nullptr;

    /// 1 while a drain is posted but has not started. Written from a Reldex
    /// worker thread (the waker) and from the Qt thread (`drain()`), so it is
    /// atomic; it coalesces the waker's post with the drain's own re-post.
    QAtomicInt m_drainPosted { 0 };

    /// True for the duration of a `drain()` body. Only ever touched on this
    /// object's thread, so a plain bool is enough -- what it guards against is
    /// re-entry through a nested event loop, not another thread.
    bool m_draining = false;

    QHash<quint64, QPointer<SessionController>> m_sessions;
    QHash<quint64, QPointer<ConnectionManager>> m_hubSinks;

    int m_drainEventBudget = 256; // ADR-0003 D5
    int m_drainTimeBudgetMs = 4; // ADR-0003 D5

    qint64 m_drainCount = 0;
    qint64 m_drainedEvents = 0;
    qint64 m_orphanEvents = 0;
};
