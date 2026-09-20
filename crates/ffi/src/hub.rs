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
use crate::strings::write_out_struct;

/// Called when the hub's event queue goes from empty to non-empty.
///
/// Edge-triggered and coalesced: a burst of completions produces **one** call.
/// It must not block and must not call back into `reldex_*` (ADR-0003 D5); any
/// FFI entry from inside it returns `RELDEX_STATUS_REENTRANT` rather than
/// deadlocking. The C++ trampoline should do exactly one thing:
/// `QMetaObject::invokeMethod(bridge, &Bridge::drain, Qt::QueuedConnection)`.
///
/// It is called from a Reldex thread, never from the caller's.
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

impl ReldexHub {
    fn new() -> Self {
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

/// Creates the hub. Returns `NULL` only if allocation failed.
///
/// One per application is the intended shape; nothing here forbids more.
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
/// reference is dropped. A session thread still finishing keeps the hub alive
/// a moment longer and then releases it; events queued in the meantime are
/// freed unread, along with any batch they carry.
///
/// Like dropping a `DatabaseSession`, this **never commits**: a transaction
/// still open when the hub is destroyed is rolled back by the server, exactly
/// as if the connection had died (`SPEC.md` §10).
///
/// # Safety
///
/// `hub` must be null, or a pointer [`reldex_hub_create`] returned that has
/// not already been destroyed. No other thread may be inside a `reldex_*` call
/// on this hub.
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
                    return ReldexStatus::InvalidState;
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
/// suggests 256 events / 4 ms, then re-post).
///
/// Returns `false` if `out` is null, unaligned, or its `struct_size` is too
/// small — the event is **not** consumed in that case, so a mis-sized struct
/// cannot silently lose a batch.
///
/// # Safety
///
/// `hub` must be a live hub, and `out` must point at a writable
/// [`ReldexEvent`] whose `struct_size` the caller has initialized.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_hub_next_event(hub: *mut ReldexHub, out: *mut ReldexEvent) -> bool {
    entry_value(false, || {
        if out.is_null() || !out.is_aligned() {
            set_last_argument_error("reldex_hub_next_event: `out` is null or unaligned");
            return false;
        }
        // Validate the out struct *before* taking an event off the queue: a
        // refused write must not consume the event (and leak its batch).
        // SAFETY: `out` is non-null and aligned per the check above, and the
        // caller promises its leading `u32` is initialized.
        let probe = unsafe { write_out_struct(out, ReldexEvent::default()) };
        if !probe {
            set_last_argument_error("reldex_hub_next_event: `out` has too small a struct_size");
            return false;
        }
        let take = |hub: &Arc<ReldexHub>| {
            let Some(event) = hub.next_event() else {
                return false;
            };
            // SAFETY: `out` was just proven writable and large enough by the
            // probe above, and nothing between then and now can have changed
            // it — the caller is single-threaded per D5 rule 3.
            unsafe { write_out_struct(out, event.into_c()) }
        };
        // SAFETY: delegated to this function's contract for `hub`.
        unsafe { with_hub(hub, take) }.unwrap_or(false)
    })
}
