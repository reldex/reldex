//! Phase 0 validation harness (`SPEC.md` §26): proves the database
//! architecture works end to end without the Qt Quick desktop UI
//! (`docs/architecture/ARCHITECTURE.md` §3).
//!
//! # Usage
//!
//! ```text
//! reldex-core-poc ping
//! reldex-core-poc query "SELECT * FROM all_users" [max-rows]
//! reldex-core-poc exec  "BEGIN NULL; END;"
//! ```
//!
//! Connection settings come from the environment, never from the command line,
//! so the password cannot end up in a shell history or a process listing:
//!
//! - `RELDEX_TEST_ORACLE_DSN` — Easy Connect (`host:port/service`) or a full
//!   TNS descriptor. The Phase 0 container answers to a **service name**; the
//!   `//host:port:SID` shorthand does not work against it.
//! - `RELDEX_TEST_ORACLE_USER`
//! - `RELDEX_TEST_ORACLE_PASSWORD`
//!
//! `tools/oracle-test-db/run-it.ps1` (and `.sh`) load these from the untracked
//! `tools/oracle-test-db/.env`.
//!
//! # Dependency rules (`docs/architecture/ARCHITECTURE.md` §2)
//!
//! - Allowed: `reldex-db-driver-api`, and — for this Phase 0 slice only —
//!   `reldex-driver-oracle-thin`, so the binary can act as the composition root
//!   that picks a concrete driver.
//! - Forbidden: Qt/QML/FFI code.
//!
//! **Documented deviation.** The architecture intends this binary to reach the
//! database through `reldex-db-core`, and the earlier skeleton of this file said
//! so. Phase 0 Workstream A deliberately does not depend on `reldex-db-core`
//! yet: its session layer is being built in parallel (Workstream B), and the
//! point of this harness is to exercise the **driver contract** on its own, so a
//! failure here is unambiguously a driver problem. The `db-core` dependency
//! comes back when the session layer lands, and
//! `crates/db-core/tests/dependency_rules.rs` — which forbids `db-core` from
//! depending on a driver, and a driver from depending on `db-core` — is
//! unaffected either way.
//!
//! No `oracledb` type is named anywhere below: everything after the one line
//! that constructs [`OracleThinDriver`] is a `reldex-db-driver-api` contract
//! type, which is the property Phase 0 is here to prove.

// A CLI harness with no UI: stdout *is* the output surface, so the
// workspace-wide `print_stdout` lint would be noise here. Errors go to stderr
// and the exit code, never to stdout.
#![allow(
    clippy::print_stdout,
    reason = "this binary's entire purpose is to print results to a terminal"
)]

use std::env;
use std::fmt::Write as _;
use std::num::NonZeroUsize;
use std::process::ExitCode;
use std::time::Instant;

use reldex_db_driver_api::{
    ColumnMetadata, ConnectionParams, Credentials, DatabaseConnection, DatabaseDriver, DbError,
    DbResult, Endpoint, ErrorKind, LobLocator, Secret, Statement, ValueRef,
};
use reldex_driver_oracle_thin::OracleThinDriver;

/// How many rows `query` prints when the caller does not say.
const DEFAULT_MAX_ROWS: usize = 20;

/// How many rows a batch asks for at a time, so printing stays bounded even for
/// a table far larger than memory.
const BATCH_ROWS: usize = 100;

/// The longest rendering of a single cell before it is elided.
const MAX_CELL_CHARS: usize = 60;

/// How much of a LOB is read for display.
const LOB_PREVIEW_BYTES: usize = 256;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Never stdout: a caller piping `query` output must not find an
            // error mixed into the rows. `DbError`'s own rendering carries the
            // native code and never carries a credential.
            eprintln!("error: {error}");
            if let Some(native) = error.native() {
                eprintln!("  native: {native}");
            }
            if let Some(position) = error.position() {
                eprintln!("  at: {position}");
            }
            eprintln!(
                "  kind: {:?}, session: {:?}",
                error.kind(),
                error.session_state()
            );
            ExitCode::FAILURE
        }
    }
}

fn run() -> DbResult<()> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        return Err(usage_error());
    };

    match command.as_str() {
        "ping" => ping(),
        "query" => {
            let sql = args.next().ok_or_else(usage_error)?;
            let max_rows = match args.next() {
                Some(value) => value.parse::<usize>().map_err(|error| {
                    DbError::new(
                        ErrorKind::Configuration,
                        format!("\"{value}\" is not a row count: {error}"),
                    )
                })?,
                None => DEFAULT_MAX_ROWS,
            };
            query(&sql, max_rows)
        }
        "exec" => {
            let sql = args.next().ok_or_else(usage_error)?;
            exec(&sql)
        }
        _ => Err(usage_error()),
    }
}

fn usage_error() -> DbError {
    DbError::new(
        ErrorKind::Configuration,
        "usage: reldex-core-poc ping | query \"<sql>\" [max-rows] | exec \"<sql>\"\n\
         connection settings come from RELDEX_TEST_ORACLE_DSN, \
         RELDEX_TEST_ORACLE_USER and RELDEX_TEST_ORACLE_PASSWORD",
    )
}

/// Reads one required environment variable.
///
/// The password is read the same way as the rest and is wrapped in
/// [`Secret`] immediately, so it is never a plain `String` this program can
/// accidentally print.
fn required(name: &str) -> DbResult<String> {
    env::var(name).map_err(|_| {
        DbError::new(
            ErrorKind::Configuration,
            format!("{name} is not set; see tools/oracle-test-db/README.md"),
        )
    })
}

fn connect() -> DbResult<Box<dyn DatabaseConnection>> {
    let dsn = required("RELDEX_TEST_ORACLE_DSN")?;
    let user = required("RELDEX_TEST_ORACLE_USER")?;
    let password = Secret::new(required("RELDEX_TEST_ORACLE_PASSWORD")?);

    let params = ConnectionParams::new(
        Endpoint::ConnectString(dsn),
        Credentials::UserPassword {
            username: user,
            password,
        },
    );
    OracleThinDriver::new().connect(&params)
}

fn ping() -> DbResult<()> {
    let driver = OracleThinDriver::new();
    let started = Instant::now();
    let mut connection = connect()?;
    let connected = started.elapsed();

    let started = Instant::now();
    connection.ping()?;
    let pinged = started.elapsed();

    println!("driver:      {}", driver.name());
    println!("connection:  {}", connection.id());
    println!("connect:     {connected:.1?}");
    println!("ping:        {pinged:.1?}");
    println!("cancel:      {:?}", connection.capabilities().cancel());
    println!("transaction: {:?}", connection.transaction_state());
    connection.close()
}

fn query(sql: &str, max_rows: usize) -> DbResult<()> {
    let mut connection = connect()?;
    let batch_rows =
        NonZeroUsize::new(BATCH_ROWS.min(max_rows.max(1))).unwrap_or(NonZeroUsize::MIN);
    let statement = Statement::new(sql).with_fetch_rows(batch_rows);

    let started = Instant::now();
    let mut outcome = connection.execute(&statement)?;
    let executed = started.elapsed();

    report_warnings(&outcome);
    let Some(mut cursor) = outcome.take_cursor() else {
        println!(
            "{:?} returned no rows ({} affected)",
            outcome.statement_kind(),
            outcome.rows_affected().unwrap_or(0)
        );
        return finish(connection, &outcome);
    };

    let columns: Vec<ColumnMetadata> = cursor.columns().to_vec();
    println!("{} column(s):", columns.len());
    for (index, column) in columns.iter().enumerate() {
        println!(
            "  {:>3}  {:<30} {:<28} {}",
            index + 1,
            column.name(),
            format!("{:?}", column.sql_type()),
            describe(column)
        );
    }
    println!("execute: {executed:.1?}");
    println!();

    let mut printed = 0_usize;
    let started = Instant::now();
    while printed < max_rows {
        let wanted =
            NonZeroUsize::new((max_rows - printed).min(BATCH_ROWS)).unwrap_or(NonZeroUsize::MIN);
        let mut batch = cursor.fetch_batch(wanted)?;
        if batch.row_count() == 0 {
            break;
        }
        for row in 0..batch.row_count() {
            let mut line = String::new();
            for column in 0..batch.column_count() {
                if column > 0 {
                    line.push_str(" | ");
                }
                let text = match batch.value(row, column) {
                    Some(value) => render(value),
                    None => "<missing>".to_owned(),
                };
                line.push_str(&text);
            }
            // A LOB is a locator until it is taken; read a preview here so the
            // harness proves the streaming path rather than printing a handle.
            for (column, metadata) in columns.iter().enumerate() {
                let Some(locator) = batch.column_mut(column).and_then(|c| c.take_lob(row)) else {
                    continue;
                };
                let _ = write!(line, "\n      [{}] {}", metadata.name(), preview(locator)?);
            }
            println!("{line}");
            printed += 1;
        }
        if cursor.is_exhausted() {
            break;
        }
    }
    let fetched = started.elapsed();
    cursor.close()?;

    println!();
    println!("{printed} row(s) in {fetched:.1?}");
    finish(connection, &outcome)
}

fn exec(sql: &str) -> DbResult<()> {
    let mut connection = connect()?;
    let started = Instant::now();
    let mut outcome = connection.execute(&Statement::new(sql))?;
    let elapsed = started.elapsed();

    println!("kind:     {:?}", outcome.statement_kind());
    if let Some(rows) = outcome.rows_affected() {
        println!("affected: {rows}");
    }
    if outcome.committed_implicitly() {
        println!("note:     the server committed this statement implicitly");
    }
    report_warnings(&outcome);
    if let Some(cursor) = outcome.take_cursor() {
        // `exec` is for statements that do not return rows; if one does, say so
        // rather than silently discarding the result set.
        println!(
            "note:     this statement returned {} column(s); use `query` to see them",
            cursor.columns().len()
        );
        cursor.close()?;
    }
    println!("elapsed:  {elapsed:.1?}");
    finish(connection, &outcome)
}

/// Closes the connection, rolling back rather than committing anything the
/// statement left open (`SPEC.md` §10: nothing commits without being asked).
fn finish(
    mut connection: Box<dyn DatabaseConnection>,
    outcome: &reldex_db_driver_api::ExecutionOutcome,
) -> DbResult<()> {
    if !outcome.committed_implicitly() {
        connection.rollback()?;
    }
    connection.close()
}

fn report_warnings(outcome: &reldex_db_driver_api::ExecutionOutcome) {
    for warning in outcome.warnings() {
        println!("warning:  [{:?}] {}", warning.kind(), warning.message());
    }
    if outcome.compiled_with_errors() {
        println!("warning:  the object compiled with errors; query USER_ERRORS for the detail");
    }
}

fn describe(column: &ColumnMetadata) -> String {
    let mut text = String::new();
    if let Some(native) = column.native_type_name() {
        text.push_str(native);
    }
    match (column.precision(), column.scale(), column.max_size_bytes()) {
        (Some(precision), Some(scale), _) if precision > 0 => {
            let _ = write!(text, "({precision},{scale})");
        }
        (_, Some(scale), _) if scale != 0 => {
            let _ = write!(text, "({scale})");
        }
        (_, _, Some(size)) => {
            let _ = write!(text, "({size})");
        }
        _ => {}
    }
    if column.nullable() == Some(false) {
        text.push_str(" NOT NULL");
    }
    text
}

fn render(value: ValueRef<'_>) -> String {
    let text = match value {
        ValueRef::Null => return "NULL".to_owned(),
        ValueRef::Taken => return "<taken>".to_owned(),
        ValueRef::Lob(_) => return "<lob>".to_owned(),
        ValueRef::Boolean(value) => value.to_string(),
        ValueRef::Number(value) => value.to_string(),
        ValueRef::Float(value) => value.to_string(),
        ValueRef::Double(value) => value.to_string(),
        ValueRef::Text(value) | ValueRef::Json(value) => value.to_owned(),
        ValueRef::Timestamp(value) => value.to_string(),
        // Rendered distinctly, because it is the driver's text for a type this
        // contract cannot hold — not character data.
        ValueRef::Unsupported(value) => format!("~{value}"),
        ValueRef::Bytes(value) => {
            let mut text = String::with_capacity(value.len() * 2);
            for byte in value.iter().take(MAX_CELL_CHARS / 2) {
                let _ = write!(text, "{byte:02X}");
            }
            text
        }
        _ => "<unprintable>".to_owned(),
    };
    elide(&text)
}

fn elide(text: &str) -> String {
    if text.chars().count() <= MAX_CELL_CHARS {
        return text.to_owned();
    }
    let kept: String = text.chars().take(MAX_CELL_CHARS - 1).collect();
    format!("{kept}…")
}

/// Streams the first chunk of a large object, to prove the bounded read path.
fn preview(locator: LobLocator) -> DbResult<String> {
    let mut locator = locator;
    let kind = locator.kind();
    let size = locator.size_hint();
    let mut buffer = vec![0_u8; LOB_PREVIEW_BYTES];
    let read = locator.read_chunk(&mut buffer)?;
    buffer.truncate(read);

    let body = match String::from_utf8(buffer) {
        Ok(text) => elide(&text),
        Err(error) => format!("{} byte(s) of binary", error.into_bytes().len()),
    };
    Ok(match size {
        Some(size) => format!("{kind:?} ({size} bytes): {body}"),
        None => format!("{kind:?}: {body}"),
    })
}
