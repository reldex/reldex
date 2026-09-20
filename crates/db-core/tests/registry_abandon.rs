//! `SessionRegistry::abandon`, and every interleaving it has to survive
//! (`docs/exec-plans/active/phase-1.md` §B3; ADR-0002 amendment R4).
//!
//! A connect cannot be interrupted (ADR-0002 H1, spike U-15), so every
//! guarantee here is about what happens *around* one that is still running:
//! the open is answered now, the session announces its end now, and a
//! connection that turns up afterwards is closed rather than adopted. The
//! assertions are the four that matter — exactly one reply, exactly one
//! `Terminal`, never an `Opened` after an `OpenFailed`, and never a live
//! connection left behind.
//!
//! Every interleaving is **forced** with the mock's connect gate rather than
//! hoped for, and every wait is on a condition under the hang guard.

mod support;

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use reldex_db_core::{
    Abandoned, CloseDisposition, EventCaps, RegisteredSession, RequestId, SessionEvent, SessionId,
    SessionLifecycle, SessionLimits, Statement,
};
use reldex_db_driver_api::ErrorKind;
use reldex_driver_mock::{Action, BlockGate, BlockSpec, ScriptValue, ScriptedError};

/// What one session's stream said about its open and its end, checked against
/// the four invariants that hold for *every* interleaving.
struct Outcome {
    opened: usize,
    open_failed: usize,
    open_failure_kind: Option<ErrorKind>,
    terminals: usize,
    terminal_lifecycle: Option<SessionLifecycle>,
    terminal_is_last: bool,
}

fn outcome(seen: &[SessionEvent], session: SessionId) -> Outcome {
    let mine = support::of_session(seen, session);
    let mut out = Outcome {
        opened: 0,
        open_failed: 0,
        open_failure_kind: None,
        terminals: 0,
        terminal_lifecycle: None,
        terminal_is_last: mine.last().is_some_and(|event| event.is_terminal()),
    };
    let mut open_failed_at = None;
    let mut opened_at = None;
    for (index, event) in mine.iter().enumerate() {
        match event {
            SessionEvent::Opened { .. } => {
                out.opened += 1;
                opened_at = Some(index);
            }
            SessionEvent::OpenFailed { error, .. } => {
                out.open_failed += 1;
                out.open_failure_kind = Some(error.kind());
                open_failed_at = Some(index);
            }
            SessionEvent::Terminal { lifecycle, .. } => {
                out.terminals += 1;
                out.terminal_lifecycle = Some(*lifecycle);
            }
            _ => {}
        }
    }
    assert_eq!(
        out.opened + out.open_failed,
        1,
        "exactly one reply answers the open, whatever raced it: {mine:#?}"
    );
    assert_eq!(out.terminals, 1, "exactly one Terminal: {mine:#?}");
    assert!(
        out.terminal_is_last,
        "Terminal is the last thing a session says: {mine:#?}"
    );
    assert!(
        !(opened_at.is_some() && open_failed_at.is_some()),
        "a late success must never be adopted after the open was failed"
    );
    out
}

/// Abandon wins, the connect succeeds afterwards: one `OpenFailed{Cancelled}`,
/// no `Opened`, one `Terminal`, and the connection that arrived late is
/// **closed** rather than leaked.
#[test]
fn abandoning_a_connect_that_succeeds_late_closes_the_connection_it_never_adopted() {
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.block_connect(Arc::clone(&gate));
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    assert!(gate.wait_until_blocked(support::short_timeout()));

    assert_eq!(
        registry.abandon(id),
        Abandoned::Connecting,
        "the connect had not finished, so this is the case §B3 answers immediately"
    );
    // Immediately: both events are already produced, with the connect still
    // parked.
    let seen = support::drain_n(&queue, 2);
    let out = outcome(&seen, id);
    assert_eq!(out.open_failed, 1);
    assert_eq!(
        out.open_failure_kind,
        Some(ErrorKind::Cancelled),
        "the caller gave up; nothing failed and nothing was connected"
    );
    assert_eq!(
        out.terminal_lifecycle,
        Some(SessionLifecycle::Closed),
        "the abandon won, so the session ended deliberately rather than failing"
    );
    assert!(registry.get(id).is_none());

    // Now let the connect win its race with nothing.
    gate.release();
    support::wait_for("the connect finishes and its connection is closed", || {
        let counts = scenario.counts();
        counts.connects_finished == 1 && counts.connections_live() == 0
    });
    assert_eq!(
        scenario.counts().connections_opened,
        1,
        "the connect really did produce a session — this is not a vacuous assertion"
    );
    assert_eq!(
        queue.len(),
        0,
        "and the late success produced no event at all"
    );
}

/// The same, with the connect *failing* late: the abandon still owns the one
/// reply, and the late failure is silent.
#[test]
fn abandoning_a_connect_that_fails_late_still_produces_exactly_one_reply() {
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.block_connect(Arc::clone(&gate));
    scenario.fail_connect(ScriptedError::new(
        ErrorKind::Connection,
        "listener is down",
    ));
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    assert!(gate.wait_until_blocked(support::short_timeout()));
    assert_eq!(registry.abandon(id), Abandoned::Connecting);

    let seen = support::drain_n(&queue, 2);
    let out = outcome(&seen, id);
    assert_eq!(out.open_failure_kind, Some(ErrorKind::Cancelled));
    assert_eq!(out.terminal_lifecycle, Some(SessionLifecycle::Closed));

    gate.release();
    support::wait_for("the late connect finishes", || {
        scenario.counts().connects_finished == 1
    });
    assert_eq!(
        scenario.counts().connections_opened,
        0,
        "it failed, so there was never a connection"
    );
    assert_eq!(
        registry.state(id),
        Some(RegisteredSession::Ended),
        "the tombstone the abandon left is what the late failure found"
    );
    assert_eq!(
        queue.len(),
        0,
        "the late failure has no request left to answer, so it says nothing"
    );
}

/// The other order: the connect completes first, so the session really opened
/// and the abandon is what ends it.
#[test]
fn abandoning_after_the_connect_completed_ends_an_open_session_instead() {
    let scenario = support::scenario();
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    support::wait_for("the session opens", || registry.get(id).is_some());

    assert!(matches!(registry.abandon(id), Abandoned::Open { .. }));
    assert_eq!(registry.state(id), Some(RegisteredSession::Ending));
    assert!(
        registry.get(id).is_none(),
        "a session that has been told to end is not handed out for more work"
    );

    let seen = support::drain_until_terminal_of(&queue, id);
    let out = outcome(&seen, id);
    assert_eq!(out.opened, 1, "it did open — the `Opened` is real");
    assert_eq!(
        out.terminal_lifecycle,
        Some(SessionLifecycle::Closed),
        "abandoning is a deliberate end, not a failure"
    );
    support::wait_for("the connection is released", || {
        scenario.counts().connections_live() == 0
    });
    assert!(registry.retire(id));
}

/// Abandoning an open session releases the connection **without committing**.
/// The transaction the server rolls back is reported, never hidden
/// (`SPEC.md` §10).
#[test]
fn abandoning_an_open_session_never_commits_and_reports_the_transaction_it_costs() {
    let scenario = support::scenario();
    scenario.on_sql(
        "INSERT INTO t VALUES (1)",
        Action::Dml {
            rows_affected: 1,
            insert: Some(("t".to_owned(), vec![ScriptValue::from(1_i64)])),
        },
    );
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    support::wait_for("the session opens", || registry.get(id).is_some());
    let session = registry.get(id).expect("open");
    session
        .submit_execute(RequestId(2), Statement::new("INSERT INTO t VALUES (1)"))
        .expect("accepted");
    support::drain_until(&queue, |seen| support::reply_requests(seen).contains(&2));
    assert!(
        session.has_possibly_active_transaction(),
        "the insert opened one"
    );
    drop(session);

    assert_eq!(
        registry.abandon(id),
        Abandoned::Open {
            transaction_possibly_lost: true
        },
        "the caller is told what abandoning cost, so it can tell the user"
    );

    support::drain_until_terminal_of(&queue, id);
    support::wait_for("the connection is released", || {
        scenario.counts().connections_live() == 0
    });
    assert!(
        scenario.committed_rows("t").is_empty(),
        "abandoning must never commit; the row is gone with the connection"
    );
}

/// Idempotent, on either side of the transition.
#[test]
fn abandoning_twice_changes_nothing_the_second_time() {
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.block_connect(Arc::clone(&gate));
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    assert!(gate.wait_until_blocked(support::short_timeout()));
    assert_eq!(registry.abandon(id), Abandoned::Connecting);
    assert_eq!(
        registry.abandon(id),
        Abandoned::Ending,
        "the second abandon finds a session that is already ending"
    );
    assert_eq!(registry.abandon(id), Abandoned::Ending);

    gate.release();
    let seen = support::drain_n(&queue, 2);
    outcome(&seen, id);
    support::wait_for("the connect finishes and leaves nothing open", || {
        let counts = scenario.counts();
        counts.connects_finished == 1 && counts.connections_live() == 0
    });
    assert_eq!(queue.len(), 0, "and no extra events came from the repeats");

    assert!(registry.retire(id));
    assert_eq!(
        registry.abandon(id),
        Abandoned::Unknown,
        "after retiring, the registry has never heard of it"
    );
}

/// An open session abandoned twice: the first one ends it, the second is a
/// no-op, and the session still announces exactly one `Terminal`.
#[test]
fn abandoning_an_open_session_twice_still_ends_it_exactly_once() {
    let scenario = support::scenario();
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    support::wait_for("the session opens", || registry.get(id).is_some());

    assert!(matches!(registry.abandon(id), Abandoned::Open { .. }));
    assert_eq!(registry.abandon(id), Abandoned::Ending);

    let seen = support::drain_until_terminal_of(&queue, id);
    outcome(&seen, id);
    support::wait_for("the connection is released", || {
        scenario.counts().connections_live() == 0
    });
}

/// **The hard requirement.** `abandon` reserves no slot, so a session at its
/// outstanding limit can still be stopped — which matters most for the one it
/// is hardest to stop, a session that is still connecting and whose open is
/// itself the request holding the only slot.
#[test]
fn abandon_is_never_refused_at_the_request_cap() {
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.block_connect(Arc::clone(&gate));
    let one =
        SessionLimits::new().with_max_outstanding_requests(NonZeroUsize::new(1).expect("non-zero"));
    let (registry, queue) = support::registry_with(one, EventCaps::new());

    // A limit of one, and the open takes it: this session is at its cap before
    // it has even connected.
    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    assert!(gate.wait_until_blocked(support::short_timeout()));

    assert_eq!(
        registry.abandon(id),
        Abandoned::Connecting,
        "abandon has no `Resource` failure to give: it reserves nothing"
    );
    let seen = support::drain_n(&queue, 2);
    outcome(&seen, id);
    gate.release();
    support::wait_for("the connect finishes and leaves nothing open", || {
        let counts = scenario.counts();
        counts.connects_finished == 1 && counts.connections_live() == 0
    });
}

/// The same, on an open session whose queue is full of undrained replies.
#[test]
fn abandon_is_never_refused_when_an_open_sessions_replies_are_undrained() {
    const LIMIT: usize = 4;

    let scenario = support::scenario();
    let limits = SessionLimits::new()
        .with_max_outstanding_requests(NonZeroUsize::new(LIMIT).expect("non-zero"));
    let (registry, queue) = support::registry_with(limits, EventCaps::new());

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    support::wait_for("the session opens", || registry.get(id).is_some());
    let session = registry.get(id).expect("open");
    // The open holds one; fill the rest and drain nothing.
    for request in 2..=LIMIT as u64 {
        session.submit_ping(RequestId(request)).expect("accepted");
    }
    support::wait_for("the session reaches its limit", || {
        session.outstanding_requests() == LIMIT
    });
    assert_eq!(
        session
            .submit_ping(RequestId(99))
            .expect_err("an ordinary request is refused here")
            .kind(),
        ErrorKind::Resource
    );
    drop(session);

    assert!(matches!(registry.abandon(id), Abandoned::Open { .. }));

    let seen = support::drain_until_terminal_of(&queue, id);
    assert_eq!(
        support::reply_requests(&seen),
        (1..=LIMIT as u64).collect::<Vec<_>>(),
        "every accepted request is still answered exactly once; the refused one is not"
    );
    outcome(&seen, id);
    assert!(
        seen.len() <= 2 * LIMIT + EventCaps::new().max_unsolicited_per_session().get() + 3,
        "the published bound `2R + U + 3` still holds with an open and an abandon in it: {} \
         events",
        seen.len()
    );
}

/// The race, run both ways many times, asserting only what is true of *both*
/// orders. This is the test that would catch a second reply, a missing
/// `Terminal`, an `Opened` after an `OpenFailed`, or a leaked connection.
#[test]
fn abandon_racing_the_connect_holds_the_invariants_in_both_orders() {
    const ROUNDS: usize = 60;

    let scenario = support::scenario();
    let (registry, queue) = support::registry();
    let registry = Arc::new(registry);

    let mut cancelled = 0_usize;
    let mut opened = 0_usize;
    for round in 0..ROUNDS {
        let gate = BlockGate::new();
        scenario.block_connect(Arc::clone(&gate));
        let id = registry.open(
            support::driver(&scenario),
            support::params(),
            RequestId(round as u64),
        );

        // Two threads, started together, racing: one releases the connect, the
        // other abandons. Which wins is genuinely undetermined; the
        // invariants are not.
        let releaser = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || gate.release())
        };
        let abandoner = {
            let registry = Arc::clone(&registry);
            thread::spawn(move || registry.abandon(id))
        };
        releaser.join().expect("the releaser does not panic");
        let verdict = abandoner.join().expect("the abandoner does not panic");

        let seen = support::drain_until_terminal_of(&queue, id);
        let out = outcome(&seen, id);
        match verdict {
            Abandoned::Connecting => {
                assert_eq!(out.open_failure_kind, Some(ErrorKind::Cancelled));
                assert_eq!(out.terminal_lifecycle, Some(SessionLifecycle::Closed));
                cancelled += 1;
            }
            Abandoned::Open { .. } => {
                assert_eq!(out.opened, 1, "if it opened, the `Opened` is real");
                assert_eq!(out.terminal_lifecycle, Some(SessionLifecycle::Closed));
                opened += 1;
            }
            other => panic!("a session that was connecting cannot be {other:?}"),
        }
        registry.retire(id);
        support::wait_for("no connection outlives its round", || {
            let counts = scenario.counts();
            counts.connects_finished == round + 1 && counts.connections_live() == 0
        });
    }

    assert_eq!(
        cancelled + opened,
        ROUNDS,
        "every round resolved into exactly one of the two orders"
    );
}

/// Dropping the registry while sessions are still connecting: prompt, no join
/// on a parked connect, every open answered, every session announced, and
/// every late connection closed.
#[test]
fn dropping_the_registry_while_sessions_are_connecting_answers_them_and_leaks_nothing() {
    const SESSIONS: usize = 4;

    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.block_connect(Arc::clone(&gate));
    let (registry, queue) = support::registry();

    let ids: Vec<SessionId> = (0..SESSIONS)
        .map(|index| {
            registry.open(
                support::driver(&scenario),
                support::params(),
                RequestId(index as u64),
            )
        })
        .collect();
    assert!(gate.wait_until_blocked(support::short_timeout()));
    assert_eq!(
        scenario.counts().connections_opened,
        0,
        "the gate was set before the first `open`, so no connect can have finished"
    );

    // Prompt: the registry must not wait for a connect it cannot interrupt.
    support::with_timeout_guard(support::short_timeout(), move || drop(registry));

    let seen = support::drain_until(&queue, |seen| {
        seen.iter().filter(|event| event.is_terminal()).count() >= SESSIONS
    });
    for id in &ids {
        let out = outcome(&seen, *id);
        assert_eq!(out.open_failure_kind, Some(ErrorKind::Cancelled));
        assert_eq!(out.terminal_lifecycle, Some(SessionLifecycle::Closed));
    }

    gate.release();
    support::wait_for("every late connection is closed on its own thread", || {
        let counts = scenario.counts();
        counts.connects_finished == SESSIONS
            && counts.connections_opened == SESSIONS
            && counts.connections_live() == 0
    });
}

/// Dropping the registry with open sessions: none of them commits, and each
/// announces its end exactly once.
#[test]
fn dropping_the_registry_abandons_open_sessions_without_committing() {
    const SESSIONS: usize = 4;

    let scenario = support::scenario();
    scenario.on_sql(
        "INSERT INTO t VALUES (1)",
        Action::Dml {
            rows_affected: 1,
            insert: Some(("t".to_owned(), vec![ScriptValue::from(1_i64)])),
        },
    );
    let (registry, queue) = support::registry();

    let ids: Vec<SessionId> = (0..SESSIONS)
        .map(|index| {
            let id = registry.open(
                support::driver(&scenario),
                support::params(),
                RequestId(index as u64 * 10),
            );
            support::wait_for("the session opens", || registry.get(id).is_some());
            let session = registry.get(id).expect("open");
            session
                .submit_execute(
                    RequestId(index as u64 * 10 + 1),
                    Statement::new("INSERT INTO t VALUES (1)"),
                )
                .expect("accepted");
            id
        })
        .collect();

    support::with_timeout_guard(support::short_timeout(), move || drop(registry));

    let seen = support::drain_until(&queue, |seen| {
        seen.iter().filter(|event| event.is_terminal()).count() >= SESSIONS
    });
    for id in &ids {
        let out = outcome(&seen, *id);
        assert_eq!(out.opened, 1);
        assert_eq!(out.terminal_lifecycle, Some(SessionLifecycle::Closed));
    }
    support::wait_for("every connection is released", || {
        scenario.counts().connections_live() == 0
    });
    assert!(
        scenario.committed_rows("t").is_empty(),
        "dropping the registry must never commit anything"
    );
}

/// A consumer that walks away mid-connect. Nothing here may panic, and the
/// slots the discarded events held must come back.
#[test]
fn dropping_the_queue_while_a_session_is_connecting_is_survivable() {
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.block_connect(Arc::clone(&gate));
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    assert!(gate.wait_until_blocked(support::short_timeout()));
    drop(queue);

    assert_eq!(registry.abandon(id), Abandoned::Connecting);
    gate.release();
    support::wait_for("the connect finishes and leaves nothing open", || {
        let counts = scenario.counts();
        counts.connects_finished == 1 && counts.connections_live() == 0
    });
    assert!(registry.retire(id));
}

/// Abandoning a session whose worker is inside an uninterruptible statement
/// returns straight away; the session's `Terminal` follows when the driver
/// call finally returns.
#[test]
fn abandon_does_not_wait_for_a_worker_that_is_busy() {
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN blocked; END;",
        Action::Block(BlockSpec::new(Arc::clone(&gate))),
    );
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    support::wait_for("the session opens", || registry.get(id).is_some());
    let session = registry.get(id).expect("open");
    session
        .submit_execute(RequestId(2), Statement::new("BEGIN blocked; END;"))
        .expect("accepted");
    assert!(gate.wait_until_blocked(support::short_timeout()));
    drop(session);

    let ended = Arc::new(AtomicUsize::new(0));
    {
        let ended = Arc::clone(&ended);
        // The abandon returns while the statement is still parked. If it
        // waited, this counter would stay at zero until the gate released.
        assert!(matches!(registry.abandon(id), Abandoned::Open { .. }));
        ended.fetch_add(1, Ordering::SeqCst);
    }
    assert_eq!(ended.load(Ordering::SeqCst), 1);
    assert!(
        gate.wait_until_blocked(support::short_timeout()),
        "and the statement really is still parked"
    );

    gate.release();
    let seen = support::drain_until_terminal_of(&queue, id);
    outcome(&seen, id);
    support::wait_for("the connection is released", || {
        scenario.counts().connections_live() == 0
    });
}

/// Abandoning something the registry never had, or has already let go of.
#[test]
fn abandoning_an_unknown_session_says_so() {
    let scenario = support::scenario();
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    support::wait_for("the session opens", || registry.get(id).is_some());
    let stranger = {
        let (other, _queue) = support::registry();
        let stranger = other.open(support::driver(&scenario), support::params(), RequestId(1));
        support::wait_for("the other registry's session opens", || {
            other.get(stranger).is_some()
        });
        stranger
    };
    assert_eq!(
        registry.abandon(stranger),
        Abandoned::Unknown,
        "one registry never speaks for another's sessions"
    );

    assert!(matches!(registry.abandon(id), Abandoned::Open { .. }));
    support::drain_until_terminal_of(&queue, id);
    assert!(registry.retire(id));
    assert_eq!(registry.abandon(id), Abandoned::Unknown);
    support::wait_for("every connection is released", || {
        scenario.counts().connections_live() == 0
    });
}

/// Retiring a session that is still connecting must not drop it silently: the
/// open it accepted is still answered, and its end is still announced.
#[test]
fn retiring_a_connecting_session_answers_its_open_first() {
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.block_connect(Arc::clone(&gate));
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    assert!(gate.wait_until_blocked(support::short_timeout()));
    assert!(registry.retire(id));
    assert_eq!(registry.state(id), None);

    let seen = support::drain_n(&queue, 2);
    let out = outcome(&seen, id);
    assert_eq!(out.open_failure_kind, Some(ErrorKind::Cancelled));

    gate.release();
    support::wait_for("the connect finishes and leaves nothing open", || {
        let counts = scenario.counts();
        counts.connects_finished == 1 && counts.connections_live() == 0
    });
}

/// A session that was abandoned while connecting refuses later work with the
/// reason it actually had, rather than a generic "closed".
#[test]
fn a_close_submitted_after_an_abandoned_open_reports_the_cancellation() {
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.block_connect(Arc::clone(&gate));
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    assert!(gate.wait_until_blocked(support::short_timeout()));
    assert_eq!(registry.abandon(id), Abandoned::Connecting);
    let seen = support::drain_n(&queue, 2);
    match support::of_session(&seen, id).first() {
        Some(SessionEvent::OpenFailed { error, .. }) => {
            assert_eq!(error.kind(), ErrorKind::Cancelled);
            assert!(
                error.to_string().contains("abandoned"),
                "and says what happened: {error}"
            );
        }
        other => panic!("expected OpenFailed first, got {other:?}"),
    }
    assert!(
        registry.get(id).is_none(),
        "there is no handle to submit anything through"
    );

    gate.release();
    support::wait_for("the connect finishes and leaves nothing open", || {
        let counts = scenario.counts();
        counts.connects_finished == 1 && counts.connections_live() == 0
    });
    let _ = CloseDisposition::Rollback;
}
