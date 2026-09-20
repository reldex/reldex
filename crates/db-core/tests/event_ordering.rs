//! The five ordering guarantees an [`reldex_db_core::EventQueue`] consumer may
//! rely on (`docs/exec-plans/active/phase-1.md` §B2; ADR-0002's event-path
//! amendment). One test per rule, named after it.
//!
//! None of these asserts a timing *upper* bound: every wait is a hang guard,
//! and every outcome is forced by construction (a gate the test opens, a queue
//! the test fills) rather than by how fast a thread happens to run.

mod support;

use std::sync::Arc;

use reldex_db_core::{CloseDisposition, RequestId, SessionEvent, SessionLifecycle, Statement};
use reldex_db_driver_api::{CancelKind, Capabilities, ErrorKind, SessionState};
use reldex_driver_mock::{Action, BlockGate, BlockSpec, ColumnSpec, QuerySource, ScriptValue};

fn insert(row: &str) -> Action {
    Action::Dml {
        rows_affected: 1,
        insert: Some(("t".to_owned(), vec![ScriptValue::from(row)])),
    }
}

fn select() -> Action {
    Action::query(QuerySource::Table {
        table: "t".to_owned(),
        columns: vec![ColumnSpec::new(
            "NAME",
            reldex_db_driver_api::SqlType::VARCHAR,
        )],
    })
}

/// Rule 1: per session, events are delivered in the order the worker produced
/// them.
///
/// Four requests are submitted back to back without waiting for any of them,
/// so the only thing that can order the replies is the worker's own FIFO.
#[test]
fn rule_1_per_session_events_are_delivered_in_production_order() {
    let scenario = support::scenario();
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert("a"));
    let (session, queue) = support::open_events(&scenario);

    session.submit_ping(RequestId(1)).expect("ping accepted");
    session
        .submit_execute(RequestId(2), Statement::new("INSERT INTO t VALUES ('a')"))
        .expect("execute accepted");
    session
        .submit_commit(RequestId(3))
        .expect("commit accepted");
    session.submit_ping(RequestId(4)).expect("ping accepted");

    let seen = support::drain_until(&queue, |seen| support::reply_requests(seen).len() >= 4);
    assert_eq!(
        support::reply_requests(&seen),
        vec![1, 2, 3, 4],
        "one session's replies must arrive in the order its worker produced them"
    );

    // The same holds for the events that are not replies: `Executing` for
    // request 2 sits between request 1's reply and request 2's.
    let positions: Vec<usize> = seen
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            matches!(event, SessionEvent::Executing { request, .. } if *request == RequestId(2))
        })
        .map(|(index, _)| index)
        .collect();
    assert_eq!(positions.len(), 1, "exactly one Executing for one execute");
}

/// Rule 2: every accepted request produces exactly one reply event — never
/// zero, never two — including the ones that fail.
#[test]
fn rule_2_every_accepted_request_produces_exactly_one_reply() {
    let scenario = support::scenario();
    scenario.on_sql("SELECT * FROM t", select());
    let (session, queue) = support::open_events(&scenario);

    // A mix of shapes, including two that cannot succeed: a fetch of a result
    // that was never opened, and a rollback to a savepoint that does not
    // exist. A failure is still a reply.
    session
        .submit_execute(RequestId(1), Statement::new("SELECT * FROM t"))
        .expect("accepted");
    session.submit_ping(RequestId(2)).expect("accepted");
    session
        .submit_rollback_to_savepoint(
            RequestId(3),
            reldex_db_core::SavepointName::new("nope").expect("valid savepoint name"),
        )
        .expect("accepted");
    session.submit_rollback(RequestId(4)).expect("accepted");

    let seen = support::drain_until(&queue, |seen| support::reply_requests(seen).len() >= 4);
    let mut replies = support::reply_requests(&seen);
    replies.sort_unstable();
    assert_eq!(replies, vec![1, 2, 3, 4], "exactly one reply per request");
    assert_eq!(
        session.outstanding_requests(),
        0,
        "every answered request releases the slot it reserved"
    );
}

/// Rule 3: `Terminal` is delivered exactly once, after the reply of every
/// request that was queued when the transition was observed.
///
/// A statement is parked on a gate; three more requests queue behind it; the
/// parked statement is then cancelled in a way that destroys the session. All
/// four replies must precede the single `Terminal`.
#[test]
fn rule_3_terminal_follows_every_reply_accepted_before_the_transition() {
    let scenario = support::scenario();
    scenario.set_capabilities(Capabilities::none().with_cancel(CancelKind::Native));
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN blocked; END;",
        Action::Block(
            BlockSpec::new(Arc::clone(&gate)).with_cancelled_session_state(SessionState::Lost),
        ),
    );
    let (session, queue) = support::open_events(&scenario);

    session
        .submit_execute(RequestId(1), Statement::new("BEGIN blocked; END;"))
        .expect("accepted");
    assert!(
        gate.wait_until_blocked(support::short_timeout()),
        "the worker should have reached the gate"
    );
    for request in 2..=4 {
        session.submit_ping(RequestId(request)).expect("accepted");
    }

    // Destroys the session under the parked statement.
    let _ = session.cancel();

    let seen = support::drain_to_terminal(&queue);
    let terminals = seen.iter().filter(|event| event.is_terminal()).count();
    assert_eq!(terminals, 1, "Terminal is emitted exactly once");

    let terminal_at = seen
        .iter()
        .position(SessionEvent::is_terminal)
        .expect("Terminal");
    let replies_before: Vec<u64> = support::reply_requests(&seen[..terminal_at]);
    assert_eq!(
        replies_before,
        vec![1, 2, 3, 4],
        "every request queued before the transition is answered before Terminal"
    );

    match &seen[terminal_at] {
        SessionEvent::Terminal {
            lifecycle, cause, ..
        } => {
            assert_eq!(*lifecycle, SessionLifecycle::Lost);
            let cause = cause.as_ref().expect("a lost session says why");
            assert_eq!(cause.kind(), ErrorKind::Cancelled);
        }
        other => panic!("expected Terminal, got {other:?}"),
    }

    // Rule 3's second half: a request submitted *after* the transition still
    // gets its one reply, and it is allowed to follow Terminal.
    session.submit_ping(RequestId(9)).expect("accepted");
    let after = support::drain_until(&queue, |seen| !seen.is_empty());
    assert_eq!(support::reply_requests(&after), vec![9]);
}

/// Rule 4: `Executing` precedes the matching `Executed`, and follows any
/// earlier request's reply on that session. It carries the deadline actually
/// armed on the statement.
#[test]
fn rule_4_executing_precedes_its_executed_and_follows_the_previous_reply() {
    let scenario = support::scenario();
    scenario.on_sql("SELECT * FROM t", select());
    let (session, queue) = support::open_events(&scenario);

    let deadline = std::time::Duration::from_secs(30);
    session.submit_ping(RequestId(1)).expect("accepted");
    session
        .submit_execute(
            RequestId(2),
            Statement::new("SELECT * FROM t").with_deadline(deadline),
        )
        .expect("accepted");

    let seen = support::drain_until(&queue, |seen| support::reply_requests(seen).len() >= 2);
    let kinds: Vec<&str> = seen
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Completed { .. } => Some("completed"),
            SessionEvent::Executing { .. } => Some("executing"),
            SessionEvent::Executed { .. } => Some("executed"),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["completed", "executing", "executed"],
        "Executing sits between the previous reply and its own Executed"
    );

    let armed = seen.iter().find_map(|event| match event {
        SessionEvent::Executing { deadline, .. } => Some(*deadline),
        _ => None,
    });
    assert_eq!(
        armed,
        Some(Some(deadline)),
        "the UI is told the limit that was really armed, not the one it hoped for"
    );
}

/// Rule 5: no ordering is promised across sessions.
///
/// Two sessions share one queue. The first submits a statement that parks on a
/// gate; the second then submits a ping, which is answered while the first is
/// still blocked. The later request's reply therefore arrives first — which is
/// exactly the freedom this rule reserves — while each session's own
/// subsequence stays in order.
#[test]
fn rule_5_no_ordering_is_promised_across_sessions() {
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN blocked; END;",
        Action::Block(BlockSpec::new(Arc::clone(&gate))),
    );

    // Both sessions feed the *same* queue: that is the shape the rule is about.
    let (sessions, queue) = support::open_events_fan_in(&scenario, 2);
    let first = &sessions[0];
    let second = &sessions[1];

    first
        .submit_execute(RequestId(1), Statement::new("BEGIN blocked; END;"))
        .expect("accepted");
    assert!(
        gate.wait_until_blocked(support::short_timeout()),
        "the first session's worker should have reached the gate"
    );
    second.submit_ping(RequestId(2)).expect("accepted");

    let before_release = support::drain_until(&queue, |seen| {
        seen.iter()
            .any(|event| event.request() == Some(RequestId(2)) && event.is_reply())
    });
    assert_eq!(
        support::reply_requests(&before_release),
        vec![2],
        "the second session was answered while the first was still blocked, which is the \
         cross-session interleaving rule 5 reserves"
    );

    gate.release();
    let after = support::drain_until(&queue, |seen| {
        seen.iter()
            .any(|event| event.request() == Some(RequestId(1)) && event.is_reply())
    });
    assert_eq!(support::reply_requests(&after), vec![1]);

    // Per session, order still holds: the first session's own subsequence was
    // Executing then Executed, in that order.
    let first_events: Vec<&SessionEvent> = before_release
        .iter()
        .chain(after.iter())
        .filter(|event| event.session() == first.id())
        .collect();
    assert!(
        matches!(
            first_events.as_slice(),
            [
                SessionEvent::Executing { .. },
                SessionEvent::Executed { .. }
            ]
        ),
        "per-session order survives the interleaving: {first_events:#?}"
    );

    let _ = first.close(Some(CloseDisposition::Rollback));
    let _ = second.close(Some(CloseDisposition::Rollback));
}
