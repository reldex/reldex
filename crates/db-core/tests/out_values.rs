//! Output binds through `DatabaseSession`, including a nested `REF CURSOR`.
//!
//! Contract gap C-1: `reldex_db_driver_api::OutValues` used to expose only
//! `&Value`, so the `Box<dyn Cursor>` inside a `Value::Cursor` could never be
//! owned and a `REF CURSOR` was unreadable end to end. With
//! `ExecutionOutcome::take_out_values` the core can own it — and because a
//! cursor is a handle derived from the connection, `db-core` keeps it on the
//! session's worker thread and hands the caller a [`ResultSetId`] instead,
//! exactly as it does for the cursor an ordinary query produces
//! (ADR-0002 D1/D2).

mod support;

use std::thread;

use reldex_db_core::{OutValue, Statement, StatementKind};
use reldex_db_driver_api::SqlType;
use reldex_driver_mock::{Action, ColumnSpec, QueryPlan, QuerySource, ScriptValue};

const OPEN_CURSOR: &str = "BEGIN OPEN :rc FOR SELECT id FROM t; END;";

fn script_a_ref_cursor(rows: i64) -> std::sync::Arc<reldex_driver_mock::Scenario> {
    let scenario = support::scenario();
    let plan = QueryPlan::new(
        vec![ColumnSpec::new("ID", SqlType::Number)],
        (0..rows).map(|id| vec![ScriptValue::from(id)]).collect(),
    );
    scenario.on_sql(
        OPEN_CURSOR,
        Action::RefCursorOut {
            name: "rc".to_owned(),
            source: QuerySource::Fixed(plan),
        },
    );
    scenario
}

#[test]
fn a_ref_cursor_out_bind_becomes_a_result_handle_on_the_worker_thread() {
    let scenario = script_a_ref_cursor(5);
    let session = support::open(&scenario);
    let connection_id = session.connection_id();

    let outcome = session
        .execute(Statement::new(OPEN_CURSOR))
        .wait()
        .expect("execute");
    assert_eq!(outcome.statement_kind, StatementKind::PlSqlBlock);
    assert!(
        outcome.result.is_none(),
        "the block itself produced no result set; the cursor came through a bind"
    );
    assert!(!outcome.out_values.is_empty());

    let result = match outcome.out_values.named("rc") {
        Some(OutValue::Result(id)) => *id,
        other => panic!("expected a nested result handle, got {other:?}"),
    };
    assert!(
        outcome.out_values.named("other").is_none(),
        "only the declared bind is reported"
    );

    // It is fetched through the ordinary result API, in bounded batches.
    let mut total = 0;
    let mut batches = 0;
    loop {
        let batch = session
            .fetch_batch(result, support::n(2))
            .wait()
            .expect("fetching a nested cursor is an ordinary fetch");
        if batch.is_empty() {
            break;
        }
        assert!(batch.row_count() <= 2);
        batches += 1;
        total += batch.row_count();
    }
    assert_eq!(total, 5);
    assert_eq!(batches, 3, "2 + 2 + 1");

    session
        .close_result(result)
        .wait()
        .expect("closing a nested result is an ordinary close");
    // Closing twice is a no-op, and a closed handle is gone.
    session.close_result(result).wait().expect("idempotent");
    let error = session
        .fetch_batch(result, support::n(2))
        .wait()
        .expect_err("a closed result handle no longer fetches");
    assert!(error.message().contains("result handle"), "{error}");

    // Everything the cursor did happened on the session's one worker thread.
    let seen = scenario.thread_ids_seen(connection_id);
    assert_eq!(
        seen.len(),
        1,
        "a nested cursor must not migrate off its connection's worker thread"
    );
    assert_ne!(seen[0], thread::current().id());
}

#[test]
fn a_nested_result_is_released_when_the_session_closes() {
    let scenario = script_a_ref_cursor(3);
    let session = support::open(&scenario);
    let outcome = session
        .execute(Statement::new(OPEN_CURSOR))
        .wait()
        .expect("execute");
    let result = match outcome.out_values.named("rc") {
        Some(OutValue::Result(id)) => *id,
        other => panic!("expected a nested result handle, got {other:?}"),
    };
    // Left open on purpose: the session owns it, so closing the session must
    // release it rather than leak the worker thread or the cursor.
    session
        .close(Some(reldex_db_core::CloseDisposition::Rollback))
        .expect("close");
    let error = session
        .fetch_batch(result, support::n(2))
        .wait()
        .expect_err("the session and everything derived from it are gone");
    assert!(!error.message().is_empty());
}
