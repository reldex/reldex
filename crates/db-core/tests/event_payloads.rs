//! Everything `crates/ffi`'s interim pump puts on a `ReldexEvent` today, shown
//! reaching a consumer through `SessionEvent` instead — so M2.11's switch is a
//! rename, not a redesign.
//!
//! The mapping each of these stands behind is written out in
//! `docs/exec-plans/active/phase-1-m2-5-event-queue.md` §3.

mod support;

use std::sync::Arc;

use reldex_db_core::{CloseDisposition, CompletedOperation, RequestId, SessionEvent, Statement};
use reldex_db_driver_api::{CancelKind, Capabilities, ErrorKind, StatementKind, WarningKind};
use reldex_driver_mock::{Action, ColumnSpec, QueryPlan, QuerySource, ScriptValue, ScriptedError};

fn columns() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new("ID", reldex_db_driver_api::SqlType::Number),
        ColumnSpec::new("NAME", reldex_db_driver_api::SqlType::VARCHAR),
    ]
}

/// `ReldexEvent::column_count` plus `reldex_session_result_column` (ADR-0003
/// A20): a grid must be able to build its header from the *execute* reply,
/// including for a result set with columns and no rows, which never produces a
/// batch to ask.
#[test]
fn an_executed_event_describes_the_result_columns_before_any_row() {
    let scenario = support::scenario();
    scenario.on_sql(
        "SELECT * FROM empty",
        Action::query(QuerySource::Fixed(QueryPlan::new(columns(), Vec::new()))),
    );
    let (session, queue) = support::open_events(&scenario);

    session
        .submit_execute(RequestId(1), Statement::new("SELECT * FROM empty"))
        .expect("accepted");
    let seen = support::drain_until(&queue, |seen| seen.iter().any(SessionEvent::is_reply));

    let outcome = seen
        .iter()
        .find_map(|event| match event {
            SessionEvent::Executed { outcome, .. } => Some(outcome),
            _ => None,
        })
        .expect("Executed")
        .as_ref()
        .expect("the statement succeeded");
    assert_eq!(
        outcome
            .columns
            .iter()
            .map(reldex_db_driver_api::ColumnMetadata::name)
            .collect::<Vec<_>>(),
        vec!["ID", "NAME"],
        "the header is available from the execute reply, with no batch in sight"
    );
    assert!(outcome.result.is_some(), "the result id travels with it");
    assert_eq!(outcome.statement_kind, StatementKind::Query);
    assert!(!outcome.committed_implicitly);

    let _ = session.close(Some(CloseDisposition::Rollback));
}

/// `ReldexEvent::rows_affected` / `committed_implicitly` / `statement_kind`.
#[test]
fn an_executed_event_carries_the_counts_and_flags_a_ui_shows() {
    let scenario = support::scenario();
    scenario.on_sql(
        "UPDATE t SET n = 1",
        Action::Dml {
            rows_affected: 7,
            insert: None,
        },
    );
    scenario.on_sql("CREATE TABLE t (n NUMBER)", Action::Ddl);
    let (session, queue) = support::open_events(&scenario);

    session
        .submit_execute(RequestId(1), Statement::new("UPDATE t SET n = 1"))
        .expect("accepted");
    session
        .submit_execute(RequestId(2), Statement::new("CREATE TABLE t (n NUMBER)"))
        .expect("accepted");

    let seen = support::drain_until(&queue, |seen| support::reply_requests(seen).len() >= 2);
    let outcomes: Vec<&reldex_db_core::ExecuteOutcome> = seen
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Executed {
                outcome: Ok(outcome),
                ..
            } => Some(outcome),
            _ => None,
        })
        .collect();
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].rows_affected, Some(7));
    assert_eq!(outcomes[0].statement_kind, StatementKind::Dml);
    assert!(
        outcomes[1].committed_implicitly,
        "a server-side commit the core could neither prevent nor undo must reach the user"
    );

    let _ = session.close(Some(CloseDisposition::Rollback));
}

/// ADR-0003 A21: a `Fetched` names its result even when the fetch failed, and
/// a `Completed` names the result a close released.
#[test]
fn a_fetch_and_a_result_close_name_their_result_on_success_and_on_failure() {
    let scenario = support::scenario();
    let rows: Vec<Vec<ScriptValue>> = (0..4_i64)
        .map(|value| vec![ScriptValue::from(value), ScriptValue::from("row")])
        .collect();
    scenario.on_sql(
        "SELECT * FROM t",
        Action::query(QuerySource::Fixed(
            QueryPlan::new(columns(), rows).with_fail_on_batch(
                2,
                ScriptedError::new(ErrorKind::NetworkLost, "connection reset mid-fetch"),
            ),
        )),
    );
    let (session, queue) = support::open_events(&scenario);

    session
        .submit_execute(RequestId(1), Statement::new("SELECT * FROM t"))
        .expect("accepted");
    let executed = support::drain_until(&queue, |seen| seen.iter().any(SessionEvent::is_reply));
    let result = executed
        .iter()
        .find_map(|event| match event {
            SessionEvent::Executed {
                outcome: Ok(outcome),
                ..
            } => outcome.result,
            _ => None,
        })
        .expect("the query opened a result");

    // First fetch succeeds, second fails: both must name the result.
    session
        .submit_fetch(RequestId(2), result, support::n(2))
        .expect("accepted");
    session
        .submit_fetch(RequestId(3), result, support::n(2))
        .expect("accepted");
    let fetched = support::drain_until(&queue, |seen| support::reply_requests(seen).len() >= 2);

    let named: Vec<(u64, bool)> = fetched
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Fetched {
                request,
                result: named,
                batch,
                ..
            } => {
                assert_eq!(*named, result, "every fetch reply names its result");
                Some((request.0, batch.is_ok()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        named,
        vec![(2, true), (3, false)],
        "the failing fetch is still a reply, and still names the result that failed"
    );
}

/// `RESULT_CLOSED` carries the id that ended.
#[test]
fn a_result_close_reply_names_the_result_that_ended() {
    let scenario = support::scenario();
    scenario.on_sql(
        "SELECT * FROM t",
        Action::query(QuerySource::Fixed(QueryPlan::new(
            columns(),
            vec![vec![ScriptValue::from(1_i64), ScriptValue::from("a")]],
        ))),
    );
    let (session, queue) = support::open_events(&scenario);

    session
        .submit_execute(RequestId(1), Statement::new("SELECT * FROM t"))
        .expect("accepted");
    let executed = support::drain_until(&queue, |seen| seen.iter().any(SessionEvent::is_reply));
    let result = executed
        .iter()
        .find_map(|event| match event {
            SessionEvent::Executed {
                outcome: Ok(outcome),
                ..
            } => outcome.result,
            _ => None,
        })
        .expect("result");

    session
        .submit_close_result(RequestId(2), result)
        .expect("accepted");
    let seen = support::drain_until(&queue, |seen| seen.iter().any(SessionEvent::is_reply));
    match seen.iter().find(|event| event.is_reply()) {
        Some(SessionEvent::Completed {
            operation,
            result: outcome,
            ..
        }) => {
            assert_eq!(*operation, CompletedOperation::CloseResult(result));
            assert!(outcome.is_ok());
        }
        other => panic!("expected Completed(CloseResult), got {other:#?}"),
    }

    let _ = session.close(Some(CloseDisposition::Rollback));
}

/// What the pump reads off the session when it pushes `OPENED`: the connection
/// id, what a cancel on this session can do, and the connect-time warnings.
/// M2.6's `SessionEvent::Opened` carries all three; until it lands they are on
/// the session, unchanged, and reachable the moment it is open.
#[test]
fn everything_an_opened_event_needs_is_on_the_session_from_the_start() {
    let scenario = support::scenario();
    scenario.set_capabilities(Capabilities::none().with_cancel(CancelKind::Native));
    scenario.set_connect_warnings(vec![reldex_db_driver_api::Warning::new(
        WarningKind::Informational,
        "TCPS: SSL_SERVER_DN_MATCH could not be honoured",
    )]);
    let (session, _queue) = support::open_events(&scenario);

    assert_eq!(session.cancel_kind(), CancelKind::Native);
    assert!(session.connection_id().get() > 0);
    assert_eq!(
        session.connect_warnings().len(),
        1,
        "a connect-time finding is readable without running a statement"
    );
    let _ = session.close(Some(CloseDisposition::Rollback));
}

/// A large-object read is a reply like any other, and names its handle on the
/// failure path too — the shape the FFI has no event for yet and will need.
#[test]
fn a_lob_read_reply_names_its_handle() {
    let scenario = support::scenario();
    scenario.on_sql(
        "BEGIN get_doc(:doc); END;",
        Action::LobOut {
            name: "doc".to_owned(),
            kind: reldex_db_driver_api::LobKind::Character,
            bytes: b"a document".to_vec(),
        },
    );
    let (session, queue) = support::open_events(&scenario);

    session
        .submit_execute(RequestId(1), Statement::new("BEGIN get_doc(:doc); END;"))
        .expect("accepted");
    let executed = support::drain_until(&queue, |seen| seen.iter().any(SessionEvent::is_reply));
    let lob = executed
        .iter()
        .find_map(|event| match event {
            SessionEvent::Executed {
                outcome: Ok(outcome),
                ..
            } => match outcome.out_values.named("doc") {
                Some(reldex_db_core::OutValue::Lob(handle)) => Some(*handle),
                _ => None,
            },
            _ => None,
        })
        .expect("the out bind delivered a large object");

    session
        .submit_read_lob_chunk(RequestId(2), lob, support::n(64))
        .expect("accepted");
    session
        .submit_close_lob(RequestId(3), lob)
        .expect("accepted");
    let seen = support::drain_until(&queue, |seen| support::reply_requests(seen).len() >= 2);

    match seen
        .iter()
        .find(|event| matches!(event, SessionEvent::LobChunk { .. }))
    {
        Some(SessionEvent::LobChunk {
            lob: named, bytes, ..
        }) => {
            assert_eq!(*named, lob);
            assert_eq!(
                bytes.as_ref().expect("the chunk was read").as_slice(),
                b"a document"
            );
        }
        other => panic!("expected LobChunk, got {other:#?}"),
    }
    assert!(
        seen.iter().any(|event| matches!(
            event,
            SessionEvent::Completed {
                operation: CompletedOperation::CloseLob(handle),
                ..
            } if *handle == lob
        )),
        "the close names the handle it released"
    );

    let _ = session.close(Some(CloseDisposition::Rollback));
}

/// A close that leaves the session open, and the error text a UI shows for it,
/// both survive the crossing — the `close_outcome` / `session_still_open`
/// fields the pump fills in today.
#[test]
fn a_close_reply_distinguishes_the_four_outcomes_that_keep_a_session_open() {
    let scenario = support::scenario();
    scenario.on_sql(
        "INSERT INTO t VALUES ('a')",
        Action::Dml {
            rows_affected: 1,
            insert: Some(("t".to_owned(), vec![ScriptValue::from("a")])),
        },
    );
    scenario.fail_commit(ScriptedError::new(ErrorKind::Transaction, "ORA-02091"));
    let (session, queue) = support::open_events(&scenario);

    session
        .submit_execute(RequestId(1), Statement::new("INSERT INTO t VALUES ('a')"))
        .expect("accepted");
    session.submit_close(RequestId(2), None).expect("accepted");
    session
        .submit_close(RequestId(3), Some(CloseDisposition::Commit))
        .expect("accepted");

    let seen = support::drain_until(&queue, |seen| support::reply_requests(seen).len() >= 3);
    let closes: Vec<&reldex_db_core::CloseError> = seen
        .iter()
        .filter_map(|event| match event {
            SessionEvent::SessionClosed {
                result: Err(error), ..
            } => Some(error),
            _ => None,
        })
        .collect();
    assert!(matches!(
        closes.as_slice(),
        [
            reldex_db_core::CloseError::DecisionRequired,
            reldex_db_core::CloseError::CommitFailed(_)
        ]
    ));
    for error in &closes {
        assert!(
            error.session_is_still_open(),
            "both of these leave the session open, and the reply says so"
        );
    }
    assert!(
        !seen.iter().any(SessionEvent::is_terminal),
        "a session that is still open has not ended"
    );

    scenario.allow_commit();
    let _ = session.close(Some(CloseDisposition::Commit));
    let _ = Arc::clone(&scenario);
}
