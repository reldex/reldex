//! Physical-device evidence harness for Phase 0 success criterion 7
//! (`docs/exec-plans/active/phase-0.md`), Android half.
//!
//! A sibling of [`reldex-core-poc`](../main.rs) built for the device rather
//! than the desktop: it is `adb push`ed to a physical ARM64 Android phone and
//! run over `adb shell`, so the same Rust stack the desktop uses
//! (`reldex-db-driver-api` → `reldex-db-core` → `reldex-driver-oracle-thin` →
//! `oracledb`, pure Rust, `rustls`/`aws-lc-rs`) is exercised on real hardware
//! against a real database. See
//! `docs/exec-plans/active/phase-0-android-device.md` for the run, the results
//! and — importantly — what this does and does not prove.
//!
//! Unlike `main.rs`, this binary goes through **`reldex-db-core`**: every
//! database call runs on the session's dedicated worker thread (ADR-0002
//! D1/D2), so a green run is evidence for the core's threading model on
//! Android's bionic/`libc` too, not only for the driver.
//!
//! # Usage
//!
//! ```text
//! reldex-device-check                       # every check
//! reldex-device-check ping types            # a subset, in the given order
//! ```
//!
//! Checks: `facts`, `ping`, `types`, `transaction`, `bulk`, `tcps`,
//! `deadline`.
//!
//! Connection settings come from the environment, never from the command line,
//! so a password cannot end up in a shell history or in the device's process
//! list:
//!
//! - `RELDEX_TEST_ORACLE_DSN`, `RELDEX_TEST_ORACLE_USER`,
//!   `RELDEX_TEST_ORACLE_PASSWORD` — as for `reldex-core-poc`.
//! - `RELDEX_TEST_ORACLE_TCPS_DSN`, `RELDEX_TEST_ORACLE_TCPS_CA_DIR` —
//!   optional; the `tcps` check skips and says so when either is absent. The
//!   CA directory holds `ewallet.pem` with the **public** test CA certificate
//!   and no private key.
//! - `RELDEX_DEVICE_CHECK_BULK_ROWS` — optional row count for `bulk`
//!   (default 100 000).
//!
//! `tools/android-device/run-on-device.ps1` builds, pushes, opens the
//! `adb reverse` tunnels, supplies the environment and cleans up.
//!
//! # Scope
//!
//! This is Phase 0 evidence, not product code: it asserts and reports, and it
//! holds no business logic of its own — every database behaviour it checks is
//! `db-core`'s or the driver's. It is deliberately close in shape to the
//! `oracle-it` spike tests (S2 fidelity, S3 session, S4 deadline, S8 TCPS,
//! S14 large result) so a difference between desktop and device is visible as
//! a difference in the same assertion, not in a differently-written one.

// A CLI harness with no UI: stdout *is* the output surface, so the
// workspace-wide `print_stdout` lint would be noise here.
#![allow(
    clippy::print_stdout,
    reason = "this binary's entire purpose is to print results to a terminal"
)]

use std::env;
use std::fmt::Write as _;
use std::num::NonZeroUsize;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reldex_db_core::{
    CloseDisposition, DatabaseSession, DbError, DbResult, ErrorKind, SessionManager, Statement,
    ValueRef,
};
use reldex_db_driver_api::{
    Bind, ConnectionParams, Credentials, Endpoint, ExtensionValue, Extensions, Secret, TlsMode,
};
use reldex_driver_oracle_thin::{EXT_WALLET_DIR, OracleThinDriver};

/// Rows the `bulk` check fetches when `RELDEX_DEVICE_CHECK_BULK_ROWS` is unset.
const DEFAULT_BULK_ROWS: u64 = 100_000;

/// Rows per `fetch_batch` call in the `bulk` check. Bounded on purpose: the
/// point is that a large result never has to fit in memory (`SPEC.md` §13).
const BULK_BATCH_ROWS: usize = 1_000;

/// Thai text with combining marks above and below the base character, matching
/// spike S2's constant so the two assertions are the same assertion.
const THAI: &str = "ทดสอบภาษาไทย";

/// A non-BMP character: one Rust `char`, two UTF-16 code units, four UTF-8
/// bytes — the value that breaks a driver sizing buffers in UCS-2.
const EMOJI: &str = "🐘";

/// A statement the server can interrupt but that will not finish on its own,
/// copied from spike S4 so the deadline check matches what S4 asserts.
const LONG_SQL: &str =
    "SELECT /*+ NO_PARALLEL */ COUNT(*) FROM all_objects a, all_objects b, all_objects c";

/// The deadline the `deadline` check arms. Short on purpose: this is evidence,
/// not a benchmark.
const DEADLINE: Duration = Duration::from_secs(3);

fn main() -> ExitCode {
    let names: Vec<String> = env::args().skip(1).collect();
    let selected: Vec<&str> = if names.is_empty() {
        vec![
            "facts",
            "ping",
            "types",
            "transaction",
            "bulk",
            "tcps",
            "deadline",
        ]
    } else {
        names.iter().map(String::as_str).collect()
    };

    let mut tally = Tally::default();
    for name in selected {
        let outcome = match name {
            "facts" => facts(),
            "ping" => ping(),
            "types" => types(),
            "transaction" => transaction(),
            "bulk" => bulk(),
            "tcps" => tcps(),
            "deadline" => deadline(),
            other => Outcome::Fail(format!("no such check: {other}")),
        };
        tally.record(name, &outcome);
    }

    println!();
    println!(
        "summary: {} passed, {} failed, {} skipped",
        tally.passed, tally.failed, tally.skipped
    );
    if tally.failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// What one check concluded.
enum Outcome {
    /// The check held. The text is the evidence a reader should see.
    Pass(String),
    /// The check did not hold, or could not run because something failed.
    Fail(String),
    /// The check could not run and that is expected; the text says why.
    Skip(String),
}

impl Outcome {
    /// Turns a `DbResult` into a failure whose text keeps the native code.
    fn from(result: DbResult<String>) -> Self {
        match result {
            Ok(detail) => Self::Pass(detail),
            Err(error) => Self::Fail(describe(&error)),
        }
    }
}

#[derive(Default)]
struct Tally {
    passed: usize,
    failed: usize,
    skipped: usize,
}

impl Tally {
    fn record(&mut self, name: &str, outcome: &Outcome) {
        let (tag, detail) = match outcome {
            Outcome::Pass(detail) => {
                self.passed += 1;
                ("PASS", detail)
            }
            Outcome::Fail(detail) => {
                self.failed += 1;
                ("FAIL", detail)
            }
            Outcome::Skip(detail) => {
                self.skipped += 1;
                ("SKIP", detail)
            }
        };
        println!();
        println!("[{tag}] {name}");
        for line in detail.lines() {
            println!("       {line}");
        }
    }
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

/// (g) Proves the process really is this binary, on this device's CPU and OS.
fn facts() -> Outcome {
    let mut detail = String::new();
    let _ = writeln!(
        detail,
        "std::env::consts::OS/ARCH = {}/{}",
        env::consts::OS,
        env::consts::ARCH
    );
    let _ = writeln!(
        detail,
        "pointer width = {} bits, pid = {}",
        usize::BITS,
        std::process::id()
    );
    let _ = writeln!(
        detail,
        "argv[0] = {}",
        env::args().next().unwrap_or_else(|| "<unknown>".to_owned())
    );
    match peak_rss_kib() {
        Some(kib) => {
            let _ = writeln!(detail, "VmHWM at start = {kib} kB");
        }
        None => {
            let _ = writeln!(detail, "VmHWM = unavailable (/proc/self/status unreadable)");
        }
    }

    // The claim this check exists to make. Anything else here is context.
    if env::consts::OS == "android" && env::consts::ARCH == "aarch64" {
        Outcome::Pass(detail)
    } else {
        let _ = write!(detail, "expected android/aarch64");
        Outcome::Fail(detail)
    }
}

/// (a) Connect and ping, through `db-core`'s worker thread.
fn ping() -> Outcome {
    Outcome::from(with_session(&(), |session, _| {
        let started = Instant::now();
        session.ping().wait()?;
        let pinged = started.elapsed();
        let banner = scalar(session, "SELECT banner FROM v$version WHERE ROWNUM = 1")
            .unwrap_or_else(|_| "<v$version not visible to this user>".to_owned());
        Ok(format!(
            "session {} on connection {}, cancel kind {:?}\n\
             ping {pinged:.1?}, lifecycle {}\n\
             server: {banner}",
            session.id(),
            session.connection_id(),
            session.cancel_kind(),
            session.session_state()
        ))
    }))
}

/// (b) Typed data: NUMBER, DATE, TIMESTAMP, and Thai/emoji text through both
/// VARCHAR2 and NVARCHAR2, each checked against the server's own rendering.
fn types() -> Outcome {
    Outcome::from(with_session(&(), |session, _| {
        let row = one_row(
            session,
            "SELECT CAST(12345.6789 AS NUMBER(12,4)),
                    CAST(-170141183460469231731687303715884105727 AS NUMBER),
                    DATE '2026-09-19',
                    TIMESTAMP '2026-09-19 13:45:56.123456',
                    TO_CHAR(TIMESTAMP '2026-09-19 13:45:56.123456',
                            'YYYY-MM-DD\"T\"HH24:MI:SS.FF9')
             FROM dual",
        )?;
        expect(&row, 0, "12345.6789", "NUMBER(12,4)")?;
        expect(
            &row,
            1,
            "-170141183460469231731687303715884105727",
            "a 39-digit NUMBER (no f64 rounding)",
        )?;
        expect(&row, 2, "2026-09-19T00:00:00", "DATE")?;
        expect(&row, 3, "2026-09-19T13:45:56.123456000", "TIMESTAMP(6)")?;
        // The server's own text for the same value, so the driver's decoding is
        // checked against the database rather than against this file.
        expect(&row, 4, "2026-09-19T13:45:56.123456000", "server TO_CHAR")?;

        // Text has to be written and read back: a literal in the SQL text only
        // proves the statement survived the wire in one direction.
        let table = unique("devchk_txt");
        exec(
            session,
            &format!("CREATE TABLE {table} (v VARCHAR2(100 CHAR), nv NVARCHAR2(100))"),
        )?;
        let mixed = format!("{THAI} {EMOJI}");
        let insert = Statement::new(format!("INSERT INTO {table} (v, nv) VALUES (:1, :2)"))
            .with_positional_binds(vec![
                Bind::input(mixed.as_str()),
                Bind::input(mixed.as_str()),
            ]);
        session.execute(insert).wait()?;

        let stored = one_row(
            session,
            &format!("SELECT v, nv, LENGTHB(v), LENGTH(v) FROM {table}"),
        )?;
        expect(&stored, 0, &mixed, "VARCHAR2 round trip")?;
        expect(&stored, 1, &mixed, "NVARCHAR2 round trip")?;
        // AL32UTF8: Thai is 3 bytes per code point, the elephant 4, plus one
        // ASCII space. Asserted so a byte-level corruption that happens to
        // round-trip through the driver's own decoder would still fail.
        let bytes = mixed.len();
        expect(&stored, 2, &bytes.to_string(), "server LENGTHB")?;
        let chars = mixed.chars().count();
        expect(&stored, 3, &chars.to_string(), "server LENGTH (characters)")?;

        drop_table(session, &table);
        Ok(format!(
            "NUMBER, DATE, TIMESTAMP and the server's TO_CHAR all matched exactly\n\
             text round-tripped byte-exact: {mixed:?} ({bytes} bytes, {chars} characters)"
        ))
    }))
}

/// (c) A transaction: insert → rollback → gone; insert → commit → there.
fn transaction() -> Outcome {
    Outcome::from(with_session(&(), |session, _| {
        let table = unique("devchk_tx");
        exec(
            session,
            &format!("CREATE TABLE {table} (id NUMBER(5) PRIMARY KEY, note VARCHAR2(100))"),
        )?;
        // DDL committed implicitly; from here on the transaction is ours.
        let mut detail = String::new();

        exec(
            session,
            &format!("INSERT INTO {table} VALUES (1, 'rolled back')"),
        )?;
        let _ = writeln!(
            detail,
            "after INSERT: has_possibly_active_transaction = {}",
            session.has_possibly_active_transaction()
        );
        session.rollback().wait()?;
        let after_rollback = scalar(session, &format!("SELECT COUNT(*) FROM {table}"))?;
        if after_rollback != "0" {
            drop_table(session, &table);
            return Err(DbError::new(
                ErrorKind::Transaction,
                format!("rollback left {after_rollback} row(s) behind"),
            ));
        }
        let _ = writeln!(detail, "rollback: row is gone (COUNT(*) = 0)");

        exec(
            session,
            &format!("INSERT INTO {table} VALUES (2, 'committed {THAI}')"),
        )?;
        session.commit().wait()?;
        let after_commit = one_row(session, &format!("SELECT COUNT(*), MAX(note) FROM {table}"))?;
        expect(&after_commit, 0, "1", "row count after COMMIT")?;
        expect(
            &after_commit,
            1,
            &format!("committed {THAI}"),
            "committed value",
        )?;
        let _ = writeln!(detail, "commit: the row is there, with its text intact");

        drop_table(session, &table);
        let _ = write!(detail, "table {table} dropped");
        Ok(detail)
    }))
}

/// (d) A moderately large result, fetched in bounded batches.
fn bulk() -> Outcome {
    let rows = env::var("RELDEX_DEVICE_CHECK_BULK_ROWS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_BULK_ROWS);

    Outcome::from(with_session(&rows, |session, rows| {
        let before = peak_rss_kib();
        let batch_rows = NonZeroUsize::new(BULK_BATCH_ROWS).unwrap_or(NonZeroUsize::MIN);
        let statement = Statement::new(format!(
            "SELECT level AS n, TO_CHAR(level) AS t \
             FROM dual CONNECT BY level <= {rows}"
        ))
        .with_fetch_rows(batch_rows);

        let started = Instant::now();
        let outcome = session.execute(statement).wait()?;
        let result = outcome.result.ok_or_else(|| {
            DbError::new(
                ErrorKind::DriverInternal,
                "the query returned no result set",
            )
        })?;

        // Every value is touched, so the timing is a real decode-and-read
        // figure rather than the cost of discarding rows unread.
        let mut seen = 0_u64;
        let mut checksum = 0_u64;
        loop {
            let batch = session.fetch_batch(result, batch_rows).wait()?;
            if batch.is_empty() {
                break;
            }
            for row in 0..batch.row_count() {
                if let Some(ValueRef::Number(value)) = batch.value(row, 0) {
                    checksum = checksum.wrapping_add(value.to_string().len() as u64);
                }
                if let Some(ValueRef::Text(value)) = batch.value(row, 1) {
                    checksum = checksum.wrapping_add(value.len() as u64);
                }
                seen += 1;
            }
        }
        let elapsed = started.elapsed();
        session.close_result(result).wait()?;

        if seen != *rows {
            return Err(DbError::new(
                ErrorKind::DriverInternal,
                format!("asked for {rows} row(s), read {seen}"),
            ));
        }

        let per_second = if elapsed.as_secs_f64() > 0.0 {
            (seen as f64) / elapsed.as_secs_f64()
        } else {
            f64::INFINITY
        };
        let after = peak_rss_kib();
        let memory = match (before, after) {
            (Some(before), Some(after)) => format!(
                "peak RSS (VmHWM) {before} kB before, {after} kB after (+{} kB)",
                after.saturating_sub(before)
            ),
            _ => "peak RSS unavailable".to_owned(),
        };
        Ok(format!(
            "{seen} row(s), 2 column(s), in {elapsed:.1?} = {per_second:.0} rows/s\n\
             fetched {BULK_BATCH_ROWS} rows at a time; checksum {checksum}\n\
             {memory}"
        ))
    }))
}

/// (e) TCPS with certificate and host-name verification on.
fn tcps() -> Outcome {
    let (Some(dsn), Some(wallet)) = (
        optional("RELDEX_TEST_ORACLE_TCPS_DSN"),
        optional("RELDEX_TEST_ORACLE_TCPS_CA_DIR"),
    ) else {
        return Outcome::Skip(
            "RELDEX_TEST_ORACLE_TCPS_DSN or RELDEX_TEST_ORACLE_TCPS_CA_DIR is not set;\n\
             the TLS listener was not configured for this run"
                .to_owned(),
        );
    };

    let mut extensions = Extensions::new();
    extensions.set(EXT_WALLET_DIR, ExtensionValue::Text(wallet.clone()));
    let params = match credentials(Endpoint::ConnectString(dsn.clone())) {
        Ok(params) => params
            .with_tls(TlsMode::Required)
            .with_extensions(extensions),
        Err(error) => return Outcome::Fail(describe(&error)),
    };

    Outcome::from((|| {
        let session = open(params)?;
        let protocol = scalar(
            &session,
            "SELECT sys_context('USERENV', 'NETWORK_PROTOCOL') FROM dual",
        )?;
        // The server's opinion, not the client's configuration: anything less
        // would be the client believing itself.
        if !protocol.eq_ignore_ascii_case("tcps") {
            return Err(DbError::new(
                ErrorKind::Connection,
                format!(
                    "the session reports NETWORK_PROTOCOL = {protocol}, so it is not encrypted"
                ),
            ));
        }
        let service = scalar(
            &session,
            "SELECT sys_context('USERENV', 'SERVICE_NAME') FROM dual",
        )?;
        let row = one_row(
            &session,
            &format!("SELECT COUNT(*), MAX('{THAI}') FROM dual CONNECT BY level <= 5"),
        )?;
        expect(&row, 0, "5", "a query over the encrypted transport")?;
        expect(&row, 1, THAI, "Thai text over the encrypted transport")?;
        let detail = format!(
            "dial {dsn}\n\
             server says USERENV.NETWORK_PROTOCOL = {protocol}, SERVICE_NAME = {service}\n\
             certificate and host-name verification are on and cannot be turned off in this driver\n\
             CA trusted from the pushed wallet directory (public certificate only)"
        );
        close(session);
        Ok(detail)
    })())
}

/// (f) A per-statement deadline, mirroring what spike S4 asserts.
fn deadline() -> Outcome {
    Outcome::from(with_session(&(), |session, _| {
        // **A query's work happens on the fetch.** This driver describes before
        // it fetches (the U-3 mitigation: it asks for zero prefetched rows so a
        // select list it cannot decode safely is refused before any value is
        // decoded), so `execute` returns as soon as the server has described
        // the result and the row source only starts producing when rows are
        // asked for. Stopping at `execute` would time the describe, not the
        // statement — spike S4's `run_to_first_batch` exists for the same
        // reason, and this mirrors it. The deadline stays armed on the
        // connection across both calls.
        let started = Instant::now();
        let error = match session
            .execute(Statement::new(LONG_SQL).with_deadline(DEADLINE))
            .wait()
            .and_then(|outcome| match outcome.result {
                Some(result) => session
                    .fetch_batch(result, NonZeroUsize::MIN)
                    .wait()
                    .map(drop),
                None => Ok(()),
            }) {
            Ok(()) => {
                return Err(DbError::new(
                    ErrorKind::Timeout,
                    "the deadline did not stop the statement",
                ));
            }
            Err(error) => error,
        };
        let elapsed = started.elapsed();
        if elapsed >= Duration::from_secs(60) {
            return Err(DbError::new(
                ErrorKind::Timeout,
                format!(
                    "the statement ran for {elapsed:.1?}, so it was not stopped by the deadline"
                ),
            ));
        }

        // What S4 asserts on the desktop, re-asserted here: the *kind* decides
        // what the caller may believe about the session, and the session must
        // then behave that way. Reporting "recoverable" and not recovering is
        // the failure this catches.
        let lifecycle = session.session_state();
        let probe = scalar(session, "SELECT 1 FROM dual");
        let survived = probe.is_ok();
        let verdict = match error.kind() {
            ErrorKind::Timeout if survived => {
                "Timeout promises a recoverable session, and the session recovered"
            }
            ErrorKind::Timeout => {
                return Err(DbError::new(
                    ErrorKind::Timeout,
                    format!(
                        "the driver reported Timeout, which promises a recoverable session, \
                         but the session did not survive: {}",
                        probe.err().map_or_else(String::new, |e| describe(&e))
                    ),
                ));
            }
            ErrorKind::NetworkLost if !survived => {
                "NetworkLost was reported and the session is honestly gone \
                 (upstream gap U-6, the same outcome S4 records on the desktop)"
            }
            ErrorKind::NetworkLost => {
                return Err(DbError::new(
                    ErrorKind::NetworkLost,
                    "the driver reported NetworkLost but the session still works",
                ));
            }
            other => {
                return Err(DbError::new(
                    other,
                    format!(
                        "a deadline must produce a timeout-class error, not {other:?}: {}",
                        describe(&error)
                    ),
                ));
            }
        };
        Ok(format!(
            "armed {DEADLINE:.1?}, stopped after {elapsed:.1?} \
             (overshoot {:.1?})\n\
             error: {}\n\
             db-core lifecycle after the failure: {lifecycle}\n\
             {verdict}",
            elapsed.saturating_sub(DEADLINE),
            describe(&error)
        ))
    }))
}

// ---------------------------------------------------------------------------
// Session and statement helpers
// ---------------------------------------------------------------------------

/// Opens a plaintext session, runs `body`, then closes the session.
///
/// `context` is passed through so a check can carry a parameter without
/// capturing it by reference in a closure that also borrows the session.
fn with_session<C>(
    context: &C,
    body: impl FnOnce(&DatabaseSession, &C) -> DbResult<String>,
) -> DbResult<String> {
    let params = credentials(Endpoint::ConnectString(required("RELDEX_TEST_ORACLE_DSN")?))?;
    let session = open(params)?;
    let outcome = body(&session, context);
    close(session);
    outcome
}

/// Opens one session through `db-core`, so every call below runs on its worker
/// thread rather than this one.
fn open(params: ConnectionParams) -> DbResult<DatabaseSession> {
    SessionManager::new().open_session(Arc::new(OracleThinDriver::new()), params)
}

/// Closes a session, rolling back rather than committing anything left open
/// (`SPEC.md` §10: nothing commits without being asked).
fn close(session: DatabaseSession) {
    if let Err(error) = session.close(Some(CloseDisposition::Rollback)) {
        println!("       note: closing the session reported {error}");
    }
}

/// Builds parameters for the ordinary test user against `endpoint`.
fn credentials(endpoint: Endpoint) -> DbResult<ConnectionParams> {
    Ok(ConnectionParams::new(
        endpoint,
        Credentials::UserPassword {
            username: required("RELDEX_TEST_ORACLE_USER")?,
            password: Secret::new(required("RELDEX_TEST_ORACLE_PASSWORD")?),
        },
    ))
}

/// Reads a required environment variable, naming it and never its value.
fn required(name: &str) -> DbResult<String> {
    match env::var(name) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => Err(DbError::new(
            ErrorKind::Configuration,
            format!("{name} is not set; run tools/android-device/run-on-device.ps1"),
        )),
    }
}

/// Reads an optional environment variable.
fn optional(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

/// Runs a statement that is not expected to return rows, releasing the result
/// if one appears anyway.
fn exec(session: &DatabaseSession, sql: &str) -> DbResult<()> {
    let outcome = session.execute(Statement::new(sql)).wait()?;
    if let Some(result) = outcome.result {
        session.close_result(result).wait()?;
    }
    Ok(())
}

/// Fetches exactly one row and renders every column.
fn one_row(session: &DatabaseSession, sql: &str) -> DbResult<Vec<String>> {
    let outcome = session.execute(Statement::new(sql)).wait()?;
    let result = outcome.result.ok_or_else(|| {
        DbError::new(
            ErrorKind::DriverInternal,
            "the statement returned no result set",
        )
    })?;
    let batch = session.fetch_batch(result, NonZeroUsize::MIN).wait()?;
    if batch.is_empty() {
        session.close_result(result).wait()?;
        return Err(DbError::new(
            ErrorKind::DriverInternal,
            "the statement returned no rows",
        ));
    }
    let values: Vec<String> = (0..batch.column_count())
        .map(|column| render(batch.value(0, column).unwrap_or(ValueRef::Null)))
        .collect();
    session.close_result(result).wait()?;
    Ok(values)
}

/// The first column of the first row.
fn scalar(session: &DatabaseSession, sql: &str) -> DbResult<String> {
    one_row(session, sql)?.into_iter().next().ok_or_else(|| {
        DbError::new(
            ErrorKind::DriverInternal,
            "the statement returned no columns",
        )
    })
}

/// Asserts one rendered column, failing with what was expected and what came
/// back rather than with a bare "assertion failed".
fn expect(row: &[String], column: usize, wanted: &str, what: &str) -> DbResult<()> {
    let found = row.get(column).map_or("<missing>", String::as_str);
    if found == wanted {
        return Ok(());
    }
    Err(DbError::new(
        ErrorKind::DataConversion,
        format!("{what}: expected {wanted:?}, got {found:?}"),
    ))
}

/// A table name unique to this process and second, so a re-run never collides
/// with a leftover from a run that was interrupted before its cleanup.
fn unique(prefix: &str) -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() % 100_000);
    format!("{prefix}_{}_{stamp}", std::process::id())
}

/// Drops a table, reporting rather than hiding a failure: a leftover table on
/// the owner's database is exactly the kind of thing that must not be silent.
fn drop_table(session: &DatabaseSession, table: &str) {
    if let Err(error) = exec(session, &format!("DROP TABLE {table} PURGE")) {
        println!("       note: dropping {table} failed: {}", describe(&error));
    }
}

/// One line for an error, keeping the native database code.
fn describe(error: &DbError) -> String {
    let mut text = format!("[{:?}] {error}", error.kind());
    if let Some(native) = error.native() {
        let _ = write!(text, " (native {native})");
    }
    let _ = write!(text, " session={:?}", error.session_state());
    text
}

/// Renders one value the way `reldex-core-poc` does, so the two harnesses
/// print the same text for the same cell.
fn render(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Null => "NULL".to_owned(),
        ValueRef::Taken => "<taken>".to_owned(),
        ValueRef::Lob(_) => "<lob>".to_owned(),
        ValueRef::Boolean(value) => value.to_string(),
        ValueRef::Number(value) => value.to_string(),
        ValueRef::Float(value) => value.to_string(),
        ValueRef::Double(value) => value.to_string(),
        ValueRef::Text(value) | ValueRef::Json(value) => value.to_owned(),
        ValueRef::Timestamp(value) => value.to_string(),
        ValueRef::Unsupported(value) => format!("~{value}"),
        ValueRef::Bytes(value) => {
            let mut text = String::with_capacity(value.len() * 2);
            for byte in value {
                let _ = write!(text, "{byte:02X}");
            }
            text
        }
        _ => "<unprintable>".to_owned(),
    }
}

/// Peak resident set size in kibibytes, read from the kernel rather than
/// estimated. `None` where `/proc` does not carry it.
fn peak_rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|value| value.split_whitespace().next()?.parse().ok())
}
