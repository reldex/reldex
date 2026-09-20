//! The connect-time warning channel (contract gap C-6, ADR-0002 amendment).
//!
//! `DatabaseDriver::connect` returns a connection or an error and nothing in
//! between, so a driver that notices something non-fatal while opening a
//! session used to have to refuse the session, stay silent, or smuggle the
//! finding onto the first statement that happened to succeed. The third is what
//! the Oracle driver did for the `SSL_SERVER_DN_MATCH` guard, and it loses the
//! finding entirely for a session that never executes anything.
//!
//! These tests pin the replacement end to end: the core asks the driver exactly
//! once, immediately after `connect`, and the findings reach the session's owner
//! without a statement being run and without leaking into any statement's own
//! warnings.

mod support;

use reldex_db_core::{CloseDisposition, Statement, Warning};
use reldex_db_driver_api::WarningKind;
use reldex_driver_mock::Action;

fn finding(text: &str) -> Warning {
    Warning::new(WarningKind::Informational, text)
}

fn a_connect_time_finding_reaches_the_sessions_owner_without_any_statement_being_run(
    path: support::ReplyPath,
) {
    let scenario = support::scenario();
    scenario.set_connect_warnings(vec![finding(
        "this endpoint sets a parameter this driver does not use",
    )]);

    let session = support::open_on(&scenario, path);

    let warnings = session.connect_warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0].kind(), WarningKind::Informational);
    assert!(
        warnings[0]
            .message()
            .contains("a parameter this driver does not use"),
        "{warnings:?}"
    );

    // The whole point of the channel: nothing was executed, pinged or fetched,
    // and the finding still arrived.
    session.close(None).expect("close");
}

fn a_connect_time_finding_is_reported_once_and_never_on_a_statement(path: support::ReplyPath) {
    const SQL: &str = "SELECT 1 FROM dual";

    let scenario = support::scenario();
    scenario.set_connect_warnings(vec![finding("the descriptor asks for something inert")]);
    scenario.on_sql(
        SQL,
        Action::Dml {
            rows_affected: 1,
            insert: None,
        },
    );

    let session = support::open_on(&scenario, path);
    assert_eq!(session.connect_warnings().len(), 1);

    let outcome = session
        .execute(Statement::new(SQL))
        .wait()
        .expect("execute");
    assert!(
        outcome.warnings.is_empty(),
        "a connect-time finding must not be mixed into a statement's own warnings: {:?}",
        outcome.warnings
    );

    // Reading the session's copy again is not a second report: it is the same
    // fixed list, which is what makes reading it late safe.
    assert_eq!(session.connect_warnings().len(), 1);
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("close");
}

fn a_driver_with_nothing_to_say_reports_no_connect_warnings(path: support::ReplyPath) {
    let scenario = support::scenario();
    let session = support::open_on(&scenario, path);
    assert!(session.connect_warnings().is_empty());
    session.close(None).expect("close");
}

support::both_paths! {
    a_connect_time_finding_reaches_the_sessions_owner_without_any_statement_being_run,
    a_connect_time_finding_is_reported_once_and_never_on_a_statement,
    a_driver_with_nothing_to_say_reports_no_connect_warnings,
}
