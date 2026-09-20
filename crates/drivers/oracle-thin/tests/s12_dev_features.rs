//! Spike S12 — the database-specific developer features an Oracle IDE cannot
//! ship without (`phase-0.md`, Workstream E; `SPEC.md` §8 "Operations",
//! §16 "Object browser and metadata").
//!
//! Phase 0 had only incidental evidence here: `USER_ERRORS` was queried inside
//! S5's compile-error test and `V$SESSION` inside S4's privileged-cancel
//! candidate, each for another test's purpose. `EXPLAIN PLAN` and `DBMS_XPLAN`
//! had not been run at all.
//!
//! # Permission failures are not driver failures
//!
//! The skill's rule, and `AGENTS.md`'s: a feature that fails because
//! `RELDEX_TEST` lacks a privilege is a **permission finding**, recorded as
//! such, and never counted against the driver. Every test below that can hit
//! one says which it got.
//!
//! # The `LONG` probes are quarantined
//!
//! `ALL_VIEWS.TEXT`, `ALL_TRIGGERS.TRIGGER_BODY` and
//! `ALL_TAB_COLUMNS.DATA_DEFAULT` are `LONG`, a type an Oracle IDE cannot
//! avoid because the dictionary uses it. A decode this upstream version cannot
//! do aborts the process rather than failing (U-4), which would hide every
//! other result in this file, so those probes are `#[ignore]`d and run one at a
//! time:
//!
//! ```text
//! tools/oracle-test-db/run-it.ps1 s12_dev_features -- --ignored --exact \
//!     a_long_column_from_the_dictionary_is_read_or_refused
//! ```

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "spike results are measurements a human reads from the test output"
)]

mod common;

use std::num::NonZeroUsize;
use std::time::Instant;

use common::{
    connect, exec, exec_quietly, measurement, observation, params, query, render, scalar,
    try_connect, unique,
};
use reldex_db_driver_api::{
    DatabaseConnection, DbError, ErrorKind, ExtensionValue, Extensions, Statement, StatementKind,
    ValueRef,
};

/// Classifies a failure as a permission finding or a driver finding, and says
/// so in the log.
///
/// `ORA-00942` is Oracle's answer for both "there is no such object" and "you
/// may not see it", which is why the distinction has to be drawn here rather
/// than inferred from the kind alone.
fn report_failure(feature: &str, error: &DbError) -> bool {
    let native = error
        .native()
        .map_or(0, reldex_db_driver_api::NativeError::code);
    let permission = matches!(error.kind(), ErrorKind::Permission)
        || matches!(native, 942 | 1031 | 1039 | 6550 | 4043);
    if permission {
        observation(format!(
            "PERMISSION FINDING — {feature} is not available to this test user \
             (ORA-{native:05}); this is a grant question, not a driver defect: {error}"
        ));
    } else {
        observation(format!(
            "DRIVER FINDING — {feature} failed for a non-permission reason: kind={:?} \
             session_state={:?} ORA-{native:05}: {error}",
            error.kind(),
            error.session_state()
        ));
    }
    permission
}

/// The name as the dictionary stores it.
///
/// `unique()` produces lower-case identifiers, and an unquoted identifier is
/// folded to upper case by the server, so every dictionary predicate and every
/// `DBMS_METADATA` call has to ask for the stored form.
fn stored(name: &str) -> String {
    name.to_uppercase()
}

/// A session with the automatic `CREATE TRIGGER` rewrite switched off, so the
/// statements below reach upstream exactly as the spike sent them.
fn connect_without_the_rewrite() -> Box<dyn DatabaseConnection> {
    let mut extensions = Extensions::new();
    extensions.set(
        reldex_driver_oracle_thin::EXT_REWRITE_TRIGGER_DDL,
        ExtensionValue::Flag(false),
    );
    try_connect(&params().with_extensions(extensions)).expect("the test database accepts us")
}

/// Submits DDL through a PL/SQL block, so the Oracle crate's bind-placeholder
/// scan does not see `:NEW` or `:OLD` in the text.
///
/// This is the workaround the driver's own error message names, and — since
/// U-18 — applies for the caller by default; see
/// `a_trigger_body_that_mentions_new_is_refused_with_the_reason_when_the_rewrite_is_off`
/// for the finding it works around.
fn exec_ddl_via_plsql(connection: &mut dyn DatabaseConnection, ddl: &str) {
    exec(
        connection,
        &format!("BEGIN EXECUTE IMMEDIATE q'[{ddl}]'; END;"),
    );
}

/// Every row of a query, rendered, so a whole plan can be logged.
fn rows(connection: &mut dyn DatabaseConnection, sql: &str) -> Vec<String> {
    let batch = query(connection, sql);
    (0..batch.row_count())
        .map(|row| render(batch.value(row, 0).unwrap_or(ValueRef::Null)))
        .collect()
}

// ---------------------------------------------------------------------------
// EXPLAIN PLAN and DBMS_XPLAN
// ---------------------------------------------------------------------------

#[test]
fn explain_plan_is_classified_as_a_non_query_and_fills_the_plan_table() {
    let mut connection = connect();
    let table = unique("s12_plan");
    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(9) PRIMARY KEY, note VARCHAR2(80))"),
    );
    exec(
        connection.as_mut(),
        &format!(
            "INSERT INTO {table} SELECT level, 'row ' || level FROM dual \
             CONNECT BY level <= 500"
        ),
    );
    connection.commit().expect("commit");

    let tag = unique("s12_stmt");
    let outcome = connection.execute(&Statement::new(format!(
        "EXPLAIN PLAN SET STATEMENT_ID = '{tag}' FOR \
         SELECT note FROM {table} WHERE id = 42"
    )));
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            let permission = report_failure("EXPLAIN PLAN (PLAN_TABLE)", &error);
            exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
            assert!(
                permission,
                "EXPLAIN PLAN failed for a reason that is not a missing grant"
            );
            return;
        }
    };

    // An `EXPLAIN PLAN` returns no rows. The classifier has no branch for the
    // keyword, so it lands on `Other` — which routes to the non-query path and
    // reports the transaction as unpredictable. Both are correct: the statement
    // writes rows into PLAN_TABLE, so it does open a transaction.
    assert!(!outcome.has_cursor(), "EXPLAIN PLAN produced a cursor");
    let kind = outcome.statement_kind();
    observation(format!(
        "EXPLAIN PLAN classified as {kind:?} (no cursor, \
         transaction_state_is_unpredictable = {})",
        kind.transaction_state_is_unpredictable()
    ));
    assert_eq!(
        kind,
        StatementKind::Other,
        "an unclassified statement must fall to Other, never to Query"
    );

    let stored = scalar(
        connection.as_mut(),
        &format!("SELECT COUNT(*) FROM PLAN_TABLE WHERE statement_id = '{tag}'"),
    );
    assert_ne!(stored, "0", "EXPLAIN PLAN wrote nothing into PLAN_TABLE");
    observation(format!(
        "PLAN_TABLE is reachable from RELDEX_TEST through the public synonym and holds \
         {stored} row(s) for this statement id"
    ));

    // The part a user actually reads.
    let display = rows(
        connection.as_mut(),
        &format!(
            "SELECT plan_table_output FROM \
             TABLE(DBMS_XPLAN.DISPLAY('PLAN_TABLE', '{tag}', 'ALL'))"
        ),
    );
    assert!(
        display.len() > 3,
        "DBMS_XPLAN.DISPLAY returned only {} line(s)",
        display.len()
    );
    let text = display.join("\n");
    assert!(
        text.contains("Plan hash value") && text.contains("Id"),
        "DBMS_XPLAN.DISPLAY did not produce a plan:\n{text}"
    );
    measurement("s12.explain_plan_display_lines", display.len());
    observation(format!(
        "DBMS_XPLAN.DISPLAY returned {} lines; the access path chosen was {}",
        display.len(),
        if text.contains("INDEX UNIQUE SCAN") {
            "INDEX UNIQUE SCAN"
        } else if text.contains("TABLE ACCESS FULL") {
            "TABLE ACCESS FULL"
        } else {
            "something this test does not name"
        }
    ));

    exec_quietly(
        connection.as_mut(),
        &format!("DELETE FROM PLAN_TABLE WHERE statement_id = '{tag}'"),
    );
    connection.commit().expect("commit the cleanup");
    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn dbms_xplan_display_cursor_reports_the_plan_that_actually_ran() {
    let mut connection = connect();
    let table = unique("s12_cur");
    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(9), note VARCHAR2(80))"),
    );
    exec(
        connection.as_mut(),
        &format!(
            "INSERT INTO {table} SELECT level, 'row ' || level FROM dual \
             CONNECT BY level <= 200"
        ),
    );
    connection.commit().expect("commit");

    // Run something, then ask the server what it just did. `DISPLAY_CURSOR`
    // with no arguments reads `V$SESSION.PREV_SQL_ID` for this session, so it
    // needs `V$SESSION`, `V$SQL` and `V$SQL_PLAN`.
    let counted = scalar(
        connection.as_mut(),
        &format!("SELECT COUNT(*) FROM {table} WHERE note LIKE 'row 1%'"),
    );
    assert_ne!(counted, "0");

    let result = connection.execute(&Statement::new(
        "SELECT plan_table_output FROM TABLE(DBMS_XPLAN.DISPLAY_CURSOR(NULL, NULL, 'ALL'))",
    ));
    match result {
        Err(error) => {
            let permission = report_failure("DBMS_XPLAN.DISPLAY_CURSOR", &error);
            exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
            assert!(
                permission,
                "DISPLAY_CURSOR failed for a reason that is not a missing grant"
            );
            return;
        }
        Ok(mut outcome) => {
            let mut cursor = outcome.take_cursor().expect("cursor");
            let batch = cursor
                .fetch_batch(NonZeroUsize::new(200).expect("non-zero"))
                .expect("fetch the plan");
            cursor.close().expect("close");
            let text: Vec<String> = (0..batch.row_count())
                .map(|row| render(batch.value(row, 0).unwrap_or(ValueRef::Null)))
                .collect();
            let joined = text.join("\n");
            measurement("s12.display_cursor_lines", text.len());
            // `DISPLAY_CURSOR` answers with a diagnostic sentence rather than an
            // error when the privilege is missing; that is a permission finding
            // too, and it must not be read as a passing test.
            if joined.contains("not have privilege") || joined.contains("insufficient") {
                observation(format!(
                    "PERMISSION FINDING — DBMS_XPLAN.DISPLAY_CURSOR ran but reported a \
                     missing privilege rather than a plan: {joined}"
                ));
            } else {
                assert!(
                    joined.contains("SQL_ID") || joined.contains("Plan hash value"),
                    "DISPLAY_CURSOR produced neither a plan nor a privilege \
                     message:\n{joined}"
                );
                observation(format!(
                    "DBMS_XPLAN.DISPLAY_CURSOR returned {} lines of real plan for the \
                     previous statement in this session; RELDEX_TEST's SELECT ANY \
                     DICTIONARY is enough",
                    text.len()
                ));
            }
        }
    }

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

// ---------------------------------------------------------------------------
// V$ and the dictionary
// ---------------------------------------------------------------------------

#[test]
fn dynamic_performance_views_are_queryable_as_a_capability() {
    let mut connection = connect();

    let banner = scalar(
        connection.as_mut(),
        "SELECT banner FROM v$version WHERE ROWNUM = 1",
    );
    observation(format!("v$version -> {banner}"));
    assert!(
        banner.contains("Oracle"),
        "v$version returned something unexpected: {banner}"
    );

    let session = scalar(
        connection.as_mut(),
        "SELECT sid || '/' || serial# || '/' || status || '/' || \
         NVL(client_info, 'no client info') FROM v$session \
         WHERE sid = SYS_CONTEXT('USERENV','SID')",
    );
    observation(format!("v$session, own row -> {session}"));

    // The setting that decides whether a server ever notices a dead client.
    let expire = connection.execute(&Statement::new(
        "SELECT NVL(MAX(value), 'not set') FROM v$parameter WHERE name = 'sqlnet.expire_time'",
    ));
    match expire {
        Ok(_) => {}
        Err(error) => {
            report_failure("v$parameter", &error);
        }
    }

    // Server-side dead connection detection lives in sqlnet.ora, which is not a
    // parameter; what *is* visible is whether anything has configured it.
    let dcd = scalar(
        connection.as_mut(),
        "SELECT COUNT(*) FROM v$session WHERE type = 'USER'",
    );
    measurement("s12.user_sessions_at_test_time", &dcd);
    connection.close().expect("close");
}

#[test]
fn the_metadata_dictionary_answers_every_object_group_the_browser_needs() {
    let mut connection = connect();

    // One fixture of each shape `SPEC.md` §16 lists, so the dictionary has
    // something of this user's own to find.
    let table = unique("s12_meta");
    let view = unique("s12_v");
    let package = unique("s12_pkg");
    let sequence = unique("s12_seq");
    let synonym = unique("s12_syn");
    let trigger = unique("s12_trg");
    exec(
        connection.as_mut(),
        &format!(
            "CREATE TABLE {table} (id NUMBER(9) PRIMARY KEY, \
             note VARCHAR2(80) DEFAULT 'a default value', made DATE DEFAULT SYSDATE)"
        ),
    );
    exec(
        connection.as_mut(),
        &format!("CREATE VIEW {view} AS SELECT id, note FROM {table} WHERE id > 0"),
    );
    exec(
        connection.as_mut(),
        &format!(
            "CREATE PACKAGE {package} AS \
               FUNCTION doubled(n NUMBER) RETURN NUMBER; \
             END {package};"
        ),
    );
    exec(
        connection.as_mut(),
        &format!(
            "CREATE PACKAGE BODY {package} AS \
               FUNCTION doubled(n NUMBER) RETURN NUMBER IS BEGIN RETURN n * 2; END; \
             END {package};"
        ),
    );
    exec(connection.as_mut(), &format!("CREATE SEQUENCE {sequence}"));
    exec(
        connection.as_mut(),
        &format!("CREATE SYNONYM {synonym} FOR {table}"),
    );
    exec_ddl_via_plsql(
        connection.as_mut(),
        &format!(
            "CREATE TRIGGER {trigger} BEFORE INSERT ON {table} FOR EACH ROW \
             BEGIN :NEW.made := SYSDATE; END;"
        ),
    );

    let owner = scalar(connection.as_mut(), "SELECT USER FROM dual");
    let (table, view, package, sequence, synonym, trigger) = (
        stored(&table),
        stored(&view),
        stored(&package),
        stored(&sequence),
        stored(&synonym),
        stored(&trigger),
    );
    let checks: Vec<(&str, String)> = vec![
        (
            "ALL_OBJECTS",
            format!(
                "SELECT COUNT(*) FROM all_objects WHERE owner = '{owner}' \
                 AND object_name IN ('{table}', '{view}', '{package}', '{sequence}', \
                 '{synonym}', '{trigger}')"
            ),
        ),
        (
            "ALL_TABLES",
            format!(
                "SELECT COUNT(*) FROM all_tables WHERE owner = '{owner}' AND table_name = '{table}'"
            ),
        ),
        (
            "ALL_TAB_COLUMNS",
            format!(
                "SELECT COUNT(*) FROM all_tab_columns WHERE owner = '{owner}' \
                 AND table_name = '{table}'"
            ),
        ),
        (
            "ALL_CONSTRAINTS",
            format!(
                "SELECT COUNT(*) FROM all_constraints WHERE owner = '{owner}' \
                 AND table_name = '{table}' AND constraint_type = 'P'"
            ),
        ),
        (
            "ALL_SOURCE",
            format!(
                "SELECT COUNT(*) FROM all_source WHERE owner = '{owner}' \
                 AND name = '{package}'"
            ),
        ),
        (
            "ALL_ERRORS",
            format!("SELECT COUNT(*) FROM all_errors WHERE owner = '{owner}'"),
        ),
        (
            "ALL_DEPENDENCIES",
            format!(
                "SELECT COUNT(*) FROM all_dependencies WHERE owner = '{owner}' \
                 AND name = '{view}'"
            ),
        ),
        (
            "ALL_SYNONYMS",
            format!(
                "SELECT COUNT(*) FROM all_synonyms WHERE owner = '{owner}' AND synonym_name = '{synonym}'"
            ),
        ),
        (
            "ALL_SEQUENCES",
            format!(
                "SELECT COUNT(*) FROM all_sequences WHERE sequence_owner = '{owner}' \
                 AND sequence_name = '{sequence}'"
            ),
        ),
        (
            "ALL_TRIGGERS (no LONG column selected)",
            format!(
                "SELECT COUNT(*) FROM all_triggers WHERE owner = '{owner}' \
                 AND trigger_name = '{trigger}'"
            ),
        ),
    ];

    let mut results = Vec::new();
    for (name, sql) in &checks {
        match connection.execute(&Statement::new(sql.clone())) {
            Ok(mut outcome) => {
                let mut cursor = outcome.take_cursor().expect("cursor");
                let batch = cursor.fetch_batch(NonZeroUsize::MIN).expect("fetch");
                cursor.close().expect("close");
                let count = render(batch.value(0, 0).unwrap_or(ValueRef::Null));
                results.push(format!("{name}={count}"));
            }
            Err(error) => {
                report_failure(name, &error);
                results.push(format!("{name}=FAILED"));
            }
        }
    }
    observation(format!("dictionary views queried: {}", results.join(", ")));
    assert!(
        !results.iter().any(|entry| entry.ends_with("FAILED")),
        "a dictionary view could not be queried: {results:?}"
    );
    // The six fixtures, plus the primary-key index and the package body, all
    // show up in ALL_OBJECTS; the assertion is on the six this test named.
    assert!(
        results[0].ends_with("=6") || results[0].ends_with("=7"),
        "ALL_OBJECTS did not find the six fixture objects: {}",
        results[0]
    );

    exec_quietly(connection.as_mut(), &format!("DROP TRIGGER {trigger}"));
    exec_quietly(connection.as_mut(), &format!("DROP SYNONYM {synonym}"));
    exec_quietly(connection.as_mut(), &format!("DROP SEQUENCE {sequence}"));
    exec_quietly(connection.as_mut(), &format!("DROP PACKAGE {package}"));
    exec_quietly(connection.as_mut(), &format!("DROP VIEW {view}"));
    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn a_trigger_body_that_mentions_new_is_refused_with_the_reason_when_the_rewrite_is_off() {
    // An Oracle IDE has to be able to create a trigger, and a trigger body
    // without `:NEW` or `:OLD` is rare. `oracledb`'s SQL parser scans every
    // statement — DDL included — for `:name` and turns each hit into a bind
    // placeholder, so the plain `CREATE TRIGGER` that SQL*Plus accepts comes
    // back asking for a bind value the caller never wrote.
    //
    // This is what the spike found, and it is still what upstream does. The
    // driver now works around it by rewriting the statement (U-18,
    // `s12b_trigger_rewrite.rs`), so reaching the refusal takes switching that
    // off — which is exactly what this test checks the off switch does.
    let mut connection = connect_without_the_rewrite();
    let table = unique("s12_trg_t");
    let trigger = unique("s12_trg_x");
    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(9), made DATE)"),
    );

    let ddl = format!(
        "CREATE TRIGGER {trigger} BEFORE INSERT ON {table} FOR EACH ROW \
         BEGIN :NEW.made := SYSDATE; END;"
    );
    let error = connection
        .execute(&Statement::new(ddl.clone()))
        .expect_err("upstream parses `:NEW` as a bind, so this cannot succeed today");
    observation(format!(
        "CREATE TRIGGER with `:NEW` in its body -> kind={:?} session_state={:?}: {error}",
        error.kind(),
        error.session_state()
    ));
    assert_eq!(
        error.kind(),
        ErrorKind::Unsupported,
        "the caller supplied no binds, so blaming them for a missing bind value is not an \
         honest report"
    );
    assert!(
        error.message().contains(":NEW"),
        "the message must name the cause: {error}"
    );
    assert!(
        error.message().contains("EXECUTE IMMEDIATE"),
        "the message must name the workaround: {error}"
    );

    // The session is untouched, and the documented workaround does work.
    connection
        .ping()
        .expect("the refusal must not cost the session");
    exec_ddl_via_plsql(connection.as_mut(), &ddl);
    let owner = scalar(connection.as_mut(), "SELECT USER FROM dual");
    let created = scalar(
        connection.as_mut(),
        &format!(
            "SELECT status FROM all_triggers WHERE owner = '{owner}' \
             AND trigger_name = '{}'",
            stored(&trigger)
        ),
    );
    assert_eq!(created, "ENABLED");
    observation(
        "the same DDL inside `BEGIN EXECUTE IMMEDIATE q'[…]'; END;` created the trigger; \
         the parser skips quoted strings, so the workaround is reliable, and it is what the \
         driver now applies automatically unless `oracle.rewrite_trigger_ddl` is false — \
         at the cost of a 32767-byte limit and moved error positions",
    );

    exec_quietly(connection.as_mut(), &format!("DROP TRIGGER {trigger}"));
    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn dbms_metadata_get_ddl_returns_a_readable_clob_for_a_table_and_a_package() {
    let mut connection = connect();
    let table = unique("s12_ddl");
    let package = unique("s12_dpk");
    exec(
        connection.as_mut(),
        &format!(
            "CREATE TABLE {table} (id NUMBER(9) PRIMARY KEY, \
             note VARCHAR2(80) DEFAULT 'x', ต NVARCHAR2(20))"
        ),
    );
    exec(
        connection.as_mut(),
        &format!("CREATE PACKAGE {package} AS PROCEDURE greet(who VARCHAR2); END {package};"),
    );

    for (what, sql) in [
        (
            "TABLE",
            format!(
                "SELECT DBMS_METADATA.GET_DDL('TABLE', '{}') FROM dual",
                stored(&table)
            ),
        ),
        (
            "PACKAGE",
            format!(
                "SELECT DBMS_METADATA.GET_DDL('PACKAGE', '{}') FROM dual",
                stored(&package)
            ),
        ),
    ] {
        let mut outcome = match connection.execute(&Statement::new(sql)) {
            Ok(outcome) => outcome,
            Err(error) => {
                assert!(
                    report_failure(&format!("DBMS_METADATA.GET_DDL({what})"), &error),
                    "GET_DDL failed for a reason that is not a missing grant"
                );
                continue;
            }
        };
        let mut cursor = outcome.take_cursor().expect("cursor");
        let mut batch = cursor.fetch_batch(NonZeroUsize::MIN).expect("fetch");
        cursor.close().expect("close");

        assert!(
            matches!(batch.value(0, 0), Some(ValueRef::Lob(_))),
            "GET_DDL returns a CLOB and must arrive as a locator"
        );
        let mut locator = batch
            .column_mut(0)
            .and_then(|column| column.take_lob(0))
            .expect("a CLOB locator");
        let mut buffer = vec![0_u8; 8192];
        let mut text = Vec::new();
        loop {
            let read = locator.read_chunk(&mut buffer).expect("read the DDL");
            if read == 0 {
                break;
            }
            text.extend_from_slice(&buffer[..read]);
        }
        let ddl = String::from_utf8(text).expect("DDL must be valid UTF-8");
        measurement(&format!("s12.get_ddl_{what}_bytes"), ddl.len());
        assert!(
            ddl.contains("CREATE"),
            "GET_DDL({what}) produced something that is not DDL: {ddl}"
        );
        if what == "TABLE" {
            assert!(
                ddl.contains('ต'),
                "a Thai column name did not survive GET_DDL: {ddl}"
            );
        }
        observation(format!(
            "DBMS_METADATA.GET_DDL('{what}') streamed {} bytes as a CLOB locator{}",
            ddl.len(),
            if what == "TABLE" {
                ", Thai identifier included"
            } else {
                ""
            }
        ));
    }

    exec_quietly(connection.as_mut(), &format!("DROP PACKAGE {package}"));
    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn a_large_dictionary_result_streams_at_a_measurable_rate() {
    // The first data point for `SPEC.md` §19's "metadata browsing with 100,000+
    // objects". No comparative claim is made: this is one machine, one
    // container, one batch size.
    let mut connection = connect();
    let total = scalar(connection.as_mut(), "SELECT COUNT(*) FROM all_objects");
    let batch_size = NonZeroUsize::new(1000).expect("non-zero");

    let statement = Statement::new(
        "SELECT owner, object_name, object_type, status, created \
         FROM all_objects ORDER BY owner, object_name",
    )
    .with_fetch_rows(batch_size);

    let started = Instant::now();
    let mut outcome = connection.execute(&statement).expect("query all_objects");
    let first_batch_at = started.elapsed();
    let mut cursor = outcome.take_cursor().expect("cursor");
    let mut rows = 0_u64;
    let mut batches = 0_u64;
    loop {
        let batch = cursor.fetch_batch(batch_size).expect("fetch a batch");
        if batch.row_count() == 0 {
            break;
        }
        rows += batch.row_count() as u64;
        batches += 1;
    }
    let elapsed = started.elapsed();
    cursor.close().expect("close");

    let per_second = if elapsed.as_secs_f64() > 0.0 {
        rows as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };
    measurement("s12.all_objects_rows", rows);
    measurement("s12.all_objects_batches", batches);
    measurement("s12.all_objects_time", format!("{elapsed:.2?}"));
    measurement(
        "s12.all_objects_rows_per_second",
        format!("{per_second:.0}"),
    );
    measurement(
        "s12.all_objects_time_to_first_batch",
        format!("{first_batch_at:.1?}"),
    );
    observation(format!(
        "ALL_OBJECTS: the server counts {total} rows; {rows} were fetched in {batches} \
         batches of {batch_size} through `fetch_batch` in {elapsed:.2?} \
         ({per_second:.0} rows/s, 5 columns: 3 VARCHAR2, 1 VARCHAR2 status, 1 DATE). \
         Method: one session, `Statement::with_fetch_rows(1000)`, wall clock around \
         execute plus every fetch, batches dropped as they arrive. Single sample on one \
         machine — no comparative claim"
    ));
    assert!(rows > 0, "ALL_OBJECTS returned nothing");
    connection.close().expect("close");
}

// ---------------------------------------------------------------------------
// `LONG` — quarantined, because a decode this driver cannot do aborts (U-4)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "a LONG column may abort the process (U-4); run it alone and record the result"]
fn a_long_column_from_the_dictionary_is_read_or_refused() {
    // `ALL_VIEWS.TEXT`, `ALL_TRIGGERS.TRIGGER_BODY` and
    // `ALL_TAB_COLUMNS.DATA_DEFAULT` are `LONG`. An Oracle IDE cannot avoid
    // them: they hold the view's own SQL, the trigger's body and a column's
    // default expression, and there is no non-LONG source for any of the three
    // in 19c. The question this answers is what the driver does with one —
    // exact text, truncated text, an error, or an abort.
    let mut connection = connect();
    let table = unique("s12_long");
    let view = unique("s12_lv");
    let trigger = unique("s12_lt");
    let default_text = "'a default nobody would guess: ทดสอบ'";
    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(9), note VARCHAR2(80) DEFAULT {default_text})"),
    );
    exec(
        connection.as_mut(),
        &format!(
            "CREATE VIEW {view} AS SELECT id, note FROM {table} \
             WHERE note IS NOT NULL AND id BETWEEN 1 AND 999999"
        ),
    );
    exec_ddl_via_plsql(
        connection.as_mut(),
        &format!(
            "CREATE TRIGGER {trigger} BEFORE INSERT ON {table} FOR EACH ROW \
             BEGIN :NEW.note := NVL(:NEW.note, 'set by the trigger'); END;"
        ),
    );

    let owner = scalar(connection.as_mut(), "SELECT USER FROM dual");
    for (what, sql, expected) in [
        (
            "ALL_VIEWS.TEXT",
            format!(
                "SELECT text FROM all_views WHERE owner = '{owner}' AND view_name = '{}'",
                stored(&view)
            ),
            "BETWEEN 1 AND 999999",
        ),
        (
            "ALL_TRIGGERS.TRIGGER_BODY",
            format!(
                "SELECT trigger_body FROM all_triggers WHERE owner = '{owner}' \
                 AND trigger_name = '{}'",
                stored(&trigger)
            ),
            "set by the trigger",
        ),
        (
            "ALL_TAB_COLUMNS.DATA_DEFAULT",
            format!(
                "SELECT data_default FROM all_tab_columns WHERE owner = '{owner}' \
                 AND table_name = '{}' AND column_name = 'NOTE'",
                stored(&table)
            ),
            "ทดสอบ",
        ),
    ] {
        observation(format!(
            "about to read {what} — if this line is the last output, the \
                            process aborted inside the decode (U-4)"
        ));
        match connection.execute(&Statement::new(sql)) {
            Ok(mut outcome) => {
                let mut cursor = outcome.take_cursor().expect("cursor");
                let column = cursor
                    .columns()
                    .first()
                    .map(|meta| {
                        format!(
                            "{} declared {:?} native {:?}",
                            meta.name(),
                            meta.sql_type(),
                            meta.native_type_name()
                        )
                    })
                    .unwrap_or_default();
                let batch = cursor.fetch_batch(NonZeroUsize::MIN).expect("fetch");
                cursor.close().expect("close");
                let value = render(batch.value(0, 0).unwrap_or(ValueRef::Null));
                measurement(&format!("s12.long_{what}_bytes"), value.len());
                observation(format!(
                    "{what}: {column}; {} bytes read, {}",
                    value.len(),
                    if value.contains(expected) {
                        format!("contains the expected text ({expected:?}) — exact")
                    } else {
                        format!("does NOT contain {expected:?} — truncated or altered: {value}")
                    }
                ));
                assert!(
                    value.contains(expected),
                    "{what} did not come back whole: {value}"
                );
            }
            Err(error) => {
                report_failure(what, &error);
                panic!("{what} could not be read: {error}");
            }
        }
    }

    exec_quietly(connection.as_mut(), &format!("DROP TRIGGER {trigger}"));
    exec_quietly(connection.as_mut(), &format!("DROP VIEW {view}"));
    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
#[ignore = "a LONG column may abort the process (U-4); run it alone and record the result"]
fn a_long_value_longer_than_one_packet_is_read_whole_or_visibly_short() {
    // The question a small LONG cannot answer: does a value that does not fit
    // in one network packet come back whole? A view's text is the case an IDE
    // hits first — "show me the source of this view" — and a silently truncated
    // one is the worst possible answer, because it looks like valid SQL.
    let mut connection = connect();
    let table = unique("s12_big_t");
    let view = unique("s12_big_v");
    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(9))"),
    );
    // ~5 000 predicates: comfortably past 64 KB of view text.
    let predicate = (1..=5_000)
        .map(|n| format!("id <> {n}"))
        .collect::<Vec<_>>()
        .join(" AND ");
    exec(
        connection.as_mut(),
        &format!("CREATE VIEW {view} AS SELECT id FROM {table} WHERE {predicate}"),
    );

    let owner = scalar(connection.as_mut(), "SELECT USER FROM dual");
    // The server's own length, in characters, from a NUMBER column.
    let declared: usize = scalar(
        connection.as_mut(),
        &format!(
            "SELECT text_length FROM all_views WHERE owner = '{owner}' AND view_name = '{}'",
            stored(&view)
        ),
    )
    .parse()
    .expect("TEXT_LENGTH is a number");
    assert!(
        declared > 64 * 1024,
        "the fixture view is only {declared} characters; it has to exceed one packet"
    );

    observation(format!(
        "about to read a {declared}-character LONG — if this line is the last output, the \
         process aborted inside the decode (U-4)"
    ));
    match connection.execute(&Statement::new(format!(
        "SELECT text FROM all_views WHERE owner = '{owner}' AND view_name = '{}'",
        stored(&view)
    ))) {
        Ok(mut outcome) => {
            let mut cursor = outcome.take_cursor().expect("cursor");
            let batch = cursor.fetch_batch(NonZeroUsize::MIN).expect("fetch");
            cursor.close().expect("close");
            let value = render(batch.value(0, 0).unwrap_or(ValueRef::Null));
            let read = value.chars().count();
            measurement("s12.long_view_text_declared_characters", declared);
            measurement("s12.long_view_text_read_characters", read);
            if read == declared {
                observation(format!(
                    "a {declared}-character LONG came back whole ({read} characters), and \
                     it ends where the view does"
                ));
            } else {
                observation(format!(
                    "FINDING: a LONG of {declared} characters was delivered as {read} — \
                     the value is SHORT by {} characters and nothing said so. An IDE \
                     showing this as a view's source would show valid-looking, wrong SQL",
                    declared.saturating_sub(read)
                ));
            }
            assert_eq!(
                read, declared,
                "the LONG value was truncated without an error"
            );
            assert!(
                value.trim_end().ends_with("id <> 5000"),
                "the tail of the view text is missing"
            );
        }
        Err(error) => {
            report_failure("a LONG longer than one packet", &error);
            panic!("a long LONG could not be read: {error}");
        }
    }

    exec_quietly(connection.as_mut(), &format!("DROP VIEW {view}"));
    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
#[ignore = "a LONG RAW column may abort the process (U-4); run it alone and record the result"]
fn a_long_raw_column_is_read_or_refused() {
    let mut connection = connect();
    let table = unique("s12_lraw");
    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(5), payload LONG RAW)"),
    );
    exec(
        connection.as_mut(),
        &format!("INSERT INTO {table} VALUES (1, UTL_RAW.CAST_TO_RAW('00FF107F80DEADBEEF'))"),
    );
    connection.commit().expect("commit");

    observation(
        "about to read a LONG RAW column — if this line is the last output, the process \
         aborted inside the decode (U-4)",
    );
    match connection.execute(&Statement::new(format!(
        "SELECT payload FROM {table} WHERE id = 1"
    ))) {
        Ok(mut outcome) => {
            let mut cursor = outcome.take_cursor().expect("cursor");
            let declared = cursor
                .columns()
                .first()
                .map(|meta| format!("{:?}/{:?}", meta.sql_type(), meta.native_type_name()))
                .unwrap_or_default();
            let batch = cursor.fetch_batch(NonZeroUsize::MIN).expect("fetch");
            cursor.close().expect("close");
            let value = render(batch.value(0, 0).unwrap_or(ValueRef::Null));
            observation(format!(
                "LONG RAW: declared {declared}; read back {} characters of hex: {value}",
                value.len()
            ));
        }
        Err(error) => {
            report_failure("LONG RAW", &error);
        }
    }

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}
