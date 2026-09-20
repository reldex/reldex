//! The hub: the event queue, the edge-triggered waker, and the object every
//! other call hangs off (ADR-0003 D5/D7).
//!
//! # Why there is no polling anywhere
//!
//! Events are produced by session pump threads (see [`crate::session`]), each
//! of which is **blocked** in `Completion::wait` until its session replies.
//! Pushing an event that makes the queue non-empty calls the waker once; the
//! adapter drains with [`reldex_hub_next_event`] until it returns false. No
//! part of this path sleeps, retries or has a tick, so no polling interval can
//! sit between a row arriving and the UI hearing about it — which is exactly
//! what spike S15's K2 (first-row latency) and K4 (per-batch cost) would
//! otherwise measure instead of the boundary.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use reldex_db_core::SessionManager;

use crate::error::set_last_argument_error;
use crate::event::{QueuedEvent, ReldexEvent};
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
/// It is called from a Reldex thread, never from the caller's.
///
/// The header's typedef for this is **hand-written** in `cbindgen.toml`'s
/// `after_includes`, not generated: cbindgen emits its typedefs above the
/// `extern "C"` block it opens for the functions, which in C++ would make this
/// a pointer to a C++-linkage function while `reldex_hub_set_waker` takes a
/// C-linkage one. If the parameter list here changes, change it there too —
/// `tests/header.rs` checks that the header declares it, not that the two
/// agree.
pub type ReldexWakeFn = Option<extern "C" fn(user_data: *mut c_void)>;

struct WakerSlot {
    func: extern "C" fn(*mut c_void),
    user_data: *mut c_void,
}

// SAFETY: `user_data` is an opaque token this library never dereferences; it
// only hands it back to `func`. ADR-0003 D5 makes the adapter responsible for
// the callback being safe to invoke from a Reldex thread, and
// `reldex_hub_set_waker` guarantees the unregister side: it does not return
// while a call is in flight, so the adapter can tear its bridge down.
unsafe impl Send for WakerSlot {}
// SAFETY: as above — the slot is only ever read behind the hub's `RwLock`.
unsafe impl Sync for WakerSlot {}

/// The application hub: one event queue, one waker, one session registry.
///
/// Opaque to C. Create it with [`reldex_hub_create`] and release it with
/// [`reldex_hub_destroy`], last of all.
pub struct ReldexHub {
    events: Mutex<VecDeque<QueuedEvent>>,
    waker: RwLock<Option<WakerSlot>>,
    pub(crate) sessions: Mutex<HashMap<u64, Arc<SessionEntry>>>,
    pub(crate) next_session_id: AtomicU64,
    pub(crate) manager: SessionManager,
    destroyed: AtomicBool,
}

impl Drop for ReldexHub {
    fn drop(&mut self) {
        crate::counters::destroyed(crate::counters::Kind::Hub);
    }
}

impl ReldexHub {
    fn new() -> Self {
        crate::counters::created(crate::counters::Kind::Hub);
        Self {
            events: Mutex::new(VecDeque::new()),
            waker: RwLock::new(None),
            sessions: Mutex::new(HashMap::new()),
            next_session_id: AtomicU64::new(1),
            manager: SessionManager::new(),
            destroyed: AtomicBool::new(false),
        }
    }

    /// Queues one event and wakes the consumer if the queue was empty.
    ///
    /// The queue lock is released *before* the waker is called, so a waker
    /// that breaks the contract and calls back in blocks on the re-entrancy
    /// guard rather than on this mutex.
    pub(crate) fn push_event(&self, event: QueuedEvent) {
        let was_empty = {
            let mut queue = self
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let was_empty = queue.is_empty();
            queue.push_back(event);
            was_empty
        };
        if was_empty {
            self.wake();
        }
    }

    fn wake(&self) {
        if self.destroyed.load(Ordering::Acquire) {
            return;
        }
        let slot = self
            .waker
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(waker) = slot.as_ref() else {
            return;
        };
        // Held across the call on purpose: `set_waker` takes the write lock, so
        // it cannot return while this is running — which is exactly the
        // use-after-free ADR-0003 D5 rule 2 and spike criterion K5 are about.
        let _guard = WakerGuard::enter();
        (waker.func)(waker.user_data);
    }

    fn next_event(&self) -> Option<QueuedEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
    }

    fn pending_events(&self) -> usize {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn set_waker(&self, func: ReldexWakeFn, user_data: *mut c_void) {
        let mut slot = self
            .waker
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = func.map(|func| WakerSlot { func, user_data });
    }

    fn is_destroyed(&self) -> bool {
        self.destroyed.load(Ordering::Acquire)
    }
}

/// Runs `body` with the hub the caller named, or reports why it could not.
///
/// The hub is reference-counted: this borrows the caller's reference without
/// touching the count, so a session pump can clone it and keep the hub alive
/// past [`reldex_hub_destroy`] while it finishes.
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

/// Records why a hub call was refused, so `reldex_last_error_take()` after a
/// non-OK status always describes *that* call.
fn set_last_hub_error(message: &str) -> ReldexStatus {
    crate::error::set_last_error(reldex_db_core::DbError::internal(format!(
        "reldex-ffi: {message}"
    )));
    ReldexStatus::InvalidState
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
/// waits for any wake already in flight, D5 rule 2), every session is marked
/// closed and asked to cancel whatever it is running, and the hub's own
/// reference is dropped.
///
/// # What "promptly" costs when a statement cannot be interrupted
///
/// Prompt is not the same as finished, and the difference is worth stating
/// plainly. A session parked inside an uninterruptible driver call — which is
/// the *normal* case on `oracledb` 26.0.0-beta.3, whose cancel cannot reach a
/// running statement (ADR-0002 D2, spike S4) — cannot be stopped by this
/// call. Its cancel is best-effort and does nothing there. So until that
/// statement returns on its own, or the process exits:
///
/// * the session's pump thread stays alive, blocked;
/// * `db-core`'s worker thread for that session and the connection it owns
///   stay alive with it;
/// * the hub's own allocation stays alive, because the pump holds a reference;
/// * every event already queued stays queued, **including any `ReldexBatch`
///   it carries**, and is freed only when the last pump finally exits.
///
/// None of that is reachable by the caller any more, so it is a leak for as
/// long as it lasts. It is the price of never blocking the UI thread on a
/// 10-second statement (spike criterion K6), and it is bounded by the
/// statement, not by Reldex. A caller that needs the memory back before then
/// has no option through this ABI, because the driver offers none.
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
        if hub.is_null() {
            return;
        }
        // SAFETY: delegated to this function's contract; the borrow ends before
        // the reference is dropped below.
        unsafe {
            with_hub(hub, |hub| {
                hub.destroyed.store(true, Ordering::Release);
                // Unregister first: after this returns, no wake is in flight
                // and none will start, so the adapter's bridge can go away.
                hub.set_waker(None, std::ptr::null_mut());
                let entries: Vec<Arc<SessionEntry>> = {
                    let mut sessions = hub
                        .sessions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    sessions.drain().map(|(_, entry)| entry).collect()
                };
                for entry in entries {
                    entry.shut_down();
                }
            });
        }
        // SAFETY: the caller promises this pointer came from `Arc::into_raw` in
        // `reldex_hub_create` and has not been destroyed, so this consumes
        // exactly the one strong reference that call created.
        drop(unsafe { Arc::from_raw(hub.cast_const()) });
    });
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
///   not an implementation detail: a wake runs on a Reldex thread and holds
///   the lock that keeps the waker alive, so a call back in would deadlock or
///   re-enter the queue it is being told about. Any entry from inside a waker
///   reports `RELDEX_STATUS_REENTRANT` (or `false`/`0` from the functions that
///   return no status) and does nothing.
/// * **It may not let a C++ exception escape.** Unwinding a C++ exception
///   through this `extern "C"` frame into Rust is undefined behaviour, and
///   the `catch_unwind` on every Reldex entry point does **not** contain it —
///   that catches Rust panics, which are a different mechanism. A waker that
///   can throw must wrap its own body in `try { … } catch (...) { }`.
/// * **It may not block.** It is called on a session's pump thread, and
///   blocking there stalls that session's events. One
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
                if hub.is_destroyed() {
                    return set_last_hub_error("reldex_hub_set_waker: the hub has been destroyed");
                }
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
        unsafe { with_hub(hub, |hub| hub.pending_events()) }.unwrap_or(0)
    })
}

/// Takes the next event, or reports that there is none. Never blocks.
///
/// Returns `true` when `out` was filled. The caller then **owns** `out->error`
/// and `out->batch` when they are non-null. Drain in a loop until this returns
/// `false`, budgeting the loop so a flood cannot starve rendering (ADR-0003 D5
/// suggests 256 events / 4 ms, then re-post) — and see
/// [`reldex_hub_set_waker`] for why a caller that stops early must re-post
/// itself rather than wait for another wake.
///
/// **`false` means `*out` was not touched**, whether the queue was empty or
/// the argument was rejected (null, unaligned, or a `struct_size` this build
/// cannot fill). The event is never consumed in the rejected case either, so a
/// mis-sized struct cannot silently lose a batch. Only the `struct_size` field
/// is read on the way in; nothing is written unless an event is being
/// delivered.
///
/// # The event queue is unbounded
///
/// Nothing here applies back-pressure. Every accepted request eventually
/// queues exactly one event, and a `FETCHED` event holds a `ReldexBatch` whose
/// rows are **the caller's memory** from the moment it is handed out. A
/// caller that keeps fetching without draining, or drains without releasing,
/// grows that queue without limit. The bound has to come from the adapter:
/// keep a small number of fetches in flight per result, release each batch
/// when the model is done with it, and use [`reldex_hub_pending_events`] as
/// the only signal this ABI gives about the backlog.
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
            let Some(event) = hub.next_event() else {
                return false;
            };
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
