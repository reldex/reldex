//! Spike S3 — session and transaction semantics (ADR-0001).
//!
//! Kill criterion: auto-commit cannot be turned off, or transaction boundaries
//! are not controllable. `SPEC.md` §10 is built on the guarantee that nothing
//! commits unless the user asked for it.

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "spike results are measurements a human reads from the test output"
)]

mod common;

use common::{connect, exec, exec_quietly, observation, scalar, unique};
use reldex_db_driver_api::{SavepointName, Statement, StatementKind, TransactionState};

#[test]
fn nothing_commits_until_it_is_asked_to() {
    let table = unique("s3_ac");
    let mut writer = connect();
    let mut reader = connect();

    // DDL commits on the server, so the table is visible to the reader at once
    // — which is itself part of what this spike has to establish.
    exec(
        writer.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(5), note VARCHAR2(20))"),
    );

    let outcome = exec(
        writer.as_mut(),
        &format!("INSERT INTO {table} VALUES (1, 'uncommitted')"),
    );
    assert_eq!(outcome.statement_kind(), StatementKind::Dml);
    assert!(
        !outcome.committed_implicitly(),
        "an INSERT must not report an implicit commit"
    );
    assert_eq!(outcome.rows_affected(), Some(1));

    // The writer sees its own uncommitted row; a second session must not.
    assert_eq!(
        scalar(writer.as_mut(), &format!("SELECT COUNT(*) FROM {table}")),
        "1"
    );
    assert_eq!(
        scalar(reader.as_mut(), &format!("SELECT COUNT(*) FROM {table}")),
        "0",
        "auto-commit is ON: another session can already see the row"
    );
    observation("auto-commit is off: an INSERT is invisible to a second session");

    // A second statement in the same transaction still sees the first.
    exec(
        writer.as_mut(),
        &format!("INSERT INTO {table} VALUES (2, 'also uncommitted')"),
    );
    assert_eq!(
        scalar(writer.as_mut(), &format!("SELECT COUNT(*) FROM {table}")),
        "2"
    );
    assert_eq!(
        scalar(reader.as_mut(), &format!("SELECT COUNT(*) FROM {table}")),
        "0"
    );

    writer.commit().expect("commit");
    assert_eq!(
        scalar(reader.as_mut(), &format!("SELECT COUNT(*) FROM {table}")),
        "2",
        "the rows did not become visible after a commit"
    );
    assert_eq!(writer.transaction_state(), TransactionState::Inactive);

    // And a rollback really discards.
    exec(
        writer.as_mut(),
        &format!("INSERT INTO {table} VALUES (3, 'rolled back')"),
    );
    writer.rollback().expect("rollback");
    assert_eq!(writer.transaction_state(), TransactionState::Inactive);
    assert_eq!(
        scalar(writer.as_mut(), &format!("SELECT COUNT(*) FROM {table}")),
        "2"
    );
    observation("commit made the rows visible; rollback discarded the next one");

    exec_quietly(writer.as_mut(), &format!("DROP TABLE {table} PURGE"));
    writer.close().expect("close");
    reader.close().expect("close");
}

#[test]
fn a_savepoint_can_be_rolled_back_to_without_losing_the_transaction() {
    let table = unique("s3_sp");
    let mut connection = connect();
    let mut reader = connect();
    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(5))"),
    );

    exec(
        connection.as_mut(),
        &format!("INSERT INTO {table} VALUES (1)"),
    );
    let name = SavepointName::new("before_two").expect("a plain identifier");
    connection.savepoint(&name).expect("savepoint");
    exec(
        connection.as_mut(),
        &format!("INSERT INTO {table} VALUES (2)"),
    );
    assert_eq!(
        scalar(
            connection.as_mut(),
            &format!("SELECT COUNT(*) FROM {table}")
        ),
        "2"
    );

    connection
        .rollback_to_savepoint(&name)
        .expect("rollback to savepoint");
    assert_eq!(
        scalar(
            connection.as_mut(),
            &format!("SELECT COUNT(*) FROM {table}")
        ),
        "1",
        "rolling back to the savepoint did not undo the second row"
    );
    // The first row is still uncommitted, i.e. the transaction survived.
    assert_eq!(
        scalar(reader.as_mut(), &format!("SELECT COUNT(*) FROM {table}")),
        "0",
        "rolling back to a savepoint ended the whole transaction"
    );
    connection.commit().expect("commit");
    assert_eq!(
        scalar(reader.as_mut(), &format!("SELECT COUNT(*) FROM {table}")),
        "1"
    );
    observation("SAVEPOINT + ROLLBACK TO undid one statement and kept the transaction");

    // A savepoint name is a validated identifier, so injection is impossible.
    assert!(SavepointName::new("a; DROP TABLE x --").is_err());

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
    reader.close().expect("close");
}

#[test]
fn ddl_commits_on_the_server_and_the_driver_says_so() {
    let table = unique("s3_ddl");
    let mut connection = connect();
    let mut reader = connect();
    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(5))"),
    );

    exec(
        connection.as_mut(),
        &format!("INSERT INTO {table} VALUES (1)"),
    );
    assert_eq!(
        scalar(reader.as_mut(), &format!("SELECT COUNT(*) FROM {table}")),
        "0"
    );

    // DDL commits whatever was open before it, on the server, whatever the
    // client asked for. The contract requires that to be reported rather than
    // hidden, because the user's pending work has just been made permanent.
    let outcome = exec(
        connection.as_mut(),
        &format!("CREATE INDEX {table}_ix ON {table} (id)"),
    );
    assert_eq!(outcome.statement_kind(), StatementKind::Ddl);
    assert!(
        outcome.committed_implicitly(),
        "DDL must report that it committed"
    );
    assert_eq!(connection.transaction_state(), TransactionState::Inactive);
    assert_eq!(
        scalar(reader.as_mut(), &format!("SELECT COUNT(*) FROM {table}")),
        "1",
        "the DDL did not commit the pending INSERT after all"
    );
    observation("CREATE INDEX committed the open transaction and reported StatementKind::Ddl");

    // `ALTER SESSION` is *not* DDL and does not commit — which is why the
    // classifier separates them.
    exec(
        connection.as_mut(),
        &format!("INSERT INTO {table} VALUES (2)"),
    );
    let outcome = exec(
        connection.as_mut(),
        "ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY-MM-DD'",
    );
    assert_eq!(outcome.statement_kind(), StatementKind::SessionControl);
    assert!(!outcome.committed_implicitly());
    assert_eq!(
        scalar(reader.as_mut(), &format!("SELECT COUNT(*) FROM {table}")),
        "1",
        "ALTER SESSION committed the open transaction"
    );
    observation("ALTER SESSION is SessionControl and did not commit");
    connection.rollback().expect("rollback");

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
    reader.close().expect("close");
}

#[test]
fn session_state_does_not_leak_between_connections() {
    let mut first = connect();
    let mut second = connect();

    // 1. An NLS setting.
    let before = scalar(
        second.as_mut(),
        "SELECT TO_CHAR(DATE '2026-09-19') FROM dual",
    );
    exec(
        first.as_mut(),
        "ALTER SESSION SET NLS_DATE_FORMAT = '\"leaked-\"YYYY'",
    );
    assert_eq!(
        scalar(
            first.as_mut(),
            "SELECT TO_CHAR(DATE '2026-09-19') FROM dual"
        ),
        "leaked-2026"
    );
    let after = scalar(
        second.as_mut(),
        "SELECT TO_CHAR(DATE '2026-09-19') FROM dual",
    );
    assert_eq!(before, after, "an NLS setting leaked to the other session");
    observation(format!(
        "NLS_DATE_FORMAT changed in one session; the other still renders {after}"
    ));

    // 2. A global temporary table's contents are per-session by definition, so
    //    this checks that the two connections really are two sessions.
    let table = unique("s3_gtt");
    exec(
        first.as_mut(),
        &format!("CREATE GLOBAL TEMPORARY TABLE {table} (id NUMBER(5)) ON COMMIT PRESERVE ROWS"),
    );
    exec(first.as_mut(), &format!("INSERT INTO {table} VALUES (1)"));
    first.commit().expect("commit");
    assert_eq!(
        scalar(first.as_mut(), &format!("SELECT COUNT(*) FROM {table}")),
        "1"
    );
    assert_eq!(
        scalar(second.as_mut(), &format!("SELECT COUNT(*) FROM {table}")),
        "0",
        "temporary table rows were visible in another session"
    );

    // 3. Package state.
    let package = unique("s3_pkg");
    exec(
        first.as_mut(),
        &format!(
            "CREATE OR REPLACE PACKAGE {package} AS \
               counter NUMBER := 0; \
               PROCEDURE bump; \
               FUNCTION counter_value RETURN NUMBER; \
             END;"
        ),
    );
    exec(
        first.as_mut(),
        &format!(
            "CREATE OR REPLACE PACKAGE BODY {package} AS \
               PROCEDURE bump IS BEGIN counter := counter + 1; END; \
               FUNCTION counter_value RETURN NUMBER IS BEGIN RETURN counter; END; \
             END;"
        ),
    );
    exec(first.as_mut(), &format!("BEGIN {package}.bump; END;"));
    exec(first.as_mut(), &format!("BEGIN {package}.bump; END;"));
    assert_eq!(
        scalar(
            first.as_mut(),
            &format!("SELECT {package}.counter_value FROM dual")
        ),
        "2"
    );
    assert_eq!(
        scalar(
            second.as_mut(),
            &format!("SELECT {package}.counter_value FROM dual")
        ),
        "0",
        "package state leaked between sessions"
    );
    observation("NLS settings, temporary-table rows and package state are all per-session");

    exec_quietly(first.as_mut(), &format!("DROP PACKAGE {package}"));
    exec_quietly(first.as_mut(), &format!("DROP TABLE {table} PURGE"));
    first.close().expect("close");
    second.close().expect("close");
}

#[test]
fn closing_a_connection_with_an_open_transaction_rolls_back_rather_than_commits() {
    let table = unique("s3_close");
    let mut setup = connect();
    exec(
        setup.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(5))"),
    );

    let mut writer = connect();
    exec(writer.as_mut(), &format!("INSERT INTO {table} VALUES (1)"));
    assert_eq!(writer.transaction_state(), TransactionState::Unknown);
    writer.close().expect("close over an open transaction");

    assert_eq!(
        scalar(setup.as_mut(), &format!("SELECT COUNT(*) FROM {table}")),
        "0",
        "closing the connection committed the open transaction"
    );
    observation("closing a connection with an open transaction rolled it back");

    exec_quietly(setup.as_mut(), &format!("DROP TABLE {table} PURGE"));
    setup.close().expect("close");
}

#[test]
fn the_reported_transaction_state_is_conservative_but_never_wrong() {
    let table = unique("s3_state");
    let mut connection = connect();
    assert_eq!(
        connection.transaction_state(),
        TransactionState::Inactive,
        "a fresh session has no transaction"
    );

    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(5))"),
    );
    assert_eq!(connection.transaction_state(), TransactionState::Inactive);

    exec(
        connection.as_mut(),
        &format!("INSERT INTO {table} VALUES (1)"),
    );
    assert_eq!(
        connection.transaction_state(),
        TransactionState::Unknown,
        "after DML the driver must not claim to know"
    );

    connection.commit().expect("commit");
    assert_eq!(connection.transaction_state(), TransactionState::Inactive);

    // A plain SELECT is also reported as Unknown, deliberately: `SELECT … FOR
    // UPDATE` opens a transaction and the classifier cannot tell them apart
    // without parsing the whole statement.
    let _ = connection.execute(&Statement::new(format!("SELECT * FROM {table}")));
    assert_eq!(connection.transaction_state(), TransactionState::Unknown);
    observation(
        "transaction state: Inactive after connect/DDL/commit, Unknown after DML or a query",
    );

    connection.rollback().expect("rollback");
    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}
