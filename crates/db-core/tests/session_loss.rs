//! Session-loss semantics: network loss mid-fetch marks the session lost,
//! subsequent calls fail fast, and the manager never silently reconnects or
//! replaces it (`SPEC.md` §18).

mod support;

use reldex_db_core::{CloseDisposition, CloseError, SessionLifecycle, SessionState, Statement};
use reldex_db_driver_api::{ErrorKind, NativeError};
use reldex_driver_mock::{Action, ColumnSpec, QueryPlan, QuerySource, ScriptValue, ScriptedError};

fn insert_action(row: &str) -> Action {
    Action::Dml {
        rows_affected: 1,
        insert: Some(("t".to_owned(), vec![ScriptValue::from(row)])),
    }
}

/// Opens a session, runs one DML, then loses the network under it.
fn session_with_a_lost_transaction(
    scenario: &std::sync::Arc<reldex_driver_mock::Scenario>,
    path: support::ReplyPath,
) -> support::Session {
    scenario.on_sql("INSERT INTO t VALUES ('a')", insert_action("a"));
    scenario.on_sql(
        "SELECT 1 FROM dual",
        Action::Fail(
            ScriptedError::new(ErrorKind::NetworkLost, "connection reset")
                .with_native(3113, "ORA-03113: end-of-file on communication channel"),
        ),
    );
    let session = support::open_on(scenario, path);
    session
        .execute(Statement::new("INSERT INTO t VALUES ('a')"))
        .wait()
        .expect("insert");
    assert!(session.has_possibly_active_transaction());
    let error = session
        .execute(Statement::new("SELECT 1 FROM dual"))
        .wait()
        .expect_err("scripted network loss");
    assert_eq!(error.kind(), ErrorKind::NetworkLost);
    assert!(session.is_lost());
    session
}

fn network_loss_mid_fetch_marks_the_session_lost_and_fails_fast_afterwards(
    path: support::ReplyPath,
) {
    let scenario = support::scenario();
    let columns = vec![ColumnSpec::new("N", reldex_db_driver_api::SqlType::Number)];
    let rows: Vec<Vec<ScriptValue>> = (0..10_i64).map(|v| vec![ScriptValue::from(v)]).collect();
    let plan = QueryPlan::new(columns, rows).with_fail_on_batch(
        2,
        ScriptedError::new(ErrorKind::NetworkLost, "connection reset mid-fetch"),
    );
    scenario.on_sql("SELECT * FROM big", Action::query(QuerySource::Fixed(plan)));

    let session = support::open_on(&scenario, path);
    let outcome = session
        .execute(Statement::new("SELECT * FROM big"))
        .wait()
        .expect("execute should succeed");
    let result = outcome.result.expect("query produced a result set");

    let first = session
        .fetch_batch(result, support::n(3))
        .wait()
        .expect("first batch is fine");
    assert_eq!(first.row_count(), 3);
    assert!(!session.is_lost());

    let error = session
        .fetch_batch(result, support::n(3))
        .wait()
        .expect_err("the second batch is scripted to lose the network");
    assert_eq!(error.kind(), ErrorKind::NetworkLost);

    assert!(
        session.is_lost(),
        "a NetworkLost error must move the session to Lost"
    );
    assert_eq!(session.session_state(), SessionLifecycle::Lost);
    assert!(!session.session_state().is_usable());

    let connection_id = session.connection_id();

    // Every later request fails fast with a clear error instead of touching
    // the driver again, and the session is never silently replaced: the
    // connection id never changes and no reconnect is attempted.
    let error = session
        .fetch_batch(result, support::n(3))
        .wait()
        .expect_err("further fetches on the lost session must fail fast");
    assert_eq!(error.session_state(), SessionState::Lost);

    let error = session
        .execute(Statement::new("SELECT 1 FROM dual"))
        .wait()
        .expect_err("new statements on the lost session must fail fast too");
    assert_eq!(error.session_state(), SessionState::Lost);

    let error = session
        .commit()
        .wait()
        .expect_err("commit must also fail fast");
    assert_eq!(error.session_state(), SessionState::Lost);

    assert_eq!(
        session.connection_id(),
        connection_id,
        "db-core must never silently reconnect or replace a lost session"
    );
}

/// `close(Some(Commit))` on a lost session used to return `Ok(())` while
/// committing nothing at all.
///
/// The repro from the review: DML, network loss, `close(None)` says a decision
/// is required, the user picks Commit, `close` reports success — and the table
/// is empty. That is the silent data loss `SPEC.md` §10 exists to prevent, told
/// backwards: the user was assured their work was saved.
fn close_with_commit_on_a_lost_session_reports_the_loss_instead_of_succeeding(
    path: support::ReplyPath,
) {
    let scenario = support::scenario();
    let session = session_with_a_lost_transaction(&scenario, path);

    let error = session
        .close(Some(CloseDisposition::Commit))
        .expect_err("nothing could be committed, so close must not report success");
    let CloseError::Failed(cause) = &error else {
        panic!("expected a failed close, got {error:?}");
    };
    assert_eq!(cause.session_state(), SessionState::Lost);
    let rendered = cause.to_string();
    assert!(
        rendered.contains("nothing was committed"),
        "the error must say the commit did not happen: {rendered}"
    );
    assert!(
        scenario.committed_rows("t").is_empty(),
        "and nothing may actually have been committed"
    );
    // Resources are still released. Closing again is still idempotent — it
    // runs nothing and changes nothing — but it does **not** report success:
    // idempotency answers "this session is already over", never "your commit
    // happened". A second close that said `Ok(())` would hide exactly the loss
    // the first one reported (`SPEC.md` §10, ADR-0002 E7).
    assert_eq!(scenario.counts().connections_closed, 1);
    let again = session
        .close(None)
        .expect_err("a lost session never closes cleanly, however many times it is asked");
    let CloseError::Failed(cause) = &again else {
        panic!("expected a failed close, got {again:?}");
    };
    assert_eq!(
        cause.session_state(),
        SessionState::Lost,
        "and it keeps saying why, with the original classification"
    );
    assert_eq!(scenario.counts().connections_closed, 1, "nothing ran again");
}

/// `close(None)` on a lost session must report the loss, not ask for a decision
/// about a transaction that no longer exists.
fn close_without_a_disposition_on_a_lost_session_reports_the_loss(path: support::ReplyPath) {
    let scenario = support::scenario();
    let session = session_with_a_lost_transaction(&scenario, path);
    assert!(
        session.has_possibly_active_transaction(),
        "core-side tracking still believes a transaction was open"
    );

    let error = session
        .close(None)
        .expect_err("the loss must be reported, not hidden behind a prompt");
    assert!(
        !matches!(error, CloseError::DecisionRequired),
        "asking the user to choose between Commit and Rollback for a transaction the server \
         already rolled back is a question with no true answer"
    );
    assert!(!error.session_is_still_open());
}

/// The fail-fast error a lost session gives every later command must keep the
/// original classification, not flatten it into a sentence.
fn the_terminal_error_keeps_the_kind_and_native_code_of_the_loss(path: support::ReplyPath) {
    let scenario = support::scenario();
    let session = session_with_a_lost_transaction(&scenario, path);

    let error = session
        .execute(Statement::new("SELECT 1 FROM dual"))
        .wait()
        .expect_err("the session is lost");
    assert_eq!(
        error.kind(),
        ErrorKind::NetworkLost,
        "a UI cannot tell the user why the session went away if every later error is `Connection`"
    );
    assert_eq!(error.native().map(NativeError::code), Some(3113));
    assert!(
        error
            .native()
            .is_some_and(|native| native.message().contains("ORA-03113")),
        "the server's own text must survive"
    );
    assert_eq!(error.session_state(), SessionState::Lost);
}

/// A failed revalidation `ping` must fail the command it was validating for,
/// not let it run anyway.
fn a_failed_revalidation_ping_fails_the_command_instead_of_running_it(path: support::ReplyPath) {
    let scenario = support::scenario();
    scenario.on_sql(
        "SELECT slow FROM dual",
        Action::Fail(ScriptedError::new(ErrorKind::Timeout, "deadline elapsed")),
    );
    scenario.on_sql(
        "SELECT 1 FROM dual",
        Action::query(QuerySource::Fixed(QueryPlan::new(
            vec![ColumnSpec::new("N", reldex_db_driver_api::SqlType::Number)],
            vec![vec![ScriptValue::from(1_i64)]],
        ))),
    );

    let session = support::open_on(&scenario, path);
    let error = session
        .execute(Statement::new("SELECT slow FROM dual"))
        .wait()
        .expect_err("scripted timeout");
    assert_eq!(error.session_state(), SessionState::NeedsValidation);
    assert_eq!(session.session_state(), SessionLifecycle::NeedsValidation);

    // The session did not survive validation.
    scenario.fail_ping(ScriptedError::new(ErrorKind::NetworkLost, "gone"));

    let error = session
        .execute(Statement::new("SELECT 1 FROM dual"))
        .wait()
        .expect_err("a command whose revalidation failed must not be executed");
    assert_eq!(error.session_state(), SessionState::Lost);
    assert!(
        session.is_lost(),
        "the failed ping must have moved the session to Lost"
    );
    assert_eq!(
        session.session_state(),
        SessionLifecycle::Lost,
        "and it must stay there"
    );
}

support::both_paths! {
    network_loss_mid_fetch_marks_the_session_lost_and_fails_fast_afterwards,
    close_with_commit_on_a_lost_session_reports_the_loss_instead_of_succeeding,
    close_without_a_disposition_on_a_lost_session_reports_the_loss,
    the_terminal_error_keeps_the_kind_and_native_code_of_the_loss,
    a_failed_revalidation_ping_fails_the_command_instead_of_running_it,
}
