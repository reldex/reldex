//! Leak accounting: everything this library hands out must come back.
//!
//! Spike criterion K5 is written in terms of ASan, which is not available on
//! the development machine — and the failure most likely to actually happen at
//! this boundary is not a use-after-free anyway. It is a `ReldexBatch*` or a
//! `ReldexError*` that nobody released, or a session whose pump thread never
//! exited: invisible to every other test until memory runs out.
//! [`reldex_live_counts`] turns that into an assertion, and these are the three
//! paths worth asserting it on.
//!
//! The counters are process-wide, so the tests in this binary take a lock and
//! run one at a time rather than measuring each other. Nothing here asserts a
//! duration: a pump thread's exit is observed with the deadline helper, which
//! fails only when something is genuinely stuck.

#![allow(
    unsafe_code,
    reason = "these tests drive the crate's own C ABI, which is what they exist to prove"
)]

mod support;

use std::sync::{Mutex, MutexGuard};

use reldex_ffi::{
    ReldexCloseDisposition, ReldexEventKind, ReldexLiveCounts, ReldexMockScenarioConfig,
    ReldexMockStatement, ReldexStatus, reldex_hub_pending_events, reldex_live_counts,
};

use support::{Harness, take_error, wait_until};

/// Serialises the tests in this binary; the counters belong to the process.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn exclusively() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn counts() -> ReldexLiveCounts {
    let mut counts = ReldexLiveCounts::default();
    // SAFETY: `counts` is a real local with `struct_size` set.
    let status = unsafe { reldex_live_counts(std::ptr::from_mut(&mut counts)) };
    assert_eq!(status, ReldexStatus::Ok);
    counts
}

/// Clears this thread's last-error slot, which holds a live `ReldexError`.
fn forget_last_error() {
    support::free_error(reldex_ffi::reldex_last_error_take());
}

/// Waits until every count is back to `baseline`, then asserts it.
///
/// A hub's teardown finishes on its session pump threads, so the counts fall
/// slightly after `reldex_hub_destroy` returns — waiting for it is the honest
/// shape, and the deadline makes a genuine leak fail rather than hang.
fn settles_back_to(baseline: ReldexLiveCounts, what: &str) {
    wait_until(what, || counts() == baseline);
    assert_eq!(counts(), baseline, "{what}");
}

fn config() -> ReldexMockScenarioConfig {
    ReldexMockScenarioConfig {
        rows: 64,
        seed: 5,
        ..ReldexMockScenarioConfig::default()
    }
}

#[test]
fn a_whole_lifecycle_returns_every_count_to_its_baseline() {
    let _guard = exclusively();
    forget_last_error();
    let baseline = counts();

    {
        let harness = Harness::new();
        assert_eq!(counts().hubs, baseline.hubs + 1, "the hub is counted");

        let session = harness.open(config());
        assert_eq!(counts().sessions, baseline.sessions + 1);

        assert_eq!(
            harness.execute(session, 10, ReldexMockStatement::GeneratedQuery),
            ReldexStatus::Ok
        );
        let executed = harness.next_event();
        assert!(executed.error.is_null());
        let result = executed.result;

        assert_eq!(harness.fetch(session, 20, result, 64), ReldexStatus::Ok);
        let fetched = harness.next_event();
        assert_eq!(fetched.kind, ReldexEventKind::Fetched as i32);
        assert!(!fetched.batch.is_null());
        assert_eq!(
            counts().batches,
            baseline.batches + 1,
            "the caller now owns one batch"
        );
        support::release_batch(&fetched);
        assert_eq!(counts().batches, baseline.batches, "and has given it back");

        assert_eq!(harness.close_result(session, 30, result), ReldexStatus::Ok);
        let closed = harness.next_event();
        assert_eq!(closed.kind, ReldexEventKind::ResultClosed as i32);

        assert_eq!(
            harness.close(session, 40, ReldexCloseDisposition::None),
            ReldexStatus::Ok
        );
        let ended = harness.next_event();
        assert_eq!(ended.kind, ReldexEventKind::SessionClosed as i32);
        assert!(ended.error.is_null(), "a SELECT opens no transaction");
    }

    settles_back_to(baseline, "every count to return to its baseline");
}

#[test]
fn destroying_a_hub_with_undrained_events_frees_what_they_hold() {
    let _guard = exclusively();
    forget_last_error();
    let baseline = counts();

    const FETCHES: u64 = 4;
    {
        let harness = Harness::new();
        let session = harness.open(config());
        assert_eq!(
            harness.execute(session, 10, ReldexMockStatement::GeneratedQuery),
            ReldexStatus::Ok
        );
        let executed = harness.next_event();
        let result = executed.result;

        for request in 0..FETCHES {
            assert_eq!(
                harness.fetch(session, 20 + request, result, 16),
                ReldexStatus::Ok
            );
        }
        // Deliberately drain nothing. The batches exist, queued, and are this
        // library's to free.
        wait_until("every fetch to be answered", || {
            // SAFETY: the hub is live.
            let pending = unsafe { reldex_hub_pending_events(harness.hub()) };
            pending >= FETCHES as usize
        });
        assert!(
            counts().batches >= baseline.batches + FETCHES as usize,
            "a batch inside an undrained event is live: its rows are in memory"
        );
    }

    settles_back_to(
        baseline,
        "an undrained queue to be freed with the hub that owns it",
    );
}

#[test]
fn the_contained_pump_panic_leaks_nothing() {
    let _guard = exclusively();
    forget_last_error();
    let baseline = counts();

    {
        let harness = Harness::new();
        let session = harness.open(config());
        // The panicking request, and one queued behind it that the containment
        // has to answer as well.
        assert_eq!(
            harness.execute(session, 10, ReldexMockStatement::PumpPanic),
            ReldexStatus::Ok
        );
        assert_eq!(
            harness.execute(session, 11, ReldexMockStatement::GeneratedQuery),
            ReldexStatus::Ok
        );

        let first = harness.next_event();
        assert_eq!(first.request, 10);
        assert!(
            take_error(&first).is_some(),
            "the contained panic is reported as a failure"
        );
        let second = harness.next_event();
        assert_eq!(second.request, 11);
        assert!(
            take_error(&second).is_some(),
            "the request behind it is answered too"
        );
        assert_eq!(
            counts().errors,
            baseline.errors,
            "both error objects were freed"
        );
    }

    settles_back_to(
        baseline,
        "the session lost to a pump panic to release everything",
    );
}
