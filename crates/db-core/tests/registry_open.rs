//! The non-blocking open (`docs/exec-plans/active/phase-1.md` §B3,
//! ADR-0002 amendment R1–R3).
//!
//! What `SessionRegistry::open` promises: it returns a `SessionId` without
//! waiting for the connect, the connect runs on the session's own worker
//! thread, and its outcome arrives as exactly one `Opened` or `OpenFailed`
//! carrying the caller's `RequestId`, followed — for every session the
//! registry ever announced — by exactly one `Terminal`.
//!
//! Every wait here is on a *condition*, under the generous hang guard. None of
//! it asserts timing.

mod support;

use std::num::NonZeroUsize;
use std::sync::Arc;

use reldex_db_core::{
    Abandoned, CloseDisposition, EventCaps, RegisteredSession, RequestId, SessionEvent, SessionId,
    SessionLifecycle, SessionLimits, Statement,
};
use reldex_db_driver_api::{CancelKind, ErrorKind, NativeError, Warning, WarningKind};
use reldex_driver_mock::{BlockGate, ScriptedError};

/// The one event that answers an open, and what it said.
fn open_reply(seen: &[SessionEvent], session: SessionId) -> &SessionEvent {
    let replies: Vec<&SessionEvent> = seen
        .iter()
        .filter(|event| {
            event.session() == session
                && matches!(
                    event,
                    SessionEvent::Opened { .. } | SessionEvent::OpenFailed { .. }
                )
        })
        .collect();
    assert_eq!(
        replies.len(),
        1,
        "an open produces exactly one reply, never zero and never two: {seen:#?}"
    );
    replies[0]
}

fn terminals(seen: &[SessionEvent], session: SessionId) -> Vec<&SessionEvent> {
    seen.iter()
        .filter(|event| event.is_terminal() && event.session() == session)
        .collect()
}

#[test]
fn an_open_that_succeeds_reports_the_connection_and_hands_out_a_session() {
    let scenario = support::scenario();
    scenario.set_connect_warnings(vec![Warning::new(
        WarningKind::Informational,
        "a parameter that did nothing",
    )]);
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(7));
    // `Opening` or already `Open` — the connect races this thread, and which
    // one wins is not a promise. That it is *registered* is.
    assert!(
        registry.state(id).is_some(),
        "a session is registered the moment `open` returns"
    );

    let seen = support::drain_n(&queue, 1);
    match open_reply(&seen, id) {
        SessionEvent::Opened {
            request,
            connection,
            cancel_kind,
            warnings,
            ..
        } => {
            assert_eq!(
                *request,
                RequestId(7),
                "the caller's correlation id, echoed"
            );
            assert_eq!(
                *cancel_kind,
                CancelKind::Unsupported,
                "what a cancel on this session can actually do, up front"
            );
            assert_eq!(
                warnings.len(),
                1,
                "the connect-time findings C-6 exists for reach the caller with the open"
            );
            let session = registry
                .get(id)
                .expect("an opened session is available for submission");
            assert_eq!(session.id(), id);
            assert_eq!(
                session.connection_id(),
                *connection,
                "the event names the connection the handle owns"
            );
            assert_eq!(
                session.connect_warnings().len(),
                1,
                "and the handle still has its own copy, for a UI that asks late"
            );
        }
        other => panic!("expected Opened, got {other:?}"),
    }
    assert_eq!(registry.state(id), Some(RegisteredSession::Open));

    // The session works, and its replies follow the `Opened`.
    let session = registry.get(id).expect("open");
    session.submit_ping(RequestId(8)).expect("accepted");
    session
        .submit_close(RequestId(9), Some(CloseDisposition::Rollback))
        .expect("accepted");
    let rest = support::drain_until_terminal_of(&queue, id);
    let all: Vec<&SessionEvent> = seen
        .iter()
        .chain(rest.iter())
        .filter(|event| event.session() == id)
        .collect();
    let replies: Vec<u64> = all
        .iter()
        .filter(|event| event.is_reply())
        .filter_map(|event| event.request().map(|request| request.0))
        .collect();
    assert_eq!(
        replies,
        vec![7, 8, 9],
        "the open is answered first, and every request exactly once"
    );
    assert!(
        all.last().is_some_and(|event| event.is_terminal()),
        "Terminal is last: {all:#?}"
    );
}

#[test]
fn an_open_that_fails_reports_the_driver_error_with_its_native_code() {
    let scenario = support::scenario();
    scenario.fail_connect(
        ScriptedError::new(ErrorKind::Authentication, "invalid username/password")
            .with_native(1017, "ORA-01017: invalid username/password; logon denied"),
    );
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    let seen = support::drain_until_terminal_of(&queue, id);

    match open_reply(&seen, id) {
        SessionEvent::OpenFailed { request, error, .. } => {
            assert_eq!(*request, RequestId(1));
            assert_eq!(
                error.kind(),
                ErrorKind::Authentication,
                "the driver's own classification survives; a UI cannot tell a bad password from \
                 a down listener if every open failure says `Connection`"
            );
            assert_eq!(error.native().map(NativeError::code), Some(1017));
        }
        other => panic!("expected OpenFailed, got {other:?}"),
    }

    let terminal = terminals(&seen, id);
    assert_eq!(terminal.len(), 1, "exactly one Terminal, announced once");
    match terminal[0] {
        SessionEvent::Terminal {
            lifecycle, cause, ..
        } => {
            assert_eq!(
                *lifecycle,
                SessionLifecycle::Lost,
                "a connect that failed never produced a session; `Lost` with the cause is the \
                 honest announcement"
            );
            assert!(cause.is_some(), "and it carries why");
        }
        other => panic!("expected Terminal, got {other:?}"),
    }
    assert!(
        registry.get(id).is_none(),
        "a session that never opened is never handed out"
    );
    assert_eq!(registry.state(id), Some(RegisteredSession::Ended));
}

/// The driver bounds its own connect on a helper thread (ADR-0002 H1, M2.1);
/// what the core owes is to report the expiry as the connection failure it is,
/// unrelabelled. The *timing* of the expiry is the driver's and is tested in
/// `crates/drivers/oracle-thin/src/connect_timeout.rs`.
#[test]
fn a_connect_timeout_expiry_arrives_as_open_failed_with_a_connection_error() {
    let scenario = support::scenario();
    scenario.fail_connect(ScriptedError::new(
        ErrorKind::Connection,
        "reldex-driver: the connection attempt did not finish within connect_timeout",
    ));
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    let seen = support::drain_until_terminal_of(&queue, id);

    match open_reply(&seen, id) {
        SessionEvent::OpenFailed { error, .. } => {
            assert_eq!(error.kind(), ErrorKind::Connection);
            assert!(
                error.to_string().contains("connect_timeout"),
                "the driver's own wording reaches the user: {error}"
            );
        }
        other => panic!("expected OpenFailed, got {other:?}"),
    }
    assert_eq!(terminals(&seen, id).len(), 1);
}

/// K6, on the one call that has no session to contain it yet.
#[test]
fn a_driver_that_panics_while_connecting_is_contained_and_reported() {
    let scenario = support::scenario();
    scenario.panic_connect("reldex-driver-mock: a driver that reached the impossible");
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(4));
    let seen = support::drain_until_terminal_of(&queue, id);

    match open_reply(&seen, id) {
        SessionEvent::OpenFailed { error, .. } => {
            assert_eq!(
                error.kind(),
                ErrorKind::DriverInternal,
                "a contained panic is a driver bug, and is classified as one"
            );
            assert!(
                error.to_string().contains("panicked"),
                "and says so: {error}"
            );
        }
        other => panic!("expected OpenFailed, got {other:?}"),
    }
    assert_eq!(
        terminals(&seen, id).len(),
        1,
        "the session still announces its end exactly once"
    );
    assert_eq!(
        scenario.counts().connections_live(),
        0,
        "nothing was opened, so nothing can be left open"
    );
}

/// The open is a request like any other: it reserves a slot, and the slot comes
/// back when the consumer drains the reply.
#[test]
fn the_open_reserves_one_slot_and_gives_it_back_when_its_reply_is_drained() {
    let scenario = support::scenario();
    let limits =
        SessionLimits::new().with_max_outstanding_requests(NonZeroUsize::new(2).expect("non-zero"));
    let (registry, queue) = support::registry_with(limits, EventCaps::new());

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    support::wait_for("the session opens", || {
        registry.state(id) == Some(RegisteredSession::Open)
    });
    let session = registry.get(id).expect("open");
    assert_eq!(
        session.outstanding_requests(),
        1,
        "the undrained `Opened` still holds the open's slot"
    );

    session.submit_ping(RequestId(2)).expect("one slot left");
    assert_eq!(
        session
            .submit_ping(RequestId(3))
            .expect_err("and only one")
            .kind(),
        ErrorKind::Resource
    );

    let seen = support::drain_n(&queue, 2);
    assert_eq!(support::reply_requests(&seen), vec![1, 2]);
    support::wait_for("both slots come back", || {
        session.outstanding_requests() == 0
    });

    let _ = session.close(Some(CloseDisposition::Rollback));
}

/// Nothing can be submitted to a connection that does not exist yet: §B3 gives
/// the caller a `SessionId`, not a handle, and the handle only appears when the
/// session is open.
#[test]
fn a_session_that_is_still_connecting_hands_out_no_handle() {
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.block_connect(Arc::clone(&gate));
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    assert!(gate.wait_until_blocked(support::short_timeout()));
    assert_eq!(registry.state(id), Some(RegisteredSession::Opening));
    assert!(
        registry.get(id).is_none(),
        "no handle while the connect is still running, so nothing can be submitted"
    );
    assert_eq!(queue.len(), 0, "and nothing has been answered yet");

    gate.release();
    let seen = support::drain_n(&queue, 1);
    assert!(matches!(open_reply(&seen, id), SessionEvent::Opened { .. }));
    assert!(registry.get(id).is_some());

    let session = registry.get(id).expect("open");
    let _ = session.close(Some(CloseDisposition::Rollback));
}

/// `open` returns without waiting for the connect. Asserted by *ordering*, not
/// by a clock: the call returns while the driver is still parked inside
/// `connect`, which it could not do if it were waiting for it.
#[test]
fn open_returns_while_the_connect_is_still_running() {
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.block_connect(Arc::clone(&gate));
    let (registry, _queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    // The connect has not returned — the gate is still holding it — and this
    // thread is already here.
    assert!(gate.wait_until_blocked(support::short_timeout()));
    assert_eq!(registry.state(id), Some(RegisteredSession::Opening));

    assert_eq!(registry.abandon(id), Abandoned::Connecting);
    gate.release();
}

/// Fan-in: many sessions, one queue. Per session the order is exact — `Opened`
/// before any reply, `Terminal` last — while across sessions nothing is
/// promised and events interleave freely (ordering rule 5).
#[test]
fn many_concurrent_opens_keep_per_session_order() {
    const SESSIONS: usize = 32;

    let scenario = support::scenario();
    let (registry, queue) = support::registry();

    let ids: Vec<SessionId> = (0..SESSIONS)
        .map(|index| {
            registry.open(
                support::driver(&scenario),
                support::params(),
                RequestId(index as u64 * 10),
            )
        })
        .collect();

    for (index, id) in ids.iter().enumerate() {
        support::wait_for("every session opens", || {
            registry.state(*id) == Some(RegisteredSession::Open)
        });
        let session = registry.get(*id).expect("open");
        session
            .submit_execute(RequestId(index as u64 * 10 + 1), Statement::new("SELECT 1"))
            .expect("accepted");
        session
            .submit_close(
                RequestId(index as u64 * 10 + 2),
                Some(CloseDisposition::Rollback),
            )
            .expect("a close is never refused for lack of a slot");
    }

    let seen = support::drain_until(&queue, |seen| {
        seen.iter().filter(|event| event.is_terminal()).count() >= SESSIONS
    });

    for (index, id) in ids.iter().enumerate() {
        let mine = support::of_session(&seen, *id);
        let replies: Vec<u64> = mine
            .iter()
            .filter(|event| event.is_reply())
            .filter_map(|event| event.request().map(|request| request.0))
            .collect();
        let base = index as u64 * 10;
        assert_eq!(
            replies,
            vec![base, base + 1, base + 2],
            "per session: the open is answered first, then each request exactly once"
        );
        assert!(
            matches!(mine.first(), Some(SessionEvent::Opened { .. })),
            "`Opened` precedes every other event of its session: {mine:#?}"
        );
        assert!(
            mine.last().is_some_and(|event| event.is_terminal()),
            "and `Terminal` is last: {mine:#?}"
        );
        assert_eq!(
            mine.iter().filter(|event| event.is_terminal()).count(),
            1,
            "exactly once"
        );
        registry.retire(*id);
    }
    assert_eq!(
        scenario.counts().connections_live(),
        0,
        "every connection this test opened was closed"
    );
}

/// Retiring is what the registry lets go on, and `Terminal` is the only event
/// that says it may.
#[test]
fn retiring_a_session_releases_the_registrys_hold_on_it() {
    let scenario = support::scenario();
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    support::wait_for("the session opens", || registry.get(id).is_some());
    let session = registry.get(id).expect("open");
    session
        .submit_close(RequestId(2), Some(CloseDisposition::Rollback))
        .expect("accepted");
    drop(session);

    let seen = support::drain_until_terminal_of(&queue, id);
    assert_eq!(terminals(&seen, id).len(), 1);
    assert_eq!(
        registry.state(id),
        Some(RegisteredSession::Open),
        "the handle stays valid until the registry is told to release it"
    );

    assert!(registry.retire(id), "there was something to retire");
    assert_eq!(registry.state(id), None);
    assert!(!registry.retire(id), "and only once");
    assert_eq!(scenario.counts().connections_live(), 0);
}
