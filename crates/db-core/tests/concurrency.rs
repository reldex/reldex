//! N independent sessions make progress in parallel and never share state
//! (`SPEC.md` §24.17; `docs/architecture/ARCHITECTURE.md` §6).

mod support;

use std::thread;

use reldex_db_core::Statement;
use reldex_driver_mock::{Action, ColumnSpec, QuerySource, ScriptValue};

const SESSION_COUNT: usize = 8;

#[test]
fn eight_concurrent_sessions_make_progress_independently_and_never_share_state() {
    let scenario = support::scenario();
    for i in 0..SESSION_COUNT {
        let table = format!("t{i}");
        scenario.on_sql(
            format!("INSERT INTO {table} VALUES ('row')"),
            Action::Dml {
                rows_affected: 1,
                insert: Some((table.clone(), vec![ScriptValue::from("row")])),
            },
        );
        scenario.on_sql(
            format!("SELECT * FROM {table}"),
            Action::Query(QuerySource::Table {
                table,
                columns: vec![ColumnSpec::new(
                    "NAME",
                    reldex_db_driver_api::SqlType::VARCHAR,
                )],
            }),
        );
    }

    let handles: Vec<_> = (0..SESSION_COUNT)
        .map(|i| {
            let scenario = std::sync::Arc::clone(&scenario);
            thread::spawn(move || {
                let session = support::open(&scenario);
                let table = format!("t{i}");
                for _ in 0..5 {
                    session
                        .execute(Statement::new(format!(
                            "INSERT INTO {table} VALUES ('row')"
                        )))
                        .wait()
                        .expect("insert should succeed independently of other sessions");
                }
                session.commit().wait().expect("commit");

                let outcome = session
                    .execute(Statement::new(format!("SELECT * FROM {table}")))
                    .wait()
                    .expect("select");
                let result = outcome.result.expect("select produced a result");
                let batch = session
                    .fetch_batch(result, support::n(100))
                    .wait()
                    .expect("fetch");
                assert_eq!(
                    batch.row_count(),
                    5,
                    "session {i} must see exactly its own five committed rows"
                );
                session.id()
            })
        })
        .collect();

    let ids: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("no session thread should panic"))
        .collect();

    let mut unique = ids.clone();
    unique.sort_unstable_by_key(|id| id.get());
    unique.dedup();
    assert_eq!(
        unique.len(),
        SESSION_COUNT,
        "every session must have a distinct id"
    );

    for i in 0..SESSION_COUNT {
        assert_eq!(
            scenario.committed_rows(&format!("t{i}")).len(),
            5,
            "table t{i} must hold exactly the rows its own session committed"
        );
    }
}
