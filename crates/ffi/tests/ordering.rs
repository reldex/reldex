//! Per-session ordering, exactly-one-reply, and the independence of one
//! session from another that is blocked (ADR-0003 D5; spike criterion K6).
//!
//! This is also the `db-core`-level independence proof the mock crate's own
//! tests could not give: there, a blocked statement and a live one are two
//! driver connections; here they are two real sessions, each with its own
//! worker thread and its own pump, feeding one shared event queue.

#![allow(
    unsafe_code,
    reason = "these tests drive the crate's own C ABI, which is what they exist to prove"
)]

mod support;

use std::collections::HashSet;

use reldex_ffi::{
    ReldexCloseDisposition, ReldexEvent, ReldexEventKind, ReldexMockScenarioConfig,
    ReldexMockStatement, ReldexStatus, reldex_mock_release_block,
};

use support::{Harness, OwnedBatch};

fn config(rows: u64) -> ReldexMockScenarioConfig {
    ReldexMockScenarioConfig {
        rows,
        // Zero means "block until released", so nothing in this file sleeps or
        // races a timer.
        block_duration_ms: 0,
        ..ReldexMockScenarioConfig::default()
    }
}

#[test]
fn a_blocked_session_does_not_delay_another_and_every_request_is_answered_once() {
    let harness = Harness::new();
    let blocked = harness.open(config(10));
    let live = harness.open(config(40));

    // The blocked session parks its worker inside the driver call and will not
    // answer until released.
    assert_eq!(
        harness.execute(blocked, 100, ReldexMockStatement::Block),
        ReldexStatus::Ok
    );

    // Meanwhile the other session runs a full execute + three fetches.
    assert_eq!(
        harness.execute(live, 200, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let executed = harness.next_event();
    assert_eq!(executed.session, live);
    assert_eq!(executed.request, 200);
    let result = executed.result;

    for request in [201_u64, 202, 203] {
        assert_eq!(harness.fetch(live, request, result, 15), ReldexStatus::Ok);
    }

    let mut seen: Vec<(u64, u64)> = vec![(executed.session, executed.request)];
    for expected in [201_u64, 202, 203] {
        let event = harness.next_event();
        assert_eq!(
            event.session, live,
            "the blocked session cannot produce an event while it is blocked"
        );
        assert_eq!(
            event.request, expected,
            "one session's replies arrive in the order it submitted them"
        );
        assert_eq!(event.kind, ReldexEventKind::Fetched as i32);
        OwnedBatch(event.batch);
        seen.push((event.session, event.request));
    }

    // Nothing from the blocked session has arrived, which is the point.
    assert!(
        harness.poll_event().is_none(),
        "no event should be waiting while one session is blocked and the other is idle"
    );

    // Release it, and its single reply arrives.
    // SAFETY: the hub is live and the session id is one it issued.
    let status = unsafe { reldex_mock_release_block(harness.hub(), blocked) };
    assert_eq!(status, ReldexStatus::Ok);
    let released = harness.next_event();
    assert_eq!(released.session, blocked);
    assert_eq!(released.request, 100);
    assert_eq!(released.kind, ReldexEventKind::Executed as i32);
    assert!(
        released.error.is_null(),
        "a released block succeeds rather than failing"
    );
    seen.push((released.session, released.request));

    // Exactly one reply per accepted request, and no extras.
    let unique: HashSet<(u64, u64)> = seen.iter().copied().collect();
    assert_eq!(unique.len(), seen.len(), "no request was answered twice");
    assert_eq!(seen.len(), 5);
    assert!(harness.poll_event().is_none(), "and none was answered late");
}

#[test]
fn a_request_that_races_a_close_still_gets_its_one_reply() {
    // Requests accepted before a close runs are answered by `db-core` with the
    // session's terminal error; the pump drains them rather than dropping
    // them, so the adapter never waits forever for a reply that will not come.
    let harness = Harness::new();
    let session = harness.open(config(50_000));
    assert_eq!(
        harness.execute(session, 1, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let executed = harness.next_event();
    let result = executed.result;

    // Queue several fetches and a close behind them, all accepted.
    let mut expected: Vec<u64> = Vec::new();
    for request in 2..8_u64 {
        assert_eq!(
            harness.fetch(session, request, result, 1_000),
            ReldexStatus::Ok
        );
        expected.push(request);
    }
    assert_eq!(
        harness.close(session, 8, ReldexCloseDisposition::Rollback),
        ReldexStatus::Ok
    );
    expected.push(8);

    let mut answered: Vec<u64> = Vec::new();
    while answered.len() < expected.len() {
        let event: ReldexEvent = harness.next_event();
        assert_eq!(event.session, session);
        OwnedBatch(event.batch);
        if !event.error.is_null() {
            // SAFETY: the error came from the event and is freed once.
            unsafe { reldex_ffi::reldex_error_free(event.error) };
        }
        answered.push(event.request);
    }
    assert_eq!(
        answered, expected,
        "every accepted request is answered exactly once, in order"
    );
    assert!(harness.poll_event().is_none());
}

#[test]
fn a_panic_in_the_pump_answers_every_request_behind_it_instead_of_stranding_them() {
    // The pump runs on a thread of ours, so the `catch_unwind` on every
    // `extern "C"` body cannot reach it. Without containment here, a panic
    // would leave every outstanding request unanswered and the adapter's
    // spinner would never stop — the worst kind of failure, because nothing
    // reports it.
    //
    // `ReldexMockStatement::PumpPanic` is a reserved statement text that makes
    // the pump panic when it reaches that request. It changes no ABI and is
    // compiled in only with the mock driver.
    let harness = Harness::new();
    let session = harness.open(config(50_000));
    assert_eq!(
        harness.execute(session, 1, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let executed = harness.next_event();
    assert_eq!(executed.request, 1);
    let result = executed.result;

    // The panic, then more work queued behind it. All are accepted, so all
    // must be answered — rule 5 does not have an exception for "the library
    // broke".
    assert_eq!(
        harness.execute(session, 2, ReldexMockStatement::PumpPanic),
        ReldexStatus::Ok
    );
    let mut expected = vec![2_u64];
    for request in 3..7_u64 {
        // A request may be refused if the pump has already torn the session
        // down; only the accepted ones are owed a reply.
        if harness.fetch(session, request, result, 1_000) == ReldexStatus::Ok {
            expected.push(request);
        }
    }

    let mut answered: Vec<u64> = Vec::new();
    let mut lost = 0_u32;
    while answered.len() < expected.len() {
        let event: ReldexEvent = harness.next_event();
        assert_eq!(event.session, session);
        OwnedBatch(event.batch);
        if !event.error.is_null() {
            if event.session_state == reldex_ffi::ReldexSessionState::Lost as i32 {
                lost += 1;
            }
            // SAFETY: the error came from the event and is freed once.
            unsafe { reldex_ffi::reldex_error_free(event.error) };
        }
        answered.push(event.request);
    }

    assert_eq!(
        answered, expected,
        "every accepted request must be answered exactly once, in order, even after a panic"
    );
    assert!(
        lost >= 1,
        "the session must be reported lost, not merely failed"
    );
    assert!(
        harness.poll_event().is_none(),
        "and nothing is answered twice"
    );

    // The session is gone: nothing further is accepted, so nothing further is
    // owed.
    assert_eq!(
        harness.execute(session, 99, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::InvalidState
    );
    support::free_error(reldex_ffi::reldex_last_error_take());
}
