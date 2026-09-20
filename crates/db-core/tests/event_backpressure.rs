//! What bounds the event queue, and what happens at the bound
//! (`docs/exec-plans/active/phase-1.md` §B2, "Back-pressure").
//!
//! Two halves, in two places:
//!
//! * **Reply events** are bounded here, by
//!   [`reldex_db_core::SessionLimits::max_outstanding_requests`] — the one
//!   synchronous failure the submit API has. That is what this file tests. The
//!   slot a request takes is released when the **consumer drains** its reply,
//!   not when the worker produces it, which is what makes the limit bound the
//!   queue rather than merely the work in flight.
//! * **Unsolicited events** are bounded by
//!   [`reldex_db_core::EventCaps::max_unsolicited_per_session`], with the drop
//!   counted and reported rather than hidden. Its producer
//!   (`SessionEvent::ServerOutput`, M2.7) is crate-private until that task
//!   lands, so the drop-count tests live beside the policy, in
//!   `crates/db-core/src/events.rs`
//!   (`server_output_over_the_cap_is_dropped_and_the_lines_are_reported`,
//!   `transaction_state_is_coalesced_in_place_and_never_dropped`,
//!   `one_session_hitting_the_cap_does_not_affect_another`). What this file
//!   adds is the other half of that promise: a **reply** is never dropped, at
//!   any cap.

mod support;

use std::num::NonZeroUsize;
use std::sync::Arc;

use reldex_db_core::{CloseDisposition, EventCaps, RequestId, SessionLimits, Statement};
use reldex_db_driver_api::ErrorKind;
use reldex_driver_mock::{Action, BlockGate, BlockSpec};

fn limits(outstanding: usize) -> SessionLimits {
    SessionLimits::new().with_max_outstanding_requests(
        NonZeroUsize::new(outstanding).expect("test limit must be non-zero"),
    )
}

#[test]
fn the_default_outstanding_limit_is_the_documented_one() {
    assert_eq!(
        SessionLimits::new().max_outstanding_requests(),
        SessionLimits::DEFAULT_MAX_OUTSTANDING_REQUESTS
    );
    assert_eq!(
        SessionLimits::DEFAULT_MAX_OUTSTANDING_REQUESTS.get(),
        1024,
        "§B2 fixes the default at 1,024"
    );
}

/// The one synchronous failure in the submit API: it accepts nothing, so no
/// event follows, and ordering rule 2 is untouched.
#[test]
fn submitting_past_the_outstanding_limit_is_refused_with_no_event() {
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN blocked; END;",
        Action::Block(BlockSpec::new(Arc::clone(&gate))),
    );
    let (session, queue) = support::open_events_with(&scenario, limits(3), EventCaps::new());

    // Park the worker so nothing is answered while the limit fills up.
    session
        .submit_execute(RequestId(1), Statement::new("BEGIN blocked; END;"))
        .expect("accepted");
    assert!(gate.wait_until_blocked(support::short_timeout()));
    session.submit_ping(RequestId(2)).expect("accepted");
    session.submit_ping(RequestId(3)).expect("accepted");
    assert_eq!(session.outstanding_requests(), 3);

    let refused = session
        .submit_ping(RequestId(4))
        .expect_err("the fourth request is over the limit");
    assert_eq!(refused.kind(), ErrorKind::Resource);
    assert_eq!(
        session.outstanding_requests(),
        3,
        "a refused submit reserves nothing"
    );

    gate.release();
    let seen = support::drain_until(&queue, |seen| support::reply_requests(seen).len() >= 3);
    assert_eq!(
        support::reply_requests(&seen),
        vec![1, 2, 3],
        "the refused request produced no event at all — never a fourth, never a failure one"
    );
    assert_eq!(session.outstanding_requests(), 0);

    // The slot is free again now that the replies have been produced.
    session.submit_ping(RequestId(5)).expect("accepted again");
    let after = support::drain_until(&queue, |seen| !support::reply_requests(seen).is_empty());
    assert_eq!(support::reply_requests(&after), vec![5]);

    let _ = session.close(Some(CloseDisposition::Rollback));
}

/// A reply is never subject to the unsolicited-event cap: even at a cap of
/// one, every accepted request is still answered.
#[test]
fn replies_are_never_dropped_however_small_the_unsolicited_cap_is() {
    let scenario = support::scenario();
    let caps = EventCaps::new().with_max_unsolicited_per_session(
        NonZeroUsize::new(1).expect("a cap of one is the harshest there is"),
    );
    let (session, queue) = support::open_events_with(&scenario, SessionLimits::new(), caps);

    const REQUESTS: u64 = 200;
    for request in 1..=REQUESTS {
        session
            .submit_ping(RequestId(request))
            .expect("accepted under the default outstanding limit");
    }

    let seen = support::drain_until(&queue, |seen| {
        support::reply_requests(seen).len() >= REQUESTS as usize
    });
    assert_eq!(
        support::reply_requests(&seen),
        (1..=REQUESTS).collect::<Vec<_>>(),
        "every reply survives, in order, whatever the unsolicited cap is"
    );
    assert_eq!(
        queue.dropped_unsolicited(),
        0,
        "nothing unsolicited was even produced here, let alone dropped"
    );

    let _ = session.close(Some(CloseDisposition::Rollback));
}

/// The bound is on the **queue**, not on the worker.
///
/// A consumer that never drains is the case that matters: the worker answers
/// everything immediately, so a limit released when the *reply is produced*
/// would be released instantly and bound nothing at all — a submitter in a
/// retry-on-`Resource` loop could then grow the queue without limit. A slot is
/// held until the consumer takes the reply out, so the queue stops at the
/// limit and the submitter is refused instead.
#[test]
fn a_consumer_that_never_drains_stops_the_submitter_rather_than_the_queue_growing() {
    const LIMIT: usize = 8;
    const ATTEMPTS: u64 = 500;

    let scenario = support::scenario();
    let (session, queue) = support::open_events_with(&scenario, limits(LIMIT), EventCaps::new());

    let mut accepted = 0;
    let mut refused = 0;
    for request in 1..=ATTEMPTS {
        match session.submit_ping(RequestId(request)) {
            Ok(()) => accepted += 1,
            Err(error) => {
                assert_eq!(error.kind(), ErrorKind::Resource);
                refused += 1;
            }
        }
        assert!(
            queue.len() <= LIMIT,
            "a ping produces exactly one event, so an undrained queue can never hold more \
             than the limit; it holds {} after {request} attempts",
            queue.len()
        );
    }
    assert_eq!(accepted, LIMIT, "only the limit was ever accepted");
    assert_eq!(refused as u64, ATTEMPTS - LIMIT as u64);
    assert_eq!(queue.dropped_unsolicited(), 0, "and nothing was dropped");

    // Every accepted request is still answered exactly once, in order.
    support::wait_for("the worker answers what it accepted", || {
        queue.len() == LIMIT
    });
    assert_eq!(session.outstanding_requests(), LIMIT);

    // Draining frees slots, one for one.
    let drained = support::drain_n(&queue, 3);
    assert_eq!(support::reply_requests(&drained), vec![1, 2, 3]);
    assert_eq!(session.outstanding_requests(), LIMIT - 3);
    for request in 1001..=1003 {
        session
            .submit_ping(RequestId(request))
            .expect("a drained reply frees the slot it held");
    }
    assert_eq!(
        session
            .submit_ping(RequestId(1004))
            .expect_err("and only those")
            .kind(),
        ErrorKind::Resource
    );

    let rest = support::drain_until(&queue, |seen| support::reply_requests(seen).len() >= LIMIT);
    assert_eq!(
        support::reply_requests(&rest),
        vec![4, 5, 6, 7, 8, 1001, 1002, 1003]
    );
    let _ = session.close(Some(CloseDisposition::Rollback));
}

/// A close is never refused for lack of a slot.
///
/// It reserves against one *more* than the limit, because refusing the one
/// request that shrinks a session's footprint — when the complaint is that the
/// session has too much outstanding — is backwards. The exemption is exactly
/// one: a second close while the first is still undrained is refused like
/// anything else, so the published bound grows by one event per session and no
/// further.
#[test]
fn a_close_is_accepted_at_the_cap_and_the_exemption_is_exactly_one() {
    const LIMIT: usize = 4;

    let scenario = support::scenario();
    let (session, queue) = support::open_events_with(&scenario, limits(LIMIT), EventCaps::new());

    for request in 1..=LIMIT as u64 {
        session.submit_ping(RequestId(request)).expect("accepted");
    }
    support::wait_for("the session reaches its limit", || {
        session.outstanding_requests() == LIMIT
    });
    assert_eq!(
        session
            .submit_ping(RequestId(50))
            .expect_err("an ordinary request is refused at the limit")
            .kind(),
        ErrorKind::Resource
    );

    session
        .submit_close(RequestId(100), Some(CloseDisposition::Rollback))
        .expect("a close is never refused for lack of a slot");
    assert_eq!(
        session
            .submit_close(RequestId(101), Some(CloseDisposition::Rollback))
            .expect_err("but the exemption is one, not unlimited")
            .kind(),
        ErrorKind::Resource
    );

    let seen = support::drain_to_terminal(&queue);
    assert_eq!(
        support::reply_requests(&seen),
        vec![1, 2, 3, 4, 100],
        "every accepted request answered once; the refused ones produced nothing"
    );
    assert!(
        seen.len() <= 2 * LIMIT + EventCaps::new().max_unsolicited_per_session().get() + 3,
        "the published bound holds with the close exemption in it: {} events",
        seen.len()
    );
    assert_eq!(session.outstanding_requests(), 0);
    assert_eq!(queue.len(), 0);
}

/// Dropping the queue ends the stream, so nothing is waiting to be drained and
/// every slot it held comes back. A session whose consumer walked away is not
/// left permanently unable to submit — it may still have a close to run.
#[test]
fn dropping_the_queue_releases_every_slot_it_was_holding() {
    const LIMIT: usize = 4;

    let scenario = support::scenario();
    let (session, queue) = support::open_events_with(&scenario, limits(LIMIT), EventCaps::new());

    for request in 1..=LIMIT as u64 {
        session.submit_ping(RequestId(request)).expect("accepted");
    }
    support::wait_for("the replies reach the queue", || queue.len() == LIMIT);
    assert_eq!(session.outstanding_requests(), LIMIT);

    drop(queue);
    assert_eq!(
        session.outstanding_requests(),
        0,
        "a queue nobody can read holds nothing, including slots"
    );

    // It keeps coming back to zero: a discarded reply releases its slot at
    // once, so nothing accumulates and a submitter is only ever held up by
    // what the worker has genuinely not reached yet.
    for request in 100..200 {
        // The limit still bounds *in-flight* work when there is no consumer —
        // which is the point: a sink nobody reads must not accept unbounded
        // work either. Waiting for the worker is all that is needed.
        support::wait_for("a slot frees up", || session.outstanding_requests() < LIMIT);
        session.submit_ping(RequestId(request)).expect("accepted");
    }
    support::wait_for("every discarded reply releases its slot", || {
        session.outstanding_requests() == 0
    });

    support::with_timeout_guard(support::short_timeout(), move || {
        session
            .close(Some(CloseDisposition::Rollback))
            .expect("a session outliving its consumer still closes");
    });
}

/// A session that ends with replies still in the queue keeps its slots until
/// they are drained — the events are the consumer's, and outlive the session
/// that produced them.
#[test]
fn a_session_that_ends_with_undrained_replies_frees_its_slots_when_they_are_drained() {
    const LIMIT: usize = 6;

    let scenario = support::scenario();
    let (session, queue) = support::open_events_with(&scenario, limits(LIMIT), EventCaps::new());

    for request in 1..=3 {
        session.submit_ping(RequestId(request)).expect("accepted");
    }
    session
        .submit_close(RequestId(4), Some(CloseDisposition::Rollback))
        .expect("accepted");
    support::wait_for("the session ends with its replies undrained", || {
        session.outstanding_requests() == 4
    });

    let seen = support::drain_to_terminal(&queue);
    assert_eq!(support::reply_requests(&seen), vec![1, 2, 3, 4]);
    assert_eq!(
        session.outstanding_requests(),
        0,
        "draining an ended session's replies still releases its slots"
    );
    assert_eq!(
        queue.len(),
        0,
        "and nothing of it is left behind in the queue"
    );
}

/// A session with no queue bound refuses every event-path submit, rather than
/// accepting a request whose reply would have nowhere to go.
#[test]
fn submitting_without_a_bound_queue_is_refused() {
    let scenario = support::scenario();
    let session = support::open(&scenario);
    let error = session
        .submit_ping(RequestId(1))
        .expect_err("no queue is bound");
    assert_eq!(error.kind(), ErrorKind::DriverInternal);
    assert_eq!(session.outstanding_requests(), 0);
}

/// Binding twice is refused: one session's events belong to one consumer, or
/// its ordering guarantee means nothing.
#[test]
fn binding_a_second_queue_is_refused() {
    let scenario = support::scenario();
    let (session, _queue) = support::open_events(&scenario);
    let (second, _spare) = reldex_db_core::event_channel(EventCaps::new());
    let error = session
        .bind_events(second)
        .expect_err("a session routes to one queue");
    assert_eq!(error.kind(), ErrorKind::DriverInternal);
}
