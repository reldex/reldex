//! Nothing a session hands out may leak, and nothing it caught may be poked
//! again: dropped completions, per-session limits, foreign handles, and a
//! driver call that panicked.

mod support;

use std::sync::Arc;

use reldex_db_core::{CloseDisposition, SessionLimits, Statement};
use reldex_db_driver_api::{ErrorKind, LobKind, SqlType};
use reldex_driver_mock::{
    Action, BlockGate, BlockSpec, ColumnSpec, QueryPlan, QuerySource, ScriptValue,
};

const BLOCKER: &str = "BEGIN long_running; END;";

fn one_row_query() -> Action {
    Action::query(QuerySource::Fixed(QueryPlan::new(
        vec![ColumnSpec::new("N", SqlType::Number)],
        vec![vec![ScriptValue::from(1_i64)]],
    )))
}

/// A caller that stops listening must not strand the cursor the statement
/// opened.
///
/// The cursor is registered before the reply is sent, so if the reply cannot be
/// delivered the [`reldex_db_core::ResultId`] is gone and nothing can ever close
/// the cursor — it used to live until the session did. The blocked statement in
/// front makes the race deterministic: the query has not run yet when its
/// completion is dropped, so the reply is guaranteed to fail.
#[test]
fn a_dropped_execute_completion_releases_the_cursor_it_registered() {
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.on_sql(BLOCKER, Action::Block(BlockSpec::new(Arc::clone(&gate))));
    scenario.on_sql("SELECT 1 FROM dual", one_row_query());

    let session = support::open(&scenario);
    let blocked = session.execute(Statement::new(BLOCKER));
    assert!(gate.wait_until_blocked(support::short_timeout()));

    let abandoned = session.execute(Statement::new("SELECT 1 FROM dual"));
    drop(abandoned);

    gate.release();
    blocked.wait().expect("the blocked statement completes");
    // A command that runs after it is a sequence point: everything before has
    // been processed.
    session.ping().wait().expect("ping");

    let counts = scenario.counts();
    assert_eq!(counts.cursors_opened, 1);
    assert_eq!(
        counts.cursors_closed, 1,
        "a cursor nobody can reach any more must be closed, not merely forgotten"
    );
}

#[test]
fn a_dropped_fetch_completion_releases_the_large_objects_it_parked() {
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.on_sql(BLOCKER, Action::Block(BlockSpec::new(Arc::clone(&gate))));
    scenario.on_sql(
        "SELECT doc FROM t",
        Action::query(QuerySource::Fixed(QueryPlan::new(
            vec![ColumnSpec::new(
                "DOC",
                SqlType::CharacterLob { national: false },
            )],
            vec![vec![ScriptValue::Lob {
                kind: LobKind::Character,
                bytes: b"some content".to_vec(),
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

    // Block the worker, then submit a fetch and stop listening for it.
    let blocked = session.execute(Statement::new(BLOCKER));
    assert!(gate.wait_until_blocked(support::short_timeout()));
    drop(session.fetch_batch(result, support::n(10)));
    gate.release();
    blocked.wait().expect("the blocked statement completes");
    session.ping().wait().expect("ping");

    // The locator was parked and then released, all on the worker thread.
    let seen = scenario.thread_ids_seen(connection_id);
    assert_eq!(seen.len(), 1, "{seen:?}");
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("close");
}

#[test]
fn a_commit_closes_the_cursors_it_invalidates() {
    // A commit may invalidate every open cursor (ADR-0002 D2). `db-core` used
    // to drop them, which never gives the driver its chance to release
    // server-side state; it must close them, on the worker.
    let scenario = support::scenario();
    scenario.on_sql("SELECT 1 FROM dual", one_row_query());
    let session = support::open(&scenario);
    session
        .execute(Statement::new("SELECT 1 FROM dual"))
        .wait()
        .expect("execute");
    session
        .execute(Statement::new("SELECT 1 FROM dual"))
        .wait()
        .expect("execute again");
    assert_eq!(scenario.counts().cursors_opened, 2);
    assert_eq!(scenario.counts().cursors_closed, 0);

    session.commit().wait().expect("commit");
    assert_eq!(
        scenario.counts().cursors_closed,
        2,
        "every cursor a commit invalidates must be closed, not dropped"
    );
}

#[test]
fn a_session_closing_closes_the_cursors_it_still_holds() {
    let scenario = support::scenario();
    scenario.on_sql("SELECT 1 FROM dual", one_row_query());
    let session = support::open(&scenario);
    session
        .execute(Statement::new("SELECT 1 FROM dual"))
        .wait()
        .expect("execute");
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("close");
    assert_eq!(scenario.counts().cursors_closed, 1);
    assert_eq!(scenario.counts().connections_closed, 1);
}

#[test]
fn opening_more_results_than_the_limit_is_refused_with_a_clear_error() {
    let scenario = support::scenario();
    scenario.on_sql("SELECT 1 FROM dual", one_row_query());
    let session = support::open_with_limits(
        &scenario,
        SessionLimits::new().with_max_open_results(support::n(2)),
    );

    let first = session
        .execute(Statement::new("SELECT 1 FROM dual"))
        .wait()
        .expect("first result")
        .result
        .expect("cursor");
    session
        .execute(Statement::new("SELECT 1 FROM dual"))
        .wait()
        .expect("second result");

    let error = session
        .execute(Statement::new("SELECT 1 FROM dual"))
        .wait()
        .expect_err("the third result exceeds the session's limit");
    assert_eq!(error.kind(), ErrorKind::Resource);
    assert!(
        error.message().contains("open results"),
        "the error must say what ran out: {error}"
    );
    assert_eq!(
        scenario.counts().cursors_closed,
        1,
        "the refused cursor must be closed, not leaked"
    );

    // Freeing one makes room again; the session is otherwise unharmed.
    session.close_result(first).wait().expect("close_result");
    session
        .execute(Statement::new("SELECT 1 FROM dual"))
        .wait()
        .expect("room again");
}

/// A panicking driver call must be contained, and the torn connection must not
/// be called into again.
#[test]
fn a_panicking_driver_call_is_contained_and_the_torn_connection_is_not_closed() {
    let scenario = support::scenario();
    scenario.on_sql("SELECT 1 FROM dual", one_row_query());
    scenario.on_sql(
        "SELECT boom FROM dual",
        Action::Panic("reldex-driver-mock: scripted panic".to_owned()),
    );

    let session = support::open(&scenario);
    // Open a cursor first, so there is something to release afterwards.
    session
        .execute(Statement::new("SELECT 1 FROM dual"))
        .wait()
        .expect("execute");

    let error = session
        .execute(Statement::new("SELECT boom FROM dual"))
        .wait()
        .expect_err("a panicking driver call must be reported, not unwound");
    assert_eq!(error.kind(), ErrorKind::DriverInternal);
    assert!(error.message().contains("panicked"), "{error}");
    // The panic's own message, not a placeholder. `Action::Panic` panics with a
    // `String`, so this also pins the downcast: reporting "a non-string
    // payload" for every contained panic would throw away the one thing that
    // says what went wrong.
    assert!(
        error
            .message()
            .contains("reldex-driver-mock: scripted panic"),
        "{error}"
    );
    assert!(session.is_lost());

    assert_eq!(
        scenario.counts().connections_closed,
        0,
        "calling `close()` on a connection whose internals just panicked is as likely to \
         panic again as to release anything; it is dropped instead"
    );
    assert_eq!(
        scenario.counts().cursors_closed,
        0,
        "and the same applies to every handle derived from it"
    );

    // Closing reports the loss, releases what is left, and still does not call
    // into the torn connection.
    let close = session.close(None);
    assert!(close.is_err(), "the session was lost: {close:?}");
    assert_eq!(scenario.counts().connections_closed, 0);
}
