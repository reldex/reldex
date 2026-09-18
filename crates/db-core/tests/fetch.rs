//! Multi-batch fetch to exhaustion, LOB streaming on the worker thread, an
//! `Unsupported` column that does not fail the batch, and proof that the
//! caller's own thread never runs driver code (ADR-0002 D1/D2).

mod support;

use std::thread;

use reldex_db_core::Statement;
use reldex_db_driver_api::{LobKind, SqlType};
use reldex_driver_mock::{Action, ColumnSpec, QueryPlan, QuerySource, ScriptValue};

#[test]
fn multi_batch_fetch_runs_to_exhaustion() {
    let scenario = support::scenario();
    let columns = vec![ColumnSpec::new("N", SqlType::Number)];
    let rows: Vec<Vec<ScriptValue>> = (0..7_i64).map(|v| vec![ScriptValue::from(v)]).collect();
    scenario.on_sql(
        "SELECT * FROM t",
        Action::Query(QuerySource::Fixed(QueryPlan::new(columns, rows))),
    );

    let session = support::open(&scenario);
    let outcome = session
        .execute(Statement::new("SELECT * FROM t"))
        .wait()
        .expect("execute");
    let result = outcome.result.expect("cursor");

    let mut total = 0;
    let mut batches = 0;
    loop {
        let batch = session
            .fetch_batch(result, support::n(3))
            .wait()
            .expect("fetch should not fail before exhaustion");
        if batch.is_empty() {
            break;
        }
        batches += 1;
        total += batch.row_count();
    }
    assert_eq!(total, 7);
    assert_eq!(batches, 3, "3 + 3 + 1 rows across three non-empty batches");

    // Once exhausted, fetching again stays a clean empty batch, not an error.
    let again = session
        .fetch_batch(result, support::n(3))
        .wait()
        .expect("fetch after exhaustion");
    assert!(again.is_empty());
}

#[test]
fn unsupported_column_does_not_fail_the_batch() {
    let scenario = support::scenario();
    let columns = vec![
        ColumnSpec::new("NAME", SqlType::VARCHAR),
        ColumnSpec::new("SPAN", SqlType::Unsupported)
            .with_native_type_name("INTERVAL DAY TO SECOND"),
    ];
    let rows = vec![
        vec![
            ScriptValue::from("a"),
            ScriptValue::Unsupported("+01 02:03:04".to_owned()),
        ],
        vec![ScriptValue::from("b"), ScriptValue::Null],
    ];
    scenario.on_sql(
        "SELECT * FROM t",
        Action::Query(QuerySource::Fixed(QueryPlan::new(columns, rows))),
    );

    let session = support::open(&scenario);
    let outcome = session
        .execute(Statement::new("SELECT * FROM t"))
        .wait()
        .expect("execute");
    let result = outcome.result.expect("cursor");
    let batch = session
        .fetch_batch(result, support::n(10))
        .wait()
        .expect("fetch");

    assert_eq!(batch.row_count(), 2);
    assert_eq!(batch.value(0, 0).and_then(|v| v.as_str()), Some("a"));
    assert_eq!(
        batch.value(0, 1).and_then(|v| v.as_unsupported_text()),
        Some("+01 02:03:04")
    );
    assert!(!batch.value(0, 1).expect("cell").is_null());
    // A genuine NULL in an unsupported column is still a NULL, and the other
    // column is unaffected.
    assert!(batch.value(1, 1).expect("cell").is_null());
    assert_eq!(batch.value(1, 0).and_then(|v| v.as_str()), Some("b"));
}

#[test]
fn lob_streaming_reads_in_chunks_on_the_worker_thread() {
    let scenario = support::scenario();
    let columns = vec![ColumnSpec::new(
        "DOC",
        SqlType::CharacterLob { national: false },
    )];
    let content = "hello, this is a lob read in small pieces";
    scenario.on_sql(
        "SELECT doc FROM t",
        Action::Query(QuerySource::Fixed(QueryPlan::new(
            columns,
            vec![vec![ScriptValue::Lob {
                kind: LobKind::Character,
                bytes: content.as_bytes().to_vec(),
            }]],
        ))),
    );

    let session = support::open(&scenario);
    let connection_id = session.connection_id();
    let outcome = session
        .execute(Statement::new("SELECT doc FROM t"))
        .wait()
        .expect("execute");
    let result = outcome.result.expect("cursor");
    let mut batch = session
        .fetch_batch(result, support::n(10))
        .wait()
        .expect("fetch");

    let locator = batch
        .column_mut(0)
        .and_then(|column| column.take_lob(0))
        .expect("row 0 holds a LOB");

    let mut locator = locator;
    let mut collected = Vec::new();
    loop {
        let (returned, chunk) = session
            .read_lob_chunk(locator, support::n(5))
            .wait()
            .expect("reading a chunk should succeed");
        locator = returned;
        if chunk.is_empty() {
            break;
        }
        collected.extend_from_slice(&chunk);
    }
    assert_eq!(String::from_utf8(collected).expect("valid utf-8"), content);

    // The read happened on the session's one worker thread, never on this
    // test's own calling thread.
    let seen = scenario.thread_ids_seen(connection_id);
    assert_eq!(
        seen.len(),
        1,
        "only the worker thread should have touched the connection"
    );
    assert_ne!(seen[0], thread::current().id());
}

#[test]
fn caller_thread_never_executes_driver_code() {
    let scenario = support::scenario();
    scenario.on_sql(
        "SELECT 1 FROM dual",
        Action::Query(QuerySource::Fixed(QueryPlan::new(
            vec![ColumnSpec::new("N", SqlType::Number)],
            vec![vec![ScriptValue::from(1_i64)]],
        ))),
    );
    let session = support::open(&scenario);
    let connection_id = session.connection_id();

    let outcome = session
        .execute(Statement::new("SELECT 1 FROM dual"))
        .wait()
        .expect("execute");
    let result = outcome.result.expect("cursor");
    session
        .fetch_batch(result, support::n(10))
        .wait()
        .expect("fetch");
    session.commit().wait().expect("commit");
    session.ping().wait().expect("ping");

    let seen = scenario.thread_ids_seen(connection_id);
    assert_eq!(
        seen.len(),
        1,
        "every driver call for one connection must land on one thread"
    );
    assert_ne!(
        seen[0],
        thread::current().id(),
        "the calling (test) thread must never itself execute driver code"
    );
}
