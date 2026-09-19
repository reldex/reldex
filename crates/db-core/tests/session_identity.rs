//! Session identity and per-session isolation
//! (`docs/exec-plans/active/phase-0.md` Workstream B).

mod support;

use reldex_driver_mock::{Action, ColumnSpec, QuerySource, ScriptValue};

fn rows_seen(session: &reldex_db_core::DatabaseSession, sql: &str) -> usize {
    let outcome = session
        .execute(reldex_db_core::Statement::new(sql))
        .wait()
        .expect("select should execute");
    let result = outcome.result.expect("select produces a result set");
    let batch = session
        .fetch_batch(result, support::n(100))
        .wait()
        .expect("fetch should succeed");
    let count = batch.row_count();
    session
        .close_result(result)
        .wait()
        .expect("close_result should succeed");
    count
}

#[test]
fn session_id_is_stable_across_many_commands() {
    let scenario = support::scenario();
    scenario.on_sql(
        "SELECT 1 FROM dual",
        Action::query(QuerySource::Fixed(reldex_driver_mock::QueryPlan::new(
            vec![ColumnSpec::new("N", reldex_db_driver_api::SqlType::Number)],
            vec![vec![ScriptValue::from(1_i64)]],
        ))),
    );
    let session = support::open(&scenario);
    let id = session.id();
    let connection_id = session.connection_id();

    for _ in 0..20 {
        session
            .execute(reldex_db_core::Statement::new("SELECT 1 FROM dual"))
            .wait()
            .expect("execute should succeed");
        session.ping().wait().expect("ping should succeed");
        assert_eq!(session.id(), id, "the session id must never change");
        assert_eq!(
            session.connection_id(),
            connection_id,
            "the session must keep owning the same connection"
        );
    }
}

#[test]
fn uncommitted_state_survives_multiple_statements_on_the_same_session() {
    let scenario = support::scenario();
    let columns = vec![ColumnSpec::new(
        "NAME",
        reldex_db_driver_api::SqlType::VARCHAR,
    )];
    scenario.on_sql(
        "INSERT INTO t VALUES ('a')",
        Action::Dml {
            rows_affected: 1,
            insert: Some(("t".to_owned(), vec![ScriptValue::from("a")])),
        },
    );
    scenario.on_sql(
        "INSERT INTO t VALUES ('b')",
        Action::Dml {
            rows_affected: 1,
            insert: Some(("t".to_owned(), vec![ScriptValue::from("b")])),
        },
    );
    scenario.on_sql(
        "SELECT * FROM t",
        Action::query(QuerySource::Table {
            table: "t".to_owned(),
            columns,
        }),
    );

    let session = support::open(&scenario);
    session
        .execute(reldex_db_core::Statement::new("INSERT INTO t VALUES ('a')"))
        .wait()
        .expect("insert a");
    assert_eq!(rows_seen(&session, "SELECT * FROM t"), 1);

    session
        .execute(reldex_db_core::Statement::new("INSERT INTO t VALUES ('b')"))
        .wait()
        .expect("insert b");
    assert_eq!(
        rows_seen(&session, "SELECT * FROM t"),
        2,
        "both uncommitted inserts must still be visible to their own session"
    );
}

#[test]
fn a_second_session_cannot_see_or_inherit_uncommitted_state() {
    let scenario = support::scenario();
    let columns = vec![ColumnSpec::new(
        "NAME",
        reldex_db_driver_api::SqlType::VARCHAR,
    )];
    scenario.on_sql(
        "INSERT INTO t VALUES ('a')",
        Action::Dml {
            rows_affected: 1,
            insert: Some(("t".to_owned(), vec![ScriptValue::from("a")])),
        },
    );
    scenario.on_sql(
        "SELECT * FROM t",
        Action::query(QuerySource::Table {
            table: "t".to_owned(),
            columns,
        }),
    );

    let owner = support::open(&scenario);
    let other = support::open(&scenario);
    assert_ne!(owner.id(), other.id());
    assert_ne!(
        owner.connection_id(),
        other.connection_id(),
        "each session owns its own connection"
    );

    owner
        .execute(reldex_db_core::Statement::new("INSERT INTO t VALUES ('a')"))
        .wait()
        .expect("insert on the owning session");

    assert_eq!(rows_seen(&owner, "SELECT * FROM t"), 1);
    assert_eq!(
        rows_seen(&other, "SELECT * FROM t"),
        0,
        "a second session must not inherit another session's uncommitted state"
    );

    owner.commit().wait().expect("commit");

    assert_eq!(
        rows_seen(&other, "SELECT * FROM t"),
        1,
        "once committed, the row becomes visible everywhere"
    );
}
