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
fn with_the_rewrite_off_a_sqlplus_terminator_silently_creates_an_invalid_trigger() {
    // What the normalisation is worth, measured rather than argued. The off
    // switch means "send my trigger DDL exactly as I wrote it", so this is the
    // server's own answer to a trailing SQL*Plus `/`, and it is worse than a
    // rejection: the statement **succeeds** and leaves a trigger that does not
    // compile. A client that passes the `/` through tells its user the trigger
    // was created, and the user finds out otherwise the next time the table is
    // written to.
    let mut connection = connect_without_the_rewrite();
    let table = unique("s12b_raw_t");
    let trigger = unique("s12b_raw_g");
    audited_table(connection.as_mut(), &table);

    let outcome = connection
        .execute(&Statement::new(format!(
            "CREATE TRIGGER {trigger} BEFORE INSERT ON {table} FOR EACH ROW\n\
             BEGIN NULL; END;\n/"
        )))
        .expect("the server accepts it, which is the whole problem");
    drop(outcome);

    let owner = scalar(connection.as_mut(), "SELECT USER FROM dual");
    let status = scalar(
        connection.as_mut(),
        &format!(
            "SELECT status FROM all_objects WHERE owner = '{owner}' AND \
             object_name = '{}' AND object_type = 'TRIGGER'",
            stored(&trigger)
        ),
    );
    assert_eq!(
        status, "INVALID",
        "if this ever reports VALID, the server started ignoring the terminator and the \
         normalisation in `rewrite.rs` is no longer load-bearing"
    );
    observation(format!(
        "with the rewrite off, `CREATE TRIGGER …\\n/` is accepted and leaves a {status} \
         trigger — the normalisation the switch disables is what prevents that"
    ));

    exec_quietly(connection.as_mut(), &format!("DROP TRIGGER {trigger}"));
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
fn a_sqlplus_terminator_is_understood_whether_or_not_the_trigger_needs_rewriting() {
    // The asymmetry this test exists to prevent: the `/` used to be stripped
    // only on the way into the rewrite, so a trigger mentioning `:NEW` worked
    // and the identical trigger without one failed ORA-00911 — a difference the
    // user cannot see the cause of.
    let mut connection = connect();
    let table = unique("s12b_s_t");
    audited_table(connection.as_mut(), &table);

    let owner = scalar(connection.as_mut(), "SELECT USER FROM dual");
    for (label, body) in [
        ("with a placeholder", "BEGIN :NEW.note := 'slash'; END;"),
        ("without one", "BEGIN NULL; END;"),
    ] {
        let trigger = unique("s12b_s_g");
        let outcome = connection
            .execute(&Statement::new(format!(
                "CREATE TRIGGER {trigger} BEFORE INSERT ON {table} FOR EACH ROW\n{body}\n/"
            )))
            .unwrap_or_else(|error| {
                panic!("a trailing SQL*Plus terminator must be understood {label}: {error}")
            });
        assert_eq!(outcome.statement_kind(), StatementKind::Ddl);
        drop(outcome);
        // Acceptance proves nothing here: the server takes the `/` and creates
        // a trigger that does not compile (see
        // `with_the_rewrite_off_a_sqlplus_terminator_silently_creates_an_invalid_trigger`).
        // Only the status says whether the terminator was understood.
        assert_eq!(
            scalar(
                connection.as_mut(),
                &format!(
                    "SELECT status FROM all_objects WHERE owner = '{owner}' AND \
                     object_name = '{}' AND object_type = 'TRIGGER'",
                    stored(&trigger)
                ),
            ),
            "VALID",
            "the trigger {label} compiled from text that still held its `/`"
        );
        observation(format!(
            "CREATE TRIGGER {label}, submitted with a trailing `/` line: created VALID"
        ));
        exec_quietly(connection.as_mut(), &format!("DROP TRIGGER {trigger}"));
    }

    // And a `/` that is not a terminator line of its own is still part of the
    // statement — stripping that one would change what the trigger computes.
    let trigger = unique("s12b_s_d");
    exec(
        connection.as_mut(),
        &format!(
            "CREATE TRIGGER {trigger} BEFORE INSERT ON {table} FOR EACH ROW\n\
             BEGIN :NEW.note := TO_CHAR(:NEW.id / 2); END;"
        ),
    );
    exec(
        connection.as_mut(),
        &format!("INSERT INTO {table} (id) VALUES (9)"),
    );
    assert_eq!(
        scalar(
            connection.as_mut(),
            &format!("SELECT note FROM {table} WHERE id = 9"),
        ),
        "4.5",
        "the division survived; a `/` inside the statement is not a terminator"
    );
    connection.rollback().expect("rollback");
    exec_quietly(connection.as_mut(), &format!("DROP TRIGGER {trigger}"));

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn a_call_trigger_works_with_and_without_the_terminator_a_user_would_type() {
    // A `CALL` trigger's body is not a PL/SQL block, so the `;` a SQL*Plus user
    // types after it is punctuation the server rejects — inside
    // `EXECUTE IMMEDIATE` and sent directly alike. The `END;` of a PL/SQL body
    // is the opposite: it must survive, which every other test here covers.
    let mut connection = connect();
    let table = unique("s12b_call_t");
    let procedure = unique("s12b_call_p");
    let log = unique("s12b_call_l");
    audited_table(connection.as_mut(), &table);
    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {log} (id NUMBER(9))"),
    );
    exec(
        connection.as_mut(),
        &format!(
            "CREATE PROCEDURE {procedure}(p_id NUMBER) IS BEGIN \
             INSERT INTO {log} (id) VALUES (p_id); END;"
        ),
    );

    for (label, terminator) in [("without a terminator", ""), ("with one", ";")] {
        let trigger = unique("s12b_call_g");
        let outcome = connection
            .execute(&Statement::new(format!(
                "CREATE TRIGGER {trigger} AFTER INSERT ON {table} FOR EACH ROW \
                 CALL {procedure}(:NEW.id){terminator}"
            )))
            .unwrap_or_else(|error| panic!("a CALL trigger {label} must work: {error}"));
        assert_eq!(outcome.statement_kind(), StatementKind::Ddl);
        assert!(
            outcome
                .warnings()
                .iter()
                .any(|warning| warning.message().contains("was rewritten")),
            "a CALL trigger mentions `:NEW`, so it is rewritten like any other: {:?}",
            outcome.warnings()
        );
        drop(outcome);
        observation(format!("a CALL trigger {label} was rewritten and created"));

        // It fires, which is the only proof that the body survived intact.
        exec(
            connection.as_mut(),
            &format!("INSERT INTO {table} (id) VALUES (42)"),
        );
        assert_eq!(
            scalar(
                connection.as_mut(),
                &format!("SELECT COUNT(*) FROM {log} WHERE id = 42"),
            ),
            "1",
            "the CALL trigger {label} did not fire"
        );
        connection.rollback().expect("rollback");
        exec_quietly(connection.as_mut(), &format!("DROP TRIGGER {trigger}"));
    }

    exec_quietly(connection.as_mut(), &format!("DROP PROCEDURE {procedure}"));
    exec_quietly(connection.as_mut(), &format!("DROP TABLE {log} PURGE"));
    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn the_plsql_string_literal_limit_is_on_the_value_not_on_the_source_text() {
    // The refusal says 32767 bytes, and measures the **body** — which is the
    // literal's value on both wrapping paths, but is *not* its source length on
    // either: `q'[…]'` adds four characters, and the doubled-quote fallback adds
    // one for every quote in the body. A 32 KB trigger full of quotes therefore
    // emits a literal whose source text is well over 32767 bytes, and whether
    // that is allowed decides whether the driver must measure the body or the
    // emitted literal.
    //
    // So the question is measured, not assumed: does PL/SQL limit what the
    // literal *is worth* or how long it is *written*? Probing the literal
    // directly costs one round trip each; building 32 KB triggers would cost far
    // more and prove the same thing.
    let mut connection = connect();

    // (label, value bytes, quotes inside the value, write it as a q-string)
    let probes = [
        ("q-string at the limit", 32767_usize, 0_usize, true),
        ("q-string one byte over", 32768, 0, true),
        // The decisive pair. 4000 quotes double to 8000 characters, so the
        // source text is 4000 bytes longer than the value in each case.
        ("doubled quotes at the limit", 32767, 4000, false),
        ("doubled quotes one byte over", 32768, 4000, false),
        // The worst source text the fallback can emit: a body at the limit made
        // almost entirely of quotes doubles to very nearly 64 KB. If a statement
        // that long were rejected for its *length*, measuring the body would
        // still let the driver send something the server refuses for a reason
        // that has nothing to do with the trigger — which is what the refusal
        // exists to prevent.
        ("doubled quotes, worst case source", 32767, 32743, false),
    ];

    for (label, value_bytes, quotes, q_string) in probes {
        let value = format!("{}{}", "x".repeat(value_bytes - quotes), "'".repeat(quotes));
        assert_eq!(value.len(), value_bytes);
        let literal = if q_string {
            format!("q'[{value}]'")
        } else {
            format!("'{}'", value.replace('\'', "''"))
        };
        let source_bytes = literal.len();
        let outcome = connection.execute(&Statement::new(format!(
            "BEGIN DECLARE s VARCHAR2(32767); BEGIN s := {literal}; END; END;"
        )));
        let accepted = outcome.is_ok();
        observation(format!(
            "{label}: value {value_bytes} bytes, literal source {source_bytes} bytes -> {}",
            match &outcome {
                Ok(_) => "accepted".to_owned(),
                Err(error) => format!("rejected: {error}"),
            }
        ));
        assert_eq!(
            accepted,
            value_bytes <= 32767,
            "{label}: the server's answer does not match \"the limit is on the value\"; \
             the driver measures the body, so if the limit is really on the source text \
             (value {value_bytes}, source {source_bytes}) the refusal in `rewrite.rs` is \
             measuring the wrong thing"
        );
    }
    observation(
        "the limit is on the literal's value, not on its source text: a 36 KB source that \
         is worth 32767 bytes is accepted, and a 32768-byte value is rejected however it is \
         written. Measuring the body — which is the value on both wrapping paths — is \
         therefore the right check",
    );
    connection.close().expect("close");
}
