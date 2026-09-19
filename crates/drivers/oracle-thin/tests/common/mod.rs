//! Shared helpers for the opt-in Oracle integration spikes (ADR-0001 S1–S9).
//!
//! Every test in `tests/` is behind the `oracle-it` feature, so
//! `cargo test --workspace` stays green without a database. Run them through
//! `tools/oracle-test-db/run-it.ps1` (or `.sh`), which loads the untracked
//! `tools/oracle-test-db/.env` into the environment these helpers read.
//!
//! **No credential appears in test source.** Everything comes from
//! `RELDEX_TEST_ORACLE_DSN`, `RELDEX_TEST_ORACLE_USER` and
//! `RELDEX_TEST_ORACLE_PASSWORD`; the privileged extras
//! (`RELDEX_TEST_ORACLE_SYSTEM_USER`, `..._SYSTEM_PASSWORD`) are optional and
//! only spike S4 looks for them.

#![allow(dead_code, reason = "each spike file uses a different subset")]
#![allow(
    unreachable_pub,
    reason = "a shared `mod common;` is private in each test crate that includes it"
)]
#![allow(
    clippy::print_stdout,
    reason = "spike results are measurements a human reads from the test output"
)]

use std::env;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU32, Ordering};

use reldex_db_driver_api::{
    ConnectionParams, Credentials, DatabaseConnection, DatabaseDriver, DbResult, Endpoint,
    ExecutionOutcome, ExtensionValue, Extensions, RowBatch, Secret, Statement, ValueRef,
};
use reldex_driver_oracle_thin::OracleThinDriver;

/// The variable holding the test service's connect string.
pub const DSN: &str = "RELDEX_TEST_ORACLE_DSN";
/// The variable holding the test user name.
pub const USER: &str = "RELDEX_TEST_ORACLE_USER";
/// The variable holding the test user's password.
pub const PASSWORD: &str = "RELDEX_TEST_ORACLE_PASSWORD";
/// Optional: a privileged user for spike S4's `ALTER SYSTEM CANCEL SQL`.
pub const SYSTEM_USER: &str = "RELDEX_TEST_ORACLE_SYSTEM_USER";
/// Optional: that user's password.
pub const SYSTEM_PASSWORD: &str = "RELDEX_TEST_ORACLE_SYSTEM_PASSWORD";

/// Reads a required setting, failing with the variable's name and never its
/// value.
pub fn setting(name: &str) -> String {
    match env::var(name) {
        Ok(value) if !value.is_empty() => value,
        _ => panic!("{name} is not set; run tools/oracle-test-db/run-it.ps1 (or .sh)"),
    }
}

/// Parameters for the ordinary test user.
pub fn params() -> ConnectionParams {
    ConnectionParams::new(
        Endpoint::ConnectString(setting(DSN)),
        Credentials::UserPassword {
            username: setting(USER),
            password: Secret::new(setting(PASSWORD)),
        },
    )
}

/// Parameters for the privileged user, when one is configured.
pub fn system_params() -> Option<ConnectionParams> {
    let username = env::var(SYSTEM_USER).ok().filter(|v| !v.is_empty())?;
    let password = env::var(SYSTEM_PASSWORD).ok().filter(|v| !v.is_empty())?;
    Some(ConnectionParams::new(
        Endpoint::ConnectString(setting(DSN)),
        Credentials::UserPassword {
            username,
            password: Secret::new(password),
        },
    ))
}

/// Opens a connection as the test user.
pub fn connect() -> Box<dyn DatabaseConnection> {
    OracleThinDriver::new()
        .connect(&params())
        .expect("the test database should accept the configured credentials")
}

/// Opens a connection that will decode `TIMESTAMP WITH TIME ZONE` columns.
///
/// The driver refuses them by default because a value carrying a **named
/// region** aborts the process inside `oracledb` (U-3 with U-4) and the two
/// encodings cannot be told apart before the decode. Tests that deliberately
/// exercise the offset-only form — which does work — opt in here, and so
/// document exactly what the switch buys and what it costs.
pub fn connect_decoding_timestamp_with_time_zone() -> Box<dyn DatabaseConnection> {
    let mut extensions = Extensions::new();
    extensions.set(
        reldex_driver_oracle_thin::EXT_ALLOW_TIMESTAMP_WITH_TIME_ZONE,
        ExtensionValue::Flag(true),
    );
    OracleThinDriver::new()
        .connect(&params().with_extensions(extensions))
        .expect("the test database should accept the configured credentials")
}

/// Opens a connection as the test user, reporting the failure instead of
/// panicking.
pub fn try_connect(params: &ConnectionParams) -> DbResult<Box<dyn DatabaseConnection>> {
    OracleThinDriver::new().connect(params)
}

/// A name no concurrently running test can collide with.
///
/// Every spike creates its own objects and drops them again, so a failed run
/// leaves at most a handful of uniquely named leftovers rather than corrupting
/// a shared fixture.
pub fn unique(prefix: &str) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Oracle identifiers are 30 characters in 19c's default compatibility, so
    // keep the prefix short at the call site.
    format!("{prefix}_{:x}_{n}", std::process::id())
}

/// Runs a statement and returns its outcome, with the SQL in the failure.
pub fn exec(connection: &mut dyn DatabaseConnection, sql: &str) -> ExecutionOutcome {
    match connection.execute(&Statement::new(sql)) {
        Ok(outcome) => outcome,
        Err(error) => panic!("{sql}\n  failed: {error}"),
    }
}

/// Runs a statement that is allowed to fail (cleanup).
pub fn exec_quietly(connection: &mut dyn DatabaseConnection, sql: &str) {
    let _ = connection.execute(&Statement::new(sql));
}

/// Runs a query and returns every row in one batch.
pub fn query(connection: &mut dyn DatabaseConnection, sql: &str) -> RowBatch {
    let mut outcome = exec(connection, sql);
    let mut cursor = outcome
        .take_cursor()
        .unwrap_or_else(|| panic!("{sql}\n  returned no cursor"));
    let batch = cursor
        .fetch_batch(NonZeroUsize::new(1000).expect("non-zero"))
        .unwrap_or_else(|error| panic!("{sql}\n  fetch failed: {error}"));
    cursor.close().expect("closing a cursor cannot fail");
    batch
}

/// The single scalar a one-row, one-column query produced, rendered as text.
pub fn scalar(connection: &mut dyn DatabaseConnection, sql: &str) -> String {
    let batch = query(connection, sql);
    assert_eq!(batch.row_count(), 1, "{sql}\n  expected exactly one row");
    render(batch.value(0, 0).unwrap_or(ValueRef::Null))
}

/// A stable text rendering of a cell, for assertions and for the log.
pub fn render(value: ValueRef<'_>) -> String {
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
        ValueRef::Bytes(value) => value.iter().fold(String::new(), |mut text, byte| {
            use std::fmt::Write as _;
            let _ = write!(text, "{byte:02X}");
            text
        }),
        _ => "<unprintable>".to_owned(),
    }
}

/// Prints one measurement in a form the results document can quote verbatim.
pub fn measurement(label: &str, value: impl std::fmt::Display) {
    println!("MEASUREMENT {label} = {value}");
}

/// Prints one observation the results document should record.
pub fn observation(text: impl std::fmt::Display) {
    println!("OBSERVATION {text}");
}
