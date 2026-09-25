//! The public session API: [`SessionManager`], [`DatabaseSession`] and the
//! request/completion shape built on top of the worker thread in
//! [`crate::worker`].

use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use reldex_db_driver_api::{
    CancelKind, CancelOutcome, Column, ConnectionId, ConnectionParams, DatabaseDriver, DbError,
    DbResult, RowBatch, SavepointName, ServerOutputSetting, Statement, StatementKind, ValueRef,
    Warning,
};

use crate::events::{CompletedOperation, EventSink, RequestId};
use crate::ids::{LobHandle, ResultId, SessionId};
use crate::reply::{CloseReplyTo, ReplyPayload, ReplyTo};
use crate::shared::{SessionLifecycle, SessionShared};
use crate::worker::{self, CloseIntent, Command};

/// How long [`Drop`] waits for a session's worker thread before detaching it.
///
/// Dropping a session must never hang, and a worker blocked inside a driver
/// call cannot be interrupted by `db-core` — only asked to stop, through the
/// driver's own [`reldex_db_driver_api::CancelHandle`], which on a
/// [`CancelKind::PreArmedDeadline`] driver can do nothing at all. So drop asks,
/// waits this long, and then lets go.
///
/// Half a second is long enough for an idle session, or one whose driver
/// honoured the cancel, to shut down cleanly on a loaded machine, and short
/// enough that closing a worksheet never looks like a freeze. Past it the
/// worker thread is **detached**, not killed: it still owns the connection,
/// every cursor and every parked large object, and it closes all of them when
/// the blocked call finally returns. Nothing leaks; the release is just late.
pub const DROP_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);

/// Where the reply to an abandon issued by
/// [`DatabaseSession::begin_abandon`] arrives.
///
/// Split out from the issuing call so that a caller ending *many* sessions can
/// issue every abandon first and then wait for all of them against one
/// deadline — see [`crate::SessionRegistry`]'s `Drop`.
pub(crate) type AbandonReply = mpsc::Receiver<Result<(), CloseError>>;

/// One value a statement wrote back through an output bind.
///
/// Mirrors [`reldex_db_driver_api::Value`] with two substitutions: a nested
/// `REF CURSOR` and a large-object locator are both handles derived from the
/// connection, so neither ever leaves the worker thread. `db-core` keeps them
/// and reports a [`ResultId`] or a [`LobHandle`] instead (ADR-0002 D1/D2).
#[derive(Debug)]
pub enum OutValue {
    /// Plain data. Never `reldex_db_driver_api::Value::Cursor` and never
    /// `reldex_db_driver_api::Value::Lob`.
    Value(reldex_db_driver_api::Value),
    /// A nested result set, open on this session's worker thread. Fetch it with
    /// [`DatabaseSession::fetch_batch`] and release it with
    /// [`DatabaseSession::close_result`], like any other result.
    Result(ResultId),
    /// A large object, parked on this session's worker thread. Read it with
    /// [`DatabaseSession::read_lob_chunk`] and release it with
    /// [`DatabaseSession::close_lob`].
    Lob(LobHandle),
}

/// The values a statement wrote back through output binds, in the shape its
/// binds had.
///
/// The core-side mirror of [`reldex_db_driver_api::OutValues`]; see
/// [`OutValue`] for what changes on the way through.
#[derive(Debug)]
pub enum OutValues {
    /// The statement had no output binds.
    None,
    /// Index-aligned with the statement's positional binds; `None` marks a bind
    /// that was input-only.
    Positional(Vec<Option<OutValue>>),
    /// Named output binds, without their placeholder prefix.
    Named(Vec<(Box<str>, OutValue)>),
}

impl OutValues {
    /// The value written back for a positional bind.
    #[must_use]
    pub fn positional(&self, index: usize) -> Option<&OutValue> {
        match self {
            Self::Positional(values) => values.get(index)?.as_ref(),
            Self::None | Self::Named(_) => None,
        }
    }

    /// The value written back for a named bind.
    #[must_use]
    pub fn named(&self, name: &str) -> Option<&OutValue> {
        match self {
            Self::Named(values) => values
                .iter()
                .find(|(key, _)| &**key == name)
                .map(|(_, value)| value),
            Self::None | Self::Positional(_) => None,
        }
    }

    /// Whether anything was written back.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::None => true,
            Self::Positional(values) => values.iter().all(Option::is_none),
            Self::Named(values) => values.is_empty(),
        }
    }
}

/// Everything one [`DatabaseSession::execute`] produced.
///
/// Mirrors [`reldex_db_driver_api::ExecutionOutcome`], with the driver's
/// cursor replaced by the [`ResultId`] `db-core` now owns on the worker
/// thread — the cursor itself never leaves it (ADR-0002 D1/D2). A nested
/// `REF CURSOR` or a large object returned through an output bind is handled
/// the same way; see [`OutValue`].
/// `#[non_exhaustive]` so a later field — M2.5's event queue will want at
/// least one — is an additive change rather than a breaking one. Nothing
/// outside `db-core` constructs an `ExecuteOutcome`; the worker thread is its
/// only producer, so the attribute costs nothing today.
#[derive(Debug)]
#[non_exhaustive]
pub struct ExecuteOutcome {
    /// The result set's id, if the statement produced one. Pass this to
    /// [`DatabaseSession::fetch_batch`] and, once done,
    /// [`DatabaseSession::close_result`].
    pub result: Option<ResultId>,
    /// The result set's columns, in select-list order; empty when the
    /// statement produced no result.
    ///
    /// The driver's `Cursor` never leaves the worker thread (ADR-0002 D1/D2)
    /// and [`FetchedBatch`] carries only storage kinds, so this is the one
    /// place a caller can learn a column's *name* and declared type. It is
    /// copied off the cursor on the worker thread at execute time — plain data,
    /// like everything else that crosses the boundary — because a grid needs
    /// headers before its first row arrives.
    pub columns: Vec<reldex_db_driver_api::ColumnMetadata>,
    /// How many rows the statement changed, if the driver reported it.
    pub rows_affected: Option<u64>,
    /// What kind of statement the server ran, as far as the driver can tell.
    pub statement_kind: StatementKind,
    /// Whether the server committed the transaction as a side effect of this
    /// statement regardless of any client setting (`SPEC.md` §10; typically
    /// DDL). `db-core` has already reset its own tracking; the caller still
    /// needs to know, so it can tell the user.
    pub committed_implicitly: bool,
    /// Non-fatal messages the statement produced (for example, PL/SQL
    /// "compiled with errors").
    pub warnings: Vec<Warning>,
    /// The values the statement wrote back through output binds. A nested
    /// `REF CURSOR` or large object among them is already registered on this
    /// session; see [`OutValue`].
    pub out_values: OutValues,
}

/// Server output collected for requests answered through a [`Completion`].
///
/// The event path delivers server output as
/// [`crate::SessionEvent::ServerOutput`]; a request answered through a
/// `Completion` has no stream to put it on, so the worker appends it here
/// instead, **before** it answers that request — once
/// [`Completion::wait`] returns, the statement's output is already in place,
/// including the output of a statement that failed after printing. Read it
/// with [`DatabaseSession::take_server_output`].
///
/// Bounded, and honest about it: at most
/// [`ServerOutputLog::MAX_RETAINED_LINES`] lines and
/// [`ServerOutputLog::MAX_RETAINED_BYTES`] bytes are kept between two takes.
/// The kept lines are always a **prefix** of the output: from the first line
/// that does not fit, every later line is counted in
/// [`ServerOutputLog::dropped`] instead, even one small enough to fit, until
/// the next take. The log is per session, not per statement; see
/// [`DatabaseSession::take_server_output`]. This path is for tools and tests.
/// A UI uses the event path, whose bound is the consumer's own drain.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct ServerOutputLog {
    /// The lines, oldest first. An empty line is an empty string.
    pub lines: Vec<Box<str>>,
    /// How many lines were discarded because the log was full.
    pub dropped: u32,
    /// The first read that failed since the last take: the output after it
    /// may be incomplete. `None` when every read succeeded.
    pub failure: Option<DbError>,
    /// How many reads failed since the last take, including the one in
    /// [`ServerOutputLog::failure`].
    pub failures: u32,
    /// How many lines, across every read since the last take, were not valid
    /// UTF-8 on the wire and were delivered with U+FFFD in place of the
    /// invalid bytes rather than dropped — the completion-path twin of
    /// [`crate::SessionEvent::ServerOutput`]'s `invalid_utf8_lines`.
    ///
    /// This is the full count for every line **drained from the server**,
    /// not only the ones [`ServerOutputLog::lines`] had room to retain: once
    /// the bound above starts refusing lines (see [`ServerOutputLog::dropped`]),
    /// a refused line's own validity cannot be attributed individually, so
    /// the count is never reduced to account for it — consistent with
    /// `dropped` itself never hiding what was lost. Zero normally.
    pub invalid_utf8_lines: u32,
}

impl ServerOutputLog {
    /// The most lines kept between two takes.
    pub const MAX_RETAINED_LINES: usize = 10_000;

    /// The most bytes of text kept between two takes.
    pub const MAX_RETAINED_BYTES: usize = 1024 * 1024;

    /// Whether nothing was collected: no lines, no drops, no failure, no
    /// invalid UTF-8.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
            && self.dropped == 0
            && self.failures == 0
            && self.invalid_utf8_lines == 0
    }
}

/// One fetched batch: plain data, plus a handle for every large object the
/// worker took out of it.
///
/// A driver puts live [`reldex_db_driver_api::LobLocator`]s straight into the
/// batch's LOB columns, and a locator is a handle derived from the connection —
/// using it, or even *dropping* it, off the owning worker thread runs driver
/// code on the wrong thread (ADR-0002 D1/D2). So before a batch is sent to the
/// caller, the worker takes every locator out of it (the contract's
/// [`reldex_db_driver_api::Column::take_lob`], which leaves the cell as
/// [`ValueRef::Taken`] rather than faking a NULL) and parks it beside the
/// cursors. What crosses the thread boundary is this: plain data, and opaque
/// [`LobHandle`]s.
///
/// Find the handle for a cell with [`FetchedBatch::lob`], read it with
/// [`DatabaseSession::read_lob_chunk`], and release it with
/// [`DatabaseSession::close_lob`] — or let it die with its result or its
/// session.
#[derive(Debug)]
pub struct FetchedBatch {
    batch: RowBatch,
    /// `(row, column, handle)`, in row-major order.
    lobs: Vec<(usize, usize, LobHandle)>,
}

impl FetchedBatch {
    pub(crate) const fn new(batch: RowBatch, lobs: Vec<(usize, usize, LobHandle)>) -> Self {
        Self { batch, lobs }
    }

    /// How many rows the batch holds.
    #[must_use]
    pub const fn row_count(&self) -> usize {
        self.batch.row_count()
    }

    /// How many columns the batch holds.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.batch.column_count()
    }

    /// Whether the batch holds no rows. An empty batch means the result is
    /// exhausted.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.batch.is_empty()
    }

    /// Borrows one cell. A cell whose large object was parked reads back as
    /// [`ValueRef::Taken`] — never as NULL.
    #[must_use]
    pub fn value(&self, row: usize, column: usize) -> Option<ValueRef<'_>> {
        self.batch.value(row, column)
    }

    /// One column.
    #[must_use]
    pub fn column(&self, index: usize) -> Option<&Column> {
        self.batch.column(index)
    }

    /// The plain-data batch underneath.
    #[must_use]
    pub const fn rows(&self) -> &RowBatch {
        &self.batch
    }

    /// Takes the plain-data batch out, leaving the large-object handles valid.
    #[must_use]
    pub fn into_rows(self) -> RowBatch {
        self.batch
    }

    /// The handle for the large object at `(row, column)`, if that cell held
    /// one.
    ///
    /// `None` for a cell that is NULL, that is not a LOB column, or that is out
    /// of range.
    #[must_use]
    pub fn lob(&self, row: usize, column: usize) -> Option<LobHandle> {
        self.lobs
            .iter()
            .find(|(r, c, _)| *r == row && *c == column)
            .map(|(_, _, handle)| *handle)
    }

    /// Every large object this batch carried, as `(row, column, handle)`.
    pub fn lobs(&self) -> impl Iterator<Item = (usize, usize, LobHandle)> + '_ {
        self.lobs.iter().copied()
    }
}

/// What to do with a possibly-open transaction when closing a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloseDisposition {
    /// Commit the open transaction before closing.
    Commit,
    /// Roll the open transaction back before closing.
    Rollback,
}

/// Why [`DatabaseSession::close`] did not close the session.
///
/// Three of the four variants leave the session **open and usable**, so the
/// caller can ask the user and try again; only [`CloseError::Failed`] reports a
/// session that is gone. Which step failed is never hidden: destroying a
/// transaction because a commit failed would be exactly the silent data loss
/// `SPEC.md` §10 forbids.
#[derive(Debug)]
pub enum CloseError {
    /// A transaction may still be open
    /// ([`DatabaseSession::has_possibly_active_transaction`] was true) and no
    /// [`CloseDisposition`] was given. `SPEC.md` §10: Reldex never silently
    /// commits or hides a transaction. The session is unchanged — call
    /// `close` again with `Some(CloseDisposition::Commit)` or
    /// `Some(CloseDisposition::Rollback)`.
    ///
    /// The decision is made on the worker thread, *after* every request queued
    /// ahead of the close has run, so it cannot be taken against a stale view
    /// of the transaction.
    DecisionRequired,
    /// The commit asked for by [`CloseDisposition::Commit`] failed, so the
    /// transaction is still there and still uncommitted. **The session is left
    /// open**: the caller can retry the commit, roll back instead, or ask the
    /// user again.
    CommitFailed(DbError),
    /// The rollback asked for by [`CloseDisposition::Rollback`] failed. **The
    /// session is left open**, for the same reason as
    /// [`CloseError::CommitFailed`].
    RollbackFailed(DbError),
    /// The transaction was resolved (or there was none) but closing the
    /// connection failed — or the session was already lost, in which case
    /// nothing was committed and whatever transaction it held is gone. The
    /// session is closed either way and must not be reused.
    Failed(DbError),
}

impl CloseError {
    /// Whether the session is still open and can be used, or closed again.
    #[must_use]
    pub const fn session_is_still_open(&self) -> bool {
        matches!(
            self,
            Self::DecisionRequired | Self::CommitFailed(_) | Self::RollbackFailed(_)
        )
    }
}

impl fmt::Display for CloseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DecisionRequired => f.write_str(
                "reldex-db-core: a transaction may be open; close needs an explicit disposition",
            ),
            Self::CommitFailed(err) => write!(
                f,
                "reldex-db-core: close could not commit, so the transaction is unchanged and \
                 the session is still open: {err}"
            ),
            Self::RollbackFailed(err) => write!(
                f,
                "reldex-db-core: close could not roll back, so the transaction is unchanged \
                 and the session is still open: {err}"
            ),
            Self::Failed(err) => write!(f, "reldex-db-core: close failed: {err}"),
        }
    }
}

impl std::error::Error for CloseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::DecisionRequired => None,
            Self::CommitFailed(err) | Self::RollbackFailed(err) | Self::Failed(err) => Some(err),
        }
    }
}

/// A pending request's result, from a per-request reply channel
/// (`docs/decisions/0002-driver-api-and-concurrency-model.md` D1: "a later
/// FFI/Qt adapter can turn completions into events").
///
/// [`Completion::wait`] blocks the calling thread until the worker replies;
/// [`Completion::poll`] never blocks and [`Completion::wait_timeout`] blocks
/// only for as long as it is told to, so an event loop (the eventual C++/Qt
/// adapter, or a test) can check completions between other work instead of
/// dedicating a thread to each one.
///
/// All three **consume** the completion. That is deliberate: the reply exists
/// exactly once and is not [`Clone`], so a method that could hand it out and
/// still leave a `Completion` behind would have to fabricate something for the
/// second caller. `poll` and `wait_timeout` therefore hand the completion back
/// in their `Err` when the answer has not arrived, and nothing is ever lost.
pub struct Completion<T> {
    rx: mpsc::Receiver<DbResult<T>>,
}

impl<T> Completion<T> {
    fn worker_vanished() -> DbError {
        DbError::internal("reldex-db-core: the session worker ended without replying")
    }

    /// Blocks until the request completes.
    ///
    /// # Errors
    ///
    /// Whatever [`DbError`] the request produced, or a
    /// [`reldex_db_driver_api::ErrorKind::DriverInternal`] error if the
    /// worker thread ended without replying at all (a bug, not an expected
    /// outcome; ordinary session loss always replies).
    pub fn wait(self) -> DbResult<T> {
        self.rx
            .recv()
            .unwrap_or_else(|_| Err(Self::worker_vanished()))
    }

    /// Takes the result if it has arrived, and hands the completion back if it
    /// has not.
    ///
    /// # Errors
    ///
    /// `Err(self)` while the request is still queued or running — not a
    /// failure, just "not yet". The *request's* own failure arrives as
    /// `Ok(Err(..))`.
    pub fn poll(self) -> Result<DbResult<T>, Self> {
        match self.rx.try_recv() {
            Ok(value) => Ok(value),
            Err(mpsc::TryRecvError::Empty) => Err(self),
            Err(mpsc::TryRecvError::Disconnected) => Ok(Err(Self::worker_vanished())),
        }
    }

    /// Blocks for at most `timeout`, then hands the completion back.
    ///
    /// The request keeps running; nothing is cancelled by giving up on the
    /// wait. To actually stop a statement, use [`DatabaseSession::cancel`].
    ///
    /// # Errors
    ///
    /// `Err(self)` if `timeout` elapsed before the worker replied.
    pub fn wait_timeout(self, timeout: Duration) -> Result<DbResult<T>, Self> {
        match self.rx.recv_timeout(timeout) {
            Ok(value) => Ok(value),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(self),
            Err(mpsc::RecvTimeoutError::Disconnected) => Ok(Err(Self::worker_vanished())),
        }
    }
}

impl<T> fmt::Debug for Completion<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Completion").finish_non_exhaustive()
    }
}

/// Per-session resource bounds.
///
/// A session's command queue is unbounded on purpose (see the `worker` module),
/// so what has to be bounded instead is what a session can *accumulate*. These
/// are deliberately simple caps with clear errors rather than a policy engine:
/// the point is that a runaway caller gets a reportable failure instead of an
/// unbounded allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionLimits {
    max_open_results: NonZeroUsize,
    max_lob_chunk_bytes: NonZeroUsize,
    max_outstanding_requests: NonZeroUsize,
    server_output_chunk_lines: NonZeroUsize,
    server_output_chunk_bytes: NonZeroUsize,
}

impl SessionLimits {
    /// How many results one session may hold open before
    /// [`DatabaseSession::execute`] refuses to open another.
    pub const DEFAULT_MAX_OPEN_RESULTS: NonZeroUsize = match NonZeroUsize::new(256) {
        Some(value) => value,
        None => unreachable!(),
    };

    /// The largest buffer one [`DatabaseSession::read_lob_chunk`] may allocate,
    /// however much the caller asks for.
    pub const DEFAULT_MAX_LOB_CHUNK_BYTES: NonZeroUsize = match NonZeroUsize::new(16 * 1024 * 1024)
    {
        Some(value) => value,
        None => unreachable!(),
    };

    /// How many event-path requests one session may have accepted and
    /// **undrained** before [`DatabaseSession::submit_execute`] and its
    /// siblings refuse another.
    ///
    /// This is what bounds the reply events one session can put in an
    /// [`crate::EventQueue`]: a request takes a slot when it is accepted and
    /// gives it back when the consumer takes its reply *out of* the queue, so
    /// a consumer that stops draining stops the submitter rather than letting
    /// the queue grow. [`DatabaseSession::submit_close`] is the one exemption
    /// and gets one slot above the limit, because a session at its limit must
    /// still be able to end. 1,024 is far more than a worksheet ever has in flight
    /// (a handful of fetches, at most) and small enough that a runaway
    /// submitter is reported rather than allowed to grow the queue without
    /// limit.
    ///
    /// [`crate::SessionRegistry::open`] reserves an ordinary slot for the
    /// open, on a session that has nothing outstanding, so it fits inside this
    /// limit and cannot be refused;
    /// [`crate::SessionRegistry::abandon`] reserves none at all, which is what
    /// makes it impossible to refuse. The published bound
    /// (`3R + U + 3` events per session since M2.7, `2R + U + 3` for a session
    /// whose server output is off; see [`crate::EventQueue`]) is therefore
    /// unchanged by either.
    ///
    /// It does **not** apply to [`Completion`]-path calls: those hold their
    /// reply in the caller's own `Completion`, so they are bounded by the
    /// caller, and the command queue itself stays unbounded on purpose
    /// (ADR-0002 K9).
    pub const DEFAULT_MAX_OUTSTANDING_REQUESTS: NonZeroUsize = match NonZeroUsize::new(1024) {
        Some(value) => value,
        None => unreachable!(),
    };

    /// The most server-output lines one read may return, and therefore the
    /// most one [`crate::SessionEvent::ServerOutput`] carries.
    ///
    /// A read is one round trip whatever it returns, so this and
    /// [`SessionLimits::DEFAULT_SERVER_OUTPUT_CHUNK_BYTES`] are what bound the
    /// memory one read can take on the worker. 4,096 lines is more than the
    /// byte bound lets through for any line longer than three characters, so
    /// in practice bytes decide and this only stops a flood of empty lines.
    pub const DEFAULT_SERVER_OUTPUT_CHUNK_LINES: NonZeroUsize = match NonZeroUsize::new(4096) {
        Some(value) => value,
        None => unreachable!(),
    };

    /// The text one server-output read aims to return, in bytes.
    ///
    /// A target the driver fills up to, not a splitter: lines are returned
    /// whole, and a read may carry one more line beyond the target — the one
    /// that did not fit — so one read holds at most this plus one
    /// maximum-length line (ADR-0002 amendment T1). 32 KiB matches the primary
    /// driver's largest single transfer, so the default costs it no extra
    /// round trips.
    pub const DEFAULT_SERVER_OUTPUT_CHUNK_BYTES: NonZeroUsize = match NonZeroUsize::new(32 * 1024) {
        Some(value) => value,
        None => unreachable!(),
    };

    /// The default limits.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_open_results: Self::DEFAULT_MAX_OPEN_RESULTS,
            max_lob_chunk_bytes: Self::DEFAULT_MAX_LOB_CHUNK_BYTES,
            max_outstanding_requests: Self::DEFAULT_MAX_OUTSTANDING_REQUESTS,
            server_output_chunk_lines: Self::DEFAULT_SERVER_OUTPUT_CHUNK_LINES,
            server_output_chunk_bytes: Self::DEFAULT_SERVER_OUTPUT_CHUNK_BYTES,
        }
    }

    /// Sets the most server-output lines one read may return.
    #[must_use]
    pub const fn with_server_output_chunk_lines(mut self, lines: NonZeroUsize) -> Self {
        self.server_output_chunk_lines = lines;
        self
    }

    /// Sets the text one server-output read aims to return.
    #[must_use]
    pub const fn with_server_output_chunk_bytes(mut self, bytes: NonZeroUsize) -> Self {
        self.server_output_chunk_bytes = bytes;
        self
    }

    /// The most server-output lines one read may return.
    #[must_use]
    pub const fn server_output_chunk_lines(self) -> NonZeroUsize {
        self.server_output_chunk_lines
    }

    /// The text one server-output read aims to return, in bytes.
    #[must_use]
    pub const fn server_output_chunk_bytes(self) -> NonZeroUsize {
        self.server_output_chunk_bytes
    }

    /// Sets how many results one session may hold open at once.
    #[must_use]
    pub const fn with_max_open_results(mut self, results: NonZeroUsize) -> Self {
        self.max_open_results = results;
        self
    }

    /// Sets the largest buffer one LOB read may allocate.
    #[must_use]
    pub const fn with_max_lob_chunk_bytes(mut self, bytes: NonZeroUsize) -> Self {
        self.max_lob_chunk_bytes = bytes;
        self
    }

    /// How many results one session may hold open at once.
    #[must_use]
    pub const fn max_open_results(self) -> NonZeroUsize {
        self.max_open_results
    }

    /// The largest buffer one LOB read may allocate.
    #[must_use]
    pub const fn max_lob_chunk_bytes(self) -> NonZeroUsize {
        self.max_lob_chunk_bytes
    }

    /// Sets how many event-path requests one session may have outstanding.
    #[must_use]
    pub const fn with_max_outstanding_requests(mut self, requests: NonZeroUsize) -> Self {
        self.max_outstanding_requests = requests;
        self
    }

    /// How many event-path requests one session may have outstanding.
    #[must_use]
    pub const fn max_outstanding_requests(self) -> NonZeroUsize {
        self.max_outstanding_requests
    }
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self::new()
    }
}

/// A stable, stateful database session (`SPEC.md` §6, §9).
///
/// Owns one dedicated worker thread that owns the driver's
/// `Box<dyn DatabaseConnection>`, every cursor derived from it and every
/// large-object locator it produced, for the session's whole lifetime; no
/// database or network call this type makes ever runs on the calling thread
/// (ADR-0002 D1/D2). Requests submitted from any thread are processed by that
/// worker strictly in the order they were sent — see the `worker` module for
/// the queueing policy, including what happens to requests queued behind a
/// blocked statement.
///
/// Cloning is deliberately not offered: a session is owned once, matching
/// "a worksheet owns a stable database session" (`SPEC.md` §9). It is `Send`
/// **and** `Sync`, so several threads may submit work concurrently through a
/// shared `&DatabaseSession` (for example behind an `Arc`); every method here,
/// including [`DatabaseSession::close`], takes `&self`.
pub struct DatabaseSession {
    id: SessionId,
    connection_id: ConnectionId,
    /// `std::sync::mpsc::Sender` is `Send` but not `Sync`, so sharing a session
    /// between threads — which this type documents and tests — needs this lock.
    /// It is only ever held across a channel `send`, which never blocks.
    command_tx: Mutex<mpsc::Sender<Command>>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    shared: Arc<SessionShared>,
    cancel_handle: Arc<dyn reldex_db_driver_api::CancelHandle>,
    cancel_kind: CancelKind,
    /// Collected once, on the worker thread, right after `connect` returned.
    /// Fixed for the session's lifetime, so this needs no lock.
    connect_warnings: Vec<Warning>,
    /// Stamped by the [`SessionManager`] that opened this session; read on the
    /// event-path submit to bound outstanding requests.
    limits: SessionLimits,
}

impl DatabaseSession {
    /// Builds the handle for a worker whose `connect` has just succeeded.
    ///
    /// Both openers go through here — the blocking [`SessionManager`] and the
    /// non-blocking [`crate::SessionRegistry`] — so there is one definition of
    /// what a session handle is made of, and the two paths cannot drift.
    pub(crate) fn assemble(
        id: SessionId,
        command_tx: mpsc::Sender<Command>,
        join: thread::JoinHandle<()>,
        shared: Arc<SessionShared>,
        ready: worker::Ready,
        limits: SessionLimits,
    ) -> Self {
        Self {
            id,
            connection_id: ready.connection_id,
            command_tx: Mutex::new(command_tx),
            worker: Mutex::new(Some(join)),
            shared,
            cancel_handle: ready.cancel_handle,
            cancel_kind: ready.cancel_kind,
            connect_warnings: ready.connect_warnings,
            limits,
        }
    }

    /// This session's identifier, stable for its whole lifetime.
    #[must_use]
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// The id of the underlying driver connection this session owns.
    #[must_use]
    pub fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    /// How this session's driver implements cancellation. Constant for the
    /// session's lifetime; check before offering a Cancel control
    /// (`SPEC.md` §24.8).
    #[must_use]
    pub fn cancel_kind(&self) -> CancelKind {
        self.cancel_kind
    }

    /// Non-fatal findings the driver produced while **opening** this session.
    ///
    /// Collected once on the worker thread immediately after
    /// [`reldex_db_driver_api::DatabaseDriver::connect`] returned
    /// ([`reldex_db_driver_api::DatabaseConnection::take_connect_warnings`]),
    /// so a session that is opened, pinged and closed without ever running a
    /// statement still reports them. Typically a transport or profile parameter
    /// the driver cannot honour but that does not make the session any less
    /// safe than it was asked to be; anything that *does* is a failed `connect`
    /// instead.
    ///
    /// Constant for the session's lifetime, so it is borrowed rather than
    /// taken: reading it late — after the UI has a window to show it in — must
    /// not be the same as losing it. Never mixed into
    /// [`ExecuteOutcome::warnings`], which belongs to one statement.
    #[must_use]
    pub fn connect_warnings(&self) -> &[Warning] {
        &self.connect_warnings
    }

    /// Requests that the currently running statement, if any, stop.
    ///
    /// Callable from any thread while another thread is blocked in
    /// [`Completion::wait`] on this session's `execute` or `fetch_batch`; it
    /// never goes through the request queue, which is the point — a queue a
    /// blocked call can stall would defeat cancellation
    /// (`docs/decisions/0002-driver-api-and-concurrency-model.md` D2).
    ///
    /// # Which statement does a cancel hit?
    ///
    /// The driver contract has no statement identity: `request_cancel` cancels
    /// "whatever is running". A cancel issued just as a statement finishes can
    /// therefore be latched by a driver and land on the *next* one. `db-core`
    /// narrows that window as far as the contract allows — when it can see that
    /// no command is in flight it answers with the idempotent
    /// [`CancelOutcome::Requested`] no-op the contract permits, instead of
    /// handing the request to the driver — but it **cannot close it**: the
    /// statement may finish between that check and the driver's own. A caller
    /// that receives [`reldex_db_driver_api::ErrorKind::Cancelled`] for a
    /// statement it did not cancel is seeing that race, not a `db-core` bug.
    /// Closing it needs statement identity in the driver contract, which
    /// ADR-0002 does not have and the primary driver could not implement today
    /// (spike S4).
    ///
    /// Whatever the driver reports — including an error carrying
    /// [`reldex_db_driver_api::SessionState::Lost`] — is fed into this
    /// session's state before it is returned, so a cancel that discovers a dead
    /// session is not a fact the caller has to notice and re-report.
    ///
    /// # Errors
    ///
    /// Whatever the driver's [`reldex_db_driver_api::CancelHandle::request_cancel`]
    /// returns.
    pub fn cancel(&self) -> DbResult<CancelOutcome> {
        if self.cancel_kind == CancelKind::Native && !self.shared.driver_call_in_flight() {
            // Nothing is running, so there is nothing to interrupt. The
            // contract calls this a successful no-op (rule 4), and answering it
            // here keeps a driver from latching the request onto the next
            // statement.
            return Ok(CancelOutcome::Requested);
        }
        match self.cancel_handle.request_cancel() {
            Ok(outcome) => Ok(outcome),
            Err(err) => {
                self.shared.note_error(&err);
                Err(err)
            }
        }
    }

    /// Where the session is in its lifecycle, as of the last completed request.
    ///
    /// Never reports [`SessionLifecycle::Usable`] once the session has been
    /// closed or lost (`SPEC.md` §18).
    #[must_use]
    pub fn session_state(&self) -> SessionLifecycle {
        self.shared.lifecycle()
    }

    /// Whether the session is terminally lost. Once true it stays true:
    /// `db-core` never silently reconnects or replaces a session — open a new
    /// one instead (`SPEC.md` §18).
    #[must_use]
    pub fn is_lost(&self) -> bool {
        self.shared.is_lost()
    }

    /// Whether the session has been closed. A session that was *lost* and then
    /// closed keeps reporting [`SessionLifecycle::Lost`], because why it ended
    /// matters more than that it ended.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.lifecycle() == SessionLifecycle::Closed
    }

    /// Whether a transaction may still be open, combining the driver's own
    /// report with core-side tracking (ADR-0002 D4/D6). Conservative by
    /// design: this can be true when nothing is actually open, but is never
    /// false while something might be.
    ///
    /// Reflects the state as of the last request this handle has observed
    /// complete; a request submitted concurrently and not yet awaited is not
    /// reflected yet. **A `false` from this method is therefore a lower bound,
    /// not a verdict**: a statement already queued can still open a transaction
    /// after it is read.
    ///
    /// Nothing decides anything irreversible from this answer.
    /// [`DatabaseSession::close`] re-checks on the worker thread once the queue
    /// has drained (ADR-0002 K4), and the authoritative report of whether a
    /// session took an unresolved transaction with it is
    /// `transaction_possibly_lost` on that session's
    /// [`crate::SessionEvent::Terminal`], which is likewise computed on the
    /// worker thread at the point the session ends. Use this to drive a UI
    /// affordance; use `Terminal` to tell the user what happened.
    #[must_use]
    pub fn has_possibly_active_transaction(&self) -> bool {
        self.shared.has_possibly_active_transaction()
    }

    /// The conservative, synchronous "might this session be carrying work?"
    /// hint behind [`crate::Abandoned::Open`].
    ///
    /// Deliberately wider than
    /// [`DatabaseSession::has_possibly_active_transaction`]: it also counts
    /// requests the consumer has submitted but not drained and a driver call
    /// the worker is inside, because either can open a transaction *after* the
    /// snapshot is taken. It is still only a lower bound — the value that
    /// settles the question arrives on `Terminal`.
    pub(crate) fn may_be_carrying_work(&self) -> bool {
        self.shared.has_possibly_active_transaction()
            || self.shared.outstanding() > 0
            || self.shared.driver_call_in_flight()
    }

    fn send(&self, command: Command) -> bool {
        self.command_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .send(command)
            .is_ok()
    }

    fn submit<T: ReplyPayload>(
        &self,
        subject: T::Subject,
        make_command: impl FnOnce(ReplyTo<T>) -> Command,
    ) -> Completion<T> {
        let (tx, rx) = mpsc::channel();
        let command = make_command(ReplyTo::one_shot(tx.clone(), subject));
        if !self.send(command) {
            let _ = tx.send(Err(self.shared.terminal_error()));
        }
        Completion { rx }
    }

    /// The event-path twin of [`DatabaseSession::submit`].
    ///
    /// Everything that can refuse a request happens *before* the reply channel
    /// exists, so a refusal accepts nothing and produces no event. Once the
    /// [`ReplyTo`] is built, exactly one event follows however the command
    /// ends — including the case where it cannot be delivered at all, where
    /// the dropped reply channel emits the failure itself (see
    /// [`crate::reply`]).
    fn submit_event<T: ReplyPayload>(
        &self,
        request: RequestId,
        subject: T::Subject,
        make_command: impl FnOnce(ReplyTo<T>) -> Command,
    ) -> DbResult<()> {
        self.check_event_route()?;
        self.shared
            .reserve_request(self.limits.max_outstanding_requests().get())?;
        let reply = ReplyTo::event(Arc::clone(&self.shared), self.id, request, subject);
        let _ = self.send(make_command(reply));
        Ok(())
    }

    /// Asks the worker to release this session **without waiting for it and
    /// without committing** — what [`crate::SessionRegistry::abandon`] does to
    /// a session that is already open.
    ///
    /// The same [`CloseIntent::Abandon`] [`Drop`] uses, and the same
    /// guarantees: it never commits and never rolls back explicitly, so the
    /// server's own rollback-on-disconnect is what resolves any transaction
    /// the session held (ADR-0002 K5). The difference from `Drop` is the wait:
    /// there is none. Nothing is joined, nothing is parked on a reply, and a
    /// worker inside an uninterruptible driver call simply runs the abandon
    /// when that call returns — at which point it emits this session's one
    /// [`crate::SessionEvent::Terminal`].
    ///
    /// No event is produced *here*: the close reply goes to a channel nobody
    /// reads, deliberately, because `abandon` is not a request and the caller
    /// did not give it a [`RequestId`].
    pub(crate) fn abandon_now(&self) {
        drop(self.begin_abandon());
    }

    /// Issues the abandon and hands back the channel its reply will arrive on,
    /// so a caller that wants to wait can — without this call itself waiting.
    ///
    /// `None` means the command could not be delivered, so there is nothing to
    /// wait for: the worker has already gone.
    pub(crate) fn begin_abandon(&self) -> Option<AbandonReply> {
        // First, and before the cancel: a worker inside a server-output drain
        // checks this between round trips, so the drain stops at the next
        // boundary instead of reading a buffer nobody will see (ADR-0002
        // amendment T). It cannot interrupt the read already in progress —
        // nothing can on a driver without a native cancel — which is why
        // `abandon` never waits for the worker.
        self.shared.request_abandon();
        // Best effort, and honest about it: on a driver that cannot interrupt
        // a running call this does nothing at all, exactly as in `Drop`.
        //
        // Skipped once the session has ended, because by then
        // `DatabaseConnection::close` has run. That check narrows the window;
        // it does not close it — a session whose connection was *lost* has had
        // its connection discarded without `has_ended()` being set, and a close
        // can complete between this read and the call below. What makes those
        // cases safe is the driver contract, which requires `request_cancel`
        // after a close to be a harmless no-op (`CancelHandle` rule 9,
        // ADR-0002 amendment R7); this skip is the cheap half.
        if !self.shared.has_ended() {
            let _ = self.cancel();
        }
        let (tx, rx) = mpsc::channel();
        if self.send(Command::Close {
            intent: CloseIntent::Abandon,
            reply: CloseReplyTo::one_shot(tx),
        }) {
            Some(rx)
        } else {
            None
        }
    }

    /// Waits for an abandon issued by [`DatabaseSession::begin_abandon`] until
    /// `deadline`, then takes the worker handle so this session's [`Drop`]
    /// returns immediately.
    ///
    /// This is what lets a caller tearing down *many* sessions spend one
    /// [`DROP_SHUTDOWN_TIMEOUT`] in total rather than one per session: it
    /// issues nothing, so every abandon can already be in flight, and the
    /// deadline is the caller's, shared across all of them.
    ///
    /// Returns whether the worker finished within the deadline. When it did
    /// not, the worker is **detached**, exactly as in `Drop`: it still owns its
    /// connection and closes it when the blocked driver call returns.
    pub(crate) fn finish_abandon(&self, reply: Option<AbandonReply>, deadline: Instant) -> bool {
        let finished = match reply {
            Some(rx) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                rx.recv_timeout(remaining).is_ok()
            }
            // Nothing was issued because there was no worker to issue it to.
            None => true,
        };
        let handle = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            if finished {
                let _ = handle.join();
            } else {
                drop(handle);
            }
        }
        finished
    }

    fn check_event_route(&self) -> DbResult<()> {
        if self.shared.has_events() {
            return Ok(());
        }
        Err(DbError::internal(
            "reldex-db-core: this session has no event queue; call \
             DatabaseSession::bind_events before submitting",
        ))
    }

    // ------------------------------------------------------------- events

    /// Routes this session's replies into `sink` instead of (or as well as)
    /// per-request [`Completion`]s.
    ///
    /// Call once, immediately after opening the session and before submitting
    /// anything. Both paths stay available afterwards — they are the same
    /// commands with a different reply channel — but only requests submitted
    /// through the `submit_*` family produce events. Binding also turns on the
    /// session's unsolicited events
    /// ([`crate::SessionEvent::TransactionStateChanged`],
    /// [`crate::SessionEvent::Executing`]) and its one
    /// [`crate::SessionEvent::Terminal`].
    ///
    /// # Errors
    ///
    /// [`reldex_db_driver_api::ErrorKind::DriverInternal`] if this session
    /// already routes its events somewhere: a second queue would split one
    /// session's event stream in two and break the per-session ordering the
    /// first consumer was promised.
    ///
    /// [`reldex_db_driver_api::ErrorKind::Resource`] if this session has
    /// already announced its end. [`crate::SessionEvent::Terminal`] is emitted
    /// once, at the transition; a queue bound afterwards would never receive
    /// one, so the bind is refused instead of handing back a stream that can
    /// only fail request by request.
    pub fn bind_events(&self, sink: EventSink) -> DbResult<()> {
        self.shared.bind_events(sink)
    }

    /// How many event-path requests this session has accepted and whose reply
    /// the consumer has not yet taken out of the queue, against
    /// [`SessionLimits::max_outstanding_requests`].
    ///
    /// A request still counts once the worker has answered it and until the
    /// reply is drained — that is what makes the limit bound the queue — so
    /// this is "work this session has in the consumer's hands", not "work the
    /// worker is busy with". Dropping the [`crate::EventQueue`] releases every
    /// slot it held, so this falls back to zero.
    ///
    /// A snapshot, for diagnostics and for a caller that wants to throttle
    /// before it is refused.
    #[must_use]
    pub fn outstanding_requests(&self) -> usize {
        self.shared.outstanding()
    }

    /// Submits a statement; its reply is one
    /// [`crate::SessionEvent::Executed`] carrying `request`, preceded by one
    /// [`crate::SessionEvent::Executing`] when the worker starts it.
    ///
    /// Returns as soon as the command is queued; the reply is produced on the
    /// worker thread. One exception is worth knowing about for a consumer with
    /// a [`crate::Waker`]: when the command cannot be delivered at all — the
    /// session has already ended — the failure event is produced *here*, on
    /// the calling thread, and if it fills an empty queue this call also runs
    /// the waker before returning. See [`crate::Waker`].
    ///
    /// # Errors
    ///
    /// The only synchronous failures are "no queue is bound" and
    /// [`reldex_db_driver_api::ErrorKind::Resource`] when
    /// [`SessionLimits::max_outstanding_requests`] is already reached. Both
    /// accept nothing, so no event follows. Every other failure — including a
    /// lost or closed session — arrives as the request's own single event.
    pub fn submit_execute(&self, request: RequestId, statement: Statement) -> DbResult<()> {
        self.submit_event(request, (), |reply| Command::Execute { statement, reply })
    }

    /// Submits a fetch; its reply is one [`crate::SessionEvent::Fetched`],
    /// which names `result` whether it succeeded or failed.
    ///
    /// # Errors
    ///
    /// As [`DatabaseSession::submit_execute`].
    pub fn submit_fetch(
        &self,
        request: RequestId,
        result: ResultId,
        max_rows: NonZeroUsize,
    ) -> DbResult<()> {
        self.submit_event(request, result, |reply| Command::FetchBatch {
            result,
            max_rows,
            reply,
        })
    }

    /// Submits a commit; its reply is one [`crate::SessionEvent::Completed`]
    /// with [`CompletedOperation::Commit`].
    ///
    /// # Errors
    ///
    /// As [`DatabaseSession::submit_execute`].
    pub fn submit_commit(&self, request: RequestId) -> DbResult<()> {
        self.submit_event(request, CompletedOperation::Commit, |reply| {
            Command::Commit { reply }
        })
    }

    /// Submits a rollback; its reply is one [`crate::SessionEvent::Completed`]
    /// with [`CompletedOperation::Rollback`].
    ///
    /// # Errors
    ///
    /// As [`DatabaseSession::submit_execute`].
    pub fn submit_rollback(&self, request: RequestId) -> DbResult<()> {
        self.submit_event(request, CompletedOperation::Rollback, |reply| {
            Command::Rollback { reply }
        })
    }

    /// Submits a savepoint; its reply is one
    /// [`crate::SessionEvent::Completed`] with
    /// [`CompletedOperation::Savepoint`].
    ///
    /// # Errors
    ///
    /// As [`DatabaseSession::submit_execute`].
    pub fn submit_savepoint(&self, request: RequestId, name: SavepointName) -> DbResult<()> {
        self.submit_event(request, CompletedOperation::Savepoint, |reply| {
            Command::Savepoint { name, reply }
        })
    }

    /// Submits a rollback to a savepoint; its reply is one
    /// [`crate::SessionEvent::Completed`] with
    /// [`CompletedOperation::RollbackToSavepoint`].
    ///
    /// # Errors
    ///
    /// As [`DatabaseSession::submit_execute`].
    pub fn submit_rollback_to_savepoint(
        &self,
        request: RequestId,
        name: SavepointName,
    ) -> DbResult<()> {
        self.submit_event(request, CompletedOperation::RollbackToSavepoint, |reply| {
            Command::RollbackToSavepoint { name, reply }
        })
    }

    /// Submits a ping; its reply is one [`crate::SessionEvent::Completed`]
    /// with [`CompletedOperation::Ping`].
    ///
    /// # Errors
    ///
    /// As [`DatabaseSession::submit_execute`].
    pub fn submit_ping(&self, request: RequestId) -> DbResult<()> {
        self.submit_event(request, CompletedOperation::Ping, |reply| Command::Ping {
            reply,
        })
    }

    /// Submits a large-object read; its reply is one
    /// [`crate::SessionEvent::LobChunk`], which names `lob` whether it
    /// succeeded or failed.
    ///
    /// # Errors
    ///
    /// As [`DatabaseSession::submit_execute`].
    pub fn submit_read_lob_chunk(
        &self,
        request: RequestId,
        lob: LobHandle,
        max_bytes: NonZeroUsize,
    ) -> DbResult<()> {
        self.submit_event(request, lob, |reply| Command::ReadLobChunk {
            lob,
            max_bytes,
            reply,
        })
    }

    /// Submits a result close; its reply is one
    /// [`crate::SessionEvent::Completed`] with
    /// [`CompletedOperation::CloseResult`].
    ///
    /// # Errors
    ///
    /// As [`DatabaseSession::submit_execute`].
    pub fn submit_close_result(&self, request: RequestId, result: ResultId) -> DbResult<()> {
        self.submit_event(request, CompletedOperation::CloseResult(result), |reply| {
            Command::CloseResult { result, reply }
        })
    }

    /// Submits a large-object close; its reply is one
    /// [`crate::SessionEvent::Completed`] with
    /// [`CompletedOperation::CloseLob`].
    ///
    /// # Errors
    ///
    /// As [`DatabaseSession::submit_execute`].
    pub fn submit_close_lob(&self, request: RequestId, lob: LobHandle) -> DbResult<()> {
        self.submit_event(request, CompletedOperation::CloseLob(lob), |reply| {
            Command::CloseLob { lob, reply }
        })
    }

    /// Turns server output (`DBMS_OUTPUT` and its kind) on or off for this
    /// session; its reply is one [`crate::SessionEvent::ServerOutputConfigured`]
    /// carrying the setting actually in force (ADR-0002 amendment T).
    ///
    /// **One round trip, and the only thing that makes the session read
    /// output.** Output is off when a session opens. While it is off the
    /// worker makes no server-output call of any kind; once it is on, the
    /// worker reads the server's buffer after every statement and delivers
    /// the lines as [`crate::SessionEvent::ServerOutput`] events, before that
    /// statement's `Executed`. A read is a round trip per statement — which
    /// is exactly why this is a switch the user controls.
    ///
    /// Turning output off lets the server discard whatever it still holds.
    ///
    /// # Errors
    ///
    /// As [`DatabaseSession::submit_execute`]. A driver without
    /// [`reldex_db_driver_api::Capabilities::server_output`] answers with
    /// [`reldex_db_driver_api::ErrorKind::Unsupported`] in the reply, and no
    /// driver call is made.
    pub fn submit_set_server_output(
        &self,
        request: RequestId,
        setting: ServerOutputSetting,
    ) -> DbResult<()> {
        self.submit_event(request, (), |reply| Command::SetServerOutput {
            setting,
            reply,
        })
    }

    /// Submits a session close; its reply is one
    /// [`crate::SessionEvent::SessionClosed`], followed — only if the session
    /// actually ended — by this session's single
    /// [`crate::SessionEvent::Terminal`].
    ///
    /// Unlike [`DatabaseSession::close`] this does not block and does not join
    /// the worker thread; the worker finishes on its own once it has answered
    /// everything that was queued behind the close.
    ///
    /// A close reserves against **one more** than
    /// [`SessionLimits::max_outstanding_requests`], so a session at its limit
    /// can still be told to end: refusing the one request that shrinks a
    /// session's footprint because the session has too much outstanding is the
    /// wrong way round. The exemption is exactly one — a *second* close while
    /// the first is still undrained is refused like anything else — so the
    /// published bound grows by one event per session and no further.
    ///
    /// # Errors
    ///
    /// As [`DatabaseSession::submit_execute`], with the `Resource` limit one
    /// higher. Teardown never depends on this: [`DatabaseSession::close`] and
    /// `Drop` answer through a [`Completion`] and reserve nothing.
    pub fn submit_close(
        &self,
        request: RequestId,
        disposition: Option<CloseDisposition>,
    ) -> DbResult<()> {
        self.check_event_route()?;
        self.shared.reserve_request(
            self.limits
                .max_outstanding_requests()
                .get()
                .saturating_add(1),
        )?;
        let reply = CloseReplyTo::event(Arc::clone(&self.shared), self.id, request);
        if let Some(settled) = self.shared.settled_close() {
            // Idempotent, exactly like `DatabaseSession::close`, and decided
            // from the same single record of **how** the session ended: a
            // session a close really did end reports success, and one that was
            // lost or abandoned reports that instead. Reading "a close has
            // run" as "a close succeeded" is what made a close on a lost
            // session sometimes answer `Ok(())` once the first close had ended
            // it — the silent loss `SPEC.md` §10 forbids.
            reply.answer(settled.map_err(CloseError::Failed));
            return Ok(());
        }
        let _ = self.send(Command::Close {
            intent: CloseIntent::Explicit(disposition),
            reply,
        });
        Ok(())
    }

    /// Executes one statement. Binds, deadlines and the fetch-size hint live
    /// on [`Statement`] itself.
    ///
    /// With server output on, what the statement printed is in
    /// [`DatabaseSession::take_server_output`] by the time this completes —
    /// whether it succeeded or failed.
    #[must_use]
    pub fn execute(&self, statement: Statement) -> Completion<ExecuteOutcome> {
        self.submit((), |reply| Command::Execute { statement, reply })
    }

    /// Turns server output on or off; the completion-path twin of
    /// [`DatabaseSession::submit_set_server_output`], with the same cost and
    /// the same answer — the setting actually in force.
    #[must_use]
    pub fn set_server_output(
        &self,
        setting: ServerOutputSetting,
    ) -> Completion<ServerOutputSetting> {
        self.submit((), |reply| Command::SetServerOutput { setting, reply })
    }

    /// Takes the server output collected for requests answered through a
    /// [`Completion`], leaving the log empty. See [`ServerOutputLog`].
    ///
    /// **Never a round trip**: the worker has already read the output, after
    /// each statement, before answering it. This only hands over what it
    /// collected. Output of event-path requests is not here — it went out as
    /// [`crate::SessionEvent::ServerOutput`] instead.
    ///
    /// **One log per session, not per statement.** Taken after a single
    /// `wait()`, it holds exactly that statement's output. A caller that
    /// pipelines several completions and takes once afterwards gets their
    /// output mixed in execution order, with nothing marking where one
    /// statement's lines end. [`ServerOutputLog::failure`] is then the first
    /// failed read among them, and it is not attributed to any statement. To
    /// attribute output, take after each `wait()`, or use the event path.
    #[must_use]
    pub fn take_server_output(&self) -> ServerOutputLog {
        self.shared.take_collected_server_output()
    }

    /// Fetches the next batch of an open result. An empty batch means the
    /// result is exhausted.
    ///
    /// Large objects in the batch are parked on the worker thread and reported
    /// as [`LobHandle`]s; see [`FetchedBatch`].
    #[must_use]
    pub fn fetch_batch(
        &self,
        result: ResultId,
        max_rows: NonZeroUsize,
    ) -> Completion<FetchedBatch> {
        self.submit(result, |reply| Command::FetchBatch {
            result,
            max_rows,
            reply,
        })
    }

    /// Releases a result's resources, and every large-object handle taken from
    /// it. Safe to call even if the result was already invalidated by a commit,
    /// rollback or a fetch error.
    #[must_use]
    pub fn close_result(&self, result: ResultId) -> Completion<()> {
        self.submit(CompletedOperation::CloseResult(result), |reply| {
            Command::CloseResult { result, reply }
        })
    }

    /// Commits the open transaction.
    #[must_use]
    pub fn commit(&self) -> Completion<()> {
        self.submit(CompletedOperation::Commit, |reply| Command::Commit {
            reply,
        })
    }

    /// Rolls the open transaction back.
    #[must_use]
    pub fn rollback(&self) -> Completion<()> {
        self.submit(CompletedOperation::Rollback, |reply| Command::Rollback {
            reply,
        })
    }

    /// Establishes a savepoint.
    #[must_use]
    pub fn savepoint(&self, name: SavepointName) -> Completion<()> {
        self.submit(CompletedOperation::Savepoint, |reply| Command::Savepoint {
            name,
            reply,
        })
    }

    /// Rolls back to a savepoint, leaving the transaction open.
    #[must_use]
    pub fn rollback_to_savepoint(&self, name: SavepointName) -> Completion<()> {
        self.submit(CompletedOperation::RollbackToSavepoint, |reply| {
            Command::RollbackToSavepoint { name, reply }
        })
    }

    /// Validates that the session is still alive. Used after
    /// [`SessionLifecycle::NeedsValidation`] and on mobile resume (`SPEC.md`
    /// §18) — though `db-core` already validates automatically before the
    /// next request when needed; callers rarely need to invoke this directly.
    #[must_use]
    pub fn ping(&self) -> Completion<()> {
        self.submit(CompletedOperation::Ping, |reply| Command::Ping { reply })
    }

    /// Reads the next chunk of a parked large object, on this session's worker
    /// thread. An empty `Vec` means the object is exhausted, matching
    /// [`reldex_db_driver_api::LobStream::read_chunk`] returning `0`.
    ///
    /// Reads are sequential: the underlying stream is forward-only, so each
    /// call continues where the last one stopped. `max_bytes` bounds the buffer
    /// this allocates, but so does
    /// [`SessionLimits::max_lob_chunk_bytes`] — ask for more and you get the
    /// limit, not an unbounded allocation.
    ///
    /// The handle must have been produced by *this* session ([`FetchedBatch::lob`]
    /// or [`OutValue::Lob`]). One from another session is rejected as foreign,
    /// which is a different failure from one that was closed, and is reported as
    /// such.
    #[must_use]
    pub fn read_lob_chunk(&self, lob: LobHandle, max_bytes: NonZeroUsize) -> Completion<Vec<u8>> {
        self.submit(lob, |reply| Command::ReadLobChunk {
            lob,
            max_bytes,
            reply,
        })
    }

    /// Releases a large object early.
    ///
    /// Optional: a handle dies with the result it came from and with its
    /// session. Idempotent, like [`DatabaseSession::close_result`].
    #[must_use]
    pub fn close_lob(&self, lob: LobHandle) -> Completion<()> {
        self.submit(CompletedOperation::CloseLob(lob), |reply| {
            Command::CloseLob { lob, reply }
        })
    }

    /// Closes the session, resolving its transaction first.
    ///
    /// This is the **only** path that can commit. Dropping a session never
    /// does; see [`DatabaseSession`]'s [`Drop`].
    ///
    /// `disposition` must be `Some` when a transaction may be open — otherwise
    /// this returns [`CloseError::DecisionRequired`] and the session is left
    /// open and usable, so the caller can ask the user and call this again
    /// (`SPEC.md` §10: never silently commit or hide a transaction). That check
    /// happens on the worker thread, after every request queued ahead of the
    /// close has run, so a statement still in flight cannot have its
    /// transaction discarded by a close that raced it.
    ///
    /// If the disposition itself fails the session stays **open**
    /// ([`CloseError::CommitFailed`], [`CloseError::RollbackFailed`]): a commit
    /// that did not happen must not cost the user their transaction. If the
    /// session was already lost, this reports the loss — with the original
    /// error's kind and native code — rather than succeeding as if a commit had
    /// happened; resources are released either way.
    ///
    /// Idempotent: closing an already-closed session succeeds.
    ///
    /// # Errors
    ///
    /// [`CloseError`]; see its variants for which ones leave the session open.
    pub fn close(&self, disposition: Option<CloseDisposition>) -> Result<(), CloseError> {
        let mut worker = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if worker.is_none() {
            return self.settled_close();
        }
        let (tx, rx) = mpsc::channel();
        if !self.send(Command::Close {
            intent: CloseIntent::Explicit(disposition),
            reply: CloseReplyTo::one_shot(tx),
        }) {
            if let Some(handle) = worker.take() {
                let _ = handle.join();
            }
            return self.settled_close();
        }
        match rx.recv() {
            Ok(Err(err)) if err.session_is_still_open() => Err(err),
            Ok(outcome) => {
                if let Some(handle) = worker.take() {
                    let _ = handle.join();
                }
                outcome
            }
            Err(_) => {
                if let Some(handle) = worker.take() {
                    let _ = handle.join();
                }
                self.settled_close()
            }
        }
    }

    /// The answer a close owes when there is no worker left to ask.
    ///
    /// Every one of those cases used to return `Ok(())`, which asks the wrong
    /// question: "is there a worker?" instead of "did a close actually end
    /// this session, and did it succeed?". A session that was lost, abandoned,
    /// or whose teardown detached its worker at the shutdown deadline has a
    /// worker that is gone and a transaction that was never resolved, and a
    /// caller told `Ok(())` would believe their commit happened
    /// (`SPEC.md` §10).
    fn settled_close(&self) -> Result<(), CloseError> {
        match self.shared.settled_close() {
            Some(settled) => settled.map_err(CloseError::Failed),
            // The worker is gone and nothing recorded an end at all — so
            // nothing recorded a *clean* one either.
            None => Err(CloseError::Failed(self.shared.terminal_error())),
        }
    }
}

impl Drop for DatabaseSession {
    /// Releases the worker thread and its connection **without hanging and
    /// without committing**.
    ///
    /// Deliberately never commits and never rolls back explicitly, whatever
    /// [`DatabaseSession::has_possibly_active_transaction`] says: that decision
    /// belongs to [`DatabaseSession::close`], which is the only path that can
    /// commit anything. Dropping a session is equivalent to the connection
    /// dying — the server's own rollback-on-disconnect is what protects the
    /// data, not a choice Reldex made, which keeps `SPEC.md` §10's "never
    /// silently commit" intact structurally rather than by promise.
    ///
    /// The sequence is: ask the driver to stop whatever is running
    /// ([`DatabaseSession::cancel`]), tell the worker to abandon and release
    /// everything, then wait at most [`DROP_SHUTDOWN_TIMEOUT`]. Past that bound
    /// the worker is **detached**, not abandoned: it still owns the connection,
    /// its cursors and its parked large objects, and closes all of them when the
    /// blocked driver call finally returns. So a driver stuck in a call that
    /// cannot be interrupted — a `CancelKind::PreArmedDeadline` driver with no
    /// deadline armed, say — delays the release; it never hangs the drop and
    /// never leaks the resources.
    ///
    /// A session whose worker handle has already been taken — by
    /// [`crate::SessionRegistry`]'s own teardown, which waits for every session
    /// it holds against **one** shared deadline — returns from here
    /// immediately, because the wait has already happened.
    fn drop(&mut self) {
        if self
            .worker
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none()
        {
            return;
        }
        let reply = self.begin_abandon();
        self.finish_abandon(reply, Instant::now() + DROP_SHUTDOWN_TIMEOUT);
    }
}

/// Opens [`DatabaseSession`]s from a driver.
///
/// Stateless apart from the [`SessionLimits`] it stamps onto each session:
/// nothing here pools or replaces sessions (`SPEC.md` §9). It exists as the
/// documented entry point and the natural home for whatever session-opening
/// policy Phase 1 adds.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionManager {
    limits: SessionLimits,
}

impl SessionManager {
    /// A session manager with the default [`SessionLimits`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            limits: SessionLimits::new(),
        }
    }

    /// Uses `limits` for every session opened afterwards.
    #[must_use]
    pub const fn with_limits(mut self, limits: SessionLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The limits sessions opened by this manager get.
    #[must_use]
    pub const fn limits(&self) -> SessionLimits {
        self.limits
    }

    /// Opens a session against `driver` with `params`.
    ///
    /// Spawns the session's dedicated worker thread and blocks the calling
    /// thread only on that thread's reply that the connection is ready — the
    /// actual `connect` call runs on the worker thread, never on the caller's
    /// (`SPEC.md` §11/§19). The non-blocking shape is
    /// [`crate::SessionRegistry::open`], which answers with a
    /// [`crate::SessionEvent::Opened`] instead of a return value; this one is
    /// unchanged and stays, for tests, tools and the device-check binary.
    ///
    /// Anything non-fatal the driver noticed while opening the connection is
    /// collected once on that worker thread and reported through
    /// [`DatabaseSession::connect_warnings`]; callers that show connection
    /// diagnostics should read it as soon as this returns.
    ///
    /// # Errors
    ///
    /// Whatever [`DatabaseDriver::connect`] returned, or a
    /// [`reldex_db_driver_api::ErrorKind::DriverInternal`] error if the
    /// worker thread could not be spawned or panicked before connecting.
    pub fn open_session(
        &self,
        driver: Arc<dyn DatabaseDriver>,
        params: ConnectionParams,
    ) -> DbResult<DatabaseSession> {
        let id = SessionId::allocate();
        let shared = Arc::new(SessionShared::new(id));
        let (ready_tx, ready_rx) = mpsc::channel::<DbResult<worker::Ready>>();
        let spawned = worker::spawn(
            driver,
            params,
            id,
            self.limits,
            Arc::clone(&shared),
            Box::new(move |outcome| {
                // Unchanged behaviour: the worker hands the outcome to the
                // thread parked below, and a send that fails means that thread
                // gave up, so nothing adopts the connection.
                if ready_tx.send(outcome).is_ok() {
                    worker::Adoption::Adopted
                } else {
                    worker::Adoption::Abandoned
                }
            }),
        )?;

        match ready_rx.recv() {
            Ok(Ok(ready)) => Ok(DatabaseSession::assemble(
                id,
                spawned.command_tx,
                spawned.join,
                shared,
                ready,
                self.limits,
            )),
            Ok(Err(err)) => {
                let _ = spawned.join.join();
                Err(err)
            }
            Err(_) => {
                let panicked = spawned.join.join().is_err();
                Err(DbError::internal(if panicked {
                    "reldex-db-core: session worker thread panicked while connecting"
                } else {
                    "reldex-db-core: session worker exited without connecting"
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Completion, DatabaseSession, SessionLimits};

    #[allow(clippy::extra_unused_type_parameters)]
    const fn assert_send<T: Send>() {}

    #[allow(clippy::extra_unused_type_parameters)]
    const fn assert_sync<T: Sync>() {}

    #[test]
    fn a_session_can_be_shared_between_threads() {
        // The documented shape: one owner, many submitters behind an `Arc`.
        // `mpsc::Sender` is not `Sync`, so this only holds because the session
        // guards it — asserted here so the claim cannot rot.
        assert_send::<DatabaseSession>();
        assert_sync::<DatabaseSession>();
        assert_send::<Completion<()>>();
    }

    #[test]
    fn default_limits_are_the_documented_ones() {
        let limits = SessionLimits::default();
        assert_eq!(
            limits.max_open_results(),
            SessionLimits::DEFAULT_MAX_OPEN_RESULTS
        );
        assert_eq!(
            limits.max_lob_chunk_bytes(),
            SessionLimits::DEFAULT_MAX_LOB_CHUNK_BYTES
        );
    }
}
