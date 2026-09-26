//! The scripted world a [`crate::MockDriver`] connects to.
//!
//! Nothing here parses SQL. A test registers a canned [`Action`] against an
//! exact statement string or a predicate (see [`Scenario::on_sql`],
//! [`Scenario::on_predicate`]); [`MockConnection::execute`](crate::MockConnection)
//! looks the statement text up and runs whatever was registered. The "table
//! store" (see [`ScriptValue`], [`QuerySource::Table`]) is one of those
//! built-in primitives, not a general query engine: it is just enough shared,
//! transactional state to prove session-isolation invariants
//! (`docs/exec-plans/active/phase-0.md` Workstream B).
//!
//! # The mock must be able to be as nasty as a real driver
//!
//! A test double that only ever behaves well proves nothing about the code that
//! has to survive a real one. Everything the Phase 0 spikes found a real driver
//! doing is therefore scriptable here: a failing `commit`, `rollback` or
//! `close` ([`Scenario::fail_commit`] and friends); a fired deadline that
//! destroys the session ([`BlockSpec::with_timeout_error`], spike U-6); cursors
//! and LOB locators that stop working after a commit or rollback
//! ([`Scenario::set_invalidate_handles_on_transaction_end`], ADR-0002 D2); a
//! cancel the statement never observes ([`BlockSpec::with_unobserved_cancel`],
//! spike U-7); a cancel that lands on the *next* statement
//! ([`Scenario::set_late_cancel_lands_on_next_statement`]); and a driver call
//! that panics outright ([`Action::Panic`], spike U-4).
//!
//! A result too large to ever hold in memory is scriptable too:
//! [`Action::GeneratedQuery`] produces its rows lazily, one fetch at a time
//! (see [`crate::GeneratedQuerySpec`]), instead of replaying a pre-built
//! [`QueryPlan`].

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use reldex_db_driver_api::{
    Capabilities, ConnectionId, DbError, ErrorKind, LobKind, NativeError, Number, SessionState,
    SqlPosition, SqlType, StatementKind, Timestamp, Warning,
};

use crate::generated::GeneratedQuerySpec;

/// One cell of a scripted row or result column.
///
/// Deliberately not the full [`reldex_db_driver_api::Value`] model: the mock
/// only needs to describe fixed test fixtures, so this is the small subset
/// `docs/exec-plans/active/phase-0.md` Workstream D asks the mock to exercise
/// (NULL, `NUMBER`, text, bytes, a timestamp, an unrepresentable/"unsupported"
/// type, and a LOB read in chunks).
#[derive(Debug, Clone, PartialEq)]
pub enum ScriptValue {
    /// SQL NULL.
    Null,
    /// An exact decimal.
    Number(Number),
    /// UTF-8 character data.
    Text(String),
    /// Raw bytes.
    Bytes(Vec<u8>),
    /// A date or timestamp.
    Timestamp(Timestamp),
    /// Best-effort text for a type the contract cannot represent, matching
    /// [`reldex_db_driver_api::ColumnData::Unsupported`].
    Unsupported(String),
    /// A large object, streamed back in caller-sized chunks.
    ///
    /// `bytes` must be valid UTF-8 when `kind` is a character kind: the
    /// stream will not split a multi-byte sequence, and non-UTF-8 input would
    /// make that promise meaningless.
    Lob {
        /// What the object contains.
        kind: LobKind,
        /// The object's full content.
        bytes: Vec<u8>,
    },
}

impl From<i64> for ScriptValue {
    fn from(value: i64) -> Self {
        Self::Number(Number::from(value))
    }
}

impl From<&str> for ScriptValue {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<String> for ScriptValue {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<Vec<u8>> for ScriptValue {
    fn from(value: Vec<u8>) -> Self {
        Self::Bytes(value)
    }
}

impl From<Timestamp> for ScriptValue {
    fn from(value: Timestamp) -> Self {
        Self::Timestamp(value)
    }
}

impl<T: Into<Self>> From<Option<T>> for ScriptValue {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Null, Into::into)
    }
}

/// Describes one result column for a [`QueryPlan`] or [`QuerySource::Table`].
#[derive(Debug, Clone)]
pub struct ColumnSpec {
    name: String,
    sql_type: SqlType,
    native_type_name: Option<String>,
    max_size_bytes: Option<u32>,
}

impl ColumnSpec {
    /// Names a column and declares its vendor-neutral type.
    #[must_use]
    pub fn new(name: impl Into<String>, sql_type: SqlType) -> Self {
        Self {
            name: name.into(),
            sql_type,
            native_type_name: None,
            max_size_bytes: None,
        }
    }

    /// Declares the column's maximum size in bytes, the way a describe
    /// reports `VARCHAR2(n)` (see
    /// [`reldex_db_driver_api::ColumnMetadata::max_size_bytes`]).
    #[must_use]
    pub const fn with_max_size_bytes(mut self, max_size_bytes: u32) -> Self {
        self.max_size_bytes = Some(max_size_bytes);
        self
    }

    /// The declared maximum size in bytes, if set.
    #[must_use]
    pub const fn max_size_bytes(&self) -> Option<u32> {
        self.max_size_bytes
    }

    /// Records the server's own type name (required for
    /// [`SqlType::Unsupported`] columns; see
    /// [`reldex_db_driver_api::ColumnMetadata::native_type_name`]).
    #[must_use]
    pub fn with_native_type_name(mut self, name: impl Into<String>) -> Self {
        self.native_type_name = Some(name.into());
        self
    }

    /// The column name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The declared vendor-neutral type.
    #[must_use]
    pub const fn sql_type(&self) -> SqlType {
        self.sql_type
    }

    /// The server's own type name, if set.
    #[must_use]
    pub fn native_type_name(&self) -> Option<&str> {
        self.native_type_name.as_deref()
    }
}

/// A scripted, error, built lazily so it can be attached to more than one
/// [`Action`] without needing [`DbError`] to be [`Clone`] (it is not).
#[derive(Debug, Clone)]
pub struct ScriptedError {
    kind: ErrorKind,
    message: String,
    native: Option<(i32, String)>,
    position: Option<SqlPosition>,
    session_state: Option<SessionState>,
    retryable: bool,
}

impl ScriptedError {
    /// Describes an error of the given category.
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            native: None,
            position: None,
            session_state: None,
            retryable: false,
        }
    }

    /// Attaches a preserved native error code and message.
    #[must_use]
    pub fn with_native(mut self, code: i32, message: impl Into<String>) -> Self {
        self.native = Some((code, message.into()));
        self
    }

    /// Attaches the position in the statement text the error points at.
    ///
    /// A real server reports one for a PL/SQL compilation failure
    /// (`ORA-06550`) and the error pane highlights it (`SPEC.md` §24.14), so
    /// the mock has to be able to produce one — otherwise nothing above the
    /// driver can be tested against a positioned error without a database.
    #[must_use]
    pub fn with_position(mut self, position: SqlPosition) -> Self {
        self.position = Some(position);
        self
    }

    /// Overrides the session state the built [`DbError`] reports, instead of
    /// [`SessionState::initial_for`] the kind.
    #[must_use]
    pub fn with_session_state(mut self, session_state: SessionState) -> Self {
        self.session_state = Some(session_state);
        self
    }

    /// Marks the built error as retryable.
    #[must_use]
    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    /// Builds the [`DbError`] this describes.
    #[must_use]
    pub fn build(&self) -> DbError {
        let mut error = DbError::new(self.kind, self.message.clone());
        if let Some((code, message)) = &self.native {
            error = error.with_native(NativeError::new(*code, message.clone()));
        }
        if let Some(position) = self.position {
            error = error.with_position(position);
        }
        if let Some(session_state) = self.session_state {
            error = error.with_session_state(session_state);
        }
        error.with_retryable(self.retryable)
    }
}

/// A fixed set of rows and columns a scripted `SELECT` returns.
#[derive(Debug, Clone)]
pub struct QueryPlan {
    /// The result columns, in order.
    pub columns: Vec<ColumnSpec>,
    /// The full result set. [`Cursor::fetch_batch`](reldex_db_driver_api::Cursor::fetch_batch)
    /// slices this according to the caller's requested batch size, so one
    /// plan can exercise multi-batch fetching and exhaustion.
    pub rows: Vec<Vec<ScriptValue>>,
    /// Fault injection: if set, the `n`th call to `fetch_batch` (counting
    /// from 1) fails with this error instead of returning rows, modelling
    /// e.g. a network loss partway through a large fetch. Every later call
    /// returns the same error, never an empty "complete" batch.
    pub fail_on_batch: Option<(usize, ScriptedError)>,
}

impl QueryPlan {
    /// A query plan with no fault injection.
    #[must_use]
    pub fn new(columns: Vec<ColumnSpec>, rows: Vec<Vec<ScriptValue>>) -> Self {
        Self {
            columns,
            rows,
            fail_on_batch: None,
        }
    }

    /// Fails the `n`th `fetch_batch` call (1-based) with `error` instead of
    /// returning rows.
    #[must_use]
    pub fn with_fail_on_batch(mut self, n: usize, error: ScriptedError) -> Self {
        self.fail_on_batch = Some((n, error));
        self
    }
}

/// Where a scripted `SELECT`'s rows come from.
#[derive(Debug, Clone)]
pub enum QuerySource {
    /// A fixed result set, unrelated to the table store.
    Fixed(QueryPlan),
    /// The mock table store's current rows for `table`, as seen by the
    /// executing connection: every committed row plus that connection's own
    /// uncommitted inserts (`docs/exec-plans/active/phase-0.md` Workstream B).
    Table {
        /// The table name.
        table: String,
        /// How to present each row's [`ScriptValue`]s as columns.
        columns: Vec<ColumnSpec>,
    },
}

/// A live handle a test uses to release, or observe, a blocked statement.
///
/// Created by the test with [`BlockGate::new`] and attached to an
/// [`Action::Block`] via [`BlockSpec::new`]. The blocked worker thread parks
/// on the gate; [`BlockGate::release`] lets it succeed,
/// [`BlockGate::wait_until_blocked`] lets the test synchronize without
/// sleeping.
pub struct BlockGate {
    state: Mutex<GateState>,
    condvar: Condvar,
}

struct GateState {
    parked: u32,
    released: bool,
    cancelled: bool,
}

/// Why a parked statement stopped waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParkOutcome {
    Released,
    Cancelled,
    TimedOut,
}

impl BlockGate {
    /// Creates a gate that nothing has released or cancelled yet.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(GateState {
                parked: 0,
                released: false,
                cancelled: false,
            }),
            condvar: Condvar::new(),
        })
    }

    /// Lets a statement parked on this gate succeed.
    ///
    /// Idempotent, and safe to call whether or not anything is currently
    /// parked: a future block on the same gate would return immediately,
    /// which is why [`BlockSpec`] normally uses a fresh gate per scripted
    /// statement.
    pub fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.released = true;
        self.condvar.notify_all();
    }

    pub(crate) fn request_cancel(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cancelled = true;
        self.condvar.notify_all();
    }

    /// Blocks the calling thread until a statement is actually parked on this
    /// gate, or `timeout` elapses.
    ///
    /// Tests use this instead of a fixed `sleep` before calling
    /// [`BlockGate::release`] or requesting cancellation, so the sequence is
    /// deterministic: returns `true` once a worker thread has reached the
    /// gate, `false` if `timeout` elapsed first.
    #[must_use]
    pub fn wait_until_blocked(&self, timeout: Duration) -> bool {
        self.wait_until_parked(1, timeout)
    }

    /// Blocks until at least `count` statements are parked on this gate, or
    /// `timeout` elapses.
    ///
    /// The many-sessions form of [`BlockGate::wait_until_blocked`]: a test that
    /// needs *every* worker stuck before it measures something must be able to
    /// wait for that without sleeping.
    #[must_use]
    pub fn wait_until_parked(&self, count: u32, timeout: Duration) -> bool {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (_guard, result) = self
            .condvar
            .wait_timeout_while(guard, timeout, |state| state.parked < count)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !result.timed_out()
    }

    /// How many statements are parked on this gate right now.
    #[must_use]
    pub fn parked(&self) -> u32 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .parked
    }

    /// Parks until released, cancelled or `deadline`.
    ///
    /// `observe_cancel` is false for the "unobserved cancel" case a real server
    /// can produce (spike U-7): the request reaches the connection, and the
    /// statement carries on until its own deadline regardless.
    pub(crate) fn park(&self, deadline: Option<Instant>, observe_cancel: bool) -> ParkOutcome {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.parked += 1;
        self.condvar.notify_all();
        loop {
            if guard.released {
                guard.parked -= 1;
                return ParkOutcome::Released;
            }
            if guard.cancelled && observe_cancel {
                guard.parked -= 1;
                return ParkOutcome::Cancelled;
            }
            let Some(deadline) = deadline else {
                guard = self
                    .condvar
                    .wait(guard)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                continue;
            };
            let now = Instant::now();
            if now >= deadline {
                guard.parked -= 1;
                return ParkOutcome::TimedOut;
            }
            let (next_guard, _) = self
                .condvar
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard = next_guard;
        }
    }
}

impl fmt::Debug for BlockGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlockGate").finish_non_exhaustive()
    }
}

/// What a blocked statement reports once it stops waiting.
///
/// # Blocking for a fixed duration, then succeeding
///
/// This is already expressible with no further API: have a controller thread
/// sleep for the duration and then call [`BlockGate::release`], which is safe
/// to call whether or not anything has parked on the gate yet, so there is no
/// race to get right. This is how the S15 FFI/UI spike's "a statement blocked
/// for 10 seconds must not stall the rest of the UI" scenario is scripted —
/// see `crates/drivers/mock/tests/block_for_duration.rs` for the full test,
/// including that a shorter [`Statement::with_deadline`](reldex_db_driver_api::Statement::with_deadline) still fires as
/// a timeout while the gate is not yet released, exactly as it does for any
/// other blocked statement (ADR-0002 D2).
///
/// ```
/// use std::sync::Arc;
/// use std::time::Duration;
/// use reldex_driver_mock::{BlockGate, BlockSpec};
///
/// let gate = BlockGate::new();
/// let controller = Arc::clone(&gate);
/// std::thread::spawn(move || {
///     std::thread::sleep(Duration::from_millis(10));
///     controller.release();
/// });
/// // Register `Action::Block(BlockSpec::new(gate))` on a statement and
/// // execute it: the call blocks for ~10ms and then reports success, exactly
/// // like a slow 10-second statement in the FFI/UI spike, just scaled down.
/// let _spec = BlockSpec::new(gate);
/// ```
#[derive(Debug, Clone)]
pub struct BlockSpec {
    pub(crate) gate: Arc<BlockGate>,
    pub(crate) cancelled_session_state: Option<SessionState>,
    pub(crate) timeout_error: Option<ScriptedError>,
    pub(crate) observe_cancel: bool,
    pub(crate) release_statement_kind: StatementKind,
    pub(crate) release_rows_affected: Option<u64>,
}

impl BlockSpec {
    /// Blocks on `gate` until released or cancelled.
    #[must_use]
    pub fn new(gate: Arc<BlockGate>) -> Self {
        Self {
            gate,
            cancelled_session_state: None,
            timeout_error: None,
            observe_cancel: true,
            release_statement_kind: StatementKind::Other,
            release_rows_affected: None,
        }
    }

    /// Overrides the session state a cancellation reports (default: the
    /// [`DbError::cancelled`] default, `SessionState::NeedsValidation`).
    #[must_use]
    pub fn with_cancelled_session_state(mut self, session_state: SessionState) -> Self {
        self.cancelled_session_state = Some(session_state);
        self
    }

    /// Replaces what a fired deadline reports.
    ///
    /// The default is a plain [`ErrorKind::Timeout`] with
    /// [`SessionState::NeedsValidation`]. A real driver is not always that
    /// kind: upstream's recovery path reads the reset reply with the expired
    /// timeout still armed, so a deadline on a statement the server will not
    /// interrupt returns `NetworkLost` with the **session destroyed** (spike
    /// U-6). Scripting that is the only way to test what `db-core` does about
    /// it.
    #[must_use]
    pub fn with_timeout_error(mut self, error: ScriptedError) -> Self {
        self.timeout_error = Some(error);
        self
    }

    /// Makes this statement ignore cancellation and stop only at its deadline.
    ///
    /// Models a cancel that reaches the server but that the client never
    /// observes (spike U-7, `ALTER SYSTEM CANCEL SQL`): the request is
    /// delivered, `request_cancel` reports success, and the blocked call
    /// carries on until its own deadline fires.
    #[must_use]
    pub fn with_unobserved_cancel(mut self) -> Self {
        self.observe_cancel = false;
        self
    }

    /// Sets the statement kind reported once the gate releases successfully.
    #[must_use]
    pub fn with_release_statement_kind(mut self, kind: StatementKind) -> Self {
        self.release_statement_kind = kind;
        self
    }

    /// Sets the rows-affected count reported once the gate releases
    /// successfully.
    #[must_use]
    pub fn with_release_rows_affected(mut self, rows_affected: u64) -> Self {
        self.release_rows_affected = Some(rows_affected);
        self
    }
}

/// A canned reaction to a statement whose text matches a [`Matcher`].
#[derive(Debug, Clone)]
pub enum Action {
    /// Returns rows through a cursor.
    ///
    /// Build one with [`Action::query`] or, for the `SELECT … FOR UPDATE` case,
    /// [`Action::locking_query`].
    Query {
        /// Where the rows come from.
        source: QuerySource,
        /// Whether this query opened a transaction, the way
        /// `SELECT … FOR UPDATE` and `LOCK TABLE` do.
        ///
        /// It is still reported as [`StatementKind::Query`] — a driver has no
        /// other honest classification for it — so this is precisely the case
        /// where `db-core` cannot rely on the statement kind and has to be
        /// conservative instead.
        opens_transaction: bool,
    },
    /// Returns rows computed on demand, one fetch at a time, instead of
    /// replaying a pre-built [`QueryPlan`].
    ///
    /// This is what a result too large to hold in memory — the 1,000,000-row
    /// shape the S15 FFI/UI spike scrolls through a `TableView` — is scripted
    /// with; see [`crate::GeneratedQuerySpec`].
    GeneratedQuery(GeneratedQuerySpec),
    /// Reports `rows_affected` rows changed and `StatementKind::Dml`.
    ///
    /// If `insert` is set, the row is appended to the executing connection's
    /// *uncommitted* overlay for the named table — visible to that
    /// connection immediately, to every connection only after `commit`.
    Dml {
        /// How many rows the statement changed.
        rows_affected: u64,
        /// A row to append to the table store, uncommitted.
        insert: Option<(String, Vec<ScriptValue>)>,
    },
    /// Reports `StatementKind::Ddl`: an implicit commit (ADR-0002 D4/S9).
    /// Flushes the connection's pending table-store overlay as part of that
    /// commit, matching a real server's commit-around-DDL behaviour.
    Ddl,
    /// Reports an arbitrary statement kind with no cursor — for statements
    /// that only need "something happened" (PL/SQL blocks, session or
    /// transaction control typed as text, and so on).
    Execute {
        /// What kind of statement this was.
        statement_kind: StatementKind,
        /// Rows changed, if any.
        rows_affected: Option<u64>,
        /// Whether the statement opened a transaction, the way
        /// `SET TRANSACTION READ ONLY` does.
        opens_transaction: bool,
    },
    /// Returns a nested `REF CURSOR` through a **named** output bind, the way
    /// a PL/SQL `OPEN :rc FOR …` does.
    ///
    /// Reported as [`StatementKind::PlSqlBlock`] with
    /// `OutValues::Named([(name, Value::Cursor(..))])`. It exists so `db-core`
    /// can be exercised against the REF CURSOR path without a database; the
    /// shape is deliberately the narrow one that path needs and can grow when
    /// another output shape has to be scripted.
    RefCursorOut {
        /// The output bind's name, without its placeholder prefix.
        name: String,
        /// Where the nested cursor's rows come from.
        source: QuerySource,
    },
    /// Returns a large object through a **named** output bind.
    ///
    /// The other way a driver-owned handle reaches `db-core` through
    /// `OutValues`, and the reason `OutValue::Lob` exists.
    LobOut {
        /// The output bind's name, without its placeholder prefix.
        name: String,
        /// What the object contains.
        kind: LobKind,
        /// The object's full content.
        bytes: Vec<u8>,
    },
    /// Fails immediately with a scripted error.
    Fail(ScriptedError),
    /// Blocks the worker thread until released or cancelled. See
    /// [`BlockSpec`].
    Block(BlockSpec),
    /// Panics inside the driver call, with this message.
    ///
    /// A real driver can do this (spike U-2/U-3 found two inputs that panic
    /// inside `oracledb`), and `db-core` promises to contain it rather than
    /// letting it unwind across the worker boundary — a promise that needs a
    /// test. Note that the *real* primary driver turns such a panic into a
    /// process abort (spike U-4), which nothing can contain; this models the
    /// well-behaved case the promise is actually about.
    Panic(String),
}

impl Action {
    /// A query that does not open a transaction: the ordinary `SELECT`.
    #[must_use]
    pub const fn query(source: QuerySource) -> Self {
        Self::Query {
            source,
            opens_transaction: false,
        }
    }

    /// A query that *does* open a transaction, like `SELECT … FOR UPDATE`.
    #[must_use]
    pub const fn locking_query(source: QuerySource) -> Self {
        Self::Query {
            source,
            opens_transaction: true,
        }
    }
}

/// What a statement's SQL text must satisfy to run an [`Action`].
#[derive(Clone)]
pub enum Matcher {
    /// The trimmed statement text equals this string exactly.
    Exact(String),
    /// The statement text satisfies this predicate.
    Predicate(Arc<dyn Fn(&str) -> bool + Send + Sync>),
}

impl Matcher {
    pub(crate) fn matches(&self, sql: &str) -> bool {
        match self {
            Self::Exact(expected) => sql.trim() == expected.trim(),
            Self::Predicate(predicate) => predicate(sql),
        }
    }
}

impl fmt::Debug for Matcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(sql) => f.debug_tuple("Exact").field(sql).finish(),
            Self::Predicate(_) => f.write_str("Predicate(..)"),
        }
    }
}

/// How one scripted connection-level operation behaves.
#[derive(Clone)]
pub(crate) enum Behavior {
    Succeed,
    Fail(ScriptedError),
    /// A driver call that panics outright (spike U-4). Only `connect` scripts
    /// this today; `execute` has [`Action::Panic`].
    Panic(String),
}

impl Behavior {
    pub(crate) fn apply(&self) -> Result<(), DbError> {
        match self {
            Self::Succeed => Ok(()),
            Self::Fail(error) => Err(error.build()),
            Self::Panic(message) => panic!("{message}"),
        }
    }
}

/// What a test can count after the fact.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    /// How many cursors were opened.
    pub cursors_opened: usize,
    /// How many of them had `Cursor::close` called on them.
    pub cursors_closed: usize,
    /// How many `DatabaseDriver::connect` calls have **returned**, whether
    /// they succeeded or failed.
    ///
    /// The synchronisation point a test needs when it parks a connect with
    /// [`Scenario::block_connect`]: "the connect is over" is otherwise not
    /// observable for one that failed, and waiting on
    /// [`Counts::connections_live`] alone is vacuously true before the connect
    /// has produced anything.
    pub connects_finished: usize,
    /// How many connections `DatabaseDriver::connect` handed out.
    pub connections_opened: usize,
    /// How many connections had `DatabaseConnection::close` called on them.
    pub connections_closed: usize,
}

impl Counts {
    /// How many connections this scenario handed out and has not been asked to
    /// close.
    ///
    /// The number that matters for an abandoned connect: a `connect` that wins
    /// its race with [`Scenario::block_connect`]'s release still produced a
    /// live database session, and the only acceptable end for it is that
    /// somebody closed it. `0` is the assertion "nothing was leaked"; anything
    /// else is a session left open on the server.
    #[must_use]
    pub const fn connections_live(&self) -> usize {
        self.connections_opened
            .saturating_sub(self.connections_closed)
    }
}

struct Inner {
    capabilities: Capabilities,
    connect: Behavior,
    /// When set, every `connect` parks on this gate before its behaviour is
    /// applied, so a test can hold a connect open for as long as it needs and
    /// then decide whether it succeeds or fails.
    connect_gate: Option<Arc<BlockGate>>,
    connect_warnings: Vec<Warning>,
    ping: Behavior,
    cancel: Behavior,
    commit: Behavior,
    rollback: Behavior,
    savepoint: Behavior,
    close: Behavior,
    invalidate_handles_on_transaction_end: bool,
    late_cancel_lands_on_next_statement: bool,
    responses: Vec<(Matcher, Action)>,
    tables: HashMap<String, Vec<Vec<ScriptValue>>>,
    thread_ids: HashMap<ConnectionId, std::collections::HashSet<ThreadId>>,
    counts: Counts,
}

/// The scripted world one or more [`crate::MockConnection`]s connect to.
///
/// Always shared as `Arc<Scenario>`: one instance stands in for "the
/// database" a [`crate::MockDriver`] talks to, so connections opened from the
/// same scenario see the same committed table store, and a test keeps its own
/// handle to assert on it (thread ids touched, committed rows, and so on).
///
/// Registering a response never consumes it: the same scripted statement can
/// run any number of times, which is what lets a test insert several rows in
/// a loop or execute the same query twice.
pub struct Scenario {
    inner: Mutex<Inner>,
    /// Scripted server output (`DBMS_OUTPUT`-like); see
    /// [`crate::server_output`]. Kept apart from `inner` so its bookkeeping
    /// lives beside the code that uses it.
    pub(crate) server_output: crate::server_output::ServerOutputScript,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                capabilities: Capabilities::none()
                    .with_savepoints(true)
                    .with_exact_transaction_state(true)
                    .with_lob_streaming(true)
                    .with_error_position(true),
                connect: Behavior::Succeed,
                connect_gate: None,
                connect_warnings: Vec::new(),
                ping: Behavior::Succeed,
                cancel: Behavior::Succeed,
                commit: Behavior::Succeed,
                rollback: Behavior::Succeed,
                savepoint: Behavior::Succeed,
                close: Behavior::Succeed,
                invalidate_handles_on_transaction_end: false,
                late_cancel_lands_on_next_statement: false,
                responses: Vec::new(),
                tables: HashMap::new(),
                thread_ids: HashMap::new(),
                counts: Counts::default(),
            }),
            server_output: crate::server_output::ServerOutputScript::default(),
        }
    }
}

impl Scenario {
    /// A scenario that accepts connections and has nothing scripted yet.
    ///
    /// Default capabilities: savepoints, exact transaction state, LOB
    /// streaming and error positions on; cancellation
    /// [`reldex_db_driver_api::CancelKind::Unsupported`]. Override with
    /// [`Scenario::set_capabilities`].
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Replaces what this scenario's driver and connections claim to support.
    pub fn set_capabilities(&self, capabilities: Capabilities) {
        self.lock().capabilities = capabilities;
    }

    /// What this scenario's driver and connections claim to support.
    #[must_use]
    pub fn capabilities(&self) -> Capabilities {
        self.lock().capabilities
    }

    /// Makes every future `connect` fail with `error`.
    pub fn fail_connect(&self, error: ScriptedError) {
        self.lock().connect = Behavior::Fail(error);
    }

    /// Lets every future `connect` succeed again.
    pub fn allow_connect(&self) {
        self.lock().connect = Behavior::Succeed;
    }

    /// Makes every future `connect` **panic**, the way a driver that reached a
    /// state it considers impossible does (spike U-4).
    ///
    /// What is under test is the core's containment (ADR-0002 K6): a panic in
    /// `connect` must become a reportable failure for that one session, not an
    /// unwind across the worker boundary.
    pub fn panic_connect(&self, message: impl Into<String>) {
        self.lock().connect = Behavior::Panic(message.into());
    }

    /// Parks every future `connect` on `gate` until it is released.
    ///
    /// The connect-side twin of [`Action::Block`], and the only way to hold a
    /// connect open deterministically: a test opens a session, waits for
    /// [`BlockGate::wait_until_blocked`], does whatever it wanted to race
    /// against the connect — abandon it, drop the registry — and only then
    /// releases the gate, so the "late connection" case is forced rather than
    /// hoped for. The behaviour scripted by [`Scenario::fail_connect`] /
    /// [`Scenario::panic_connect`] is applied **after** the gate releases, so
    /// the same gate covers a late success and a late failure.
    ///
    /// The gate is shared by every connect on this scenario; use a fresh
    /// scenario, or a fresh gate, per interleaving being tested.
    pub fn block_connect(&self, gate: Arc<BlockGate>) {
        self.lock().connect_gate = Some(gate);
    }

    /// Stops parking `connect`; attempts already parked stay parked until the
    /// gate they took is released.
    pub fn unblock_connect(&self) {
        self.lock().connect_gate = None;
    }

    /// Scripts the non-fatal findings every future connection reports through
    /// [`reldex_db_driver_api::DatabaseConnection::take_connect_warnings`].
    ///
    /// The real case is a transport parameter a driver cannot honour but that
    /// does not weaken the session, so refusing the connection would be wrong
    /// and silence would leave an imported profile believing it configured
    /// something (contract gap C-6, `oracledb` U-14). Each connection gets its
    /// own copy, and takes it once.
    pub fn set_connect_warnings(&self, warnings: Vec<Warning>) {
        self.lock().connect_warnings = warnings;
    }

    /// The findings a connection opened now would report.
    pub(crate) fn connect_warnings(&self) -> Vec<Warning> {
        self.lock().connect_warnings.clone()
    }

    /// Makes every future `ping` fail with `error`.
    pub fn fail_ping(&self, error: ScriptedError) {
        self.lock().ping = Behavior::Fail(error);
    }

    /// Makes every future `ping` succeed again.
    pub fn allow_ping(&self) {
        self.lock().ping = Behavior::Succeed;
    }

    /// Makes every future `request_cancel` report `error`, whatever the
    /// connection's [`reldex_db_driver_api::CancelKind`] would otherwise say.
    ///
    /// A real cancel path can discover that the session is already gone — the
    /// error it returns is then the *first* news of that, and must not be
    /// dropped on the floor by whoever asked for the cancel.
    pub fn fail_cancel(&self, error: ScriptedError) {
        self.lock().cancel = Behavior::Fail(error);
    }

    /// Makes every future `request_cancel` behave normally again.
    pub fn allow_cancel(&self) {
        self.lock().cancel = Behavior::Succeed;
    }

    /// Makes every future `commit` fail with `error`, leaving the transaction
    /// untouched.
    pub fn fail_commit(&self, error: ScriptedError) {
        self.lock().commit = Behavior::Fail(error);
    }

    /// Makes every future `commit` succeed again.
    pub fn allow_commit(&self) {
        self.lock().commit = Behavior::Succeed;
    }

    /// Makes every future `savepoint` **and** `rollback to savepoint` fail with
    /// `error`.
    ///
    /// Both go through one behaviour because they are the same driver call arm
    /// in `db-core` (`Command::Savepoint` / `Command::RollbackToSavepoint`), and
    /// what a test needs from here is to make that arm fail — most usefully
    /// with `SessionState::Lost`, which is the case where the core must stop
    /// trusting the driver's cached transaction state.
    pub fn fail_savepoint(&self, error: ScriptedError) {
        self.lock().savepoint = Behavior::Fail(error);
    }

    /// Makes every future savepoint call succeed again.
    pub fn allow_savepoint(&self) {
        self.lock().savepoint = Behavior::Succeed;
    }

    /// Makes every future `rollback` fail with `error`, leaving the
    /// transaction untouched.
    pub fn fail_rollback(&self, error: ScriptedError) {
        self.lock().rollback = Behavior::Fail(error);
    }

    /// Makes every future `rollback` succeed again.
    pub fn allow_rollback(&self) {
        self.lock().rollback = Behavior::Succeed;
    }

    /// Makes every future `close` report `error`.
    ///
    /// The connection is still marked closed — `close` consumes it, so there is
    /// no version of this where the connection survives; what is scripted is
    /// the *report*.
    pub fn fail_close(&self, error: ScriptedError) {
        self.lock().close = Behavior::Fail(error);
    }

    /// Makes every future `close` succeed again.
    pub fn allow_close(&self) {
        self.lock().close = Behavior::Succeed;
    }

    /// Makes every cursor and LOB locator stop working after a `commit`,
    /// `rollback` or `rollback to savepoint`, reporting
    /// [`ErrorKind::Transaction`].
    ///
    /// This is what most servers really do — a LOB locator is scoped to its
    /// transaction and `ROLLBACK` commonly closes cursors (ADR-0002 D2) — and
    /// the default-off mock behaviour is the *lenient* one.
    pub fn set_invalidate_handles_on_transaction_end(&self, invalidate: bool) {
        self.lock().invalidate_handles_on_transaction_end = invalidate;
    }

    /// Makes a cancellation requested while nothing is running latch onto the
    /// **next** statement instead of being discarded.
    ///
    /// The nastiest shape of the cancel race: nothing in the driver contract
    /// carries statement identity, so a driver is free to do this.
    pub fn set_late_cancel_lands_on_next_statement(&self, latch: bool) {
        self.lock().late_cancel_lands_on_next_statement = latch;
    }

    /// Registers `action` for statements whose trimmed text equals `sql`
    /// exactly.
    pub fn on_sql(&self, sql: impl Into<String>, action: Action) {
        self.lock()
            .responses
            .push((Matcher::Exact(sql.into()), action));
    }

    /// Registers `action` for statements whose text satisfies `predicate`.
    pub fn on_predicate<F>(&self, predicate: F, action: Action)
    where
        F: Fn(&str) -> bool + Send + Sync + 'static,
    {
        self.lock()
            .responses
            .push((Matcher::Predicate(Arc::new(predicate)), action));
    }

    /// Pre-populates a table's committed rows, as if an earlier, already
    /// committed connection had inserted them.
    pub fn seed_table(&self, table: impl Into<String>, rows: Vec<Vec<ScriptValue>>) {
        self.lock()
            .tables
            .entry(table.into())
            .or_default()
            .extend(rows);
    }

    /// The table's currently committed rows, for test assertions.
    #[must_use]
    pub fn committed_rows(&self, table: &str) -> Vec<Vec<ScriptValue>> {
        self.lock().tables.get(table).cloned().unwrap_or_default()
    }

    /// Every distinct thread that has executed driver code for `connection`.
    ///
    /// Tests use this to assert that all driver calls for one connection
    /// landed on a single worker thread, and that it was never the test's own
    /// calling thread (`docs/decisions/0002-driver-api-and-concurrency-model.md`
    /// D1/D2).
    #[must_use]
    pub fn thread_ids_seen(&self, connection: ConnectionId) -> Vec<ThreadId> {
        self.lock()
            .thread_ids
            .get(&connection)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    /// How many cursors and connections were opened and closed.
    ///
    /// Lets a test assert that a handle `db-core` could no longer reach was
    /// *released*, not merely forgotten.
    #[must_use]
    pub fn counts(&self) -> Counts {
        self.lock().counts
    }

    pub(crate) fn record_thread(&self, connection: ConnectionId) {
        self.lock()
            .thread_ids
            .entry(connection)
            .or_default()
            .insert(std::thread::current().id());
    }

    pub(crate) fn record_cursor_opened(&self) {
        self.lock().counts.cursors_opened += 1;
    }

    pub(crate) fn record_cursor_closed(&self) {
        self.lock().counts.cursors_closed += 1;
    }

    /// One `connect` returned a connection. Both counters move under the same
    /// lock, so a test that waits on `connects_finished` can never observe it
    /// ahead of the connection it counts.
    pub(crate) fn record_connection_opened(&self) {
        let mut inner = self.lock();
        inner.counts.connections_opened += 1;
        inner.counts.connects_finished += 1;
    }

    /// One `connect` returned without a connection.
    pub(crate) fn record_connect_failed(&self) {
        self.lock().counts.connects_finished += 1;
    }

    pub(crate) fn record_connection_closed(&self) {
        self.lock().counts.connections_closed += 1;
    }

    pub(crate) fn connect_behavior(&self) -> Result<(), DbError> {
        // Taken out of the lock before parking: holding the scenario's mutex
        // across a park would stop every other connection in the scenario,
        // including the test thread asking what it counted.
        let gate = self.lock().connect_gate.clone();
        if let Some(gate) = gate {
            // No deadline and no cancel observation: a connect cannot be
            // interrupted (ADR-0002 H1, spike U-15), which is the property
            // the tests using this exist to exercise.
            let _ = gate.park(None, false);
        }
        let behavior = self.lock().connect.clone();
        if matches!(behavior, Behavior::Panic(_)) {
            // Recorded *before* the panic, because after it there is no code
            // left on this thread to record anything: a test that waits on
            // `Counts::connects_finished` — the only way to know every connect
            // has run without sleeping — would otherwise hang forever on the
            // `panic_connect` scenario.
            self.record_connect_failed();
        }
        behavior.apply()
    }

    pub(crate) fn ping_behavior(&self) -> Result<(), DbError> {
        self.lock().ping.apply()
    }

    pub(crate) fn cancel_behavior(&self) -> Result<(), DbError> {
        self.lock().cancel.apply()
    }

    pub(crate) fn commit_behavior(&self) -> Result<(), DbError> {
        self.lock().commit.apply()
    }

    pub(crate) fn rollback_behavior(&self) -> Result<(), DbError> {
        self.lock().rollback.apply()
    }

    pub(crate) fn savepoint_behavior(&self) -> Result<(), DbError> {
        self.lock().savepoint.apply()
    }

    pub(crate) fn close_behavior(&self) -> Result<(), DbError> {
        self.lock().close.apply()
    }

    pub(crate) fn invalidates_handles_on_transaction_end(&self) -> bool {
        self.lock().invalidate_handles_on_transaction_end
    }

    pub(crate) fn latches_late_cancel(&self) -> bool {
        self.lock().late_cancel_lands_on_next_statement
    }

    pub(crate) fn find_action(&self, sql: &str) -> Option<Action> {
        self.lock()
            .responses
            .iter()
            .find(|(matcher, _)| matcher.matches(sql))
            .map(|(_, action)| action.clone())
    }

    pub(crate) fn apply_commit(&self, ops: &[(String, Vec<ScriptValue>)]) {
        let mut inner = self.lock();
        for (table, row) in ops {
            inner
                .tables
                .entry(table.clone())
                .or_default()
                .push(row.clone());
        }
    }
}

/// The transaction epoch a connection's derived handles were created in.
///
/// Bumped on every `commit`, `rollback` and `rollback to savepoint`; a cursor
/// or LOB stream created in an earlier epoch reports
/// [`ErrorKind::Transaction`] when
/// [`Scenario::set_invalidate_handles_on_transaction_end`] is on.
#[derive(Debug, Clone)]
pub(crate) struct TransactionEpoch {
    counter: Arc<AtomicU64>,
    created_at: u64,
    invalidate: bool,
}

impl TransactionEpoch {
    pub(crate) fn new(counter: &Arc<AtomicU64>, invalidate: bool) -> Self {
        Self {
            counter: Arc::clone(counter),
            created_at: counter.load(Ordering::SeqCst),
            invalidate,
        }
    }

    /// The error a handle from an earlier transaction must report, if any.
    pub(crate) fn check(&self, handle: &str) -> Option<DbError> {
        if !self.invalidate || self.counter.load(Ordering::SeqCst) == self.created_at {
            return None;
        }
        Some(DbError::new(
            ErrorKind::Transaction,
            format!(
                "reldex-driver-mock: this {handle} was invalidated by the commit or rollback \
                 that ended the transaction it was opened in"
            ),
        ))
    }
}
