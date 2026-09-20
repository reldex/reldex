//! What `db-core`'s event path costs: allocations per event, asserted, and
//! nanoseconds per event, measured (`AGENTS.md`, "Performance"; M2.5).
//!
//! # Why this lives in `crates/ffi`
//!
//! It measures `reldex-db-core`, not this crate. Counting allocations needs a
//! `#[global_allocator]`, which cannot be written without `unsafe`, and
//! `tests/fences.rs` keeps the workspace's `unsafe_code = "deny"` opt-out to
//! the FFI boundary and the link probe — adding a third crate to that list is
//! an architecture decision (ADR-0003 D2), not something a performance test
//! gets to take. `db-core` therefore keeps `#![forbid(unsafe_code)]` and its
//! measurement is hosted here, in the one crate that already owns a counting
//! allocator, in **its own test binary** so nothing else is counted.
//!
//! The numbers are quoted in
//! `docs/exec-plans/active/phase-1-m2-5-event-queue.md` §2.

#![allow(
    unsafe_code,
    reason = "a counting global allocator cannot be written without it; see the module docs"
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use reldex_db_core::{
    CloseDisposition, DatabaseDriver, DatabaseSession, EventCaps, EventQueue, RequestId,
    SessionEvent, SessionManager, event_channel,
};
use reldex_db_driver_api::{ConnectionParams, Credentials, Endpoint};
use reldex_driver_mock::{MockDriver, Scenario};

/// How long a loop waits before declaring the path hung. A guard, never a
/// timing assertion.
const HANG_GUARD: Duration = Duration::from_secs(60);

struct Counter;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);

// SAFETY: every method forwards to `System` unchanged; the counter only
// observes.
unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: delegated to `System`, with the caller's own layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: as above.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counter = Counter;

fn allocations_during(f: impl FnOnce()) -> u64 {
    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    f();
    COUNTING.store(false, Ordering::Relaxed);
    ALLOCATIONS.load(Ordering::Relaxed)
}

fn open(scenario: &Arc<Scenario>) -> DatabaseSession {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(MockDriver::new(Arc::clone(scenario)));
    let params = ConnectionParams::new(
        Endpoint::ConnectString("mock".to_owned()),
        Credentials::External,
    );
    SessionManager::new()
        .open_session(driver, params)
        .expect("the mock driver always connects")
}

/// `count` `ping`s, each submitted and then drained, reusing `out` so the loop
/// itself contributes nothing.
fn round_trips(
    session: &DatabaseSession,
    queue: &EventQueue,
    count: u64,
    out: &mut Vec<SessionEvent>,
) {
    for request in 0..count {
        session
            .submit_ping(RequestId(request))
            .expect("accepted under the default outstanding limit");
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            out.clear();
            if queue.drain_into(16, out) > 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the ping's reply never arrived; the event path hung"
            );
            std::hint::spin_loop();
        }
    }
}

/// Carrying an event through the queue allocates nothing beyond the event.
///
/// The counter sees the *whole* round trip — the command channel, the worker's
/// reply, the queue push and the drain — so the number is an upper bound on
/// what the event path costs, not just on the queue. A `ping` reply holds no
/// heap data of its own, so anything counted here is machinery; in practice it
/// is `std::sync::mpsc`'s block amortisation on the command channel, about one
/// allocation per 31 requests.
#[test]
fn the_core_event_path_allocates_nothing_per_event_once_warm() {
    let scenario = Scenario::new();
    let session = open(&scenario);
    let (sink, queue) = event_channel(EventCaps::new());
    session
        .bind_events(sink)
        .expect("a fresh session binds once");
    let mut drained = Vec::with_capacity(64);

    // Warm up: grow the queue's ring, the command channel's blocks and the
    // mock's own bookkeeping.
    round_trips(&session, &queue, 2_000, &mut drained);

    const MEASURED: u64 = 2_000;
    let allocations = allocations_during(|| {
        round_trips(&session, &queue, MEASURED, &mut drained);
    });

    // Shown with `--nocapture`; the assertion below is what the suite checks.
    #[expect(clippy::print_stdout, reason = "the measured number is worth seeing")]
    {
        println!(
            "core event path: {allocations} allocations for {MEASURED} round trips \
             ({:.3} per event)",
            allocations as f64 / MEASURED as f64
        );
    }
    assert!(
        allocations <= MEASURED,
        "the event path must not allocate per event beyond the event itself: \
         {allocations} allocations for {MEASURED} round trips"
    );

    let _ = session.close(Some(CloseDisposition::Rollback));
}

/// Prints the per-event cost of the whole path — submit, worker reply, queue
/// push with its edge-triggered wake, and drain — at one and at eight
/// concurrent producer sessions.
///
/// Run with
/// `cargo test --release -p reldex-ffi --test core_event_cost -- --ignored --nocapture`.
#[test]
#[ignore = "a measurement for M2.5's performance note, not a pass/fail check"]
fn measure_the_core_cost_per_event() {
    for producers in [1_usize, 8] {
        const PER_SESSION: u64 = 20_000;
        let scenario = Scenario::new();
        let (sink, queue) = event_channel(EventCaps::new());
        let sessions: Vec<DatabaseSession> = (0..producers)
            .map(|_| {
                let session = open(&scenario);
                session
                    .bind_events(sink.clone())
                    .expect("a fresh session binds once");
                session
            })
            .collect();
        let total = PER_SESSION * producers as u64;

        let started = Instant::now();
        thread::scope(|scope| {
            for session in &sessions {
                scope.spawn(move || {
                    for request in 0..PER_SESSION {
                        // The default outstanding limit is 1,024, so back off
                        // rather than fail when the drainer falls behind.
                        while session.submit_ping(RequestId(request)).is_err() {
                            std::hint::spin_loop();
                        }
                    }
                });
            }
            let mut drained: Vec<SessionEvent> = Vec::with_capacity(512);
            let mut seen = 0_u64;
            while seen < total {
                drained.clear();
                let taken = queue.drain_into(256, &mut drained);
                if taken == 0 {
                    if let Some(event) = queue.wait_timeout(HANG_GUARD) {
                        drop(event);
                        seen += 1;
                    }
                } else {
                    seen += taken as u64;
                }
            }
        });
        let elapsed = started.elapsed();

        #[expect(clippy::print_stdout, reason = "this test exists to print a number")]
        {
            println!(
                "core event path, {producers} producer session(s): {:.0} ns/event ({total} \
                 events in {:.2}s)",
                elapsed.as_nanos() as f64 / total as f64,
                elapsed.as_secs_f64()
            );
        }

        for session in &sessions {
            let _ = session.close(Some(CloseDisposition::Rollback));
        }
    }
}
