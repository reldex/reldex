//! Teardown on the event path: every order the three lifetimes (queue,
//! session, worker) can end in is defined and none of them hangs or loses a
//! reply (`docs/exec-plans/active/phase-1.md` §B2; ADR-0002 K5; ADR-0003 A17 —
//! destroying a consumer must not join a parked worker).

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use reldex_db_core::{
    CloseDisposition, EventCaps, RequestId, SessionEvent, Statement, Waker, event_channel,
};
use reldex_driver_mock::{Action, BlockGate, BlockSpec, ScriptValue};

fn insert(row: &str) -> Action {
    Action::Dml {
        rows_affected: 1,
        insert: Some(("t".to_owned(), vec![ScriptValue::from(row)])),
    }
}

struct Counting(AtomicUsize);

impl Waker for Counting {
    fn wake(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// A session dropped with requests still outstanding answers every one of
/// them, and then announces its end — even though the drop itself detached a
/// worker parked inside a driver call (ADR-0002 K5).
#[test]
fn a_session_dropped_with_requests_outstanding_answers_every_one() {
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
    for request in 2..=6 {
        session.submit_ping(RequestId(request)).expect("accepted");
    }

    // Detaches the parked worker rather than waiting for it; the queue
    // outlives the session, which is the point.
    let started = Instant::now();
    drop(session);
    assert!(
        started.elapsed() < support::short_timeout(),
        "dropping a session must never wait on a statement that cannot be interrupted"
    );

    gate.release();
    let seen = support::drain_to_terminal(&queue);
    assert_eq!(
        support::reply_requests(&seen),
        vec![1, 2, 3, 4, 5, 6],
        "every request accepted before the drop still gets exactly one reply"
    );
    assert_eq!(
        seen.iter().filter(|event| event.is_terminal()).count(),
        1,
        "and the session's end is announced exactly once"
    );
}

/// The worker outlives its consumer: the queue is dropped while the session
/// keeps running. Events go nowhere, nothing wedges, and the session still
/// closes.
#[test]
fn a_worker_outliving_its_queue_keeps_working_and_closes() {
    let scenario = support::scenario();
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert("a"));
    let (session, queue) = support::open_events(&scenario);

    session.submit_ping(RequestId(1)).expect("accepted");
    drop(queue);

    support::with_timeout_guard(support::short_timeout(), move || {
        for request in 2..=20 {
            session
                .submit_execute(
                    RequestId(request),
                    Statement::new("INSERT INTO t VALUES ('a')"),
                )
                .expect("accepted");
        }
        session
            .close(Some(CloseDisposition::Rollback))
            .expect("close must not hang on a queue nobody reads");
    });
}

/// A registered waker is called exactly once when a burst fills an empty
/// queue, and not again until the queue has drained.
#[test]
fn a_registered_waker_is_called_on_the_empty_to_non_empty_edge() {
    let scenario = support::scenario();
    let (session, queue) = support::open_events(&scenario);
    let waker = Arc::new(Counting(AtomicUsize::new(0)));
    queue.set_waker(Some(Arc::clone(&waker) as Arc<dyn Waker>));

    session.submit_ping(RequestId(1)).expect("accepted");
    let _ = support::drain_until(&queue, |seen| !seen.is_empty());
    assert!(
        waker.0.load(Ordering::SeqCst) >= 1,
        "filling an empty queue must wake the consumer"
    );

    queue.set_waker(None);
    let _ = session.close(Some(CloseDisposition::Rollback));
}

/// The consumer's mirror image of ADR-0003 D5 rule 2: clearing the waker does
/// not return while one is in flight, so a consumer can be torn down straight
/// afterwards — including while a worker is racing to call it.
///
/// The iterations exist to hit that race; the assertion is that the run
/// finishes at all, with the waker freed under it every time and no wake left
/// holding a dangling `Arc`.
#[test]
fn clearing_the_waker_does_not_return_while_one_is_in_flight() {
    let scenario = support::scenario();

    support::with_timeout_guard(support::short_timeout() * 6, move || {
        for _ in 0..200 {
            let (sink, queue) = event_channel(EventCaps::new());
            let session = support::open(&scenario);
            session.bind_events(sink).expect("binds once");
            // A waker per iteration, so clearing it is the last reference: a
            // wake still running past `set_waker(None)` would be reading freed
            // state.
            let waker = Arc::new(Counting(AtomicUsize::new(0)));
            queue.set_waker(Some(Arc::clone(&waker) as Arc<dyn Waker>));

            session.submit_ping(RequestId(1)).expect("accepted");
            // After this returns, no thread is inside the waker.
            queue.set_waker(None);
            assert_eq!(
                Arc::strong_count(&waker),
                1,
                "clearing the waker must not return while one is in flight"
            );
            drop(waker);
            drop(queue);
            let _ = session.close(Some(CloseDisposition::Rollback));
        }
    });
}

/// A session closed while its queue still holds undrained events: the events
/// stay readable, because the queue owns them.
#[test]
fn events_already_queued_survive_the_session_that_produced_them() {
    let scenario = support::scenario();
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert("a"));
    let (session, queue) = support::open_events(&scenario);

    for request in 1..=5 {
        session
            .submit_execute(
                RequestId(request),
                Statement::new("INSERT INTO t VALUES ('a')"),
            )
            .expect("accepted");
    }
    session
        .submit_close(RequestId(6), Some(CloseDisposition::Rollback))
        .expect("accepted");
    // Wait for the end without draining anything the session produced first.
    let seen = support::drain_to_terminal(&queue);
    drop(session);

    assert_eq!(support::reply_requests(&seen), vec![1, 2, 3, 4, 5, 6]);
    assert!(
        seen.iter().any(SessionEvent::is_terminal),
        "the announcement is in the queue, not in the session"
    );
}
