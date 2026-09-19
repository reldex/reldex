//! Spike S12b — the automatic `CREATE TRIGGER` rewrite (upstream gap U-18).
//!
//! S12 established that `oracledb` 26.0.0-beta.3 cannot execute a trigger whose
//! body mentions `:NEW` or `:OLD`, because its SQL parser treats any `:name` as
//! a bind placeholder and then demands a value for it, and that wrapping the
//! DDL in `BEGIN EXECUTE IMMEDIATE q'[…]'; END;` works because the parser skips
//! quoted strings. The owner decided (results file §9 item 11, `SPEC.md` §8)
//! that the driver applies that workaround itself, on by default, always
//! reporting it.
//!
//! This file is the evidence for the shipped behaviour, against the live
//! database: the trigger is created **and actually fires**, the statement is
//! still reported as DDL, the warning carries the exact text sent, the off
//! switch restores the explanatory refusal, a body that collides with the
//! obvious quote delimiter still works, a compound trigger works, and a trigger
//! that compiles with errors surfaces the way it would have without the
//! rewrite. It also measures the real PL/SQL string-literal limit the refusal
//! is derived from.
//!
//! ```text
//! tools/oracle-test-db/run-it.ps1 s12b_trigger_rewrite
//! ```

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "spike results are measurements a human reads from the test output"
)]

mod common;

use common::{connect, exec, exec_quietly, observation, params, scalar, try_connect, unique};
use reldex_db_driver_api::{
    DatabaseConnection, ErrorKind, ExtensionValue, Extensions, Statement, StatementKind,
    WarningKind,
};

/// An identifier as the dictionary stores it (unquoted names are folded up).
fn stored(name: &str) -> String {
    name.to_uppercase()
}

/// A connection with the rewrite deliberately switched off.
fn connect_without_the_rewrite() -> Box<dyn DatabaseConnection> {
    let mut extensions = Extensions::new();
    extensions.set(
        reldex_driver_oracle_thin::EXT_REWRITE_TRIGGER_DDL,
        ExtensionValue::Flag(false),
    );
    try_connect(&params().with_extensions(extensions)).expect("the test database accepts us")
}

/// A table with an audit column a trigger can fill in.
fn audited_table(connection: &mut dyn DatabaseConnection, table: &str) {
    exec(
        connection,
        &format!("CREATE TABLE {table} (id NUMBER(9), note VARCHAR2(200))"),
    );
}

#[test]
fn a_trigger_that_mentions_new_is_rewritten_created_and_actually_fires() {
    let mut connection = connect();
    let table = unique("s12b_t");
    let trigger = unique("s12b_g");
    audited_table(connection.as_mut(), &table);

    let ddl = format!(
        "CREATE TRIGGER {trigger} BEFORE INSERT ON {table} FOR EACH ROW \
         BEGIN :NEW.note := NVL(:NEW.note, 'written by the trigger'); END;"
    );
    let outcome = connection
        .execute(&Statement::new(ddl.clone()))
        .expect("the driver rewrites this rather than failing on U-18");

    // Still DDL. The rewrite changes how the statement travels, not what the
    // user submitted or what `db-core` has to do about the transaction.
    assert_eq!(
        outcome.statement_kind(),
        StatementKind::Ddl,
        "a rewritten trigger must not be reported as the PL/SQL block it was sent as"
    );
    assert!(
        outcome.committed_implicitly(),
        "DDL commits on the server whether or not this driver rewrote it"
    );
    assert_eq!(
        outcome.rows_affected(),
        None,
        "the wrapper block reports one row affected; that is an artefact of `EXECUTE \
         IMMEDIATE` and nothing the trigger DDL did"
    );

    let warning = outcome
        .warnings()
        .iter()
        .find(|warning| warning.message().contains("was rewritten"))
        .expect("every rewrite is reported");
    assert_eq!(warning.kind(), WarningKind::Informational);
    assert!(warning.message().contains("U-18"), "{warning:?}");
    assert!(
        warning.message().contains("EXECUTE IMMEDIATE"),
        "{warning:?}"
    );
    assert!(
        warning
            .message()
            .contains(reldex_driver_oracle_thin::EXT_REWRITE_TRIGGER_DDL),
        "the off switch must be named: {warning:?}"
    );
    assert!(
        warning.message().contains(&ddl),
        "the exact text sent must be available for inspection: {warning:?}"
    );
    observation(format!(
        "CREATE TRIGGER with `:NEW` -> kind={:?}, rewritten and reported; warning text is \
         {} characters and carries the statement actually sent",
        outcome.statement_kind(),
        warning.message().chars().count()
    ));
    drop(outcome);

    // The trigger exists and is enabled…
    let owner = scalar(connection.as_mut(), "SELECT USER FROM dual");
    assert_eq!(
        scalar(
            connection.as_mut(),
            &format!(
                "SELECT status FROM all_triggers WHERE owner = '{owner}' AND \
                 trigger_name = '{}'",
                stored(&trigger)
            ),
        ),
        "ENABLED"
    );
    // …and it fires, which is the only proof that matters.
    exec(
        connection.as_mut(),
        &format!("INSERT INTO {table} (id) VALUES (1)"),
    );
    let note = scalar(
        connection.as_mut(),
        &format!("SELECT note FROM {table} WHERE id = 1"),
    );
    assert_eq!(note, "written by the trigger");
    observation(format!(
        "the rewritten trigger fired on INSERT and wrote {note:?}"
    ));
    connection.rollback().expect("rollback");

    exec_quietly(connection.as_mut(), &format!("DROP TRIGGER {trigger}"));
    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn the_off_switch_restores_the_explanatory_refusal_unchanged() {
    let mut connection = connect_without_the_rewrite();
    let table = unique("s12b_off_t");
    let trigger = unique("s12b_off_g");
    audited_table(connection.as_mut(), &table);

    let ddl = format!(
        "CREATE TRIGGER {trigger} BEFORE INSERT ON {table} FOR EACH ROW \
         BEGIN :NEW.note := 'x'; END;"
    );
    let error = connection
        .execute(&Statement::new(ddl))
        .expect_err("with the rewrite off, upstream's bind scan wins");
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert!(error.message().contains(":NEW"), "{error}");
    assert!(error.message().contains("EXECUTE IMMEDIATE"), "{error}");
    connection
        .ping()
        .expect("the refusal must not cost the session");
    observation(format!(
        "with \"{}\" set to false: {error}",
        reldex_driver_oracle_thin::EXT_REWRITE_TRIGGER_DDL
    ));

    // And nothing was created.
    let owner = scalar(connection.as_mut(), "SELECT USER FROM dual");
    assert_eq!(
        scalar(
            connection.as_mut(),
            &format!(
                "SELECT COUNT(*) FROM all_triggers WHERE owner = '{owner}' AND \
                 trigger_name = '{}'",
                stored(&trigger)
            ),
        ),
        "0"
    );

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn a_body_containing_the_obvious_delimiter_forces_another_one_and_still_works() {
    // The body contains `]'`, which is exactly the closing sequence of the
    // first delimiter the driver tries. A rewrite that did not notice would
    // send a statement the server truncates at the wrong place.
    let mut connection = connect();
    let table = unique("s12b_d_t");
    let trigger = unique("s12b_d_g");
    audited_table(connection.as_mut(), &table);

    // The body is valid PL/SQL that *contains* both `]'` and `}'` — the closing
    // sequences of the first two delimiters — because its own literal is
    // written with a third.
    let ddl = format!(
        "CREATE TRIGGER {trigger} BEFORE INSERT ON {table} FOR EACH ROW \
         BEGIN :NEW.note := q'<a ]' and a }}' inside>'; END;"
    );
    assert!(ddl.contains("]'") && ddl.contains("}'"));
    let outcome = connection
        .execute(&Statement::new(ddl))
        .expect("a body carrying the first delimiters' closing sequences still has to work");
    let warning = outcome
        .warnings()
        .iter()
        .find(|warning| warning.message().contains("was rewritten"))
        .expect("still reported");
    for colliding in ["IMMEDIATE q'[", "IMMEDIATE q'{", "IMMEDIATE q'<"] {
        assert!(
            !warning.message().contains(colliding),
            "a delimiter whose closing sequence is in the body was used ({colliding}): \
             {warning:?}"
        );
    }
    observation("a trigger body containing `]'`, `}'` and `>'` was wrapped with a later delimiter");
    drop(outcome);

    exec(
        connection.as_mut(),
        &format!("INSERT INTO {table} (id) VALUES (7)"),
    );
    assert_eq!(
        scalar(
            connection.as_mut(),
            &format!("SELECT note FROM {table} WHERE id = 7"),
        ),
        "a ]' and a }' inside"
    );
    connection.rollback().expect("rollback");

    exec_quietly(connection.as_mut(), &format!("DROP TRIGGER {trigger}"));
    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn a_compound_trigger_is_rewritten_and_compiles() {
    let mut connection = connect();
    let table = unique("s12b_c_t");
    let trigger = unique("s12b_c_g");
    audited_table(connection.as_mut(), &table);

    let ddl = format!(
        "CREATE OR REPLACE TRIGGER {trigger}\n\
         FOR INSERT ON {table}\n\
         COMPOUND TRIGGER\n\
         BEFORE EACH ROW IS BEGIN :NEW.note := 'compound'; END BEFORE EACH ROW;\n\
         END {trigger};"
    );
    let outcome = connection
        .execute(&Statement::new(ddl))
        .expect("a compound trigger is a trigger");
    assert_eq!(outcome.statement_kind(), StatementKind::Ddl);
    assert!(
        outcome
            .warnings()
            .iter()
            .any(|warning| warning.message().contains("was rewritten")),
        "{:?}",
        outcome.warnings()
    );
    drop(outcome);

    exec(
        connection.as_mut(),
        &format!("INSERT INTO {table} (id) VALUES (3)"),
    );
    assert_eq!(
        scalar(
            connection.as_mut(),
            &format!("SELECT note FROM {table} WHERE id = 3"),
        ),
        "compound"
    );
    connection.rollback().expect("rollback");
    observation("a compound trigger with `:NEW` was rewritten, compiled and fired");

    exec_quietly(connection.as_mut(), &format!("DROP TRIGGER {trigger}"));
    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn a_rewritten_trigger_that_does_not_compile_surfaces_as_it_would_have() {
    // Without the rewrite a `CREATE TRIGGER` that compiles with errors
    // **succeeds** and reports a warning; inside `EXECUTE IMMEDIATE` the same
    // statement raises ORA-24344 as a PL/SQL exception. The object is created
    // either way, so the driver turns that exception back into the success and
    // warning the user would otherwise have seen. This test records what the
    // server actually does.
    let mut connection = connect();
    let table = unique("s12b_e_t");
    let trigger = unique("s12b_e_g");
    audited_table(connection.as_mut(), &table);

    let ddl = format!(
        "CREATE TRIGGER {trigger} BEFORE INSERT ON {table} FOR EACH ROW \
         BEGIN :NEW.note := no_such_function_at_all(:NEW.id); END;"
    );
    let outcome = connection
        .execute(&Statement::new(ddl))
        .expect("a trigger that compiles with errors is still created, as it is without U-18");
    assert_eq!(outcome.statement_kind(), StatementKind::Ddl);

    let compile = outcome
        .warnings()
        .iter()
        .find(|warning| warning.kind() == WarningKind::CompiledWithErrors)
        .expect("the compilation failure must reach the user as a warning");
    assert_eq!(
        compile
            .native()
            .map(reldex_db_driver_api::NativeError::code),
        Some(24344),
        "the server's own code is preserved: {compile:?}"
    );
    observation(format!(
        "a trigger with a compilation error, rewritten: kind={:?}, warning kind={:?}, \
         native={:?}, message={}",
        outcome.statement_kind(),
        compile.kind(),
        compile
            .native()
            .map(reldex_db_driver_api::NativeError::code),
        compile.message()
    ));
    assert!(
        outcome
            .warnings()
            .iter()
            .any(|warning| warning.message().contains("was rewritten")),
        "and the rewrite is still reported alongside it: {:?}",
        outcome.warnings()
    );
    drop(outcome);

    // The object exists and is INVALID, which is what "created with errors"
    // means — and is exactly what a direct `CREATE TRIGGER` would have left.
    let owner = scalar(connection.as_mut(), "SELECT USER FROM dual");
    let status = scalar(
        connection.as_mut(),
        &format!(
            "SELECT status FROM all_objects WHERE owner = '{owner}' AND \
             object_name = '{}' AND object_type = 'TRIGGER'",
            stored(&trigger)
        ),
    );
    assert_eq!(status, "INVALID");
    let errors = scalar(
        connection.as_mut(),
        &format!(
            "SELECT COUNT(*) FROM all_errors WHERE owner = '{owner}' AND name = '{}'",
            stored(&trigger)
        ),
    );
    assert_ne!(errors, "0", "USER_ERRORS/ALL_ERRORS holds the diagnosis");
    observation(format!(
        "the trigger exists with status {status} and {errors} rows in ALL_ERRORS, so the \
         rewrite did not hide a created object behind a failure"
    ));

    exec_quietly(connection.as_mut(), &format!("DROP TRIGGER {trigger}"));
    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn a_trigger_without_a_placeholder_is_sent_exactly_as_written() {
    // A rewrite that fires when it is not needed is a silent change to the
    // user's statement, and it moves error positions for no reason.
    let mut connection = connect();
    let table = unique("s12b_p_t");
    let trigger = unique("s12b_p_g");
    audited_table(connection.as_mut(), &table);

    let outcome = connection
        .execute(&Statement::new(format!(
            "CREATE TRIGGER {trigger} BEFORE INSERT ON {table} BEGIN NULL; END;"
        )))
        .expect("a trigger with no placeholder needs no help");
    assert_eq!(outcome.statement_kind(), StatementKind::Ddl);
    assert!(
        !outcome
            .warnings()
            .iter()
            .any(|warning| warning.message().contains("was rewritten")),
        "nothing to work around, so nothing should have been rewritten: {:?}",
        outcome.warnings()
    );
    observation("a statement-level trigger with no `:NEW`/`:OLD` was sent unchanged");
    drop(outcome);

    exec_quietly(connection.as_mut(), &format!("DROP TRIGGER {trigger}"));
    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn the_plsql_string_literal_limit_the_refusal_is_derived_from_is_the_real_one() {
    // The refusal says 32767 bytes. That number comes from PL/SQL's `VARCHAR2`
    // limit rather than from anything this driver controls, so it is measured
    // here rather than asserted from documentation. Probing the literal
    // directly costs one round trip each way; building a 32 KB trigger would
    // cost far more and prove the same thing.
    let mut connection = connect();

    for (bytes, expected_to_work) in [(32767_usize, true), (32768_usize, false)] {
        let filler = "x".repeat(bytes);
        let sql = format!("BEGIN DECLARE s VARCHAR2(32767); BEGIN s := q'[{filler}]'; END; END;");
        let outcome = connection.execute(&Statement::new(sql));
        match (&outcome, expected_to_work) {
            (Ok(_), true) => observation(format!(
                "a {bytes}-byte PL/SQL string literal is accepted, which is the limit the \
                 rewrite refusal uses"
            )),
            (Err(error), false) => observation(format!(
                "a {bytes}-byte PL/SQL string literal is rejected: {error}"
            )),
            (Ok(_), false) => panic!(
                "a {bytes}-byte literal was accepted; the driver's {} limit is too low and \
                 the refusal message is wrong",
                32767
            ),
            (Err(error), true) => panic!(
                "a {bytes}-byte literal was rejected, so the driver's limit is too high: \
                 {error}"
            ),
        }
    }
    connection.close().expect("close");
}
