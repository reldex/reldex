//! Commit / rollback / savepoint / rollback-to-savepoint behaviour and
//! conservative transaction-state tracking (ADR-0002 D4/D6).

mod support;

use reldex_db_core::{SavepointName, Statement, TransactionState};
use reldex_db_driver_api::Capabilities;
use reldex_driver_mock::{Action, ScriptValue};

fn insert_action(row: &str) -> Action {
    Action::Dml {
        rows_affected: 1,
        insert: Some(("t".to_owned(), vec![ScriptValue::from(row)])),
    }
}

#[test]
fn commit_resolves_the_transaction() {
    let scenario = support::scenario();
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert_action("a"));
    let session = support::open(&scenario);

    assert!(!session.has_possibly_active_transaction());
    session
        .execute(Statement::new("INSERT INTO t VALUES ('a')"))
        .wait()
        .expect("insert");
    assert!(session.has_possibly_active_transaction());

    session.commit().wait().expect("commit");
    assert!(!session.has_possibly_active_transaction());
    assert_eq!(scenario.committed_rows("t").len(), 1);
}

#[test]
fn rollback_resolves_the_transaction_and_discards_it() {
    let scenario = support::scenario();
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert_action("a"));
    let session = support::open(&scenario);

    session
        .execute(Statement::new("INSERT INTO t VALUES ('a')"))
        .wait()
        .expect("insert");
    assert!(session.has_possibly_active_transaction());

    session.rollback().wait().expect("rollback");
    assert!(!session.has_possibly_active_transaction());
    assert!(scenario.committed_rows("t").is_empty());
}

#[test]
fn savepoint_and_rollback_to_savepoint_undo_only_later_work() {
    let scenario = support::scenario();
    for row in ["a", "b", "c"] {
        scenario.on_sql(
            format!("INSERT INTO t VALUES ('{row}')"),
            insert_action(row),
        );
    }
    let session = support::open(&scenario);

    session
        .execute(Statement::new("INSERT INTO t VALUES ('a')"))
        .wait()
        .expect("insert a");
    let sp = SavepointName::new("sp1").expect("valid savepoint name");
    session.savepoint(sp.clone()).wait().expect("savepoint");
    assert!(
        session.has_possibly_active_transaction(),
        "a savepoint does not close the transaction"
    );

    session
        .execute(Statement::new("INSERT INTO t VALUES ('b')"))
        .wait()
        .expect("insert b");
    session
        .execute(Statement::new("INSERT INTO t VALUES ('c')"))
        .wait()
        .expect("insert c");

    session
        .rollback_to_savepoint(sp)
        .wait()
        .expect("rollback to savepoint");
    assert!(
        session.has_possibly_active_transaction(),
        "rollback to savepoint leaves the transaction open"
    );

    session.commit().wait().expect("commit");
    assert_eq!(
        scenario.committed_rows("t"),
        vec![vec![ScriptValue::from("a")]]
    );
}

#[test]
fn rollback_to_a_savepoint_that_does_not_exist_is_reported() {
    let scenario = support::scenario();
    let session = support::open(&scenario);
    let sp = SavepointName::new("never_created").expect("valid name");
    let error = session
        .rollback_to_savepoint(sp)
        .wait()
        .expect_err("no such savepoint");
    assert_eq!(error.kind(), reldex_db_driver_api::ErrorKind::Transaction);
}

#[test]
fn ddl_reports_implicit_commit_and_resets_tracking() {
    let scenario = support::scenario();
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert_action("a"));
    scenario.on_sql("CREATE TABLE u (x NUMBER)", Action::Ddl);
    let session = support::open(&scenario);

    session
        .execute(Statement::new("INSERT INTO t VALUES ('a')"))
        .wait()
        .expect("insert");
    assert!(session.has_possibly_active_transaction());

    let outcome = session
        .execute(Statement::new("CREATE TABLE u (x NUMBER)"))
        .wait()
        .expect("ddl");
    assert_eq!(outcome.statement_kind, reldex_db_core::StatementKind::Ddl);
    assert!(outcome.committed_implicitly);
    assert!(
        !session.has_possibly_active_transaction(),
        "an implicit commit must reset core-side tracking"
    );
    // The DDL's own implicit commit also flushed the pending insert.
    assert_eq!(scenario.committed_rows("t").len(), 1);
}

#[test]
fn an_imprecise_driver_still_yields_a_conservative_combined_answer() {
    // `Capabilities::exact_transaction_state == false` (ADR-0002 D4): the
    // driver alone can only ever say `Unknown`. Core-side tracking from
    // `StatementKind` must still make `has_possibly_active_transaction` swing
    // to true after DML and back to false after commit.
    let scenario = support::scenario();
    scenario.set_capabilities(Capabilities::none().with_exact_transaction_state(false));
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert_action("a"));
    let session = support::open(&scenario);

    session
        .execute(Statement::new("INSERT INTO t VALUES ('a')"))
        .wait()
        .expect("insert");
    assert!(session.has_possibly_active_transaction());

    session.commit().wait().expect("commit");
    // The driver itself reports `Inactive` only right after commit/rollback
    // (ADR-0002 D4), so the combined answer can now be false.
    assert!(!session.has_possibly_active_transaction());
}

#[test]
fn a_default_constructed_state_is_the_safe_unknown_default() {
    // `TransactionState::default()` is `Unknown`, not `Inactive`
    // (ADR-0002, amendment S2) — the safe answer for a connection nobody has
    // classified yet. `db-core` asks the driver for its real state as soon as
    // a session opens, so an *exact* driver that genuinely knows nothing has
    // happened yet reports `Inactive` immediately...
    assert_eq!(TransactionState::default(), TransactionState::Unknown);
    assert!(TransactionState::default().may_be_open());

    let scenario = support::scenario();
    let session = support::open(&scenario);
    assert!(
        !session.has_possibly_active_transaction(),
        "an exact driver that has done nothing yet is not possibly active"
    );

    // ...while an imprecise driver can only ever say `Unknown` until its own
    // `commit`/`rollback` runs, so a freshly opened session on one is
    // conservative from the very first moment, per the same ADR trade-off
    // exercised in `an_imprecise_driver_still_yields_a_conservative_combined_answer`.
    let imprecise_scenario = support::scenario();
    imprecise_scenario.set_capabilities(Capabilities::none().with_exact_transaction_state(false));
    let imprecise_session = support::open(&imprecise_scenario);
    assert!(
        imprecise_session.has_possibly_active_transaction(),
        "an imprecise driver must not be trusted to say Inactive on its own"
    );
}
