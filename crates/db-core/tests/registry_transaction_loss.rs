//! `SessionEvent::Terminal`'s `transaction_possibly_lost` — the authoritative
//! answer to "did this session take the user's work with it?"
//!
//! `SPEC.md` §10 forbids silently committing *and* forbids hiding a
//! transaction loss. Every path that can end a session is here, because the
//! value is only useful if it is complete: a close that resolved the
//! transaction, a close that could not, an abandon, a retire, the registry's
//! teardown, a dropped handle, and a connection that died on its own.
//!
//! The flag is computed **on the worker thread**, at the point the session
//! actually ends — after every command queued ahead of the close has run, which
//! is where ADR-0002 K4 already decides. The synchronous answers a control
//! thread can read (`DatabaseSession::has_possibly_active_transaction`, and the
//! hint on `Abandoned::Open`) are lower bounds, and the first test below is the
//! case that proves the difference is real rather than theoretical.

mod support;

use std::sync::Arc;

use reldex_db_core::{
    Abandoned, CloseDisposition, CloseError, RequestId, SessionEvent, SessionLifecycle, Statement,
};
use reldex_db_driver_api::{Capabilities, ErrorKind, SessionState};
use reldex_driver_mock::{Action, BlockGate, BlockSpec, ScriptValue, ScriptedError};

const INSERT: &str = "INSERT INTO t VALUES (1)";
const PARKED: &str = "SELECT parked FROM dual";

fn insert_row() -> Action {
    Action::Dml {
        rows_affected: 1,
        insert: Some(("t".to_owned(), vec![ScriptValue::from(1_i64)])),
    }
}

/// The review's repro, and the reason this flag exists at all.
///
/// A statement is parked in the driver and an `INSERT` is queued behind it.
/// `abandon` runs on the caller's thread, where the only thing it can read is a
/// snapshot taken *before* the queued statement — so the old answer was "no
/// transaction, nothing lost", and then the worker ran the `INSERT` and the
/// server rolled it back. The loss was real and nobody was told.
///
/// Now: the synchronous hint is conservative enough to catch it, and the value
/// on `Terminal` is computed where it cannot be wrong.
#[test]
fn a_transaction_opened_after_abandon_was_called_is_still_reported_on_terminal() {
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.on_sql(
        PARKED,
        Action::Block(BlockSpec::new(Arc::clone(&gate)).with_unobserved_cancel()),
    );
    scenario.on_sql(INSERT, insert_row());
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    support::drain_until(&queue, |seen| {
        seen.iter().any(|event| event.session() == id)
    });
    let session = registry.get(id).expect("the open succeeded");

    // Park the worker inside the driver, then queue the INSERT behind it.
    session
        .submit_execute(RequestId(2), Statement::new(PARKED))
        .expect("the parked statement is accepted");
    assert!(gate.wait_until_blocked(support::short_timeout()));
    session
        .submit_execute(RequestId(3), Statement::new(INSERT))
        .expect("the INSERT is accepted and queues behind it");

    // The snapshot a control thread can read says "no transaction" — truthfully,
    // and uselessly: the INSERT has not run yet.
    assert!(
        !session.has_possibly_active_transaction(),
        "nothing has opened a transaction *yet*; this is the stale answer the flag replaces"
    );
    drop(session);

    // The synchronous hint is nonetheless `true`, because it also counts the
    // driver call in flight and the requests still outstanding — either of
    // which can open a transaction after this instant.
    assert_eq!(
        registry.abandon(id),
        Abandoned::Open {
            transaction_possibly_lost: true
        },
        "the hint must be conservative enough to catch work that has not run yet"
    );

    gate.release();
    let seen = support::drain_until_terminal_of(&queue, id);
    assert_eq!(
        support::terminal_loss(&seen, id),
        Some(true),
        "the INSERT ran after the abandon and nothing resolved it"
    );
    assert!(
        scenario.committed_rows("t").is_empty(),
        "and it really was lost: abandoning never commits"
    );
}

/// The user said commit, and it worked. Nothing was lost.
#[test]
fn a_close_that_commits_reports_no_loss() {
    let scenario = support::scenario();
    scenario.on_sql(INSERT, insert_row());
    let session = support::open_on(&scenario, support::ReplyPath::Events);

    session
        .execute(Statement::new(INSERT))
        .wait()
        .expect("the INSERT runs");
    assert!(session.has_possibly_active_transaction());

    let id = session.id();
    session
        .close(Some(CloseDisposition::Commit))
        .expect("the close commits");
    let seen = session.stashed();
    assert_eq!(
        support::terminal_loss(&seen, id),
        Some(false),
        "the transaction was resolved, exactly as asked"
    );
    assert_eq!(scenario.committed_rows("t").len(), 1);
}

/// A rollback the **user chose** is not a loss either: they were asked, they
/// answered, and the answer was carried out. Only work that disappears without
/// anyone deciding counts.
#[test]
fn a_close_the_user_rolled_back_is_not_a_loss() {
    let scenario = support::scenario();
    scenario.on_sql(INSERT, insert_row());
    let session = support::open_on(&scenario, support::ReplyPath::Events);

    session
        .execute(Statement::new(INSERT))
        .wait()
        .expect("the INSERT runs");
    let id = session.id();
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("the close rolls back");

    let seen = session.stashed();
    assert_eq!(
        support::terminal_loss(&seen, id),
        Some(false),
        "a rollback the user asked for is a decision, not a loss"
    );
    assert!(scenario.committed_rows("t").is_empty());
}

/// Nothing ran, so nothing could be lost.
#[test]
fn a_close_with_no_transaction_reports_no_loss() {
    let scenario = support::scenario();
    let session = support::open_on(&scenario, support::ReplyPath::Events);
    session.ping().wait().expect("the ping succeeds");

    let id = session.id();
    session.close(None).expect("an idle session closes");
    let seen = session.stashed();
    assert_eq!(support::terminal_loss(&seen, id), Some(false));
}

/// `CloseError::DecisionRequired` leaves the session **open**, so there is no
/// `Terminal` to carry a verdict — and must not be mistaken for one.
#[test]
fn a_close_that_needs_a_decision_ends_nothing_and_says_nothing() {
    let scenario = support::scenario();
    scenario.on_sql(INSERT, insert_row());
    let session = support::open_on(&scenario, support::ReplyPath::Events);

    session
        .execute(Statement::new(INSERT))
        .wait()
        .expect("the INSERT runs");
    assert!(matches!(
        session.close(None),
        Err(CloseError::DecisionRequired)
    ));
    assert_eq!(
        session.session_state(),
        SessionLifecycle::Usable,
        "the session is still open, so nothing has been lost yet"
    );

    let id = session.id();
    let seen = session.stashed();
    assert_eq!(
        support::terminal_loss(&seen, id),
        None,
        "no Terminal: the session did not end"
    );
    drop(seen);

    // And the decision, once made, is not a loss.
    session
        .close(Some(CloseDisposition::Commit))
        .expect("the second close commits");
    let seen = session.stashed();
    assert_eq!(support::terminal_loss(&seen, id), Some(false));
    assert_eq!(scenario.committed_rows("t").len(), 1);
}

/// Dropping a session handle resolves nothing (ADR-0002 K5), so a transaction
/// it held goes with the connection — and the session says so.
#[test]
fn dropping_a_session_reports_the_transaction_it_takes_with_it() {
    let scenario = support::scenario();
    scenario.on_sql(INSERT, insert_row());
    let (session, queue) = support::open_events(&scenario);
    let id = session.id();

    session
        .submit_execute(RequestId(1), Statement::new(INSERT))
        .expect("the INSERT is accepted");
    support::drain_until(&queue, |seen| seen.iter().any(SessionEvent::is_reply));
    drop(session);

    let seen = support::drain_until_terminal_of(&queue, id);
    assert_eq!(
        support::terminal_loss(&seen, id),
        Some(true),
        "Drop never commits and never rolls back explicitly, so the server does"
    );
    assert!(scenario.committed_rows("t").is_empty());
}

/// Retiring an **open** session is the same lossy drop, through the registry.
#[test]
fn retiring_an_open_session_reports_the_transaction_it_takes_with_it() {
    let scenario = support::scenario();
    scenario.on_sql(INSERT, insert_row());
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    support::drain_until(&queue, |seen| {
        seen.iter().any(|event| event.session() == id)
    });
    let session = registry.get(id).expect("the open succeeded");
    session
        .submit_execute(RequestId(2), Statement::new(INSERT))
        .expect("the INSERT is accepted");
    support::drain_until(&queue, |seen| seen.iter().any(SessionEvent::is_reply));
    // The registry's own hold plus this one; both must go for `Drop` to run.
    drop(session);

    assert!(registry.retire(id), "the registry was still holding it");
    let seen = support::drain_until_terminal_of(&queue, id);
    assert_eq!(
        support::terminal_loss(&seen, id),
        Some(true),
        "retiring an open session is a loss-bearing path, and reports as one"
    );
    assert!(scenario.committed_rows("t").is_empty());
}

/// So is the registry's teardown.
#[test]
fn dropping_the_registry_reports_the_transactions_it_takes_with_it() {
    let scenario = support::scenario();
    scenario.on_sql(INSERT, insert_row());
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    support::drain_until(&queue, |seen| {
        seen.iter().any(|event| event.session() == id)
    });
    let session = registry.get(id).expect("the open succeeded");
    session
        .submit_execute(RequestId(2), Statement::new(INSERT))
        .expect("the INSERT is accepted");
    support::drain_until(&queue, |seen| seen.iter().any(SessionEvent::is_reply));
    drop(session);
    drop(registry);

    let seen = support::drain_until_terminal_of(&queue, id);
    assert_eq!(support::terminal_loss(&seen, id), Some(true));
    assert!(scenario.committed_rows("t").is_empty());
}

/// A connection that dies takes its transaction with it, and the session's
/// `Terminal` is where that is reported — not merely the failing request.
#[test]
fn a_lost_connection_reports_the_transaction_it_took_with_it() {
    let scenario = support::scenario();
    scenario.on_sql(INSERT, insert_row());
    scenario.on_sql(
        "SELECT dead FROM dual",
        Action::Fail(
            ScriptedError::new(ErrorKind::NetworkLost, "the network went away")
                .with_session_state(SessionState::Lost),
        ),
    );
    let (session, queue) = support::open_events(&scenario);
    let id = session.id();

    session
        .submit_execute(RequestId(1), Statement::new(INSERT))
        .expect("the INSERT is accepted");
    session
        .submit_execute(RequestId(2), Statement::new("SELECT dead FROM dual"))
        .expect("and the statement that kills the session");

    let seen = support::drain_until_terminal_of(&queue, id);
    assert_eq!(
        support::terminal_loss(&seen, id),
        Some(true),
        "the transaction was open when the connection died"
    );
    assert!(scenario.committed_rows("t").is_empty());
    drop(session);
}

/// A session that never opened has nothing to lose, on either of the two ways
/// of never opening.
#[test]
fn a_session_that_never_opened_reports_no_loss() {
    let scenario = support::scenario();
    scenario.fail_connect(ScriptedError::new(
        ErrorKind::Connection,
        "listener is down",
    ));
    let (registry, queue) = support::registry();

    let failed = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    let seen = support::drain_until_terminal_of(&queue, failed);
    assert_eq!(
        support::terminal_loss(&seen, failed),
        Some(false),
        "a connect that failed was never connected"
    );

    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.block_connect(Arc::clone(&gate));
    let (registry2, queue2) = support::registry();
    let abandoned = registry2.open(support::driver(&scenario), support::params(), RequestId(1));
    assert!(gate.wait_until_blocked(support::short_timeout()));
    assert_eq!(registry2.abandon(abandoned), Abandoned::Connecting);

    let seen = support::drain_until_terminal_of(&queue2, abandoned);
    assert_eq!(
        support::terminal_loss(&seen, abandoned),
        Some(false),
        "an abandoned open ran no statement of this consumer's"
    );
    gate.release();
    support::wait_for("the late connect is closed", || {
        let counts = scenario.counts();
        counts.connects_finished == 1 && counts.connections_live() == 0
    });
    drop(registry);
}

/// A driver that cannot rule a transaction out reports `Unknown`, which reads
/// as "may be open" (ADR-0002 K7). The conservative answer is the only safe
/// one: an extra warning costs a sentence, a missed one costs the user's work.
#[test]
fn a_driver_that_cannot_rule_a_transaction_out_reports_the_loss() {
    let scenario = support::scenario();
    scenario.set_capabilities(Capabilities::none().with_exact_transaction_state(false));
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    support::drain_until(&queue, |seen| {
        seen.iter().any(|event| event.session() == id)
    });
    // Nothing has run at all on this session.
    assert_eq!(
        registry.abandon(id),
        Abandoned::Open {
            transaction_possibly_lost: true
        }
    );

    let seen = support::drain_until_terminal_of(&queue, id);
    assert_eq!(
        support::terminal_loss(&seen, id),
        Some(true),
        "the driver cannot say there was no transaction, so neither can we"
    );
}

/// The same session on an **exact** driver, with nothing run: the honest answer
/// is `false`, which is what keeps the conservative answers above from being
/// vacuous.
#[test]
fn an_exact_driver_with_nothing_running_reports_no_loss() {
    let scenario = support::scenario();
    let (registry, queue) = support::registry();

    let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
    support::drain_until(&queue, |seen| {
        seen.iter().any(|event| event.session() == id)
    });
    assert_eq!(
        registry.abandon(id),
        Abandoned::Open {
            transaction_possibly_lost: false
        },
        "nothing is open, nothing is outstanding, nothing is in flight"
    );

    let seen = support::drain_until_terminal_of(&queue, id);
    assert_eq!(support::terminal_loss(&seen, id), Some(false));
}
