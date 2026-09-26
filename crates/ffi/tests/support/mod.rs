//! Shared helpers for `reldex-ffi`'s integration tests: a hub with a waker
//! that a test can wait on, and the small amount of pointer handling every
//! test would otherwise repeat.
//!
//! Each test binary uses a subset, so unused-item warnings here are expected.
#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use reldex_ffi::{
    ReldexBatch, ReldexCloseDisposition, ReldexError, ReldexErrorView, ReldexEvent,
    ReldexEventKind, ReldexHub, ReldexMockScenarioConfig, ReldexMockStatement, ReldexOpenOptions,
    ReldexStatus, ReldexStr, reldex_batch_release, reldex_error_free, reldex_error_view,
    reldex_hub_create, reldex_hub_destroy, reldex_hub_next_event, reldex_hub_set_waker,
    reldex_mock_statement, reldex_session_close, reldex_session_close_result,
    reldex_session_commit, reldex_session_execute, reldex_session_fetch, reldex_session_ping,
    reldex_session_rollback, reldex_session_rollback_to_savepoint, reldex_session_savepoint,
    reldex_session_set_server_output,
};

/// How long a test waits for an event before calling it a hang.
///
/// Deliberately generous and deliberately **not** an assertion about speed:
/// CI runners stall sleeps by 5-10x, so nothing here may claim a timing upper
/// bound. It exists only so a deadlock fails the test instead of hanging the
/// suite forever.
pub(crate) const HANG_GUARD: Duration = Duration::from_secs(60);

/// A counter the waker bumps, with a condvar so a test can wait on it instead
/// of spinning.
pub(crate) struct WakeSignal {
    state: Mutex<u64>,
    condvar: Condvar,
    /// Set once the test has unregistered the waker: any wake after that is a
    /// use-after-free in waiting (spike criterion K5).
    forbidden: AtomicU64,
    violations: AtomicU64,
}

impl WakeSignal {
    /// A signal a test can register itself, for the cases that drive the hub
    /// without a [`Harness`].
    pub(crate) fn for_test() -> Arc<Self> {
        Self::new()
    }

    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(0),
            condvar: Condvar::new(),
            forbidden: AtomicU64::new(0),
            violations: AtomicU64::new(0),
        })
    }

    /// How many times the waker has been called.
    pub(crate) fn wakes(&self) -> u64 {
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// How many wakes arrived after the waker was unregistered. Must be zero.
    pub(crate) fn violations(&self) -> u64 {
        self.violations.load(Ordering::SeqCst)
    }

    /// Declares that no further wake may happen.
    pub(crate) fn forbid(&self) {
        self.forbidden.store(1, Ordering::SeqCst);
    }

    fn record(&self) {
        if self.forbidden.load(Ordering::SeqCst) == 1 {
            self.violations.fetch_add(1, Ordering::SeqCst);
        }
        let mut count = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *count += 1;
        self.condvar.notify_all();
    }

    /// Blocks until the wake count rises above `seen`, or the hang guard
    /// elapses.
    pub(crate) fn wait_past(&self, seen: u64) -> u64 {
        let count = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (count, _) = self
            .condvar
            .wait_timeout_while(count, HANG_GUARD, |count| *count <= seen)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *count
    }
}

/// The waker this module registers, for a test that registers its own signal.
pub(crate) fn wake_fn() -> extern "C" fn(*mut c_void) {
    wake
}

extern "C" fn wake(user_data: *mut c_void) {
    assert!(!user_data.is_null(), "the waker was given a null token");
    // SAFETY: `user_data` is the `Arc<WakeSignal>` the harness registered and
    // keeps alive for as long as the waker is registered.
    let signal = unsafe { &*user_data.cast::<WakeSignal>() };
    signal.record();
}

/// A hub, its waker, and the session ids a test opened.
pub(crate) struct Harness {
    hub: *mut ReldexHub,
    pub(crate) signal: Arc<WakeSignal>,
}

impl Harness {
    /// A hub with the waker registered.
    pub(crate) fn new() -> Self {
        let hub = reldex_hub_create();
        assert!(!hub.is_null(), "the hub must be created");
        let signal = WakeSignal::new();
        let token = Arc::as_ptr(&signal).cast::<c_void>().cast_mut();
        // SAFETY: the hub is live, and `signal` outlives the registration
        // because `Drop` unregisters the waker before releasing it.
        let status = unsafe { reldex_hub_set_waker(hub, Some(wake), token) };
        assert_eq!(status, ReldexStatus::Ok);
        Self { hub, signal }
    }

    /// A hub with **no** waker registered.
    pub(crate) fn without_waker() -> Self {
        let hub = reldex_hub_create();
        assert!(!hub.is_null(), "the hub must be created");
        Self {
            hub,
            signal: WakeSignal::new(),
        }
    }

    pub(crate) fn hub(&self) -> *mut ReldexHub {
        self.hub
    }

    /// Opens a session and waits for its `OPENED` event, asserting it
    /// succeeded.
    pub(crate) fn open(&self, config: ReldexMockScenarioConfig) -> u64 {
        let (id, event) = self.open_raw(config, 1);
        assert_eq!(event.kind, ReldexEventKind::Opened as i32);
        assert!(event.error.is_null(), "the mock session must open");
        assert_eq!(event.request, 1);
        assert_eq!(event.session, id);
        id
    }

    /// Opens a session and returns its id with its `OPENED` event, whatever
    /// that says.
    pub(crate) fn open_raw(
        &self,
        config: ReldexMockScenarioConfig,
        request: u64,
    ) -> (u64, ReldexEvent) {
        let options = ReldexOpenOptions {
            mock: config,
            ..ReldexOpenOptions::default()
        };
        let mut id = 0_u64;
        // SAFETY: the hub is live; `options` and `id` are real locals.
        let status = unsafe {
            reldex_ffi::reldex_hub_open_session(
                self.hub,
                std::ptr::from_ref(&options),
                request,
                std::ptr::from_mut(&mut id),
            )
        };
        assert_eq!(status, ReldexStatus::Ok, "open should be accepted");
        let event = self.next_event();
        (id, event)
    }

    /// Submits a statement by its scenario kind.
    pub(crate) fn execute(
        &self,
        session: u64,
        request: u64,
        statement: ReldexMockStatement,
    ) -> ReldexStatus {
        let sql = reldex_mock_statement(statement as i32);
        // SAFETY: the hub is live and `sql` is a `'static` string this library
        // produced.
        unsafe { reldex_session_execute(self.hub, session, request, sql, 0) }
    }

    /// Submits arbitrary statement text.
    pub(crate) fn execute_text(&self, session: u64, request: u64, sql: &str) -> ReldexStatus {
        let text = ReldexStr {
            ptr: sql.as_ptr(),
            len: sql.len(),
        };
        // SAFETY: `sql` outlives the call, which copies what it needs.
        unsafe { reldex_session_execute(self.hub, session, request, text, 0) }
    }

    pub(crate) fn fetch(
        &self,
        session: u64,
        request: u64,
        result: u64,
        max_rows: u32,
    ) -> ReldexStatus {
        // SAFETY: the hub is live.
        unsafe { reldex_session_fetch(self.hub, session, request, result, max_rows) }
    }

    pub(crate) fn close_result(&self, session: u64, request: u64, result: u64) -> ReldexStatus {
        // SAFETY: the hub is live.
        unsafe { reldex_session_close_result(self.hub, session, request, result) }
    }

    pub(crate) fn close(
        &self,
        session: u64,
        request: u64,
        disposition: ReldexCloseDisposition,
    ) -> ReldexStatus {
        // SAFETY: the hub is live.
        unsafe { reldex_session_close(self.hub, session, request, disposition as i32) }
    }

    pub(crate) fn commit(&self, session: u64, request: u64) -> ReldexStatus {
        // SAFETY: the hub is live.
        unsafe { reldex_session_commit(self.hub, session, request) }
    }

    pub(crate) fn rollback(&self, session: u64, request: u64) -> ReldexStatus {
        // SAFETY: the hub is live.
        unsafe { reldex_session_rollback(self.hub, session, request) }
    }

    pub(crate) fn savepoint(&self, session: u64, request: u64, name: &str) -> ReldexStatus {
        let text = ReldexStr {
            ptr: name.as_ptr(),
            len: name.len(),
        };
        // SAFETY: `name` outlives the call, which copies what it needs.
        unsafe { reldex_session_savepoint(self.hub, session, request, text) }
    }

    pub(crate) fn rollback_to_savepoint(
        &self,
        session: u64,
        request: u64,
        name: &str,
    ) -> ReldexStatus {
        let text = ReldexStr {
            ptr: name.as_ptr(),
            len: name.len(),
        };
        // SAFETY: `name` outlives the call, which copies what it needs.
        unsafe { reldex_session_rollback_to_savepoint(self.hub, session, request, text) }
    }

    pub(crate) fn ping(&self, session: u64, request: u64) -> ReldexStatus {
        // SAFETY: the hub is live.
        unsafe { reldex_session_ping(self.hub, session, request) }
    }

    /// Turns server output on (with no buffer limit) or off.
    pub(crate) fn set_server_output(&self, session: u64, request: u64, on: bool) -> ReldexStatus {
        let mode = if on {
            reldex_ffi::ReldexServerOutputMode::EnabledUnlimited
        } else {
            reldex_ffi::ReldexServerOutputMode::Disabled
        };
        // SAFETY: the hub is live.
        unsafe { reldex_session_set_server_output(self.hub, session, request, mode as i32, 0) }
    }

    /// Abandons a session; returns the status, the outcome and the early
    /// transaction-loss answer.
    pub(crate) fn abandon(&self, session: u64) -> (ReldexStatus, i32, bool) {
        let mut outcome = -1_i32;
        let mut lost = false;
        // SAFETY: the hub is live; both out pointers are real locals.
        let status = unsafe {
            reldex_ffi::reldex_session_abandon(
                self.hub,
                session,
                std::ptr::from_mut(&mut outcome),
                std::ptr::from_mut(&mut lost),
            )
        };
        (status, outcome, lost)
    }

    /// Releases the session's parked mock statement or connect.
    pub(crate) fn release_block(&self, session: u64) -> ReldexStatus {
        // SAFETY: the hub is live.
        unsafe { reldex_ffi::reldex_mock_release_block(self.hub, session) }
    }

    /// How many sessions the hub still holds.
    pub(crate) fn session_count(&self) -> usize {
        // SAFETY: the hub is live.
        unsafe { reldex_ffi::reldex_hub_session_count(self.hub) }
    }

    /// Takes the next event without waiting.
    pub(crate) fn poll_event(&self) -> Option<ReldexEvent> {
        let mut event = ReldexEvent::default();
        // SAFETY: the hub is live and `event` is a real local with its
        // `struct_size` set.
        let taken = unsafe { reldex_hub_next_event(self.hub, std::ptr::from_mut(&mut event)) };
        taken.then_some(event)
    }

    /// Takes the next event that is not progress, waiting until one arrives.
    ///
    /// `EXECUTING` and `TRANSACTION_STATE` (ABI 3.2) are skipped: they carry
    /// nothing a caller owns, and most tests are about replies. A test about
    /// them uses [`Self::next_any_event`].
    pub(crate) fn next_event(&self) -> ReldexEvent {
        loop {
            let event = self.next_any_event();
            if event.kind != ReldexEventKind::Executing as i32
                && event.kind != ReldexEventKind::TransactionState as i32
            {
                return event;
            }
        }
    }

    /// Takes the next event of any kind, waiting on the waker's condvar until
    /// one arrives.
    ///
    /// Never sleeps on a fixed interval: it waits to be woken, which is the
    /// same path the adapter uses.
    pub(crate) fn next_any_event(&self) -> ReldexEvent {
        let mut seen = self.signal.wakes();
        loop {
            if let Some(event) = self.poll_event() {
                return event;
            }
            let now = self.signal.wait_past(seen);
            assert!(
                now > seen || self.poll_event().is_some(),
                "no event arrived within the hang guard"
            );
            seen = now;
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // SAFETY: the hub is live and no other thread is inside a call on it.
        unsafe {
            reldex_hub_set_waker(self.hub, None, std::ptr::null_mut());
            reldex_hub_destroy(self.hub);
        }
    }
}

/// Reads an event's error and releases it.
pub(crate) fn take_error(event: &ReldexEvent) -> Option<ErrorSnapshot> {
    if event.error.is_null() {
        return None;
    }
    let mut view = ReldexErrorView::default();
    // SAFETY: the event's error is live until `reldex_error_free` below.
    let status = unsafe { reldex_error_view(event.error, std::ptr::from_mut(&mut view)) };
    assert_eq!(status, ReldexStatus::Ok);
    // SAFETY: the view's strings borrow from the still-live error.
    let snapshot = unsafe { ErrorSnapshot::read(&view) };
    // SAFETY: the error came from an event and has not been freed.
    unsafe { reldex_error_free(event.error) };
    Some(snapshot)
}

/// An owned copy of a [`ReldexErrorView`], so assertions outlive the error.
#[derive(Debug)]
pub(crate) struct ErrorSnapshot {
    pub(crate) kind: i32,
    pub(crate) session_state: i32,
    pub(crate) native_code: Option<i32>,
    pub(crate) line_column: Option<(u32, u32)>,
    pub(crate) message: String,
    pub(crate) native_message: String,
}

impl ErrorSnapshot {
    /// # Safety
    ///
    /// The view's strings must still be borrowed from a live error.
    unsafe fn read(view: &ReldexErrorView) -> Self {
        // SAFETY: delegated to this function's contract.
        let read = |text: ReldexStr| unsafe { text.as_str() }.unwrap_or_default().to_owned();
        Self {
            kind: view.kind,
            session_state: view.session_state,
            native_code: view.has_native.then_some(view.native_code),
            line_column: view.has_line_column.then_some((view.line, view.column)),
            message: read(view.message),
            native_message: read(view.native_message),
        }
    }
}

/// Releases an event's batch, if it has one.
pub(crate) fn release_batch(event: &ReldexEvent) {
    release_lines(event);
    if !event.batch.is_null() {
        // SAFETY: the batch came from a fetched event and has not been
        // released.
        unsafe { reldex_batch_release(event.batch) };
    }
}

/// Releases an event's server output lines, if it has any.
pub(crate) fn release_lines(event: &ReldexEvent) {
    if !event.server_output_lines.is_null() {
        // SAFETY: the lines came from a drained event and are released once.
        unsafe { reldex_ffi::reldex_server_output_lines_release(event.server_output_lines) };
    }
}

/// A batch pointer a test is about to read, released on drop.
pub(crate) struct OwnedBatch(pub(crate) *mut ReldexBatch);

impl Drop for OwnedBatch {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: taken from an event and released exactly once, here.
            unsafe { reldex_batch_release(self.0) };
        }
    }
}

/// Frees an error pointer a test took from `reldex_last_error_take`.
pub(crate) fn free_error(error: *mut ReldexError) {
    if !error.is_null() {
        // SAFETY: the pointer came from this library and is freed once.
        unsafe { reldex_error_free(error) };
    }
}

/// Reads an environment-configurable iteration count, so the ASan run of
/// spike criterion K5 can ask for 10,000 without CI paying for it every time.
pub(crate) fn iterations(variable: &str, default: usize) -> usize {
    std::env::var(variable)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Waits until `condition` holds, failing with `what` if [`HANG_GUARD`]
/// elapses first.
///
/// The point of spelling this out: a spin count is **not** a timeout. A loop
/// that gives up after 10,000 `yield_now`s passes on an idle laptop and fails
/// on a CI runner that descheduled the thread once — it measures scheduling,
/// not the property under test. A deadline fails only when something is
/// genuinely stuck, and it asserts no upper bound on how *fast* anything must
/// be: `HANG_GUARD` is 60 seconds precisely so that it never becomes a
/// performance assertion.
pub(crate) fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + HANG_GUARD;
    while !condition() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out after {HANG_GUARD:?} waiting for: {what}"
        );
        std::thread::yield_now();
    }
}
