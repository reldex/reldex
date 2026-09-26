//! The one file in this crate that knows a concrete driver exists.
//!
//! ADR-0003 keeps the boundary vendor-neutral, so which driver a session runs
//! against is a build-time choice: `mock-driver` (the default, and what spike
//! S15, the C smoke harness and every test here use) selects
//! `reldex-driver-mock`; a later `oracle-driver` feature will select
//! `reldex-driver-oracle-thin` and add one arm to
//! [`ReldexDriverKind`] plus one branch in [`build_driver`]. Nothing else in
//! the crate changes, because nothing else in the crate names a driver.

use std::sync::Arc;

use crate::session::ReldexOpenOptions;
use crate::status::{ReldexStatus, entry, entry_value};
use crate::strings::{CStruct, ReldexStr};

/// Which driver a session is opened against.
///
/// `0` is reserved for a kind this header predates (ADR-0003 D7).
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexDriverKind {
    /// A driver this header does not know, or none requested.
    Unknown = 0,
    /// The in-process mock driver: no database, deterministic, and the only
    /// driver a build with the `mock-driver` feature can open.
    Mock = 1,
}

/// Which scripted world a mock session connects to.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexMockScenario {
    /// A scenario this header does not know.
    Unknown = 0,
    /// The spike S14/S15 world. Its statements are listed in
    /// [`ReldexMockStatement`]: a lazily generated result of the S14 shape
    /// (`ID NUMBER`, `NAME VARCHAR2(40)` including NULL, Thai and non-BMP
    /// rows, `CREATED DATE`), a statement that blocks, one that fails with a
    /// native code and a position, and one that panics inside the driver.
    S14 = 1,
}

/// A statement the [`ReldexMockScenario::S14`] world answers.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexMockStatement {
    /// A statement this header does not know.
    Unknown = 0,
    /// The generated result: `rows` rows of the S14 shape.
    GeneratedQuery = 1,
    /// Blocks the session's worker until released — by
    /// [`reldex_mock_release_block`], by the configured duration elapsing, or
    /// by a cancel.
    Block = 2,
    /// Fails with `ORA-00942` at line 1, column 15.
    Failing = 3,
    /// Panics inside the driver call. `db-core` contains the panic and reports
    /// it as an error; nothing aborts (ADR-0002 K6).
    Panicking = 4,
    /// One row of DML, which opens a transaction — so a later close with no
    /// disposition must come back as `DECISION_REQUIRED` rather than
    /// committing anything (`SPEC.md` §10).
    Dml = 5,
    /// Retired with the interim per-session pump thread it used to panic
    /// (M2.15): there is no thread of this library's left between a session
    /// and the queue. Kept, because an enum value is never reused, and now
    /// the same contained **driver** panic as [`Self::Panicking`] — the
    /// session is lost and its `TERMINAL` follows the failed `EXECUTED`.
    PumpPanic = 6,
    /// A result set with the S14 columns and **no rows**.
    ///
    /// The case a grid gets wrong: `EXECUTED` reports three columns, the first
    /// fetch comes back empty, and there is never a batch to read the column
    /// names from. A header built from the first batch shows nothing here; one
    /// built from `reldex_session_result_column` is correct.
    ///
    /// Deliberately a separate statement rather than
    /// [`ReldexMockScenarioConfig::rows`] `= 0`, which keeps its documented
    /// meaning of "1,000".
    EmptyQuery = 7,
    /// A PL/SQL block that prints three lines of server output, the last one
    /// reported as having arrived as invalid UTF-8 (ABI 3.2). The lines are
    /// buffered only once `reldex_session_set_server_output` has turned
    /// output on, and arrive as `SERVER_OUTPUT` ahead of the block's
    /// `EXECUTED`.
    ServerOutput = 8,
    /// Loses the connection mid-statement: fails with `ORA-03113` and the
    /// session is **lost**, so its `TERMINAL` follows at once, without a
    /// close (ABI 3.2).
    LoseSession = 9,
}

/// A failure the mock world can script for every connect or every ping
/// (ABI 3.2), each with the native code a real server would report.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexMockFailure {
    /// No failure: the call succeeds.
    None = 0,
    /// `ORA-01017`, an authentication failure.
    Authentication = 1,
    /// `ORA-12541`, nothing listening at the endpoint.
    Unreachable = 2,
    /// `ORA-03113`, the connection lost.
    Lost = 3,
}

/// How a mock session's scripted world is parameterised.
///
/// Every field is optional in the ADR-0003 D7 sense: a zero means "the
/// default", so a caller may zero the struct, set `struct_size`, and get a
/// usable world.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReldexMockScenarioConfig {
    /// `sizeof(ReldexMockScenarioConfig)`.
    pub struct_size: u32,
    /// A [`ReldexMockScenario`]; `0` selects [`ReldexMockScenario::S14`].
    pub scenario: i32,
    /// How many rows [`ReldexMockStatement::GeneratedQuery`] produces.
    /// `0` means 1,000.
    pub rows: u64,
    /// Mixed into the generated values; `0` is a valid seed.
    pub seed: u64,
    /// Simulated per-fetch round-trip cost, in microseconds. `0` means none.
    pub per_fetch_latency_us: u64,
    /// Extra simulated cost before the *first* batch, in microseconds.
    /// `0` means none.
    pub first_batch_latency_us: u64,
    /// How long [`ReldexMockStatement::Block`] blocks, in milliseconds.
    /// `0` means "until [`reldex_mock_release_block`] or a cancel", which is
    /// what a deterministic test wants.
    pub block_duration_ms: u64,
    /// A [`ReldexMockFailure`] every connect fails with (ABI 3.2): the open's
    /// `OPENED` carries that error and its `TERMINAL` follows. `0`, none.
    pub connect_failure: i32,
    /// A [`ReldexMockFailure`] every `reldex_session_ping` fails with (ABI
    /// 3.2). `RELDEX_MOCK_FAILURE_LOST` also loses the session. `0`, none.
    pub ping_failure: i32,
    /// Parks every connect until [`reldex_mock_release_block`] (ABI 3.2), so
    /// a caller can act on a session that is still connecting — abandon it,
    /// or destroy the hub — deterministically.
    pub block_connect: bool,
}

// SAFETY: `#[repr(C)]`, `struct_size` first, integers and one `bool`, all
// valid when zeroed.
unsafe impl CStruct for ReldexMockScenarioConfig {
    // Everything past `struct_size` has a documented zero default, so an
    // older caller that supplies only the size gets the default world.
    const MIN_SIZE: usize = size_of::<u32>() + size_of::<i32>();
}

impl Default for ReldexMockScenarioConfig {
    /// The default world: 1,000 rows, no latency, a block that waits to be
    /// released.
    fn default() -> Self {
        Self {
            struct_size: u32::try_from(size_of::<Self>()).unwrap_or(u32::MAX),
            scenario: ReldexMockScenario::S14 as i32,
            rows: 0,
            seed: 0,
            per_fetch_latency_us: 0,
            first_batch_latency_us: 0,
            block_duration_ms: 0,
            connect_failure: ReldexMockFailure::None as i32,
            ping_failure: ReldexMockFailure::None as i32,
            block_connect: false,
        }
    }
}

/// The SQL text for one of the scenario's statements, as a borrowed `'static`
/// string. NUL-terminated, so a C caller may use it directly.
///
/// An unknown kind reports the empty string.
#[unsafe(no_mangle)]
pub extern "C" fn reldex_mock_statement(kind: i32) -> ReldexStr {
    entry_value(ReldexStr::empty(), || {
        let text = if kind == ReldexMockStatement::GeneratedQuery as i32 {
            statements::GENERATED_QUERY
        } else if kind == ReldexMockStatement::EmptyQuery as i32 {
            statements::EMPTY_QUERY
        } else if kind == ReldexMockStatement::Block as i32 {
            statements::BLOCK
        } else if kind == ReldexMockStatement::Failing as i32 {
            statements::FAILING
        } else if kind == ReldexMockStatement::Panicking as i32 {
            statements::PANICKING
        } else if kind == ReldexMockStatement::Dml as i32 {
            statements::DML
        } else if kind == ReldexMockStatement::PumpPanic as i32 {
            statements::PUMP_PANIC
        } else if kind == ReldexMockStatement::ServerOutput as i32 {
            statements::SERVER_OUTPUT
        } else if kind == ReldexMockStatement::LoseSession as i32 {
            statements::LOSE_SESSION
        } else {
            return ReldexStr::empty();
        };
        ReldexStr::borrow_nul_terminated(text, text.len() - 1)
    })
}

/// The scenario's statement texts, each with the trailing NUL a C caller
/// expects; the exported [`reldex_mock_statement`] reports the length without
/// it.
pub(crate) mod statements {
    pub(crate) const GENERATED_QUERY: &str = "SELECT * FROM reldex_generated\0";
    pub(crate) const EMPTY_QUERY: &str = "SELECT * FROM reldex_generated WHERE 1 = 0\0";
    pub(crate) const BLOCK: &str = "BEGIN reldex_block; END;\0";
    pub(crate) const FAILING: &str = "SELECT * FROM reldex_missing\0";
    pub(crate) const PANICKING: &str = "SELECT reldex_panic FROM dual\0";
    pub(crate) const DML: &str = "UPDATE reldex_rows SET n = n + 1\0";
    /// A second driver panic, kept for the retired pump-panic statement.
    pub(crate) const PUMP_PANIC: &str = "BEGIN reldex_pump_panic; END;\0";
    pub(crate) const SERVER_OUTPUT: &str = "BEGIN reldex_put_line; END;\0";
    pub(crate) const LOSE_SESSION: &str = "SELECT reldex_lost FROM dual\0";

    /// The text without its trailing NUL, for Rust callers.
    ///
    /// Only the scenario builder needs this, and it is feature-gated; the
    /// exported `reldex_mock_statement` reports the same text as a length and
    /// a pointer instead.
    #[cfg(feature = "mock-driver")]
    pub(crate) fn text(statement: &'static str) -> &'static str {
        statement.trim_end_matches('\0')
    }
}

/// Releases a session's blocked statement — and, with
/// [`ReldexMockScenarioConfig::block_connect`], its parked connect — if the
/// mock world it runs in has one.
///
/// Mock-only, and the reason [`ReldexMockScenarioConfig::block_duration_ms`]
/// may be zero: a test (or spike S15's "a blocked session must not stall the
/// UI" step) blocks a session indefinitely and releases it at a moment of its
/// choosing, with no sleep anywhere. Idempotent, safe whether or not anything
/// is currently blocked, and permanent: a released gate never blocks again.
///
/// # Safety
///
/// `hub` must be a live hub.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_mock_release_block(
    hub: *mut crate::ReldexHub,
    session: crate::ReldexSessionId,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract for `hub`.
        unsafe { crate::session::with_session(hub, session, |_hub, entry| entry.release_block()) }
    })
}

/// What [`build_driver`] hands back: the driver and parameters a session
/// will open with, plus the handles that release a blocked statement and a
/// parked connect.
pub(crate) struct DriverChoice {
    pub(crate) driver: Arc<dyn reldex_db_core::DatabaseDriver>,
    pub(crate) params: reldex_db_core::ConnectionParams,
    pub(crate) block: Option<BlockControl>,
    pub(crate) connect_block: Option<BlockControl>,
}

/// The mock's block gate, or a stand-in when no mock is linked, so the rest of
/// the crate needs no `cfg`.
#[cfg(feature = "mock-driver")]
pub(crate) type BlockControl = Arc<reldex_driver_mock::BlockGate>;
/// See the `mock-driver` definition.
#[cfg(not(feature = "mock-driver"))]
pub(crate) type BlockControl = Arc<()>;

/// Releases a parked statement. A no-op when no mock is linked.
#[cfg(feature = "mock-driver")]
pub(crate) fn release_block(control: &BlockControl) {
    control.release();
}

/// See the `mock-driver` definition.
#[cfg(not(feature = "mock-driver"))]
pub(crate) fn release_block(_control: &BlockControl) {}

/// Builds the driver a session should open against.
#[cfg(feature = "mock-driver")]
pub(crate) fn build_driver(options: &ReldexOpenOptions) -> Result<DriverChoice, ReldexStatus> {
    use std::time::Duration;

    use reldex_db_driver_api::{
        CancelKind, Capabilities, Credentials, Endpoint, ErrorKind, SqlPosition, StatementKind,
    };
    use reldex_driver_mock::{
        Action, BlockGate, BlockSpec, GeneratedQuerySpec, MockDriver, Scenario, ScriptedError,
    };

    if options.driver != ReldexDriverKind::Mock as i32 {
        return Err(ReldexStatus::InvalidArgument);
    }
    let config = options.mock;
    if config.scenario != 0 && config.scenario != ReldexMockScenario::S14 as i32 {
        return Err(ReldexStatus::InvalidArgument);
    }

    let scenario = Scenario::new();
    // Native cancellation, so `reldex_session_request_cancel` and hub teardown
    // actually reach a blocked statement; the mock's default is
    // `CancelKind::Unsupported`, which would make both silent no-ops.
    scenario.set_capabilities(
        Capabilities::none()
            .with_cancel(CancelKind::Native)
            .with_savepoints(true)
            .with_exact_transaction_state(true)
            .with_lob_streaming(true)
            .with_error_position(true),
    );

    let rows = if config.rows == 0 { 1_000 } else { config.rows };
    let mut spec = GeneratedQuerySpec::s14_shape(rows, config.seed);
    if config.per_fetch_latency_us > 0 {
        spec = spec.with_per_fetch_latency(Duration::from_micros(config.per_fetch_latency_us));
    }
    if config.first_batch_latency_us > 0 {
        spec = spec.with_first_batch_latency(Duration::from_micros(config.first_batch_latency_us));
    }
    scenario.on_sql(
        statements::text(statements::GENERATED_QUERY),
        Action::GeneratedQuery(spec),
    );
    // The same three columns, zero rows: a result whose headers exist and
    // whose batches never will.
    scenario.on_sql(
        statements::text(statements::EMPTY_QUERY),
        Action::GeneratedQuery(GeneratedQuerySpec::s14_shape(0, config.seed)),
    );

    let gate = BlockGate::new();
    scenario.on_sql(
        statements::text(statements::BLOCK),
        Action::Block(BlockSpec::new(Arc::clone(&gate))),
    );
    if config.block_duration_ms > 0 {
        // A releaser thread rather than a sleep inside the driver: the block
        // itself stays a real parked worker thread, which is the thing under
        // test. The clock starts when the session is opened, so a test that
        // wants an exact relationship to `execute` uses
        // `reldex_mock_release_block` instead.
        let releaser = Arc::clone(&gate);
        let delay = Duration::from_millis(config.block_duration_ms);
        std::thread::Builder::new()
            .name("reldex-ffi-mock-release".to_owned())
            .spawn(move || {
                std::thread::sleep(delay);
                releaser.release();
            })
            .map_err(|_| ReldexStatus::Error)?;
    }

    scenario.on_sql(
        statements::text(statements::FAILING),
        Action::Fail(
            ScriptedError::new(ErrorKind::Syntax, "table or view does not exist")
                .with_native(942, "ORA-00942: table or view does not exist")
                .with_position(SqlPosition::at_line_column(1, 15)),
        ),
    );
    for panicking in [statements::PANICKING, statements::PUMP_PANIC] {
        scenario.on_sql(
            statements::text(panicking),
            Action::Panic("reldex-ffi mock scenario: a deliberate driver panic".to_owned()),
        );
    }
    scenario.on_sql(
        statements::text(statements::SERVER_OUTPUT),
        Action::Execute {
            statement_kind: StatementKind::PlSqlBlock,
            rows_affected: None,
            opens_transaction: false,
        },
    );
    scenario.on_sql_output_with_invalid_utf8_lines(
        statements::text(statements::SERVER_OUTPUT),
        vec![
            "reldex: first line".to_owned(),
            String::new(),
            "reldex: \u{FFFD} arrived as invalid UTF-8".to_owned(),
        ],
        1,
    );
    if let Some(lost) = scripted_failure(ReldexMockFailure::Lost as i32) {
        scenario.on_sql(
            statements::text(statements::LOSE_SESSION),
            Action::Fail(lost),
        );
    }
    if let Some(error) = scripted_failure(config.connect_failure) {
        scenario.fail_connect(error);
    }
    if let Some(error) = scripted_failure(config.ping_failure) {
        scenario.fail_ping(error);
    }
    let connect_gate = config.block_connect.then(|| {
        let gate = BlockGate::new();
        scenario.block_connect(Arc::clone(&gate));
        gate
    });
    scenario.on_sql(
        statements::text(statements::DML),
        Action::Dml {
            rows_affected: 1,
            insert: None,
        },
    );

    Ok(DriverChoice {
        driver: Arc::new(MockDriver::new(scenario)),
        params: reldex_db_core::ConnectionParams::new(
            Endpoint::ConnectString("mock".to_owned()),
            Credentials::External,
        ),
        block: Some(gate),
        connect_block: connect_gate,
    })
}

/// The scripted error for a [`ReldexMockFailure`] value, or `None` for none
/// (and for a value this build does not know).
#[cfg(feature = "mock-driver")]
fn scripted_failure(failure: i32) -> Option<reldex_driver_mock::ScriptedError> {
    use reldex_db_driver_api::ErrorKind;

    let (kind, code, message) = if failure == ReldexMockFailure::Authentication as i32 {
        (
            ErrorKind::Authentication,
            1017,
            "ORA-01017: invalid username/password; logon denied",
        )
    } else if failure == ReldexMockFailure::Unreachable as i32 {
        (ErrorKind::Connection, 12541, "ORA-12541: TNS:no listener")
    } else if failure == ReldexMockFailure::Lost as i32 {
        (
            ErrorKind::NetworkLost,
            3113,
            "ORA-03113: end-of-file on communication channel",
        )
    } else {
        return None;
    };
    Some(reldex_driver_mock::ScriptedError::new(kind, message).with_native(code, message))
}

/// See the `mock-driver` definition. A build with no driver feature can create
/// a hub and read the ABI version, but cannot open a session.
#[cfg(not(feature = "mock-driver"))]
pub(crate) fn build_driver(_options: &ReldexOpenOptions) -> Result<DriverChoice, ReldexStatus> {
    Err(ReldexStatus::InvalidArgument)
}
