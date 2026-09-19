//! Spike S5 — PL/SQL, bind directions and REF CURSOR (ADR-0001).
//!
//! Kill criterion: PL/SQL cannot be executed with OUT binds, or a REF CURSOR
//! cannot be fetched. `SPEC.md` §11 and §24.14 depend on both.

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "spike results are measurements a human reads from the test output"
)]

mod common;

use std::num::NonZeroUsize;

use common::{connect, exec, exec_quietly, observation, query, render, scalar, unique};
use reldex_db_driver_api::{
    Bind, ErrorKind, NamedBind, OutBindSpec, OutValues, SessionState, SqlType, Statement,
    StatementKind, Timestamp, Value, ValueRef, WarningKind,
};

#[test]
fn an_anonymous_block_runs_and_is_classified_as_plsql() {
    let mut connection = connect();
    let outcome = exec(connection.as_mut(), "BEGIN NULL; END;");
    assert_eq!(outcome.statement_kind(), StatementKind::PlSqlBlock);
    assert!(!outcome.committed_implicitly());
    assert!(!outcome.has_cursor());

    // A `DECLARE` block and a `CALL` are the same family.
    assert_eq!(
        exec(connection.as_mut(), "DECLARE x NUMBER; BEGIN x := 1; END;").statement_kind(),
        StatementKind::PlSqlBlock
    );
    observation("BEGIN and DECLARE blocks both execute and report PlSqlBlock");
    connection.close().expect("close");
}

#[test]
fn out_and_in_out_binds_carry_numbers_text_and_dates_in_both_directions() {
    let mut connection = connect();

    let statement = Statement::new(
        "BEGIN \
           :out_num  := :in_num * 2; \
           :out_text := :in_text || ' back'; \
           :out_date := :in_date + 1; \
           :both     := :both || '!'; \
         END;",
    )
    .with_named_binds(vec![
        NamedBind::new("out_num", Bind::output(SqlType::Number)),
        NamedBind::new("in_num", Bind::input(21_i64)),
        NamedBind::new("out_text", Bind::output(SqlType::VARCHAR)),
        NamedBind::new("in_text", Bind::input("there and")),
        NamedBind::new("out_date", Bind::output(SqlType::Date)),
        NamedBind::new(
            "in_date",
            Bind::input(Timestamp::new(2026, 9, 19, 13, 45, 30).expect("valid")),
        ),
        NamedBind::new(
            "both",
            Bind::InOut {
                value: reldex_db_driver_api::BindValue::Text("in and out".to_owned()),
                spec: OutBindSpec::new(SqlType::VARCHAR),
            },
        ),
    ]);

    let outcome = connection
        .execute(&statement)
        .expect("the block should execute");
    let values = outcome.out_values();
    assert!(!values.is_empty(), "no OUT values came back");

    let number = values.named("out_num").expect("out_num");
    assert!(
        matches!(number, Value::Number(n) if n.to_string() == "42"),
        "{number:?}"
    );
    let text = values.named("out_text").expect("out_text");
    assert!(
        matches!(text, Value::Text(t) if t == "there and back"),
        "{text:?}"
    );
    let date = values.named("out_date").expect("out_date");
    assert!(
        matches!(date, Value::Timestamp(t) if t.to_string().starts_with("2026-09-20")),
        "{date:?}"
    );
    let both = values.named("both").expect("both");
    assert!(
        matches!(both, Value::Text(t) if t == "in and out!"),
        "{both:?}"
    );
    observation("OUT and IN OUT binds carried NUMBER, VARCHAR2 and DATE in both directions");

    connection.close().expect("close");
}

#[test]
fn positional_out_binds_work_too() {
    let mut connection = connect();
    let statement = Statement::new("BEGIN :1 := :2 + 1; END;")
        .with_positional_binds(vec![Bind::output(SqlType::Number), Bind::input(41_i64)]);
    let outcome = connection.execute(&statement).expect("execute");
    match outcome.out_values() {
        OutValues::Positional(values) => {
            let first = values
                .first()
                .and_then(Option::as_ref)
                .expect("the first bind is an output");
            assert!(
                matches!(first, Value::Number(n) if n.to_string() == "42"),
                "{first:?}"
            );
            assert!(
                values.get(1).is_some_and(Option::is_none),
                "an input bind must not produce an output value"
            );
        }
        other => panic!("expected positional out values, got {other:?}"),
    }
    connection.close().expect("close");
}

#[test]
fn an_out_bind_can_be_executed_repeatedly_without_losing_its_value() {
    // Upstream issue #17 reported OUT binds coming back empty on the second and
    // later executions of the same statement. This is the regression check;
    // it executes the same text six times and insists every result is right.
    let mut connection = connect();
    for round in 1..=6_i64 {
        let statement = Statement::new("BEGIN :1 := :2 * 10; END;")
            .with_positional_binds(vec![Bind::output(SqlType::Number), Bind::input(round)]);
        let outcome = connection
            .execute(&statement)
            .unwrap_or_else(|error| panic!("round {round}: {error}"));
        let value = outcome
            .out_values()
            .positional(0)
            .unwrap_or_else(|| panic!("round {round}: no out value"));
        assert!(
            matches!(value, Value::Number(n) if n.to_string() == (round * 10).to_string()),
            "round {round}: got {value:?}"
        );
    }
    observation("six consecutive executions of the same OUT-bind statement all returned correctly");
    connection.close().expect("close");
}

/// Creates the five-row table both REF CURSOR tests read.
fn ref_cursor_table(connection: &mut dyn reldex_db_driver_api::DatabaseConnection) -> String {
    let table = unique("s5_rc");
    exec(
        connection,
        &format!("CREATE TABLE {table} (id NUMBER(5), label VARCHAR2(20))"),
    );
    for id in 1..=5 {
        exec(
            connection,
            &format!("INSERT INTO {table} VALUES ({id}, 'row {id}')"),
        );
    }
    connection.commit().expect("commit");
    table
}

/// The statement that opens a REF CURSOR over `table` through an OUT bind.
fn open_ref_cursor(table: &str) -> Statement {
    Statement::new(format!(
        "BEGIN OPEN :rc FOR SELECT id, label FROM {table} ORDER BY id; END;"
    ))
    .with_named_binds(vec![NamedBind::new("rc", Bind::output(SqlType::Cursor))])
}

fn two() -> NonZeroUsize {
    NonZeroUsize::new(2).expect("non-zero")
}

#[test]
fn a_ref_cursor_out_bind_is_fetched_while_the_parent_connection_stays_usable() {
    let mut connection = connect();
    let table = ref_cursor_table(connection.as_mut());

    let mut outcome = connection
        .execute(&open_ref_cursor(&table))
        .expect("open the ref cursor");

    // Owning the cursor is what contract gap C-1 used to make impossible:
    // `OutValues` exposed only `&Value`, and every useful `Cursor` method needs
    // ownership. `take_named` moves it out and leaves `Value::Taken` behind.
    let value = outcome
        .out_values_mut()
        .take_named("rc")
        .expect("the ref cursor");
    let Value::Cursor(mut cursor) = value else {
        panic!("expected a cursor out value, got {value:?}");
    };
    assert!(
        outcome
            .out_values()
            .named("rc")
            .is_some_and(Value::is_taken),
        "a consumed OUT value must read back as taken, never as NULL"
    );
    assert!(
        outcome.out_values_mut().take_named("rc").is_none(),
        "a live cursor must not be ownable twice"
    );

    // The driver opened it and described it correctly.
    assert_eq!(cursor.columns().len(), 2);
    assert_eq!(cursor.columns()[0].name(), "ID");
    assert_eq!(cursor.columns()[1].name(), "LABEL");
    assert_eq!(cursor.columns()[0].sql_type(), SqlType::Number);
    observation(format!(
        "REF CURSOR OUT bind opened with columns {:?}",
        cursor
            .columns()
            .iter()
            .map(reldex_db_driver_api::ColumnMetadata::name)
            .collect::<Vec<_>>()
    ));

    // Fetch it in batches, and prove between every batch that the parent
    // connection is still usable — the nested cursor and its connection share
    // one session, so a driver that serialised them wrongly would deadlock or
    // corrupt the protocol here rather than later.
    let mut rows = Vec::new();
    let mut batches = 0_usize;
    loop {
        let batch = cursor.fetch_batch(two()).expect("fetch a batch");
        if batch.is_empty() {
            break;
        }
        batches += 1;
        assert!(batch.row_count() <= 2, "max_rows must bound the batch");
        for row in 0..batch.row_count() {
            rows.push((
                render(batch.value(row, 0).unwrap_or(ValueRef::Null)),
                render(batch.value(row, 1).unwrap_or(ValueRef::Null)),
            ));
        }
        assert_eq!(
            scalar(connection.as_mut(), "SELECT 42 FROM dual"),
            "42",
            "the parent connection must stay usable between nested fetches"
        );
        assert!(batches < 10, "fetch never reported exhaustion");
    }

    assert_eq!(
        rows,
        (1..=5)
            .map(|id| (id.to_string(), format!("row {id}")))
            .collect::<Vec<_>>()
    );
    assert_eq!(batches, 3, "5 rows at 2 per batch is 2 + 2 + 1");
    observation(format!(
        "REF CURSOR delivered {} rows in {batches} batches of at most 2, with the \
         parent connection answering a query between every batch",
        rows.len()
    ));

    cursor.close().expect("closing a nested cursor");
    assert_eq!(scalar(connection.as_mut(), "SELECT 1 FROM dual"), "1");
    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn a_ref_cursor_outliving_its_connection_reports_rather_than_panics() {
    // ADR-0002 D2 "Lifecycle of derived handles": after the connection closes,
    // every outstanding cursor reports `DbError` from every operation. A nested
    // cursor is a derived handle like any other.
    let mut connection = connect();
    let table = ref_cursor_table(connection.as_mut());

    let mut outcome = connection
        .execute(&open_ref_cursor(&table))
        .expect("open the ref cursor");
    let Some(Value::Cursor(mut cursor)) = outcome.out_values_mut().take_named("rc") else {
        panic!("expected a cursor out value");
    };
    assert_eq!(
        cursor.fetch_batch(two()).expect("first batch").row_count(),
        2
    );

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");

    let error = cursor
        .fetch_batch(two())
        .expect_err("a cursor whose connection is gone must report, not fetch");
    assert_eq!(error.kind(), ErrorKind::DriverInternal, "{error}");
    assert_eq!(error.session_state(), SessionState::Lost, "{error}");
    observation(format!("nested cursor after connection close: {error}"));
    // `close` is the one call that stays legal after a failed fetch. This
    // driver releases the server-side cursor by dropping the upstream handle,
    // so there is nothing left to do and nothing that can fail — the same
    // answer `a_cursor_outliving_its_connection_reports_rather_than_panics`
    // records for a top-level cursor. What matters is that it does not panic.
    cursor
        .close()
        .expect("closing a dead nested cursor is still fine");
}

#[test]
fn a_stored_procedure_a_function_and_a_package_can_all_be_called() {
    let mut connection = connect();
    let procedure = unique("s5_proc");
    let function = unique("s5_fn");
    let package = unique("s5_pkg");

    exec(
        connection.as_mut(),
        &format!(
            "CREATE OR REPLACE PROCEDURE {procedure}(p_in IN NUMBER, p_out OUT VARCHAR2) AS \
             BEGIN p_out := 'got ' || p_in; END;"
        ),
    );
    exec(
        connection.as_mut(),
        &format!(
            "CREATE OR REPLACE FUNCTION {function}(p_in IN NUMBER) RETURN NUMBER AS \
             BEGIN RETURN p_in * 3; END;"
        ),
    );
    exec(
        connection.as_mut(),
        &format!(
            "CREATE OR REPLACE PACKAGE {package} AS \
               FUNCTION doubled(p IN NUMBER) RETURN NUMBER; \
             END;"
        ),
    );
    exec(
        connection.as_mut(),
        &format!(
            "CREATE OR REPLACE PACKAGE BODY {package} AS \
               FUNCTION doubled(p IN NUMBER) RETURN NUMBER IS BEGIN RETURN p * 2; END; \
             END;"
        ),
    );

    // A procedure, through an anonymous block with an OUT bind.
    let statement = Statement::new(format!("BEGIN {procedure}(:1, :2); END;"))
        .with_positional_binds(vec![Bind::input(5_i64), Bind::output(SqlType::VARCHAR)]);
    let outcome = connection.execute(&statement).expect("call the procedure");
    let value = outcome
        .out_values()
        .positional(1)
        .expect("the OUT parameter");
    assert!(matches!(value, Value::Text(t) if t == "got 5"), "{value:?}");

    // The same procedure through `CALL`, which the classifier also treats as
    // PL/SQL.
    let statement = Statement::new(format!("CALL {procedure}(6, :1)"))
        .with_positional_binds(vec![Bind::output(SqlType::VARCHAR)]);
    let outcome = connection.execute(&statement).expect("CALL the procedure");
    assert_eq!(outcome.statement_kind(), StatementKind::PlSqlBlock);
    let value = outcome
        .out_values()
        .positional(0)
        .expect("the OUT parameter");
    assert!(matches!(value, Value::Text(t) if t == "got 6"), "{value:?}");

    // A function and a packaged function, from SQL.
    assert_eq!(
        scalar(
            connection.as_mut(),
            &format!("SELECT {function}(7) FROM dual")
        ),
        "21"
    );
    assert_eq!(
        scalar(
            connection.as_mut(),
            &format!("SELECT {package}.doubled(8) FROM dual")
        ),
        "16"
    );
    observation("a standalone procedure, a standalone function and a packaged function all worked");

    exec_quietly(connection.as_mut(), &format!("DROP PACKAGE {package}"));
    exec_quietly(connection.as_mut(), &format!("DROP FUNCTION {function}"));
    exec_quietly(connection.as_mut(), &format!("DROP PROCEDURE {procedure}"));
    connection.close().expect("close");
}

#[test]
fn dbms_output_written_by_a_block_can_be_read_back() {
    let mut connection = connect();
    exec(
        connection.as_mut(),
        "BEGIN DBMS_OUTPUT.ENABLE(1000000); END;",
    );
    exec(
        connection.as_mut(),
        "BEGIN \
           DBMS_OUTPUT.PUT_LINE('first line'); \
           DBMS_OUTPUT.PUT_LINE('ทดสอบภาษาไทย'); \
         END;",
    );

    let mut lines = Vec::new();
    loop {
        let statement = Statement::new("BEGIN DBMS_OUTPUT.GET_LINE(:line, :status); END;")
            .with_named_binds(vec![
                NamedBind::new("line", Bind::output(SqlType::VARCHAR)),
                NamedBind::new("status", Bind::output(SqlType::Number)),
            ]);
        let outcome = connection.execute(&statement).expect("GET_LINE");
        let status = outcome.out_values().named("status").expect("status");
        let done = matches!(status, Value::Number(n) if n.to_string() != "0");
        if done {
            break;
        }
        match outcome.out_values().named("line") {
            Some(Value::Text(line)) => lines.push(line.clone()),
            other => panic!("unexpected line value: {other:?}"),
        }
        assert!(lines.len() < 10, "GET_LINE never reported the end");
    }

    assert_eq!(
        lines,
        vec!["first line".to_owned(), "ทดสอบภาษาไทย".to_owned()]
    );
    observation(format!("DBMS_OUTPUT round trip returned {lines:?}"));
    connection.close().expect("close");
}

#[test]
fn a_plsql_compile_error_is_reported_as_a_warning_and_can_be_looked_up() {
    let mut connection = connect();
    let procedure = unique("s5_bad");

    // Creating an object with a body that does not compile *succeeds* on the
    // server and raises a warning, which is exactly the case `SPEC.md` §24.14
    // is about: the user must be told, and told where.
    let outcome = exec(
        connection.as_mut(),
        &format!(
            "CREATE OR REPLACE PROCEDURE {procedure} AS \
             BEGIN this_does_not_exist(); END;"
        ),
    );
    assert_eq!(outcome.statement_kind(), StatementKind::Ddl);
    assert!(
        outcome.compiled_with_errors(),
        "a procedure that does not compile must be reported, not passed over: {:?}",
        outcome.warnings()
    );
    let warning = outcome
        .warnings()
        .first()
        .expect("a warning should have been produced");
    assert_eq!(warning.kind(), WarningKind::CompiledWithErrors);
    observation(format!("compile warning: {}", warning.message()));

    // And the detail is available where Oracle keeps it.
    let errors = query(
        connection.as_mut(),
        &format!(
            "SELECT line, position, text FROM user_errors \
             WHERE name = UPPER('{procedure}') ORDER BY sequence"
        ),
    );
    assert!(errors.row_count() > 0, "USER_ERRORS had nothing to say");
    for row in 0..errors.row_count() {
        observation(format!(
            "USER_ERRORS line {} column {}: {}",
            render(errors.value(row, 0).unwrap_or(ValueRef::Null)),
            render(errors.value(row, 1).unwrap_or(ValueRef::Null)),
            render(errors.value(row, 2).unwrap_or(ValueRef::Null)),
        ));
    }

    exec_quietly(connection.as_mut(), &format!("DROP PROCEDURE {procedure}"));
    connection.close().expect("close");
}

#[test]
fn a_syntax_error_in_an_anonymous_block_carries_its_line_and_column() {
    let mut connection = connect();
    // An anonymous block does not "compile with errors": it fails, with
    // ORA-06550 carrying the position inside the block.
    let error = match connection.execute(&Statement::new(
        "BEGIN\n  no_such_procedure_at_all();\nEND;",
    )) {
        Ok(_) => panic!("this block must not run"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ErrorKind::Syntax, "{error}");
    let position = error
        .position()
        .expect("ORA-06550 carries a line and column, which must survive");
    observation(format!(
        "block syntax error at {position}: {} (native {:?})",
        error.message(),
        error.native().map(reldex_db_driver_api::NativeError::code)
    ));
    assert_eq!(position.line(), Some(2));

    // The session is untouched by a failed parse.
    assert_eq!(scalar(connection.as_mut(), "SELECT 1 FROM dual"), "1");
    connection.close().expect("close");
}

// ---------------------------------------------------------------------------
// DML RETURNING and output-slot alignment (review should-fix)
// ---------------------------------------------------------------------------

#[test]
fn a_single_row_dml_returning_gives_back_its_values() {
    // The values do not arrive through `ExecResult::out_bind_data` — that is
    // populated only for PL/SQL — but through `returned_data`, and reading the
    // wrong one failed with "invalid column index 0", a message about nothing
    // the caller had done.
    let mut connection = connect();
    let table = unique("s5_ret");
    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER, label VARCHAR2(40))"),
    );

    let insert = Statement::new(format!(
        "INSERT INTO {table} (id, label) VALUES (:id, :label) \
         RETURNING id, label INTO :out_id, :out_label"
    ))
    .with_named_binds(vec![
        NamedBind::new("id", Bind::input(7_i64)),
        NamedBind::new("label", Bind::input("ทดสอบ")),
        NamedBind::new("out_id", Bind::output(SqlType::Number)),
        NamedBind::new("out_label", Bind::output(SqlType::VARCHAR)),
    ]);
    let outcome = connection.execute(&insert).expect("insert … returning");
    assert_eq!(outcome.statement_kind(), StatementKind::Dml);
    assert_eq!(outcome.rows_affected(), Some(1));
    let values = outcome.out_values();
    let id = values.named("out_id").expect("out_id came back");
    assert!(
        matches!(id, Value::Number(n) if n.to_string() == "7"),
        "{id:?}"
    );
    let label = values.named("out_label").expect("out_label came back");
    assert!(matches!(label, Value::Text(t) if t == "ทดสอบ"), "{label:?}");
    observation("INSERT … RETURNING INTO returned both values, Thai text included");

    // An UPDATE that matches nothing returns no row, and the outputs are NULL
    // rather than an error about a missing column.
    let update = Statement::new(format!(
        "UPDATE {table} SET label = :label WHERE id = :id RETURNING label INTO :out_label"
    ))
    .with_named_binds(vec![
        NamedBind::new("label", Bind::input("unused")),
        NamedBind::new("id", Bind::input(999_i64)),
        NamedBind::new("out_label", Bind::output(SqlType::VARCHAR)),
    ]);
    let outcome = connection.execute(&update).expect("update … returning");
    assert_eq!(outcome.rows_affected(), Some(0));
    assert!(
        matches!(outcome.out_values().named("out_label"), Some(Value::Null)),
        "a RETURNING that matched no row must report NULL, not a stale value"
    );
    observation("a RETURNING that matched no row reported NULL outputs");

    // More than one returned row cannot be expressed as one value per bind, so
    // it is refused rather than silently truncated to the first.
    exec(
        connection.as_mut(),
        &format!("INSERT INTO {table} (id, label) VALUES (8, 'second')"),
    );
    let many = Statement::new(format!(
        "UPDATE {table} SET label = label RETURNING id INTO :out_id"
    ))
    .with_named_binds(vec![NamedBind::new(
        "out_id",
        Bind::output(SqlType::Number),
    )]);
    match connection.execute(&many) {
        Ok(outcome) => panic!(
            "a multi-row RETURNING must be refused, not truncated: {:?}",
            outcome.out_values()
        ),
        Err(error) => {
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
            observation(format!("multi-row RETURNING -> {error}"));
        }
    }

    connection.rollback().expect("rollback");
    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn an_in_bind_the_server_calls_in_out_fails_loudly_instead_of_shifting_slots() {
    // The wrapper numbers output slots from what the caller declared; the row
    // it indexes comes from the server's describe. Declaring an `IN OUT`
    // parameter as `In` makes the server send one more output than the caller
    // expects, and with matching types every later slot would quietly return
    // its neighbour's value.
    let mut connection = connect();
    let procedure = unique("s5_shift");
    exec(
        connection.as_mut(),
        &format!(
            "CREATE OR REPLACE PROCEDURE {procedure} \
               (a IN OUT VARCHAR2, b OUT VARCHAR2) AS \
             BEGIN a := 'first'; b := 'second'; END;"
        ),
    );

    // Declared honestly: both come back, in the right places.
    let honest = Statement::new(format!("BEGIN {procedure}(:a, :b); END;")).with_named_binds(vec![
        NamedBind::new(
            "a",
            Bind::InOut {
                value: reldex_db_driver_api::BindValue::Text("in".to_owned()),
                spec: OutBindSpec::new(SqlType::VARCHAR),
            },
        ),
        NamedBind::new("b", Bind::output(SqlType::VARCHAR)),
    ]);
    let outcome = connection.execute(&honest).expect("declared correctly");
    let values = outcome.out_values();
    assert!(
        matches!(values.named("a"), Some(Value::Text(t)) if t == "first"),
        "{:?}",
        values.named("a")
    );
    assert!(
        matches!(values.named("b"), Some(Value::Text(t)) if t == "second"),
        "{:?}",
        values.named("b")
    );

    // Declared as a plain input, which it is not.
    let wrong = Statement::new(format!("BEGIN {procedure}(:a, :b); END;")).with_named_binds(vec![
        NamedBind::new("a", Bind::input("in")),
        NamedBind::new("b", Bind::output(SqlType::VARCHAR)),
    ]);
    match connection.execute(&wrong) {
        Ok(outcome) => panic!(
            "the driver must not read output slots it cannot align: {:?}",
            outcome.out_values()
        ),
        Err(error) => {
            assert_eq!(error.kind(), ErrorKind::DataConversion, "{error}");
            assert!(error.message().contains("IN OUT"), "{error}");
            observation(format!("mis-declared IN OUT -> {error}"));
        }
    }
    // The session is unharmed by the refusal.
    assert_eq!(scalar(connection.as_mut(), "SELECT 1 FROM dual"), "1");

    exec_quietly(connection.as_mut(), &format!("DROP PROCEDURE {procedure}"));
    connection.close().expect("close");
}
