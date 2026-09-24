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
        // answered; the events are discarded on arrival rather than piling up,
        // and each one releases the slot its request held.
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

/// Closing is idempotent, and it stays idempotent when closes race.
///
/// Only one of them finds a worker to run; the others reach a session that has
/// already ended and are answered by their reply channel's `Drop`. That is the
/// same question with the same true answer — the session is closed — so every
/// one of them must report success. Reporting a failure because a close lost a
/// race would have a UI tell the user their session could not be closed when
/// it demonstrably was.
#[test]
fn concurrent_closes_all_report_success_on_a_cleanly_closed_session() {
    const CLOSERS: u64 = 16;
    const ROUNDS: usize = 40;

    let scenario = support::scenario();
    support::with_timeout_guard(support::short_timeout() * 6, move || {
        for _ in 0..ROUNDS {
            let (session, queue) = support::open_events(&scenario);
            let session = Arc::new(session);
            let threads: Vec<_> = (1..=CLOSERS)
                .map(|request| {
                    let session = Arc::clone(&session);
                    std::thread::spawn(move || {
                        session.submit_close(RequestId(request), Some(CloseDisposition::Rollback))
                    })
                })
                .collect();
            for thread in threads {
                thread
                    .join()
                    .expect("a closer must not panic")
                    .expect("accepted");
            }

            let seen = support::drain_until(&queue, |seen| {
                support::reply_requests(seen).len() >= CLOSERS as usize
            });
            let mut requests = support::reply_requests(&seen);
            requests.sort_unstable();
            assert_eq!(
                requests,
                (1..=CLOSERS).collect::<Vec<_>>(),
                "every close is answered exactly once"
            );
            for event in &seen {
                match event {
                    SessionEvent::SessionClosed { result: Ok(()), .. } => {}
                    SessionEvent::SessionClosed {
                        result: Err(error),
                        request,
                        ..
                    } => panic!(
                        "close {request} lost a race and reported failure on a session that \
                         closed cleanly: {error:?}"
                    ),
                    SessionEvent::Terminal { .. } => {}
                    other => panic!("only closes were submitted, got {other:?}"),
                }
            }
        }
    });
}

/// The other half of the same rule: a close that finds a genuinely **lost**
/// session still reports the failure. Idempotency is not a licence to claim a
/// clean close that never happened (`SPEC.md` §10).
#[test]
fn a_close_after_a_lost_session_still_reports_the_loss() {
    let scenario = support::scenario();
    scenario.on_sql(
        "SELECT 1 FROM dual",
        Action::Fail(ScriptedError::new(
            ErrorKind::NetworkLost,
            "connection reset",
        )),
    );
    let (session, queue) = support::open_events(&scenario);

    session
        .submit_execute(RequestId(1), Statement::new("SELECT 1 FROM dual"))
        .expect("accepted");
    let _ = support::drain_to_terminal(&queue);

    for request in 2..=5 {
        session
            .submit_close(RequestId(request), Some(CloseDisposition::Commit))
            .expect("accepted");
    }
    let seen = support::drain_until(&queue, |seen| support::reply_requests(seen).len() >= 4);
    for event in &seen {
        match event {
            SessionEvent::SessionClosed { result: Err(_), .. } => {}
            other => panic!("a lost session never closes cleanly, got {other:?}"),
        }
    }
}

/// Binding a queue to a session that has already announced its end is refused.
///
/// `Terminal` is emitted once, at the transition. A sink bound afterwards would
/// receive a stream that never terminates — every submit failing one by one
/// with nothing to say the session is gone — so the bind fails instead, and the
/// caller is told why.
#[test]
fn binding_a_queue_after_the_session_ended_is_refused() {
    let scenario = support::scenario();
    // A session that ran on the completion path and was closed: its end was
    // announced to nobody, and there is no second announcement to give a
    // consumer that turns up now.
    let session = support::open(&scenario);
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("closes cleanly");

    let (sink, queue) = reldex_db_core::event_channel(reldex_db_core::EventCaps::new());
    let error = session
        .bind_events(sink)
        .expect_err("a session that has ended cannot start a new stream");
    assert_eq!(error.kind(), ErrorKind::Resource);
    assert!(queue.is_empty(), "and nothing was routed to it");

    // The refusal is what keeps a caller from waiting forever: submitting is
    // refused too, rather than accepting work that could never be announced.
    assert_eq!(
        session
            .submit_ping(RequestId(1))
            .expect_err("no queue is bound")
            .kind(),
        ErrorKind::DriverInternal
    );
}

/// Deterministic form of the same rule, and the interleaving that broke it.
///
/// The looping test above only *sometimes* hits the bug, because it depends on
/// the worker reaching `mark_ended` between two submits on the test thread.
/// Waiting for the first close's reply forces that ordering every time: by the
/// time the second close is submitted, the session is recorded as ended, and
/// the short-circuit in `submit_close` decides its answer from that record.
#[test]
fn a_second_close_after_a_lost_session_reports_the_loss_deterministically() {
    let scenario = support::scenario();
    scenario.on_sql(
        "SELECT 1 FROM dual",
        Action::Fail(ScriptedError::new(
            ErrorKind::NetworkLost,
            "connection reset",
        )),
    );
    let (session, queue) = support::open_events(&scenario);

    session
        .submit_execute(RequestId(1), Statement::new("SELECT 1 FROM dual"))
        .expect("accepted");
    let _ = support::drain_to_terminal(&queue);

    // The first close runs on the worker and ends the session.
    session
        .submit_close(RequestId(2), Some(CloseDisposition::Commit))
        .expect("accepted");
    let first = support::drain_until(&queue, |seen| !support::reply_requests(seen).is_empty());
    match first.last().expect("a reply") {
        SessionEvent::SessionClosed { result: Err(_), .. } => {}
        other => panic!("the first close on a lost session must fail, got {other:?}"),
    }

    // Now the session is *recorded* as ended. A second close must still report
    // the loss: idempotency answers "this session is already over", not "your
    // commit happened".
    session
        .submit_close(RequestId(3), Some(CloseDisposition::Commit))
        .expect("accepted");
    let second = support::drain_until(&queue, |seen| !support::reply_requests(seen).is_empty());
    match second.last().expect("a reply") {
        SessionEvent::SessionClosed {
            result: Err(_),
            request,
            ..
        } => assert_eq!(*request, RequestId(3)),
        other => panic!("a lost session never closes cleanly, got {other:?}"),
    }
}
