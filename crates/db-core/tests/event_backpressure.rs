//! What bounds the event queue, and what happens at the bound
//! (`docs/exec-plans/active/phase-1.md` §B2, "Back-pressure").
//!
//! Two halves, in two places:
//!
//! * **Reply events** are bounded here, by
//!   [`reldex_db_core::SessionLimits::max_outstanding_requests`] — the one
//!   synchronous failure the submit API has. That is what this file tests.
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
