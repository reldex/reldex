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
    let Err(queued) = queued.poll() else {
        panic!("a command queued behind a blocked statement must wait, not run out of order");
    };

    gate.release();
    blocked
        .wait()
        .expect("the blocked statement completes once released");
    queued
        .wait()
        .expect("the queued command runs only after the one ahead of it finished");
}

/// A `cancel` that the driver *refuses* must not be swallowed.
///
/// `DatabaseSession::cancel` used to return the driver's `Err` straight to the
/// caller without telling the session about it, so an error reporting
/// `SessionState::Lost` left `is_lost()` false — a dead session the core still
/// believed in.
#[test]
fn a_failing_cancel_is_recorded_on_the_session_rather_than_discarded() {
    let scenario = support::scenario();
    scenario.set_capabilities(
        reldex_db_driver_api::Capabilities::none().with_cancel(CancelKind::Native),
    );
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN long_running; END;",
        Action::Block(BlockSpec::new(std::sync::Arc::clone(&gate))),
    );
    let session = support::open(&scenario);
    let completion = session.execute(Statement::new("BEGIN long_running; END;"));
    assert!(gate.wait_until_blocked(support::short_timeout()));

    // The cancel path is how the caller finds out the session is already gone.
    scenario.fail_cancel(
        reldex_driver_mock::ScriptedError::new(ErrorKind::NetworkLost, "connection reset")
            .with_native(3113, "ORA-03113: end-of-file on communication channel"),
    );
    let error = session
        .cancel()
        .expect_err("the driver reported a failure, so `cancel` must too");
    assert_eq!(error.kind(), ErrorKind::NetworkLost);
    assert_eq!(error.session_state(), SessionState::Lost);
    assert!(
        session.is_lost(),
        "a cancel that discovers a dead session must record it, not leave the core believing \
         the session is fine"
    );
    assert_eq!(
        session.session_state(),
        reldex_db_core::SessionLifecycle::Lost
    );

    scenario.allow_cancel();
    gate.release();
    let _ = completion.wait();

    // And an `Unsupported` cancel, which says nothing about the session's
    // health, must not be mistaken for one that does.
    let scenario = support::scenario();
    scenario.set_capabilities(reldex_db_driver_api::Capabilities::none());
    let session = support::open(&scenario);
    assert_eq!(session.cancel_kind(), CancelKind::Unsupported);
    let error = session
        .cancel()
        .expect_err("an Unsupported driver reports rather than pretending");
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert_eq!(
        session.session_state(),
        reldex_db_core::SessionLifecycle::Usable
    );
}

#[test]
fn a_fired_deadline_that_destroys_the_session_is_reported_as_loss() {
    let scenario = support::scenario();
    scenario.set_capabilities(
        reldex_db_driver_api::Capabilities::none().with_cancel(CancelKind::PreArmedDeadline),
    );
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN long_running; END;",
        Action::Block(
            BlockSpec::new(std::sync::Arc::clone(&gate)).with_timeout_error(
                reldex_driver_mock::ScriptedError::new(ErrorKind::NetworkLost, "session destroyed"),
            ),
        ),
    );
    let session = support::open(&scenario);
    let completion = session.execute(
        Statement::new("BEGIN long_running; END;").with_deadline(Duration::from_millis(100)),
    );
    assert!(gate.wait_until_blocked(support::short_timeout()));
    let outcome = session.cancel().expect("the driver answers honestly");
    assert!(matches!(outcome, CancelOutcome::NotInterruptible { .. }));

    let error = completion.wait().expect_err("the deadline fires");
    assert_eq!(error.kind(), ErrorKind::NetworkLost);
    assert!(
        session.is_lost(),
        "a fired deadline that destroyed the session must be visible as loss (spike U-6)"
    );
}

/// A cancel issued while nothing is running must not be handed to a driver that
/// would latch it onto the next statement.
///
/// The contract has no statement identity, so a driver is free to latch. The
/// core narrows the window by answering the "nothing is running" case itself —
/// which the contract explicitly permits as an idempotent no-op — and the race
/// that remains is documented on `DatabaseSession::cancel` rather than hidden.
#[test]
fn a_cancel_with_nothing_running_does_not_land_on_the_next_statement() {
    let scenario = support::scenario();
    scenario.set_capabilities(
        reldex_db_driver_api::Capabilities::none().with_cancel(CancelKind::Native),
    );
    scenario.set_late_cancel_lands_on_next_statement(true);
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN long_running; END;",
        Action::Block(BlockSpec::new(std::sync::Arc::clone(&gate))),
    );

    let session = support::open(&scenario);
    // Nothing is running.
    let outcome = session.cancel().expect("cancel is an idempotent no-op");
    assert!(outcome.is_requested());

    // The next statement must run normally rather than inherit that cancel.
    let completion = session.execute(Statement::new("BEGIN long_running; END;"));
    assert!(
        gate.wait_until_blocked(support::short_timeout()),
        "the statement must actually start; a latched cancel would have failed it immediately"
    );
    gate.release();
    completion
        .wait()
        .expect("a statement issued after an idle cancel must not be cancelled by it");
}

/// A cancel the statement never observes: the request is delivered, the driver
/// reports success, and the call still runs to its deadline (spike U-7).
#[test]
fn an_unobserved_cancel_still_runs_to_its_deadline() {
    let scenario = support::scenario();
    scenario.set_capabilities(
        reldex_db_driver_api::Capabilities::none().with_cancel(CancelKind::Native),
    );
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN long_running; END;",
        Action::Block(BlockSpec::new(std::sync::Arc::clone(&gate)).with_unobserved_cancel()),
    );

    let session = support::open(&scenario);
    let completion = session.execute(
        Statement::new("BEGIN long_running; END;").with_deadline(Duration::from_millis(150)),
    );
    assert!(gate.wait_until_blocked(support::short_timeout()));

    let outcome = session.cancel().expect("cancel request");
    assert!(
        outcome.is_requested(),
        "the driver believes it delivered the request, and it did"
    );

    let error = completion
        .wait()
        .expect_err("but the statement never saw it, so the deadline is what stops it");
    assert_eq!(
        error.kind(),
        ErrorKind::Timeout,
        "reporting this as `Cancelled` to make the UI look better is forbidden (ADR-0002 M5)"
    );
}
