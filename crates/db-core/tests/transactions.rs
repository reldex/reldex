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

fn commit_resolves_the_transaction(path: support::ReplyPath) {
    let scenario = support::scenario();
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert_action("a"));
    let session = support::open_on(&scenario, path);

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

fn rollback_resolves_the_transaction_and_discards_it(path: support::ReplyPath) {
    let scenario = support::scenario();
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert_action("a"));
    let session = support::open_on(&scenario, path);

    session
        .execute(Statement::new("INSERT INTO t VALUES ('a')"))
        .wait()
        .expect("insert");
    assert!(session.has_possibly_active_transaction());

    session.rollback().wait().expect("rollback");
    assert!(!session.has_possibly_active_transaction());
    assert!(scenario.committed_rows("t").is_empty());
}

fn savepoint_and_rollback_to_savepoint_undo_only_later_work(path: support::ReplyPath) {
    let scenario = support::scenario();
    for row in ["a", "b", "c"] {
        scenario.on_sql(
            format!("INSERT INTO t VALUES ('{row}')"),
            insert_action(row),
        );
    }
    let session = support::open_on(&scenario, path);

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

fn rollback_to_a_savepoint_that_does_not_exist_is_reported(path: support::ReplyPath) {
    let scenario = support::scenario();
    let session = support::open_on(&scenario, path);
    let sp = SavepointName::new("never_created").expect("valid name");
    let error = session
        .rollback_to_savepoint(sp)
        .wait()
        .expect_err("no such savepoint");
    assert_eq!(error.kind(), reldex_db_driver_api::ErrorKind::Transaction);
}

fn ddl_reports_implicit_commit_and_resets_tracking(path: support::ReplyPath) {
    let scenario = support::scenario();
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert_action("a"));
    scenario.on_sql("CREATE TABLE u (x NUMBER)", Action::Ddl);
    let session = support::open_on(&scenario, path);

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

fn an_imprecise_driver_still_yields_a_conservative_combined_answer(path: support::ReplyPath) {
    // `Capabilities::exact_transaction_state == false` (ADR-0002 D4): the
    // driver alone can only ever say `Unknown`. Core-side tracking from
    // `StatementKind` must still make `has_possibly_active_transaction` swing
    // to true after DML and back to false after commit.
    let scenario = support::scenario();
    scenario.set_capabilities(Capabilities::none().with_exact_transaction_state(false));
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert_action("a"));
    let session = support::open_on(&scenario, path);

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

fn a_default_constructed_state_is_the_safe_unknown_default(path: support::ReplyPath) {
    // `TransactionState::default()` is `Unknown`, not `Inactive`
    // (ADR-0002, amendment S2) — the safe answer for a connection nobody has
    // classified yet. `db-core` asks the driver for its real state as soon as
    // a session opens, so an *exact* driver that genuinely knows nothing has
    // happened yet reports `Inactive` immediately...
    assert_eq!(TransactionState::default(), TransactionState::Unknown);
    assert!(TransactionState::default().may_be_open());

    let scenario = support::scenario();
    let session = support::open_on(&scenario, path);
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
    let imprecise_session = support::open_on(&imprecise_scenario, path);
    assert!(
        imprecise_session.has_possibly_active_transaction(),
        "an imprecise driver must not be trusted to say Inactive on its own"
    );
}

/// `SELECT … FOR UPDATE` opens a transaction and is still a `Query`.
///
/// Core-side tracking used to key only on `StatementKind`, so on a driver with
/// exact transaction state a locking query left
/// `has_possibly_active_transaction()` false — and closing the worksheet
/// silently discarded row locks and an open transaction. No statement kind can
/// distinguish this from a plain `SELECT`, so the core has to be conservative
/// and only dismiss a query when the driver says `Inactive` afterwards.
fn a_locking_query_keeps_the_transaction_flag_even_on_an_exact_driver(path: support::ReplyPath) {
    let scenario = support::scenario();
    let plan = reldex_driver_mock::QueryPlan::new(
        vec![reldex_driver_mock::ColumnSpec::new(
            "ID",
            reldex_db_driver_api::SqlType::Number,
        )],
        vec![vec![ScriptValue::from(1_i64)]],
    );
    scenario.on_sql(
        "SELECT id FROM t",
        Action::query(reldex_driver_mock::QuerySource::Fixed(plan.clone())),
    );
    scenario.on_sql(
        "SELECT id FROM t FOR UPDATE",
        Action::locking_query(reldex_driver_mock::QuerySource::Fixed(plan)),
    );
    scenario.on_sql(
        "SET TRANSACTION READ ONLY",
        Action::Execute {
            statement_kind: reldex_db_core::StatementKind::TransactionControl,
            rows_affected: None,
            opens_transaction: true,
        },
    );

    let session = support::open_on(&scenario, path);

    // A plain query on an exact driver leaves nothing open.
    session
        .execute(Statement::new("SELECT id FROM t"))
        .wait()
        .expect("plain select");
    assert!(
        !session.has_possibly_active_transaction(),
        "an ordinary SELECT on an exact driver must not make the user answer a prompt"
    );

    // A locking one does.
    session
        .execute(Statement::new("SELECT id FROM t FOR UPDATE"))
        .wait()
        .expect("locking select");
    assert!(
        session.has_possibly_active_transaction(),
        "SELECT … FOR UPDATE opened a transaction; closing over it must prompt"
    );

    session.rollback().wait().expect("rollback");
    assert!(!session.has_possibly_active_transaction());

    // And so does transaction control, which is also not DML.
    session
        .execute(Statement::new("SET TRANSACTION READ ONLY"))
        .wait()
        .expect("set transaction");
    assert!(session.has_possibly_active_transaction());
}

/// A commit can invalidate every cursor and LOB locator it left open, and
/// `db-core` must surface that rather than return a short result that looks
/// complete.
fn handles_invalidated_by_a_commit_report_rather_than_looking_exhausted(path: support::ReplyPath) {
    let scenario = support::scenario();
    scenario.set_invalidate_handles_on_transaction_end(true);
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert_action("a"));
    scenario.on_sql(
        "SELECT id FROM t",
        Action::query(reldex_driver_mock::QuerySource::Fixed(
            reldex_driver_mock::QueryPlan::new(
                vec![reldex_driver_mock::ColumnSpec::new(
                    "ID",
                    reldex_db_driver_api::SqlType::Number,
                )],
                (0..10_i64).map(|id| vec![ScriptValue::from(id)]).collect(),
            ),
        )),
    );

    let session = support::open_on(&scenario, path);
    let outcome = session
        .execute(Statement::new("SELECT id FROM t"))
        .wait()
        .expect("select");
    let result = outcome.result.expect("cursor");
    let batch = session
        .fetch_batch(result, support::n(3))
        .wait()
        .expect("first batch");
    assert_eq!(batch.row_count(), 3);

    session
        .execute(Statement::new("INSERT INTO t VALUES ('a')"))
        .wait()
        .expect("insert");
    session.commit().wait().expect("commit");

    // `db-core` releases every result a commit invalidates, so the handle is
    // gone; what must never happen is an empty batch that reads as "complete".
    let error = session
        .fetch_batch(result, support::n(3))
        .wait()
        .expect_err("a result invalidated by the commit must report, not look exhausted");
    assert!(error.message().contains("result handle"), "{error}");
}

support::both_paths! {
    commit_resolves_the_transaction,
    rollback_resolves_the_transaction_and_discards_it,
    savepoint_and_rollback_to_savepoint_undo_only_later_work,
    rollback_to_a_savepoint_that_does_not_exist_is_reported,
    ddl_reports_implicit_commit_and_resets_tracking,
    an_imprecise_driver_still_yields_a_conservative_combined_answer,
    a_default_constructed_state_is_the_safe_unknown_default,
    a_locking_query_keeps_the_transaction_flag_even_on_an_exact_driver,
    handles_invalidated_by_a_commit_report_rather_than_looking_exhausted,
}
