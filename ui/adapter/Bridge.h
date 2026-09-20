#pragma once

// `Bridge` -- ADR-0003 D1/D5: the one object that owns the `ReldexHub*`, holds
// the waker registration, and turns the wake into a budgeted drain on the Qt
// event loop.
//
// The three rules it exists to enforce, stated where they are implemented:
//
//  * the waker callback runs on a Reldex pump thread and does **exactly one**
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

#include <QAtomicInt>
#include <QHash>
#include <QObject>
#include <QPointer>
#include <QtQml/qqmlregistration.h>

#include "Metrics.h"
#include "ReldexHandles.h"
// Included rather than forward-declared: both are Q_PROPERTY types, and moc
// needs a complete type to register a pointer property.
#include "SessionController.h"

class Bridge : public QObject
{
    Q_OBJECT
    QML_ELEMENT

    /// False when the ABI major version does not match the header this was
    /// built against, when the hub could not be created, or when the waker
    /// could not be registered. ADR-0003 D7: the adapter refuses to start on a
    /// major mismatch rather than guessing -- and a `Bridge` that could not
    /// register its waker would never drain anything, which shows up as a
    /// spinner that never stops (the failure `SPEC.md` §2 ranks worst), so it
    /// reports itself invalid instead.
    Q_PROPERTY(bool valid READ isValid CONSTANT)
    Q_PROPERTY(SessionController *session READ session CONSTANT)
    Q_PROPERTY(Metrics *metrics READ metrics CONSTANT)
    /// ADR-0003 D5's budget: at most this many events per drain. 0 removes
    /// the limit. Writable so a test can force 1 and prove the re-post.
    Q_PROPERTY(int drainEventBudget READ drainEventBudget WRITE setDrainEventBudget NOTIFY
                       drainEventBudgetChanged)
    /// The other half of the budget, in milliseconds. 0 removes the limit.
    Q_PROPERTY(int drainTimeBudgetMs READ drainTimeBudgetMs WRITE setDrainTimeBudgetMs NOTIFY
                       drainTimeBudgetMsChanged)

public:
    explicit Bridge(QObject *parent = nullptr);
    ~Bridge() override;

    [[nodiscard]] bool isValid() const noexcept { return m_hub != nullptr; }
    [[nodiscard]] ReldexHub *hub() const noexcept { return m_hub.get(); }
    [[nodiscard]] SessionController *session() const noexcept { return m_session; }
    [[nodiscard]] Metrics *metrics() const noexcept { return m_metrics; }

    [[nodiscard]] int drainEventBudget() const noexcept { return m_drainEventBudget; }
    void setDrainEventBudget(int events);
    [[nodiscard]] int drainTimeBudgetMs() const noexcept { return m_drainTimeBudgetMs; }
    void setDrainTimeBudgetMs(int milliseconds);

    /// Routes this hub's events for `id` to `controller` until it is
    /// unregistered or destroyed.
    void registerSession(quint64 id, SessionController *controller);
    void unregisterSession(quint64 id);

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
    SessionController *m_session = nullptr;
    Metrics *m_metrics = nullptr;

    /// 1 while a drain is posted but has not started. Written from a Reldex
    /// pump thread (the waker) and from the Qt thread (`drain()`), so it is
    /// atomic; it coalesces the waker's post with the drain's own re-post.
    QAtomicInt m_drainPosted { 0 };

    /// True for the duration of a `drain()` body. Only ever touched on this
    /// object's thread, so a plain bool is enough -- what it guards against is
    /// re-entry through a nested event loop, not another thread.
    bool m_draining = false;

    QHash<quint64, QPointer<SessionController>> m_sessions;

    int m_drainEventBudget = 256; // ADR-0003 D5
    int m_drainTimeBudgetMs = 4; // ADR-0003 D5

    qint64 m_drainCount = 0;
    qint64 m_drainedEvents = 0;
    qint64 m_orphanEvents = 0;
};
