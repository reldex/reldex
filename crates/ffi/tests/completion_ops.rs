//! `RELDEX_EVENT_KIND_COMPLETED` coverage (M2.11 review round 2, should-fix
//! 6): commit, rollback, savepoint, rollback_to_savepoint and ping each
//! reply with a `Completed` event carrying the right
//! `ReldexCompletedOperation`, `reldex_hub_session_count`/
//! `reldex_hub_list_sessions` report the hub's live sessions, and
//! `reldex_server_output_lines_*` behave on the inputs this crate's own test
//! surface can actually reach (the mock driver has no FFI knob to turn on
//! the `server_output` capability — see the PR's "weak points" section and
//! `ReldexEventKind::ServerOutput`'s doc comment).

#![allow(
    unsafe_code,
    reason = "these tests drive the crate's own C ABI, which is what they exist to prove"
)]

mod support;

use reldex_ffi::{
    ReldexCompletedOperation, ReldexEventKind, ReldexMockScenarioConfig, ReldexStatus,
    reldex_hub_list_sessions, reldex_hub_session_count, reldex_server_output_lines_count,
    reldex_server_output_lines_get, reldex_server_output_lines_release,
};

use support::Harness;

fn config() -> ReldexMockScenarioConfig {
    ReldexMockScenarioConfig::default()
}

#[test]
fn commit_rollback_savepoint_and_ping_each_reply_completed_with_their_own_operation() {
    let harness = Harness::new();
    let session = harness.open(config());

    assert_eq!(harness.commit(session, 2), ReldexStatus::Ok);
    let event = harness.next_event();
    assert_eq!(event.kind, ReldexEventKind::Completed as i32);
    assert_eq!(event.request, 2);
    assert_eq!(
        event.completed_operation,
        ReldexCompletedOperation::Commit as i32
    );
    assert!(event.error.is_null(), "a bare commit succeeds on the mock");

    assert_eq!(harness.rollback(session, 3), ReldexStatus::Ok);
    let event = harness.next_event();
    assert_eq!(event.kind, ReldexEventKind::Completed as i32);
    assert_eq!(event.request, 3);
    assert_eq!(
        event.completed_operation,
        ReldexCompletedOperation::Rollback as i32
    );
    assert!(event.error.is_null());

    assert_eq!(
        harness.savepoint(session, 4, "before_update"),
        ReldexStatus::Ok
    );
    let event = harness.next_event();
    assert_eq!(event.kind, ReldexEventKind::Completed as i32);
    assert_eq!(event.request, 4);
    assert_eq!(
        event.completed_operation,
        ReldexCompletedOperation::Savepoint as i32
    );
    assert!(event.error.is_null());

    assert_eq!(
        harness.rollback_to_savepoint(session, 5, "before_update"),
        ReldexStatus::Ok
    );
    let event = harness.next_event();
    assert_eq!(event.kind, ReldexEventKind::Completed as i32);
    assert_eq!(event.request, 5);
    assert_eq!(
        event.completed_operation,
        ReldexCompletedOperation::RollbackToSavepoint as i32
    );
    assert!(event.error.is_null());

    assert_eq!(harness.ping(session, 6), ReldexStatus::Ok);
    let event = harness.next_event();
    assert_eq!(event.kind, ReldexEventKind::Completed as i32);
    assert_eq!(event.request, 6);
    assert_eq!(
        event.completed_operation,
        ReldexCompletedOperation::Ping as i32
    );
    assert!(
        event.error.is_null(),
        "a ping round trip succeeds on the mock"
    );
}

#[test]
fn a_savepoint_name_the_driver_would_not_accept_is_refused_before_it_is_submitted() {
    // `SavepointName::new` refuses anything that is not a plain unquoted
    // identifier (`crates/db-driver-api/src/ids.rs`) *before* the command
    // reaches the session, so no event is produced for the refused call.
    let harness = Harness::new();
    let session = harness.open(config());

    assert_eq!(
        harness.savepoint(session, 2, "drop table t --"),
        ReldexStatus::InvalidArgument
    );
    let error = reldex_ffi::reldex_last_error_take();
    assert!(!error.is_null(), "the refusal says why");
    support::free_error(error);

    // The session is untouched: a normal savepoint still works.
    assert_eq!(harness.savepoint(session, 3, "ok_name"), ReldexStatus::Ok);
    let event = harness.next_event();
    assert_eq!(event.request, 3);
    assert!(event.error.is_null());
}

#[test]
fn hub_session_count_and_list_sessions_report_what_is_actually_open() {
    let harness = Harness::new();
    // SAFETY: the hub is live and `out` is null, asking for the count only.
    assert_eq!(
        unsafe { reldex_hub_list_sessions(harness.hub(), std::ptr::null_mut(), 0) },
        0
    );
    // SAFETY: the hub is live.
    assert_eq!(unsafe { reldex_hub_session_count(harness.hub()) }, 0);

    let first = harness.open(config());
    let (second, opened) = harness.open_raw(config(), 10);
    assert!(opened.error.is_null());

    // SAFETY: the hub is live.
    assert_eq!(unsafe { reldex_hub_session_count(harness.hub()) }, 2);

    let mut ids = [0_u64; 2];
    // SAFETY: the hub is live and `ids` has room for 2 entries.
    let total = unsafe { reldex_hub_list_sessions(harness.hub(), ids.as_mut_ptr(), ids.len()) };
    assert_eq!(total, 2);
    let mut sorted = ids;
    sorted.sort_unstable();
    let mut expected = [first, second];
    expected.sort_unstable();
    assert_eq!(sorted, expected);

    // A `capacity` smaller than the total reports the true total, `snprintf`
    // style, and still only writes what fits.
    let mut small = [0xDEAD_BEEF_u64; 1];
    // SAFETY: the hub is live and `small` has room for 1 entry.
    let total = unsafe { reldex_hub_list_sessions(harness.hub(), small.as_mut_ptr(), 1) };
    assert_eq!(
        total, 2,
        "the true total is reported even when capacity is smaller"
    );
    assert!(
        small[0] == first || small[0] == second,
        "the one slot that fit was filled with a real session id"
    );
}

#[test]
fn server_output_lines_null_and_empty_inputs_are_handled_without_a_capability_to_produce_real_ones()
{
    // The mock driver's only FFI-reachable scenario does not advertise the
    // `server_output` capability and there is no FFI knob to turn it on
    // (see the PR's "weak points" section), so this crate's test surface
    // cannot produce a real `RELDEX_EVENT_KIND_SERVER_OUTPUT` event with
    // non-empty lines. What *is* reachable, and what this test pins: a null
    // `lines` pointer is treated as "no lines" rather than dereferenced.
    // SAFETY: `lines` is documented as null-safe (reported as empty/0).
    unsafe {
        assert_eq!(reldex_server_output_lines_count(std::ptr::null()), 0);
        let empty = reldex_server_output_lines_get(std::ptr::null(), 0);
        assert_eq!(empty.len, 0);
        reldex_server_output_lines_release(std::ptr::null_mut());
    }
}
