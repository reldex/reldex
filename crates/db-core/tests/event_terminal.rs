//! `SessionEvent::Terminal` arrives exactly once per session, and every
//! accepted request is answered exactly once, on each of the paths that can
//! end a session: a close, a loss, a contained driver panic, a close racing
//! replies still in flight, and a consumer that threw its queue away
//! (`docs/exec-plans/active/phase-1.md` §B2 rules 2 and 3; ADR-0002 K6).

mod support;

use std::sync::Arc;

use reldex_db_core::{CloseDisposition, RequestId, SessionEvent, SessionLifecycle, Statement};
use reldex_db_driver_api::{CancelKind, Capabilities, ErrorKind, SessionState};
use reldex_driver_mock::{Action, BlockGate, BlockSpec, ScriptValue, ScriptedError};

fn insert(row: &str) -> Action {
    Action::Dml {
        rows_affected: 1,
        insert: Some(("t".to_owned(), vec![ScriptValue::from(row)])),
    }
}

fn terminals(seen: &[SessionEvent]) -> Vec<(SessionLifecycle, bool)> {
    seen.iter()
        .filter_map(|event| match event {
            SessionEvent::Terminal {
                lifecycle, cause, ..
            } => Some((*lifecycle, cause.is_some())),
            _ => None,
        })
        .collect()
}

#[test]
fn a_deliberate_close_yields_one_session_closed_and_one_terminal() {
    let scenario = support::scenario();
    let (session, queue) = support::open_events(&scenario);

    session
        .submit_close(RequestId(1), Some(CloseDisposition::Rollback))
        .expect("accepted");

    let seen = support::drain_to_terminal(&queue);
    assert_eq!(support::reply_requests(&seen), vec![1]);
    assert!(matches!(
        seen.first(),
        Some(SessionEvent::SessionClosed { result: Ok(()), .. })
    ));
    assert_eq!(
        terminals(&seen),
        vec![(SessionLifecycle::Closed, false)],
        "a deliberate close ends with one Terminal and nothing to explain"
    );
}

#[test]
fn a_close_that_leaves_the_session_open_emits_no_terminal() {
    let scenario = support::scenario();
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert("a"));
    let (session, queue) = support::open_events(&scenario);

    session
        .submit_execute(RequestId(1), Statement::new("INSERT INTO t VALUES ('a')"))
        .expect("accepted");
    // No disposition over a possibly-open transaction: refused, session intact.
    session.submit_close(RequestId(2), None).expect("accepted");

    let seen = support::drain_until(&queue, |seen| support::reply_requests(seen).len() >= 2);
    assert!(
        terminals(&seen).is_empty(),
        "a refused close is not the end of a session"
    );
    assert!(!session.session_state().is_terminal());

    // And the session really is still usable.
    session
        .submit_close(RequestId(3), Some(CloseDisposition::Rollback))
        .expect("accepted");
    let rest = support::drain_to_terminal(&queue);
    assert_eq!(terminals(&rest), vec![(SessionLifecycle::Closed, false)]);
}

#[test]
fn a_lost_session_yields_exactly_one_terminal_carrying_the_cause() {
    let scenario = support::scenario();
    scenario.on_sql(
        "SELECT 1 FROM dual",
        Action::Fail(
            ScriptedError::new(ErrorKind::NetworkLost, "connection reset")
                .with_native(3113, "ORA-03113: end-of-file on communication channel"),
        ),
    );
    let (session, queue) = support::open_events(&scenario);

    session
        .submit_execute(RequestId(1), Statement::new("SELECT 1 FROM dual"))
        .expect("accepted");

    let seen = support::drain_to_terminal(&queue);
    assert_eq!(support::reply_requests(&seen), vec![1]);
    let cause = seen
        .iter()
        .find_map(|event| match event {
            SessionEvent::Terminal { cause, .. } => cause.as_ref(),
            _ => None,
        })
        .expect("a lost session says why");
    assert_eq!(cause.kind(), ErrorKind::NetworkLost);
    assert_eq!(
        cause.native().map(reldex_db_driver_api::NativeError::code),
        Some(3113),
        "the native code survives into the terminal event"
    );
    assert_eq!(terminals(&seen), vec![(SessionLifecycle::Lost, true)]);

    // Closing a session that is already lost reports the loss, and does not
    // produce a second Terminal.
    session
        .submit_close(RequestId(2), Some(CloseDisposition::Commit))
        .expect("accepted");
    let after = support::drain_until(&queue, |seen| !support::reply_requests(seen).is_empty());
    assert_eq!(support::reply_requests(&after), vec![2]);
    assert!(
        terminals(&after).is_empty(),
        "Terminal is emitted once per session, at the transition, not per observer"
    );
}

/// ADR-0002 K6: a contained driver panic still answers the request and still
/// announces the session's end. Exactly one of each.
#[test]
fn a_contained_driver_panic_yields_one_reply_and_one_terminal() {
    let scenario = support::scenario();
    scenario.on_sql(
        "BEGIN boom; END;",
        Action::Panic("the driver fell over".to_owned()),
    );
    let (session, queue) = support::open_events(&scenario);

    session
        .submit_execute(RequestId(1), Statement::new("BEGIN boom; END;"))
        .expect("accepted");

    let seen = support::drain_to_terminal(&queue);
    assert_eq!(support::reply_requests(&seen), vec![1]);
    match seen.iter().find(|event| event.is_reply()) {
        Some(SessionEvent::Executed {
            outcome: Err(error),
            ..
        }) => {
            assert_eq!(error.kind(), ErrorKind::DriverInternal);
            assert_eq!(error.session_state(), SessionState::Lost);
        }
        other => panic!("expected a contained failure, got {other:#?}"),
    }
    assert_eq!(terminals(&seen), vec![(SessionLifecycle::Lost, true)]);
}

/// A close submitted while a statement is still parked in the driver: the
/// parked statement's reply, then the requests queued behind it, then the
/// close, then the single `Terminal` — each request answered exactly once.
#[test]
fn a_close_racing_in_flight_replies_answers_each_one_before_terminal() {
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN blocked; END;",
        Action::Block(BlockSpec::new(Arc::clone(&gate))),
    );
    let (session, queue) = support::open_events(&scenario);

    session
        .submit_execute(RequestId(1), Statement::new("BEGIN blocked; END;"))
        .expect("accepted");
    assert!(
        gate.wait_until_blocked(support::short_timeout()),
        "the worker should have reached the gate"
    );
    session.submit_ping(RequestId(2)).expect("accepted");
    session
        .submit_close(RequestId(3), Some(CloseDisposition::Rollback))
        .expect("accepted");
    // Queued behind the close: accepted before the session ended, so it must
    // still be answered, and before Terminal.
    session.submit_ping(RequestId(4)).expect("accepted");

    gate.release();

    let seen = support::drain_to_terminal(&queue);
    let terminal_at = seen
        .iter()
        .position(SessionEvent::is_terminal)
        .expect("Terminal");
    assert_eq!(
        support::reply_requests(&seen[..terminal_at]),
        vec![1, 2, 3, 4],
        "everything accepted before the session ended is answered before Terminal"
    );
    assert_eq!(terminals(&seen), vec![(SessionLifecycle::Closed, false)]);
}

/// A request that cannot be delivered at all — the worker has already gone —
/// is still answered exactly once, by the reply channel's own `Drop`.
#[test]
fn a_request_submitted_after_the_session_ended_still_gets_its_one_reply() {
    let scenario = support::scenario();
    let (session, queue) = support::open_events(&scenario);

    session
        .submit_close(RequestId(1), Some(CloseDisposition::Rollback))
        .expect("accepted");
    let _ = support::drain_to_terminal(&queue);

    for request in 2..=4 {
        session.submit_ping(RequestId(request)).expect("accepted");
    }
    let seen = support::drain_until(&queue, |seen| support::reply_requests(seen).len() >= 3);
    assert_eq!(support::reply_requests(&seen), vec![2, 3, 4]);
    for event in &seen {
        match event {
            SessionEvent::Completed { result: Err(_), .. } => {}
            other => panic!("a request after the end must fail, got {other:?}"),
        }
    }
    assert_eq!(session.outstanding_requests(), 0);
}

/// The consumer threw its queue away while workers were still running. Nothing
/// wedges, and the session can still be driven and closed.
#[test]
fn a_dropped_queue_does_not_wedge_the_worker() {
    let scenario = support::scenario();
    scenario.set_capabilities(Capabilities::none().with_cancel(CancelKind::Native));
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert("a"));
    let (session, queue) = support::open_events(&scenario);

    session.submit_ping(RequestId(1)).expect("accepted");
    drop(queue);

    support::with_timeout_guard(support::short_timeout(), move || {
        // Submitting into a queue nobody reads is still accepted and still
        // answered; the events simply pile up until the last sink goes.
        for request in 2..=50 {
            session.submit_ping(RequestId(request)).expect("accepted");
        }
        // And the completion path on the same session is untouched.
        session
            .execute(Statement::new("INSERT INTO t VALUES ('a')"))
            .wait()
            .expect("the completion path still works");
        session
            .close(Some(CloseDisposition::Rollback))
            .expect("closing a session whose consumer went away must not hang");
    });
}
