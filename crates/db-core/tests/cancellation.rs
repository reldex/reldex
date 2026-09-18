//! Cancellation from another thread, for both `CancelKind` classes the mock
//! can model, plus the queueing policy for commands behind a blocked
//! statement (ADR-0002 D2; `docs/exec-plans/active/phase-0.md` Workstream C).

mod support;

use std::thread;
use std::time::Duration;

use reldex_db_core::Statement;
use reldex_db_driver_api::{CancelKind, CancelOutcome, ErrorKind, SessionState};
use reldex_driver_mock::{Action, BlockGate, BlockSpec};

#[test]
fn native_cancel_interrupts_a_blocked_statement_and_leaves_the_scripted_state() {
    let scenario = support::scenario();
    scenario.set_capabilities(
        reldex_db_driver_api::Capabilities::none().with_cancel(CancelKind::Native),
    );
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN long_running; END;",
        Action::Block(
            BlockSpec::new(std::sync::Arc::clone(&gate))
                .with_cancelled_session_state(SessionState::Usable),
        ),
    );

    let session = support::open(&scenario);
    assert_eq!(session.cancel_kind(), CancelKind::Native);

    let statement = Statement::new("BEGIN long_running; END;");
    // `execute` returns a `Completion` immediately; the actual blocking
    // happens on the session's own worker thread, so waiting for it happens
    // on a second thread here purely so the test can drive cancellation
    // concurrently — `db-core` itself never blocks the caller on I/O.
    let completion = session.execute(statement);

    assert!(
        gate.wait_until_blocked(support::short_timeout()),
        "the worker thread should have reached the gate"
    );
    let outcome = session.cancel().expect("cancel request");
    assert!(outcome.is_requested());

    let error = completion
        .wait()
        .expect_err("cancelled statement must fail");
    assert_eq!(error.kind(), ErrorKind::Cancelled);
    assert_eq!(
        error.session_state(),
        SessionState::Usable,
        "the mock was scripted to report the session as still usable"
    );

    // The session itself must have stayed usable, per that scripted state.
    session
        .ping()
        .wait()
        .expect("session is still usable after cancel");
}

#[test]
fn pre_armed_deadline_cannot_interrupt_but_the_deadline_still_fires() {
    let scenario = support::scenario();
    scenario.set_capabilities(
        reldex_db_driver_api::Capabilities::none().with_cancel(CancelKind::PreArmedDeadline),
    );
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN long_running; END;",
        Action::Block(BlockSpec::new(std::sync::Arc::clone(&gate))),
    );

    let session = support::open(&scenario);
    assert_eq!(session.cancel_kind(), CancelKind::PreArmedDeadline);
    assert!(!session.cancel_kind().interrupts_running_call());

    let statement =
        Statement::new("BEGIN long_running; END;").with_deadline(Duration::from_millis(150));
    let completion = session.execute(statement);

    assert!(gate.wait_until_blocked(support::short_timeout()));
    let outcome = session.cancel().expect("cancel request");
    assert!(
        matches!(outcome, CancelOutcome::NotInterruptible { .. }),
        "a pre-armed-deadline session must not claim it interrupted anything: {outcome:?}"
    );

    let error = completion.wait().expect_err("the deadline should fire");
    assert_eq!(error.kind(), ErrorKind::Timeout);
}

#[test]
fn a_command_queued_behind_a_blocked_statement_waits_its_turn() {
    // Policy (documented on `reldex_db_core::worker`): one worker thread per
    // session drains its command channel strictly in order, so a command
    // sent while an earlier one is blocked simply waits — there is no
    // separate queue-jumping mechanism, and none is needed because
    // cancellation reaches a blocked call through the driver's
    // `CancelHandle`, never through this channel.
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN long_running; END;",
        Action::Block(BlockSpec::new(std::sync::Arc::clone(&gate))),
    );

    let session = support::open(&scenario);
    let blocked = session.execute(Statement::new("BEGIN long_running; END;"));

    assert!(gate.wait_until_blocked(support::short_timeout()));

    // Submitted after the blocked statement, so it must not complete yet.
    let queued = session.ping();
    thread::sleep(Duration::from_millis(100));
    assert!(
        queued.poll().is_none(),
        "a command queued behind a blocked statement must wait, not run out of order"
    );

    gate.release();
    blocked
        .wait()
        .expect("the blocked statement completes once released");
    queued
        .wait()
        .expect("the queued command runs only after the one ahead of it finished");
}
