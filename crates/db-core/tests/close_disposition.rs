//! `close` with a possibly-active transaction must demand a decision, and
//! `Commit`/`Rollback` dispositions must do what they say (`SPEC.md` §10).

mod support;

use reldex_db_core::{CloseDisposition, CloseError, Statement};
use reldex_driver_mock::{Action, ScriptValue};

fn insert_action(row: &str) -> Action {
    Action::Dml {
        rows_affected: 1,
        insert: Some(("t".to_owned(), vec![ScriptValue::from(row)])),
    }
}

#[test]
fn close_without_a_disposition_is_fine_when_nothing_is_active() {
    let scenario = support::scenario();
    let mut session = support::open(&scenario);
    session
        .close(None)
        .expect("nothing is active, so this must succeed");
}

#[test]
fn close_demands_a_decision_when_a_transaction_may_be_active() {
    let scenario = support::scenario();
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert_action("a"));
    let mut session = support::open(&scenario);
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

#[test]
fn close_commit_disposition_commits_before_closing() {
    let scenario = support::scenario();
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert_action("a"));
    let mut session = support::open(&scenario);
    session
        .execute(Statement::new("INSERT INTO t VALUES ('a')"))
        .wait()
        .expect("insert");

    session
        .close(Some(CloseDisposition::Commit))
        .expect("close with commit disposition");
    assert_eq!(scenario.committed_rows("t").len(), 1);
}

#[test]
fn close_rollback_disposition_discards_the_transaction() {
    let scenario = support::scenario();
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert_action("a"));
    let mut session = support::open(&scenario);
    session
        .execute(Statement::new("INSERT INTO t VALUES ('a')"))
        .wait()
        .expect("insert");

    session
        .close(Some(CloseDisposition::Rollback))
        .expect("close with rollback disposition");
    assert!(scenario.committed_rows("t").is_empty());
}

#[test]
fn close_is_idempotent() {
    let scenario = support::scenario();
    let mut session = support::open(&scenario);
    session.close(None).expect("first close");
    session
        .close(None)
        .expect("second close is a no-op success");
    session
        .close(Some(CloseDisposition::Commit))
        .expect("even with a disposition, closing again is a no-op success");
}

#[test]
fn requests_after_close_fail_fast_instead_of_hanging() {
    let scenario = support::scenario();
    let mut session = support::open(&scenario);
    session.close(None).expect("close");

    let error = session
        .execute(Statement::new("SELECT 1 FROM dual"))
        .wait()
        .expect_err("a closed session must refuse further work");
    assert_eq!(error.kind(), reldex_db_driver_api::ErrorKind::Connection);
}
