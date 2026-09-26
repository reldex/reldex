//! The hub: `db-core`'s event queue and session registry, the edge-triggered
//! waker, and the object every other call hangs off (ADR-0003 D5/D7).
//!
//! # What the hub is made of
//!
//! One `db-core` [`EventQueue`] and the [`SessionRegistry`] that opens every
//! session bound to it (`docs/exec-plans/active/phase-1-m2-5-event-queue.md`
//! §3.1, §6, §7). Each session's worker thread pushes its events straight into
//! that queue; [`reldex_hub_next_event`] takes them out and translates them on
//! the caller's thread (`session::translate`). There is **no thread of this
//! crate's** between a worker and the adapter: nothing parks per session or
//! per request, and nothing polls, so no interval can sit between a row
//! arriving and the UI hearing about it — which is what spike S15's K2
//! (first-row latency) and K4 (per-batch cost) would otherwise measure.
//!
//! The waker is the queue's own ([`EventQueue::set_waker`]): edge-triggered on
//! empty → non-empty, called with no `db-core` lock held, and read-locked for
//! the duration of a wake so that unregistering waits for one in flight. This
//! crate adds only the re-entrancy guard around the adapter's callback.

use std::collections::HashMap;
use std::ffi::c_void;
use std::hash::{BuildHasherDefault, Hasher};
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use reldex_db_core::{
    EventCaps, EventQueue, RequestId, SessionEvent, SessionId, SessionManager, SessionRegistry,
    event_channel,
};

use crate::batch::ResultColumns;
use crate::error::set_last_argument_error;
use crate::event::ReldexEvent;
use crate::session::SessionEntry;
use crate::status::{ReldexStatus, WakerGuard, entry, entry_value};
use crate::strings::{check_out_struct, write_out_struct};

/// Called when the hub's event queue goes from empty to non-empty.
///
/// Edge-triggered and coalesced: a burst of completions produces **one** call.
/// It must not block and must not call back into `reldex_*` (ADR-0003 D5); any
/// FFI entry from inside it returns `RELDEX_STATUS_REENTRANT` rather than
/// deadlocking. The C++ trampoline should do exactly one thing:
/// `QMetaObject::invokeMethod(bridge, &Bridge::drain, Qt::QueuedConnection)`.
///
/// It is called from whichever thread made the queue non-empty: usually a
/// session's worker thread, but it **can be the caller's own thread**, inside
/// the `reldex_*` call that submitted a request — a request whose session has
/// already ended is answered on the spot, and if that answer is what made the
/// queue non-empty, the submitting thread is the one that wakes
/// (`phase-1-m2-5-event-queue.md` §6). A trampoline that only posts to an
/// event loop is correct either way.
///
/// The header's typedef for this is **hand-written** in `cbindgen.toml`'s
/// `after_includes`, not generated: cbindgen emits its typedefs above the
/// `extern "C"` block it opens for the functions, which in C++ would make this
/// a pointer to a C++-linkage function while `reldex_hub_set_waker` takes a
/// C-linkage one. If the parameter list here changes, change it there too —
/// `tests/header.rs` checks that the header declares it, not that the two
/// agree.
pub type ReldexWakeFn = Option<extern "C" fn(user_data: *mut c_void)>;

/// The adapter's callback, registered with `db-core`'s queue.
struct Wake {
    func: extern "C" fn(*mut c_void),
    user_data: *mut c_void,
}

// SAFETY: `user_data` is an opaque token this library never dereferences; it
// only hands it back to `func`. ADR-0003 D5 makes the adapter responsible for
// the callback being safe to invoke from a Reldex thread, and
// `reldex_hub_set_waker` guarantees the unregister side: `EventQueue::set_waker`
// does not return while a wake is in flight, so the adapter can tear its
// bridge down.
unsafe impl Send for Wake {}
// SAFETY: as above — `func` and `user_data` are only read, never written,
// after construction.
unsafe impl Sync for Wake {}

impl reldex_db_core::Waker for Wake {
    fn wake(&self) {
        // Any `reldex_*` call the callback makes is refused while this is held
        // — including on the caller's own thread, where a submit can wake —
        // rather than deadlocking or re-entering the queue it is being told
        // about (D5 rule 1).
        let _guard = WakerGuard::enter();
        (self.func)(self.user_data);
    }
}

/// What a reply will have to say that its `db-core` event does not: the
/// caller's own request id, and — for a fetch or a result close — which of the
/// session's results it names and the columns a batch is described with.
pub(crate) struct Pending {
    pub(crate) caller: u64,
    pub(crate) result: Option<u64>,
    pub(crate) columns: Option<Arc<ResultColumns>>,
}

impl Pending {
    pub(crate) const fn caller(caller: u64) -> Self {
        Self {
            caller,
            result: None,
            columns: None,
        }
    }
}

/// Decrements the live-hub count when the hub's memory is actually released,
/// without making [`ReldexHub`] itself a `Drop` type — `reldex_hub_destroy`
/// has to move the registry out of it.
struct LiveHub;

impl LiveHub {
    fn new() -> Self {
        crate::counters::created(crate::counters::Kind::Hub);
        Self
    }
}

impl Drop for LiveHub {
    fn drop(&mut self) {
        crate::counters::destroyed(crate::counters::Kind::Hub);
    }
}

/// The application hub: one event queue, one session registry, one waker.
///
/// Opaque to C. Create it with [`reldex_hub_create`] and release it with
/// [`reldex_hub_destroy`], last of all.
pub struct ReldexHub {
    /// `EventQueue` is `Send` but not `Sync` — one consumer, by type. The
    /// mutex is what lets the hub be shared with the one call that may come
    /// from another thread (`reldex_session_request_cancel`, which never
    /// touches it); on the drain path it is uncontended.
    queue: Mutex<EventQueue>,
    pub(crate) registry: SessionRegistry,
    pub(crate) sessions: Mutex<IdMap<Arc<SessionEntry>>>,
    /// Keyed by the `db-core` request id this hub allocated (never the
    /// caller's, which is caller-chosen and unchecked): an entry lives from
    /// the moment a request is accepted until its one reply is drained.
    pending: Mutex<IdMap<Pending>>,
    next_request: AtomicU64,
    /// Column descriptions of results that were open when their session was
    /// **lost** — nothing the caller did ended their documented lifetime, so
    /// they are freed only by [`reldex_hub_destroy`]; see
    /// [`crate::reldex_session_result_column`].
    orphaned_columns: Mutex<Vec<Arc<ResultColumns>>>,
    _live: LiveHub,
}

/// Hashes the `u64` ids this crate keys its maps by: one multiply, rather than
/// SipHash on every event drained. The keys are allocated here or by
/// `db-core`, never chosen by the caller, so hash flooding is not a concern.
#[derive(Default)]
pub(crate) struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0 ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
        }
    }

    fn write_u64(&mut self, id: u64) {
        // Fibonacci hashing: spreads sequential ids over the high bits, which
        // is where the table's control bytes come from.
        self.0 = id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

/// A map keyed by one of this crate's `u64` ids.
pub(crate) type IdMap<V> = HashMap<u64, V, BuildHasherDefault<IdHasher>>;

/// Locks `mutex`, recovering from poisoning: every structure behind one of
/// this crate's mutexes stays consistent across a panic, which `catch_unwind`
/// has already turned into `RELDEX_STATUS_PANIC`.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl ReldexHub {
    fn new() -> Self {
        let (sink, queue) = event_channel(EventCaps::new());
        Self {
            queue: Mutex::new(queue),
            registry: SessionRegistry::new(SessionManager::new(), sink),
            sessions: Mutex::new(IdMap::default()),
            pending: Mutex::new(IdMap::default()),
            next_request: AtomicU64::new(1),
            orphaned_columns: Mutex::new(Vec::new()),
            _live: LiveHub::new(),
        }
    }

    /// The live entry for `session`, if this hub still has one.
    pub(crate) fn session_entry(&self, session: u64) -> Option<Arc<SessionEntry>> {
        lock(&self.sessions).get(&session).map(Arc::clone)
    }

    /// Records what a request's reply will need and allocates the `db-core`
    /// request id it is submitted under. Undo with [`Self::abandon_request`]
    /// if the submit is refused.
    pub(crate) fn begin_request(&self, pending: Pending) -> RequestId {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        lock(&self.pending).insert(id, pending);
        RequestId(id)
    }

    /// Forgets a request `db-core` refused: nothing was accepted, so no reply
    /// will come for it.
    pub(crate) fn abandon_request(&self, request: RequestId) {
        lock(&self.pending).remove(&request.0);
    }

    /// What `event` owes the caller: taken out of the table when `event` is
    /// its request's one reply, only read when it is progress (`Executing`)
    /// that the reply will still follow.
    pub(crate) fn pending_for(&self, event: &SessionEvent) -> Option<Pending> {
        let request = event.request()?;
        let mut pending = lock(&self.pending);
        if event.is_reply() {
            pending.remove(&request.0)
        } else {
            pending
                .get(&request.0)
                .map(|known| Pending::caller(known.caller))
        }
    }

    /// Output lines this session lost that no delivered event has reported.
    pub(crate) fn pending_dropped_lines(&self, session: SessionId) -> u32 {
        lock(&self.queue).pending_dropped_lines(session)
    }

    /// Takes the next `db-core` event without translating it, for the unit
    /// tests that need what a translation consumes.
    #[cfg(all(test, feature = "mock-driver"))]
    pub(crate) fn take_raw(&self) -> Option<SessionEvent> {
        lock(&self.queue).next()
    }

    /// Keeps a lost session's column descriptions until the hub goes away.
    pub(crate) fn orphan_columns(&self, columns: Vec<Arc<ResultColumns>>) {
        if !columns.is_empty() {
            lock(&self.orphaned_columns).extend(columns);
        }
    }

    fn set_waker(&self, func: ReldexWakeFn, user_data: *mut c_void) {
        let waker =
            func.map(|func| Arc::new(Wake { func, user_data }) as Arc<dyn reldex_db_core::Waker>);
        // Does not return while a wake is in flight: `EventQueue` read-locks
        // its registration for the duration of every wake (D5 rule 2, spike
        // criterion K5).
        lock(&self.queue).set_waker(waker);
    }
}

/// Runs `body` with the hub the caller named, or reports why it could not.
///
/// The hub is reference-counted only so this can borrow the caller's
/// reference without touching the count; since M2.15 nothing else holds one,
/// so [`reldex_hub_destroy`] frees it before it returns.
///
/// # Safety
///
/// `hub` must be null, or a pointer [`reldex_hub_create`] returned that has not
/// been destroyed.
pub(crate) unsafe fn with_hub<T>(
    hub: *const ReldexHub,
    body: impl FnOnce(&Arc<ReldexHub>) -> T,
) -> Option<T> {
    if hub.is_null() || !hub.is_aligned() {
        return None;
    }
    // SAFETY: the caller promises `hub` came from `Arc::into_raw` in
    // `reldex_hub_create` and is still alive. `ManuallyDrop` keeps the strong
    // count unchanged, so this borrow neither frees the hub nor leaks a
    // reference.
    let arc = ManuallyDrop::new(unsafe { Arc::from_raw(hub) });
    Some(body(&arc))
}

/// Creates the hub. Returns `NULL` only if allocation failed.
///
/// One per application is the intended shape; nothing here forbids more — but
/// note that the re-entrancy guard is per **thread**, not per hub, so a waker
/// belonging to one hub may not call into a *different* hub either (see
/// [`reldex_hub_set_waker`]).
#[unsafe(no_mangle)]
pub extern "C" fn reldex_hub_create() -> *mut ReldexHub {
    entry_value(std::ptr::null_mut(), || {
        Arc::into_raw(Arc::new(ReldexHub::new())).cast_mut()
    })
}

/// Destroys the hub, after every batch and error it handed out has been
/// released.
///
/// Returns **promptly**, even when a session is blocked inside a statement
/// that cannot be interrupted. In order: the waker is unregistered (which
/// waits for any wake already in flight, D5 rule 2); every session still open
/// is **abandoned** through `db-core`'s session registry — which never
/// commits, asks the driver to cancel whatever is running, and never waits;
/// everything still queued is discarded, freeing what it held; and the hub's
/// memory is released. No `RELDEX_EVENT_KIND_TERMINAL` is delivered for those
/// sessions: nothing could observe it.
///
/// # What "promptly" costs when a statement cannot be interrupted
///
/// Prompt is not the same as finished, and the difference is worth stating
/// plainly. A session parked inside an uninterruptible driver call — which is
/// the *normal* case on `oracledb` 26.0.0-beta.3, whose cancel cannot reach a
/// running statement (ADR-0002 D2, spike S4) — cannot be stopped by this call.
/// Waiting for the registry's teardown — at most `db-core`'s
/// `DROP_SHUTDOWN_TIMEOUT` (500 ms) in total, however many sessions are stuck
/// — therefore happens on a short-lived thread of this library's, not on the
/// caller's. Past that bound a stuck session's worker thread and the
/// connection it owns are **detached**: they stay alive until the statement
/// returns on its own, or the process exits, and then close themselves. That
/// is a leak for as long as it lasts, bounded by the statement rather than by
/// Reldex, and it is the price of never blocking the UI thread on a
/// ten-second statement (spike criterion K6). Everything this library handed
/// out or counts (`reldex_live_counts`) is released before this returns.
///
/// Like dropping a `DatabaseSession`, this **never commits**: a transaction
/// still open when the hub is destroyed is rolled back by the server, exactly
/// as if the connection had died (`SPEC.md` §10).
///
/// # Safety
///
/// `hub` must be null, or a pointer [`reldex_hub_create`] returned that has
/// not already been destroyed.
///
/// No other thread may be inside *any* `reldex_*` call on this hub — and that
/// includes [`crate::reldex_session_request_cancel`], the one function this
/// ABI otherwise lets any thread call. Sequencing the two is the caller's
/// job: this library has no internal synchronisation for it, and a cancel
/// racing this call dereferences freed memory. See
/// [`crate::reldex_session_request_cancel`] for the rule in full.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_hub_destroy(hub: *mut ReldexHub) {
    entry_value((), || {
        if hub.is_null() || !hub.is_aligned() {
            return;
        }
        // SAFETY: delegated to this function's contract; the borrow ends before
        // the reference is dropped below.
        unsafe {
            with_hub(hub, |hub| {
                // Unregister first: after this returns, no wake is in flight
                // and none will start, so the adapter's bridge can go away.
                hub.set_waker(None, std::ptr::null_mut());
                let entries: Vec<Arc<SessionEntry>> = lock(&hub.sessions)
                    .drain()
                    .map(|(_, entry)| entry)
                    .collect();
                for entry in entries {
                    // Abandon, never close: nothing is committed, nothing is
                    // waited for. Destroying the hub is also the last of the
                    // documented invalidators for a result's column
                    // descriptions, so they are freed here, on this thread.
                    entry.tear_down(&hub.registry);
                }
                lock(&hub.orphaned_columns).clear();
            });
        }
        // SAFETY: the caller promises this pointer came from `Arc::into_raw` in
        // `reldex_hub_create` and has not been destroyed, so this consumes
        // exactly the one strong reference that call created.
        let hub = unsafe { Arc::from_raw(hub.cast_const()) };
        match Arc::try_unwrap(hub) {
            Ok(hub) => {
                let ReldexHub {
                    queue,
                    registry,
                    pending,
                    ..
                } = hub;
                // Ends the stream: whatever is still queued is discarded, and
                // every event a session produces from now on is dropped on
                // arrival.
                drop(queue);
                drop(pending);
                reap(registry);
            }
            // Only a caller breaking the contract above — another thread still
            // inside a call on this hub — can hold a second reference. That
            // thread's reference frees the hub when it returns.
            Err(shared) => drop(shared),
        }
    });
}

/// Drops the registry, which waits — against one shared deadline — for every
/// session it still holds to finish its abandon. Off the caller's thread when
/// there is anything to wait for, so `reldex_hub_destroy` stays prompt; inline
/// when the registry is empty (every session retired on its `TERMINAL`, the
/// normal case) or a thread cannot be spawned.
fn reap(registry: SessionRegistry) {
    if registry.is_empty() {
        return;
    }
    let mut slot = Some(registry);
    let spawned = std::thread::Builder::new()
        .name("reldex-ffi-teardown".to_owned())
        .spawn({
            let registry = slot.take();
            move || drop(registry)
        });
    if let Err(error) = spawned {
        // The closure — and the registry inside it — was dropped with the
        // failed spawn, here, inline: correct, only not prompt.
        drop(error);
    }
}

/// Registers (or, with a null `fn`, unregisters) the wake callback.
///
/// Unregistering does not return while a wake is in progress, so the adapter
/// can destroy the object `user_data` points at immediately afterwards
/// (ADR-0003 D5 rule 2; spike criterion K5).
///
/// # What the callback may not do
///
/// * **It may not call any `reldex_*` function — on any hub.** The
///   re-entrancy guard is per *thread*, not per hub, and that is the contract,
///   not an implementation detail: a wake holds the lock that keeps the waker
///   alive, so a call back in would deadlock or re-enter the queue it is being
///   told about. Any entry from inside a waker reports
///   `RELDEX_STATUS_REENTRANT` (or `false`/`0` from the functions that return
///   no status) and does nothing.
/// * **It may not let a C++ exception escape.** Unwinding a C++ exception
///   through this `extern "C"` frame into Rust is undefined behaviour, and
///   the `catch_unwind` on every Reldex entry point does **not** contain it —
///   that catches Rust panics, which are a different mechanism. A waker that
///   can throw must wrap its own body in `try { … } catch (...) { }`.
/// * **It may not block.** It usually runs on a session's worker thread, and
///   blocking there stalls that session. One
///   `QMetaObject::invokeMethod(..., Qt::QueuedConnection)` and nothing else.
///
/// # When it fires
///
/// Only on the queue's transition from **empty to non-empty**. It is not
/// level-triggered: a caller that stops draining while
/// [`reldex_hub_next_event`] is still returning `true` gets **no further
/// wake**, because the queue never became empty. Such a caller must re-post
/// its own drain — ADR-0003 D5's budgeted `drain()` does exactly that, and
/// [`reldex_hub_pending_events`] is how it can report what it left behind.
///
/// It may run on the caller's own thread, inside the `reldex_*` call that
/// submitted a request (see [`ReldexWakeFn`]).
///
/// # Safety
///
/// `hub` must be a live hub. `user_data` is opaque to this library and is only
/// ever passed back to `fn`; it must stay valid until this function is called
/// again with a different (or null) `fn` and returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_hub_set_waker(
    hub: *mut ReldexHub,
    func: ReldexWakeFn,
    user_data: *mut c_void,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract for `hub`.
        let status = unsafe {
            with_hub(hub, |hub| {
                hub.set_waker(func, user_data);
                ReldexStatus::Ok
            })
        };
        status.unwrap_or_else(|| {
            set_last_argument_error("reldex_hub_set_waker: `hub` is null or unaligned")
        })
    })
}

/// How many events are waiting to be taken. Never blocks.
///
/// Diagnostics and drain budgeting: the adapter's `drain()` stops at a budget
/// and re-posts (ADR-0003 D5), and this is how it can report, or log, a
/// backlog it is deliberately leaving behind. It is a snapshot — a worker may
/// push another event before the caller acts on it.
///
/// # Safety
///
/// `hub` must be null (reported as 0) or a live hub.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_hub_pending_events(hub: *const ReldexHub) -> usize {
    entry_value(0, || {
        // SAFETY: delegated to this function's contract.
        unsafe { with_hub(hub, |hub| lock(&hub.queue).len()) }.unwrap_or(0)
    })
}

/// Takes the next event, or reports that there is none. Never blocks.
///
/// Returns `true` when `out` was filled. The caller then **owns** `out->error`,
/// `out->batch` and `out->server_output_lines` when they are non-null. Drain in
/// a loop until this returns `false`, budgeting the loop so a flood cannot
/// starve rendering (ADR-0003 D5 suggests 256 events / 4 ms, then re-post) —
/// and see [`reldex_hub_set_waker`] for why a caller that stops early must
/// re-post itself rather than wait for another wake.
///
/// **`false` means `*out` was not touched**, whether the queue was empty or
/// the argument was rejected (null, unaligned, or a `struct_size` this build
/// cannot fill). The event is never consumed in the rejected case either, so a
/// mis-sized struct cannot silently lose a batch. Only the `struct_size` field
/// is read on the way in; nothing is written unless an event is being
/// delivered.
///
/// Taking a session's `RELDEX_EVENT_KIND_TERMINAL` retires that session — its
/// id is not found by any later call — and releases what `db-core` held for
/// it; that can join the session's (idle, finishing) worker thread here.
///
/// # What bounds the queue
///
/// Per session, `db-core` bounds what can be waiting: a request holds one of
/// 1,024 slots from the moment it is accepted until **its reply is taken out
/// of this queue**, so a caller that keeps submitting without draining is
/// refused (`RELDEX_STATUS_ERROR`, last error `RELDEX_ERROR_KIND_RESOURCE`)
/// rather than growing the queue; progress events are bounded by the same
/// number, and unsolicited ones by a per-session cap past which server output
/// is dropped and counted (`server_output_dropped`) and a transaction-state
/// change is coalesced, never lost. No producer ever blocks on a slow
/// consumer. Memory is still the caller's to bound: a `FETCHED` event's
/// `ReldexBatch` holds its rows until released, so keep a small number of
/// fetches in flight per result and release each batch when the model is done
/// with it.
///
/// # Safety
///
/// `hub` must be a live hub, and `out` must point at a writable
/// [`ReldexEvent`] whose `struct_size` the caller has initialized.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_hub_next_event(hub: *mut ReldexHub, out: *mut ReldexEvent) -> bool {
    entry_value(false, || {
        // Validate the out struct *before* taking an event off the queue, and
        // without writing to it: a refused call must neither consume the event
        // (leaking its batch) nor scribble on the caller's memory.
        // SAFETY: the caller promises `out` is null or points at a
        // `ReldexEvent` whose leading `u32` is initialized.
        if !unsafe { check_out_struct(out.cast_const()) } {
            set_last_argument_error(
                "reldex_hub_next_event: `out` is null, unaligned, or has too small a struct_size",
            );
            return false;
        }
        let take = |hub: &Arc<ReldexHub>| {
            // The queue's lock is released before translating: translation
            // can retire a session, which must not happen under it.
            let Some(event) = lock(&hub.queue).next() else {
                return false;
            };
            let event = crate::session::translate(hub, event);
            // SAFETY: `out` was just checked non-null, aligned and large
            // enough, and the caller promises it is writable. Nothing between
            // then and now can have changed it: D5 rule 3 keeps every call but
            // cancel on one thread, and cancel does not touch `out`.
            unsafe { write_out_struct(out, event.into_c()) }
        };
        // SAFETY: delegated to this function's contract for `hub`.
        unsafe { with_hub(hub, take) }.unwrap_or(false)
    })
}

/// How many sessions this hub currently holds an entry for.
///
/// Every session from the moment `reldex_hub_open_session` returns a
/// [`crate::ReldexSessionId`] until its `RELDEX_EVENT_KIND_TERMINAL` event has
/// been **drained** — then it is retired, whether it was closed, lost,
/// abandoned or never opened. Diagnostic, like [`crate::reldex_live_counts`]'s
/// `sessions` field, which this agrees with; that one is process-wide across
/// every hub, this one is scoped to `hub`.
///
/// # Safety
///
/// `hub` must be null (reported as 0) or a live hub.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_hub_session_count(hub: *const ReldexHub) -> usize {
    entry_value(0, || {
        // SAFETY: delegated to this function's contract.
        unsafe { with_hub(hub, |hub| lock(&hub.sessions).len()) }.unwrap_or(0)
    })
}

/// Fills `out` with up to `capacity` of this hub's session ids and returns
/// how many sessions there are in total (which may be more than `capacity`,
/// exactly like `snprintf`'s return value: compare it against `capacity` to
/// know whether `out` holds all of them).
///
/// The order is unspecified — a caller after a stable ordering sorts `out`
/// itself. This is a point-in-time snapshot: a session may open or have its
/// `RELDEX_EVENT_KIND_TERMINAL` drained between this call returning and the
/// caller reading `out`.
///
/// # Safety
///
/// `hub` must be null (reported as 0) or a live hub. `out` must be null (to
/// ask only for the count) or point at `capacity` writable
/// [`crate::ReldexSessionId`]s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_hub_list_sessions(
    hub: *const ReldexHub,
    out: *mut u64,
    capacity: usize,
) -> usize {
    entry_value(0, || {
        // SAFETY: delegated to this function's contract.
        unsafe {
            with_hub(hub, |hub| {
                let sessions = lock(&hub.sessions);
                if !out.is_null() && out.is_aligned() {
                    // SAFETY: `index < capacity` (bounded by `.take`) and the
                    // caller promises `out` has room for `capacity` elements;
                    // already inside this function's outer `unsafe` block.
                    for (index, id) in sessions.keys().take(capacity).enumerate() {
                        out.add(index).write(*id);
                    }
                }
                sessions.len()
            })
        }
        .unwrap_or(0)
    })
}
