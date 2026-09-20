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
}

// SAFETY: `#[repr(C)]`, `struct_size` first, all integers, valid when zeroed.
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
        } else if kind == ReldexMockStatement::Block as i32 {
            statements::BLOCK
        } else if kind == ReldexMockStatement::Failing as i32 {
            statements::FAILING
        } else if kind == ReldexMockStatement::Panicking as i32 {
            statements::PANICKING
        } else if kind == ReldexMockStatement::Dml as i32 {
            statements::DML
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
    pub(crate) const BLOCK: &str = "BEGIN reldex_block; END;\0";
    pub(crate) const FAILING: &str = "SELECT * FROM reldex_missing\0";
    pub(crate) const PANICKING: &str = "SELECT reldex_panic FROM dual\0";
    pub(crate) const DML: &str = "UPDATE reldex_rows SET n = n + 1\0";

    /// The text without its trailing NUL, for Rust callers.
    ///
    /// Only the scenario builder needs this, and that is feature-gated; the
    /// exported `reldex_mock_statement` reports the same text as a length and
    /// a pointer instead.
    #[cfg(feature = "mock-driver")]
    pub(crate) fn text(statement: &'static str) -> &'static str {
        statement.trim_end_matches('\0')
    }
}

/// Releases a session's blocked statement, if the mock world it runs in has
/// one parked.
///
/// Mock-only, and the reason [`ReldexMockScenarioConfig::block_duration_ms`]
/// may be zero: a test (or spike S15's "a blocked session must not stall the
/// UI" step) blocks a session indefinitely and releases it at a moment of its
/// choosing, with no sleep anywhere. Idempotent, and safe whether or not
/// anything is currently blocked.
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
        unsafe { crate::session::with_session(hub, session, |entry| entry.release_block()) }
    })
}

/// What [`build_driver`] hands back: the driver and parameters a session pump
/// will open with, plus the handle that releases a blocked statement.
pub(crate) struct DriverChoice {
    pub(crate) driver: Arc<dyn reldex_db_core::DatabaseDriver>,
    pub(crate) params: reldex_db_core::ConnectionParams,
    pub(crate) block: Option<BlockControl>,
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
        CancelKind, Capabilities, Credentials, Endpoint, ErrorKind, SqlPosition,
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
    scenario.on_sql(
        statements::text(statements::PANICKING),
        Action::Panic("reldex-ffi mock scenario: a deliberate driver panic".to_owned()),
    );
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
    })
}

/// See the `mock-driver` definition. A build with no driver feature can create
/// a hub and read the ABI version, but cannot open a session.
#[cfg(not(feature = "mock-driver"))]
pub(crate) fn build_driver(_options: &ReldexOpenOptions) -> Result<DriverChoice, ReldexStatus> {
    Err(ReldexStatus::InvalidArgument)
}
