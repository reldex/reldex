//! Dropping a session must not hang or leak its worker thread — including
//! while the driver is blocked inside a call that cannot be interrupted.

mod support;

use std::sync::Arc;
use std::time::Duration;

use reldex_db_core::Statement;
use reldex_db_driver_api::{CancelKind, Capabilities};
use reldex_driver_mock::{Action, BlockGate, BlockSpec, ScriptValue};

use support::with_timeout_guard;

#[test]
fn dropping_an_idle_session_does_not_hang() {
    with_timeout_guard(Duration::from_secs(5), || {
        let scenario = support::scenario();
        let session = support::open(&scenario);
        drop(session);
    });
}

#[test]
fn dropping_a_session_with_a_possibly_active_transaction_does_not_hang_or_commit() {
    with_timeout_guard(Duration::from_secs(5), || {
        let scenario = support::scenario();
        scenario.on_sql(
            "INSERT INTO t VALUES ('a')",
            Action::Dml {
                rows_affected: 1,
                insert: Some(("t".to_owned(), vec![ScriptValue::from("a")])),
            },
        );
        let session = support::open(&scenario);
        session
            .execute(Statement::new("INSERT INTO t VALUES ('a')"))
            .wait()
            .expect("insert");
        assert!(session.has_possibly_active_transaction());
        drop(session);

        assert!(
            scenario.committed_rows("t").is_empty(),
            "dropping a session must never silently commit a possibly-active transaction"
        );
    });
}

#[test]
fn dropping_the_session_manager_side_does_not_leak_or_hang_across_many_sessions() {
    with_timeout_guard(Duration::from_secs(10), || {
        let scenario = support::scenario();
        for _ in 0..16 {
            let session = support::open(&scenario);
            drop(session);
        }
    });
}

/// `Drop` while the worker is parked inside a driver call.
///
/// Both cancel classes have to work, and they work differently: a `Native`
/// driver honours the cancel drop issues, so the worker unwinds promptly and is
/// joined; a `PreArmedDeadline` driver with no deadline armed **cannot be
/// stopped at all**, so drop must give up after
/// [`reldex_db_core::DROP_SHUTDOWN_TIMEOUT`] and detach the worker rather than
/// hang the thread that dropped the session. The detached worker still owns the
/// connection and closes it when the call returns, which the `connections_closed`
/// assertion checks.
fn dropping_while_blocked_does_not_hang(cancel: CancelKind) {
    let scenario = support::scenario();
    scenario.set_capabilities(Capabilities::none().with_cancel(cancel));
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN long_running; END;",
        Action::Block(BlockSpec::new(Arc::clone(&gate))),
    );

    let session = support::open(&scenario);
    let connection_id = session.connection_id();
    let blocked = session.execute(Statement::new("BEGIN long_running; END;"));
    assert!(
        gate.wait_until_blocked(support::short_timeout()),
        "the worker thread should have reached the gate"
    );

    let released = Arc::clone(&gate);
    let scenario_for_thread = Arc::clone(&scenario);
    with_timeout_guard(Duration::from_secs(5), move || {
        drop(blocked);
        drop(session);
        // Whether or not the worker was joined, nothing is stuck: let the
        // blocked call finish so the worker can release the connection.
        released.release();
        // The worker closes the connection once the call returns. Poll rather
        // than sleep a fixed amount.
        for _ in 0..500 {
            if scenario_for_thread.counts().connections_closed == 1 {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("a detached worker must still close its connection once the call returns");
    });

    let seen = scenario.thread_ids_seen(connection_id);
    assert_eq!(
        seen.len(),
        1,
        "even a detached worker must be the only thread that touched the connection"
    );
}

#[test]
fn dropping_a_session_blocked_in_a_native_cancel_driver_does_not_hang() {
    dropping_while_blocked_does_not_hang(CancelKind::Native);
}

#[test]
fn dropping_a_session_blocked_in_a_pre_armed_deadline_driver_does_not_hang() {
    // The hard case: nothing can stop the running call, so `Drop` has to
    // detach instead of waiting for it.
    dropping_while_blocked_does_not_hang(CancelKind::PreArmedDeadline);
}
