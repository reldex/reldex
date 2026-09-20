//! "Block for a fixed duration, then succeed" — item 2 of the M1.7 task: the
//! S15 FFI/UI spike needs a statement that blocks for ~10 seconds and then
//! completes normally, to prove that never stalls the rest of the UI.
//!
//! `Action::Block`/[`BlockSpec`] already express this with no new API:
//! a controller thread sleeps for the duration and then calls
//! [`BlockGate::release`], which is documented as safe to call whether or not
//! anything has parked on the gate yet, so there is no race between "start
//! the controller" and "the statement reaches the gate" to get right. See the
//! doc example on [`BlockSpec`] for the same pattern in isolation.
//!
//! Durations here are milliseconds, not the spike's 10 seconds, so the suite
//! stays fast; the pattern is identical at either scale.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use reldex_db_driver_api::{ErrorKind, Statement, StatementKind};
use reldex_driver_mock::{Action, BlockGate, BlockSpec};
use support::connect;

#[test]
fn a_statement_blocked_for_a_fixed_duration_then_succeeds() {
    let scenario = reldex_driver_mock::Scenario::new();
    let gate = BlockGate::new();
    let block_for = Duration::from_millis(40);

    scenario.on_sql(
        "BEGIN DBMS_SESSION.SLEEP(10); END;",
        Action::Block(
            BlockSpec::new(Arc::clone(&gate))
                .with_release_statement_kind(StatementKind::PlSqlBlock),
        ),
    );

    let controller = Arc::clone(&gate);
    std::thread::spawn(move || {
        std::thread::sleep(block_for);
        controller.release();
    });

    let mut connection = connect(&scenario);
    let started = Instant::now();
    let outcome = connection
        .execute(&Statement::new("BEGIN DBMS_SESSION.SLEEP(10); END;"))
        .expect("the statement blocks and then succeeds");
    let elapsed = started.elapsed();

    assert_eq!(outcome.statement_kind(), StatementKind::PlSqlBlock);
    assert!(
        elapsed >= block_for,
        "the call must actually have blocked for the fixed duration: took {elapsed:?}"
    );
}

#[test]
fn a_pre_armed_deadline_fires_even_though_the_block_would_otherwise_run_forever() {
    // ADR-0002 D2: a `PreArmedDeadline` driver cannot interrupt a running
    // call, but a deadline armed *before* the call still stops it at the
    // deadline — the deadline does not know or care whether the block was
    // ever going to resolve on its own.
    //
    // Deliberately no controller thread here, and so no race between two
    // timers: nothing ever calls `BlockGate::release` or requests
    // cancellation, so the deadline is the *only* way this call can return.
    // An earlier version of this test raced a 30ms deadline against a 200ms
    // release on another thread; that is exactly the shape of assertion that
    // flaked under CI load (see the sibling flake in
    // `generated_query.rs`'s `first_batch_latency_applies_once_...`), and
    // the race added nothing this test needs — the "the gate really would
    // have released it" half of the claim is already covered by
    // `a_statement_blocked_for_a_fixed_duration_then_succeeds` above.
    let scenario = reldex_driver_mock::Scenario::new();
    scenario.set_capabilities(
        reldex_db_driver_api::Capabilities::none()
            .with_cancel(reldex_db_driver_api::CancelKind::PreArmedDeadline),
    );
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN DBMS_SESSION.SLEEP(10); END;",
        Action::Block(BlockSpec::new(gate)),
    );

    let mut connection = connect(&scenario);
    let error = connection
        .execute(
            &Statement::new("BEGIN DBMS_SESSION.SLEEP(10); END;")
                .with_deadline(Duration::from_millis(30)),
        )
        .expect_err("the pre-armed deadline should fire");
    assert_eq!(error.kind(), ErrorKind::Timeout);
}
