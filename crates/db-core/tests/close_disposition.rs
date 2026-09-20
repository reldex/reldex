//! `close` with a possibly-active transaction must demand a decision, and
//! `Commit`/`Rollback` dispositions must do what they say (`SPEC.md` §10).

mod support;

use std::sync::Arc;

use reldex_db_core::{CloseDisposition, CloseError, SessionLifecycle, Statement};
use reldex_db_driver_api::ErrorKind;
use reldex_driver_mock::{Action, BlockGate, BlockSpec, ScriptValue, ScriptedError};

fn insert_action(row: &str) -> Action {
    Action::Dml {
        rows_affected: 1,
        insert: Some(("t".to_owned(), vec![ScriptValue::from(row)])),
    }
}

fn close_without_a_disposition_is_fine_when_nothing_is_active(path: support::ReplyPath) {
    let scenario = support::scenario();
    let session = support::open_on(&scenario, path);
    session
        .close(None)
        .expect("nothing is active, so this must succeed");
}

fn close_demands_a_decision_when_a_transaction_may_be_active(path: support::ReplyPath) {
    let scenario = support::scenario();
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert_action("a"));
    let session = support::open_on(&scenario, path);
    session
        .execute(Statement::new("INSERT INTO t VALUES ('a')"))
        .wait()
        .expect("insert");

    let error = session.close(None).expect_err("a decision is required");
    assert!(matches!(error, CloseError::DecisionRequired));

    // The session must still be open and usable after being refused.
    session.ping().wait().expect("session remains usable");
    assert!(session.has_possibly_active_transaction());

    session
        .close(Some(CloseDisposition::Rollback))
        .expect("closing with a disposition now succeeds");
}

fn close_commit_disposition_commits_before_closing(path: support::ReplyPath) {
    let scenario = support::scenario();
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert_action("a"));
    let session = support::open_on(&scenario, path);
    session
        .execute(Statement::new("INSERT INTO t VALUES ('a')"))
        .wait()
        .expect("insert");

    session
        .close(Some(CloseDisposition::Commit))
        .expect("close with commit disposition");
    assert_eq!(scenario.committed_rows("t").len(), 1);
}

fn close_rollback_disposition_discards_the_transaction(path: support::ReplyPath) {
    let scenario = support::scenario();
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert_action("a"));
    let session = support::open_on(&scenario, path);
    session
        .execute(Statement::new("INSERT INTO t VALUES ('a')"))
        .wait()
        .expect("insert");

    session
        .close(Some(CloseDisposition::Rollback))
        .expect("close with rollback disposition");
    assert!(scenario.committed_rows("t").is_empty());
}

fn close_is_idempotent(path: support::ReplyPath) {
    let scenario = support::scenario();
    let session = support::open_on(&scenario, path);
    session.close(None).expect("first close");
    session
        .close(None)
        .expect("second close is a no-op success");
    session
        .close(Some(CloseDisposition::Commit))
        .expect("even with a disposition, closing again is a no-op success");
}

fn requests_after_close_fail_fast_instead_of_hanging(path: support::ReplyPath) {
    let scenario = support::scenario();
    let session = support::open_on(&scenario, path);
    session.close(None).expect("close");

    let error = session
        .execute(Statement::new("SELECT 1 FROM dual"))
        .wait()
        .expect_err("a closed session must refuse further work");
    assert_eq!(error.kind(), ErrorKind::Connection);
}

fn a_closed_session_reports_closed_rather_than_usable(path: support::ReplyPath) {
    let scenario = support::scenario();
    let session = support::open_on(&scenario, path);
    assert_eq!(session.session_state(), SessionLifecycle::Usable);

    session.close(None).expect("close");

    assert_eq!(
        session.session_state(),
        SessionLifecycle::Closed,
        "a closed session that still reported `Usable` told callers they could submit work"
    );
    assert!(!session.session_state().is_usable());
    assert!(session.session_state().is_terminal());
    assert!(session.is_closed());
    assert!(
        !session.is_lost(),
        "closing on purpose is not the same as losing the session"
    );
}

/// `close(None)` must decide **on the worker**, after the queue has drained.
///
/// The DML here is still blocked in the driver when `close(None)` is called, so
/// the transaction flag the calling thread can see is a snapshot from before
/// it. Deciding against that snapshot discarded a live transaction in silence,
/// which is precisely what `SPEC.md` §10 forbids.
#[test]
fn close_without_a_disposition_decides_after_the_queued_statements_have_run() {
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.on_sql(
        "INSERT INTO t VALUES ('slow')",
        Action::Block(
            BlockSpec::new(Arc::clone(&gate))
                .with_release_statement_kind(reldex_db_core::StatementKind::Dml)
                .with_release_rows_affected(1),
        ),
    );

    let session = support::open(&scenario);
    assert!(
        !session.has_possibly_active_transaction(),
        "nothing has run yet on this exact-state driver"
    );

    let insert = session.execute(Statement::new("INSERT INTO t VALUES ('slow')"));
    assert!(gate.wait_until_blocked(support::short_timeout()));

    // Submitted while the DML is still in flight, and released from another
    // thread so `close` can block on the worker's answer.
    let releaser = std::thread::spawn({
        let gate = Arc::clone(&gate);
        move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            gate.release();
        }
    });
    let error = session
        .close(None)
        .expect_err("the queued DML opened a transaction, so a decision is required");
    assert!(matches!(error, CloseError::DecisionRequired));
    assert!(error.session_is_still_open());

    releaser.join().expect("releaser thread");
    insert.wait().expect("the DML itself still succeeded");

    // Refused, so the session is untouched and can still be closed properly.
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("closing with a disposition now succeeds");
}

/// A commit that fails during `close` must not cost the user the transaction.
fn a_failed_commit_during_close_keeps_the_transaction_and_the_session(path: support::ReplyPath) {
    let scenario = support::scenario();
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert_action("a"));
    let session = support::open_on(&scenario, path);
    session
        .execute(Statement::new("INSERT INTO t VALUES ('a')"))
        .wait()
        .expect("insert");

    scenario.fail_commit(
        ScriptedError::new(ErrorKind::Resource, "unable to extend rollback segment")
            .with_native(1650, "ORA-01650: unable to extend rollback segment"),
    );

    let error = session
        .close(Some(CloseDisposition::Commit))
        .expect_err("the commit failed, so the close must fail too");
    let CloseError::CommitFailed(cause) = &error else {
        panic!("the caller must be told which step failed, got {error:?}");
    };
    assert_eq!(cause.kind(), ErrorKind::Resource);
    assert_eq!(
        cause.native().map(reldex_db_driver_api::NativeError::code),
        Some(1650)
    );
    assert!(error.session_is_still_open());

    // The session is still open and the transaction is still there.
    session.ping().wait().expect("the session is still usable");
    assert!(session.has_possibly_active_transaction());
    assert!(
        scenario.committed_rows("t").is_empty(),
        "nothing was committed"
    );

    // So the caller can decide again — here, keep the data.
    scenario.allow_commit();
    session
        .close(Some(CloseDisposition::Commit))
        .expect("retrying the commit now succeeds");
    assert_eq!(scenario.committed_rows("t").len(), 1);
}

fn a_failed_rollback_during_close_also_leaves_the_session_open(path: support::ReplyPath) {
    let scenario = support::scenario();
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert_action("a"));
    let session = support::open_on(&scenario, path);
    session
        .execute(Statement::new("INSERT INTO t VALUES ('a')"))
        .wait()
        .expect("insert");

    scenario.fail_rollback(ScriptedError::new(ErrorKind::Other, "rollback refused"));
    let error = session
        .close(Some(CloseDisposition::Rollback))
        .expect_err("the rollback failed");
    assert!(matches!(error, CloseError::RollbackFailed(_)));
    assert!(error.session_is_still_open());

    scenario.allow_rollback();
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("retrying now succeeds");
}

fn a_failing_connection_close_is_reported_but_the_session_is_gone(path: support::ReplyPath) {
    let scenario = support::scenario();
    scenario.fail_close(ScriptedError::new(ErrorKind::Other, "close failed"));
    let session = support::open_on(&scenario, path);

    let error = session.close(None).expect_err("the close itself failed");
    assert!(matches!(error, CloseError::Failed(_)));
    assert!(
        !error.session_is_still_open(),
        "a failed close still ends the session; only the report survives"
    );
    session.close(None).expect("closing again is a no-op");
}

support::both_paths! {
    close_without_a_disposition_is_fine_when_nothing_is_active,
    close_demands_a_decision_when_a_transaction_may_be_active,
    close_commit_disposition_commits_before_closing,
    close_rollback_disposition_discards_the_transaction,
    close_is_idempotent,
    requests_after_close_fail_fast_instead_of_hanging,
    a_closed_session_reports_closed_rather_than_usable,
    a_failed_commit_during_close_keeps_the_transaction_and_the_session,
    a_failed_rollback_during_close_also_leaves_the_session_open,
    a_failing_connection_close_is_reported_but_the_session_is_gone,
}
