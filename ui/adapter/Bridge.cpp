#include "Bridge.h"

#include "Metrics.h"
#include "SessionController.h"

#include <QByteArray>
#include <QElapsedTimer>
#include <QMetaObject>
#include <QThread>
#include <QtGlobal>

#include <algorithm>
#include <type_traits>

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
static void reldexBridgeWake(void *userData) noexcept
{
    reldexBridgeWakeImpl(userData);
}
}

// ABI 3: the header exports a `noexcept` function-pointer alias precisely so
// this rule stops being a comment. If the `noexcept` above is ever removed,
// this line fails to compile instead of the process failing at run time --
// which is what letting an exception cross into Rust would do (A18).
// `is_convertible` rather than `is_same`: binding is the property that matters,
// and a plain `void(*)(void*)` does NOT convert to a `noexcept` pointer, so
// dropping the `noexcept` still fails here -- without depending on whether a
// given compiler makes C language linkage part of a function type.
#ifdef RELDEX_HAVE_WAKE_FN_NOEXCEPT
static_assert(std::is_convertible_v<decltype(&reldexBridgeWake), ReldexWakeFnNoexcept>,
              "the waker trampoline must be noexcept: an exception escaping it unwinds through "
              "an extern \"C\" frame into Rust, which catch_unwind does not contain (A18)");
#else
#error "reldex.h did not define RELDEX_HAVE_WAKE_FN_NOEXCEPT; this adapter requires C++17 or later"
#endif

namespace {

bool autoRunRequested()
{
    const QByteArray value = qgetenv("RELDEX_S15_AUTORUN");
    return !value.isEmpty() && value != "0";
}

} // namespace

bool Bridge::checkThread(const char *what) const
{
    // A runtime check, not only a Q_ASSERT: A10's rule is about a release
    // build as much as a debug one, and a violation here is a use-after-free
    // rather than a wrong answer.
    if (thread() == QThread::currentThread()) {
        return true;
    }
    qCritical("Bridge::%s called from the wrong thread; every reldex_* call this adapter "
              "makes must be on the Bridge's own thread (ADR-0003 D5 rule 3, A10)",
              what);
    Q_ASSERT_X(false, "Bridge::checkThread", what);
    return false;
}

Bridge::Bridge(QObject *parent)
    : QObject(parent)
{
    // Order matters, and the reason is lifetime rather than taste: the waker
    // is registered LAST, after every member that a wake could reach exists.
    // Registering it earlier and then throwing (or failing) would leave Rust
    // holding a callback into storage whose destructor never runs, because a
    // constructor that does not complete has no destructor call to undo it.
    m_metrics = new Metrics(this);
    // Created unconditionally, like `Metrics`, so `bridge.scrollDriver` is
    // never null in QML even on a Bridge that refuses to start. It reads the
    // environment in its constructor and does nothing else unless a
    // measurement run asked for it.
    m_scrollDriver = new ScrollDriver(this, this);

    const quint32 abi = reldex_abi_version();
    if ((abi >> 16) != static_cast<quint32>(RELDEX_ABI_VERSION_MAJOR)) {
        // ADR-0003 D7: refuse to start, do not guess. `valid` stays false and
        // every operation on this Bridge is a no-op.
        qCritical("reldex-ffi ABI major %u does not match the header's %u; refusing to start",
                  abi >> 16, static_cast<quint32>(RELDEX_ABI_VERSION_MAJOR));
        return;
    }

    // May throw (it allocates a model and reads the environment). Nothing is
    // registered with Rust yet, so an unwind here destroys the QObject base,
    // which deletes m_metrics, and leaves nothing behind.
    m_session = new SessionController(this, this);

    // Held by a handle from the moment it exists, so every failure path below
    // -- and any exception -- destroys it exactly once.
    reldex::HubHandle hub(reldex_hub_create());
    if (!hub) {
        qCritical("reldex_hub_create() failed; this Bridge reports itself invalid");
        delete m_session;
        m_session = nullptr;
        return;
    }

    const ReldexStatus status = reldex_hub_set_waker(hub.get(), &reldexBridgeWake, this);
    if (status != RELDEX_STATUS_OK) {
        // Without a waker nothing would ever post a drain, so every request
        // would be submitted and never answered: a spinner that never stops,
        // which `SPEC.md` §2 ranks as the worst failure there is. Refuse to
        // start instead, loudly.
        const reldex::ErrorHandle error(reldex_last_error_take());
        qCritical("reldex_hub_set_waker() failed with status %d; this Bridge reports itself "
                  "invalid rather than never delivering an event",
                  static_cast<int>(status));
        delete m_session;
        m_session = nullptr;
        hub.reset(); // destroys the hub; no waker was registered
        return;
    }

    // Commit. `isValid()` is true from here and nowhere earlier.
    m_hub = std::move(hub);
}

Bridge::~Bridge()
{
    if (!m_hub) {
        return;
    }

    // A10, checked BEFORE the calls it guards rather than after them. The
    // result is deliberately discarded: a destructor that refused to release
    // the hub would turn a thread bug into a leak on top of it, so this reports
    // loudly (and asserts in a debug build) and then does the only useful
    // thing left.
    static_cast<void>(checkThread("~Bridge"));

    // ADR-0003 D5 rule 2, in this order and for these reasons:
    //
    // 1. Unregister the waker. reldex_hub_set_waker(hub, NULL, NULL) does not
    //    return while a wake is in progress, so from here on nothing can call
    //    back into this object -- which is what makes destroying it safe.
    reldex_hub_set_waker(m_hub.get(), nullptr, nullptr);

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
    m_hub.reset();

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
    return m_hub ? static_cast<qint64>(reldex_hub_pending_events(m_hub.get())) : 0;
}

bool Bridge::releaseMockBlock(quint64 session)
{
    if (!m_hub) {
        return false;
    }
    return reldex_mock_release_block(m_hub.get(), session) == RELDEX_STATUS_OK;
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
    if (m_scrollDriver != nullptr && m_scrollDriver->ownsStart()) {
        // A measurement run starts the query itself, a moment later, so that
        // its "before" memory sample is taken on a window that has already
        // drawn (ui/adapter/ScrollDriver.cpp).
        return false;
    }
    return autoRunRequested() && run();
}

void Bridge::postDrain()
{
    // Mirrors the waker's discipline: the coalescing flag must not be left set
    // by a post that never happened, or no drain would ever run again.
    try {
        if (m_drainPosted.fetchAndStoreOrdered(1) != 0) {
            return;
        }
        QMetaObject::invokeMethod(this, &Bridge::drain, Qt::QueuedConnection);
    } catch (...) {
        m_drainPosted.storeRelease(0);
        throw;
    }
}

void Bridge::drain()
{
    // Cleared *before* anything else: a wake that arrives from here on must be
    // able to post another drain, or its events could sit undrained until
    // something else happened to wake us.
    m_drainPosted.storeRelease(0);
    if (!m_hub) {
        return;
    }

    if (m_draining) {
        // A nested event loop -- a modal dialog, or a QEventLoop spun inside
        // something this drain called -- can deliver a posted drain while the
        // outer one is still walking the queue. Re-entering would dispatch the
        // same event twice and re-enter the model mid-signal, so: never nest,
        // re-post instead. The outer drain is still running and will keep
        // taking events; this one just gets back in line.
        postDrain();
        return;
    }

    // Plain RAII rather than a bool pair: `dispatch()` reaches application
    // code, which may throw.
    struct DrainGuard
    {
        bool &flag;
        explicit DrainGuard(bool &target) : flag(target) { flag = true; }
        ~DrainGuard() { flag = false; }
        DrainGuard(const DrainGuard &) = delete;
        DrainGuard &operator=(const DrainGuard &) = delete;
    } guard(m_draining);

    QElapsedTimer timer;
    timer.start();
    int events = 0;
    bool budgetHit = false;
    // S15 K4 wants the boundary's share of a drain separated from Qt's. With
    // metrics off this is one relaxed atomic load and nothing else.
    const bool timing = m_metrics->isEnabled();
    qint64 boundaryNs = 0;
    ReldexEvent raw = reldex::makeEvent();
    for (;;) {
        const qint64 takeStartNs = timing ? m_metrics->nowNs() : 0;
        const bool got = reldex_hub_next_event(m_hub.get(), &raw);
        if (timing) {
            boundaryNs += m_metrics->nowNs() - takeStartNs;
        }
        if (!got) {
            break;
        }
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
    m_metrics->recordDrain(events, timer.nsecsElapsed(), boundaryNs);

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
    while (reldex_hub_next_event(m_hub.get(), &raw)) {
        reldex::BatchHandle batch(raw.batch);
        reldex::ErrorHandle error(raw.error);
        raw = reldex::makeEvent();
    }
}
