#include "Bridge.h"

#include "Metrics.h"
#include "SessionController.h"

#include <QByteArray>
#include <QElapsedTimer>
#include <QMetaObject>
#include <QThread>
#include <QtGlobal>

#include <algorithm>

/// The waker trampoline's body, as a member-accessing free function so the
/// `extern "C"` entry point below stays three lines long.
void reldexBridgeWakeImpl(void *userData) noexcept
{
    // Runs on a Reldex pump thread, never on the Qt thread.
    //
    // reldex.h, "THE WAKER": must not block, must not call ANY reldex_*
    // function on any hub, and must not let a C++ exception escape -- that
    // would unwind through an extern "C" frame into Rust, which is undefined
    // behaviour that Rust's catch_unwind does not contain (A18).
    auto *bridge = static_cast<Bridge *>(userData);
    if (bridge == nullptr) {
        return;
    }
    try {
        // Coalesce: one posted drain at a time. A wake that arrives while a
        // drain is already posted needs no second post, and a wake that
        // arrives *during* a drain does post, because drain() clears this
        // flag before it takes its first event.
        if (bridge->m_drainPosted.fetchAndStoreOrdered(1) != 0) {
            return;
        }
        // The documented thread-safe way into a Qt event loop. `bridge` is the
        // context object, so the posted call is removed by ~QObject if the
        // Bridge dies before it is delivered -- and it cannot be delivered to
        // a half-destroyed Bridge anyway, because ~Bridge unregisters the
        // waker first and that call does not return while a wake is in
        // flight (D5 rule 2).
        QMetaObject::invokeMethod(bridge, &Bridge::drain, Qt::QueuedConnection);
    } catch (...) {
        // Nothing may propagate. Clear the flag so a later wake can still get
        // a drain posted.
        bridge->m_drainPosted.storeRelease(0);
    }
}

extern "C" {
static void reldexBridgeWake(void *userData)
{
    reldexBridgeWakeImpl(userData);
}
}

namespace {

bool autoRunRequested()
{
    const QByteArray value = qgetenv("RELDEX_S15_AUTORUN");
    return !value.isEmpty() && value != "0";
}

} // namespace

Bridge::Bridge(QObject *parent)
    : QObject(parent)
    , m_metrics(new Metrics(this))
{
    const quint32 abi = reldex_abi_version();
    if ((abi >> 16) != static_cast<quint32>(RELDEX_ABI_VERSION_MAJOR)) {
        // ADR-0003 D7: refuse to start, do not guess. `valid` stays false and
        // every operation on this Bridge is a no-op.
        qCritical("reldex-ffi ABI major %u does not match the header's %u; refusing to start",
                  abi >> 16, static_cast<quint32>(RELDEX_ABI_VERSION_MAJOR));
        return;
    }
    m_hub = reldex_hub_create();
    if (m_hub == nullptr) {
        qCritical("reldex_hub_create() failed");
        return;
    }
    reldex_hub_set_waker(m_hub, &reldexBridgeWake, this);
    m_session = new SessionController(this, this);
}

Bridge::~Bridge()
{
    if (m_hub == nullptr) {
        return;
    }

    // ADR-0003 D5 rule 2, in this order and for these reasons:
    //
    // 1. Unregister the waker. reldex_hub_set_waker(hub, NULL, NULL) does not
    //    return while a wake is in progress, so from here on nothing can call
    //    back into this object -- which is what makes destroying it safe.
    reldex_hub_set_waker(m_hub, nullptr, nullptr);

    // 2. Destroy the session and its model *now*, so every ReldexBatch they
    //    hold is released before the hub is destroyed (reldex_hub_destroy
    //    documents that precondition).
    delete m_session;
    m_session = nullptr;
    m_sessions.clear();

    // 3. Take and release whatever is still queued. A queued FETCHED event
    //    owns a batch, and an undrained batch is our memory (A16).
    drainAndRelease();

    // 4. Destroy the hub. A10: no other thread may be inside any reldex_*
    //    call, including reldex_session_request_cancel. This adapter makes
    //    *every* call on this object's thread and never uses the
    //    cancel-from-any-thread allowance, so there is no thread to join.
    Q_ASSERT(thread() == QThread::currentThread());
    reldex_hub_destroy(m_hub);
    m_hub = nullptr;

    // 5. Any drain this object posted for itself is discarded by ~QObject,
    //    which removes posted events for the object being destroyed.
}

void Bridge::setDrainEventBudget(int events)
{
    const int clamped = std::max(0, events);
    if (clamped == m_drainEventBudget) {
        return;
    }
    m_drainEventBudget = clamped;
    Q_EMIT drainEventBudgetChanged();
}

void Bridge::setDrainTimeBudgetMs(int milliseconds)
{
    const int clamped = std::max(0, milliseconds);
    if (clamped == m_drainTimeBudgetMs) {
        return;
    }
    m_drainTimeBudgetMs = clamped;
    Q_EMIT drainTimeBudgetMsChanged();
}

void Bridge::registerSession(quint64 id, SessionController *controller)
{
    m_sessions.insert(id, controller);
}

void Bridge::unregisterSession(quint64 id)
{
    m_sessions.remove(id);
}

qint64 Bridge::pendingEvents() const
{
    return m_hub == nullptr ? 0 : static_cast<qint64>(reldex_hub_pending_events(m_hub));
}

bool Bridge::releaseMockBlock(quint64 session)
{
    if (m_hub == nullptr) {
        return false;
    }
    return reldex_mock_release_block(m_hub, session) == RELDEX_STATUS_OK;
}

bool Bridge::run()
{
    if (m_session == nullptr) {
        return false;
    }
    if (m_session->sessionId() == 0) {
        m_session->setRunOnOpen(true);
        return m_session->open();
    }
    return m_session->executeMockStatement(RELDEX_MOCK_STATEMENT_GENERATED_QUERY);
}

bool Bridge::autoStart()
{
    return autoRunRequested() && run();
}

void Bridge::postDrain()
{
    if (m_drainPosted.fetchAndStoreOrdered(1) != 0) {
        return;
    }
    QMetaObject::invokeMethod(this, &Bridge::drain, Qt::QueuedConnection);
}

void Bridge::drain()
{
    // Cleared *before* the first event is taken: a wake that arrives during
    // this drain must be able to post another one, or its events could sit
    // undrained until something else happened to wake us.
    m_drainPosted.storeRelease(0);
    if (m_hub == nullptr) {
        return;
    }

    QElapsedTimer timer;
    timer.start();
    int events = 0;
    bool budgetHit = false;
    ReldexEvent raw = reldex::makeEvent();
    while (reldex_hub_next_event(m_hub, &raw)) {
        dispatch(raw);
        ++events;
        if (m_drainEventBudget > 0 && events >= m_drainEventBudget) {
            budgetHit = true;
            break;
        }
        if (m_drainTimeBudgetMs > 0 && timer.elapsed() >= m_drainTimeBudgetMs) {
            budgetHit = true;
            break;
        }
        raw = reldex::makeEvent();
    }

    ++m_drainCount;
    m_drainedEvents += events;
    m_metrics->recordDrain(events, timer.nsecsElapsed());

    if (budgetHit) {
        // reldex.h, "THE WAKER": the waker fires only on empty -> non-empty,
        // so a caller that stops while events remain gets no further wake and
        // must re-post its own drain (A16).
        postDrain();
    }
    Q_EMIT drained(events, budgetHit);
}

void Bridge::dispatch(ReldexEvent &raw)
{
    // Ownership is taken here, first thing and unconditionally, so no path
    // below can leak a batch or an error (ADR-0003 D3).
    reldex::BatchHandle batch(raw.batch);
    raw.batch = nullptr;
    reldex::ErrorHandle error(raw.error);
    raw.error = nullptr;

    m_metrics->markFirstEvent();

    const auto owner = m_sessions.constFind(raw.session);
    if (owner == m_sessions.cend() || owner->isNull()) {
        ++m_orphanEvents;
        return; // the handles release what the event carried
    }
    (*owner)->handleEvent(raw, std::move(batch), std::move(error));
}

void Bridge::drainAndRelease()
{
    ReldexEvent raw = reldex::makeEvent();
    while (reldex_hub_next_event(m_hub, &raw)) {
        reldex::BatchHandle batch(raw.batch);
        reldex::ErrorHandle error(raw.error);
        raw = reldex::makeEvent();
    }
}
