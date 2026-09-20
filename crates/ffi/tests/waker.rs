//! The waker contract: edge-triggered and coalesced, safe to unregister under
//! a flood, and refusing re-entry instead of deadlocking (ADR-0003 D5 rules 1
//! and 2; spike criterion K5).

#![allow(
    unsafe_code,
    reason = "these tests drive the crate's own C ABI, which is what they exist to prove"
)]

mod support;

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicPtr, AtomicU64, Ordering};

use reldex_ffi::{
    ReldexEvent, ReldexHub, ReldexMockScenarioConfig, ReldexMockStatement, ReldexOpenOptions,
    ReldexStatus, reldex_hub_create, reldex_hub_destroy, reldex_hub_next_event,
    reldex_hub_open_session, reldex_hub_pending_events, reldex_hub_set_waker,
    reldex_mock_release_block, reldex_mock_statement, reldex_session_execute,
};

use support::{Harness, WakeSignal, iterations};

fn config() -> ReldexMockScenarioConfig {
    ReldexMockScenarioConfig {
        rows: 5,
        block_duration_ms: 0,
        ..ReldexMockScenarioConfig::default()
    }
}

#[test]
fn a_burst_of_completions_produces_exactly_one_wake() {
    // Edge-triggered on empty -> non-empty. Eight sessions are parked inside a
    // blocked statement, then all released at once; nothing is drained until
    // every one of their replies is queued, so this is a real burst and not a
    // sequence of separate ones.
    let harness = Harness::new();
    // `Harness::open` consumes each session's OPENED event, so the queue is
    // already empty here and the next wake is unambiguous.
    let sessions: Vec<u64> = (0..8).map(|_| harness.open(config())).collect();
    // SAFETY: the hub is live.
    assert_eq!(unsafe { reldex_hub_pending_events(harness.hub()) }, 0);

    for (index, session) in sessions.iter().enumerate() {
        assert_eq!(
            harness.execute(*session, 500 + index as u64, ReldexMockStatement::Block),
            ReldexStatus::Ok
        );
    }
    let before = harness.signal.wakes();

    for session in &sessions {
        // SAFETY: the hub is live and the ids are ones it issued.
        assert_eq!(
            unsafe { reldex_mock_release_block(harness.hub(), *session) },
            ReldexStatus::Ok
        );
    }

    // Wait for the whole burst to be queued, without taking anything out of
    // the queue: that is what makes the assertion below exact.
    loop {
        // SAFETY: the hub is live.
        if unsafe { reldex_hub_pending_events(harness.hub()) } == sessions.len() {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(
        harness.signal.wakes(),
        before + 1,
        "eight completions onto an empty queue must coalesce into one wake"
    );

    // Draining to empty re-arms the edge: the next completion wakes again.
    for _ in 0..sessions.len() {
        assert!(harness.poll_event().is_some());
    }
    // SAFETY: the hub is live.
    assert_eq!(unsafe { reldex_hub_pending_events(harness.hub()) }, 0);
    let rearmed = harness.signal.wakes();
    assert_eq!(
        harness.execute(sessions[0], 600, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let event = harness.next_event();
    assert_eq!(event.request, 600);
    assert_eq!(
        harness.signal.wakes(),
        rearmed + 1,
        "a push onto an empty queue wakes exactly once"
    );
}

#[test]
fn unregistering_the_waker_under_a_flood_is_safe() {
    // Spike criterion K5: destroying the C++ bridge while completions are
    // arriving. `reldex_hub_set_waker(hub, NULL, NULL)` must not return while
    // a wake is in flight, and must not let one start afterwards — so the
    // object `user_data` points at can be freed the moment it returns.
    //
    // The iteration count is modest by default so CI stays fast; the 10,000
    // iteration ASan run sets RELDEX_FFI_WAKER_ITERATIONS.
    let rounds = iterations("RELDEX_FFI_WAKER_ITERATIONS", 64);
    for _ in 0..rounds {
        let hub = reldex_hub_create();
        assert!(!hub.is_null());
        let signal = WakeSignal::for_test();
        let token = Arc::as_ptr(&signal).cast::<c_void>().cast_mut();
        // SAFETY: `signal` outlives the registration — it is unregistered
        // below before the `Arc` is dropped.
        assert_eq!(
            unsafe { reldex_hub_set_waker(hub, Some(support::wake_fn()), token) },
            ReldexStatus::Ok
        );

        let options = ReldexOpenOptions {
            mock: config(),
            ..ReldexOpenOptions::default()
        };
        let mut session = 0_u64;
        // SAFETY: the hub is live and the locals are real.
        assert_eq!(
            unsafe {
                reldex_hub_open_session(
                    hub,
                    std::ptr::from_ref(&options),
                    1,
                    std::ptr::from_mut(&mut session),
                )
            },
            ReldexStatus::Ok
        );
        let sql = reldex_mock_statement(ReldexMockStatement::GeneratedQuery as i32);
        let flood = 6_u64;
        for request in 2..2 + flood {
            // SAFETY: the hub is live and `sql` is a `'static` string.
            let status = unsafe { reldex_session_execute(hub, session, request, sql, 0) };
            // The session may still be opening; either answer is fine, and an
            // accepted request is exactly what produces the flood.
            assert!(
                status == ReldexStatus::Ok || status == ReldexStatus::InvalidState,
                "unexpected status {status:?}"
            );
        }

        // Unregister in the middle of the flood.
        // SAFETY: the hub is live.
        assert_eq!(
            unsafe { reldex_hub_set_waker(hub, None, std::ptr::null_mut()) },
            ReldexStatus::Ok
        );
        signal.forbid();

        // Let the pumps finish pushing whatever is left; any wake now would be
        // a call into an object the adapter is entitled to have freed.
        let mut drained = 0_u64;
        let mut idle = 0_u32;
        while idle < 10_000 {
            let mut event = ReldexEvent::default();
            // SAFETY: the hub is live and `event` is a real local.
            if unsafe { reldex_hub_next_event(hub, std::ptr::from_mut(&mut event)) } {
                support::release_batch(&event);
                if !event.error.is_null() {
                    // SAFETY: the error came from the event.
                    unsafe { reldex_ffi::reldex_error_free(event.error) };
                }
                drained += 1;
                idle = 0;
            } else {
                idle += 1;
                std::thread::yield_now();
            }
        }
        assert!(drained >= 1, "the open alone produces an event");
        assert_eq!(
            signal.violations(),
            0,
            "a wake arrived after set_waker(NULL) returned"
        );

        // Dropping the signal here is the use-after-free the criterion is
        // about: nothing may reference it from now on.
        drop(signal);
        // SAFETY: the hub is live, was created here, and is destroyed once.
        unsafe { reldex_hub_destroy(hub) };
    }
}

/// What the re-entrant waker observed, so the test can assert on it after the
/// fact.
struct Reentrant {
    hub: AtomicPtr<ReldexHub>,
    session: AtomicU64,
    execute_status: AtomicI32,
    set_waker_status: AtomicI32,
    next_event_taken: AtomicI32,
    calls: AtomicU64,
}

extern "C" fn reentrant_wake(user_data: *mut c_void) {
    assert!(!user_data.is_null());
    // SAFETY: `user_data` is the `Arc<Reentrant>` the test registered and keeps
    // alive until it unregisters the waker.
    let state = unsafe { &*user_data.cast::<Reentrant>() };
    let hub = state.hub.load(Ordering::SeqCst);
    let session = state.session.load(Ordering::SeqCst);
    let sql = reldex_mock_statement(ReldexMockStatement::GeneratedQuery as i32);
    // SAFETY: the hub is live for the whole test; the point of the call is
    // that the boundary refuses it.
    let execute = unsafe { reldex_session_execute(hub, session, 7_000, sql, 0) };
    state.execute_status.store(execute as i32, Ordering::SeqCst);
    // Without the guard this one would deadlock: `wake` holds the waker lock
    // for reading and this asks for it for writing, on the same thread.
    // SAFETY: as above.
    let set_waker = unsafe { reldex_hub_set_waker(hub, None, std::ptr::null_mut()) };
    state
        .set_waker_status
        .store(set_waker as i32, Ordering::SeqCst);
    let mut event = ReldexEvent::default();
    // SAFETY: as above; `event` is a real local.
    let taken = unsafe { reldex_hub_next_event(hub, std::ptr::from_mut(&mut event)) };
    state
        .next_event_taken
        .store(i32::from(taken), Ordering::SeqCst);
    state.calls.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn an_ffi_call_from_inside_the_waker_is_refused_rather_than_deadlocking() {
    let hub = reldex_hub_create();
    assert!(!hub.is_null());
    let state = Arc::new(Reentrant {
        hub: AtomicPtr::new(hub),
        session: AtomicU64::new(0),
        execute_status: AtomicI32::new(-1),
        set_waker_status: AtomicI32::new(-1),
        next_event_taken: AtomicI32::new(-1),
        calls: AtomicU64::new(0),
    });
    let token = Arc::as_ptr(&state).cast::<c_void>().cast_mut();
    // SAFETY: `state` outlives the registration.
    assert_eq!(
        unsafe { reldex_hub_set_waker(hub, Some(reentrant_wake), token) },
        ReldexStatus::Ok
    );

    let options = ReldexOpenOptions {
        mock: config(),
        ..ReldexOpenOptions::default()
    };
    let mut session = 0_u64;
    // SAFETY: the hub is live and the locals are real.
    assert_eq!(
        unsafe {
            reldex_hub_open_session(
                hub,
                std::ptr::from_ref(&options),
                1,
                std::ptr::from_mut(&mut session),
            )
        },
        ReldexStatus::Ok
    );
    state.session.store(session, Ordering::SeqCst);

    // The OPENED event's push calls the waker, which calls back in. If the
    // guard were missing this test would hang here rather than fail.
    let mut idle = 0_u32;
    while state.calls.load(Ordering::SeqCst) == 0 && idle < 100_000 {
        idle += 1;
        std::thread::yield_now();
    }
    assert!(state.calls.load(Ordering::SeqCst) >= 1, "the waker ran");
    assert_eq!(
        state.execute_status.load(Ordering::SeqCst),
        ReldexStatus::Reentrant as i32,
        "an FFI entry from inside the waker must be refused"
    );
    assert_eq!(
        state.set_waker_status.load(Ordering::SeqCst),
        ReldexStatus::Reentrant as i32,
        "including the one that would otherwise deadlock"
    );
    assert_eq!(
        state.next_event_taken.load(Ordering::SeqCst),
        0,
        "a bool-returning entry reports nothing rather than re-entering"
    );

    // The waker is still registered, because the re-entrant attempt to clear
    // it was refused; clear it properly from this thread.
    // SAFETY: the hub is live and no wake is in flight from this thread.
    assert_eq!(
        unsafe { reldex_hub_set_waker(hub, None, std::ptr::null_mut()) },
        ReldexStatus::Ok
    );
    drop(state);
    // SAFETY: the hub is live and destroyed once.
    unsafe { reldex_hub_destroy(hub) };
}
