//! The public session API: [`SessionManager`], [`DatabaseSession`] and the
//! request/completion shape built on top of the worker thread in
//! [`crate::worker`].

use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use reldex_db_driver_api::{
    CancelKind, CancelOutcome, Column, ConnectionId, ConnectionParams, DatabaseDriver, DbError,
    DbResult, RowBatch, SavepointName, Statement, StatementKind, ValueRef, Warning,
};

use crate::ids::{LobHandle, ResultId, SessionId};
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
#[derive(Debug)]
pub struct ExecuteOutcome {
    /// The result set's id, if the statement produced one. Pass this to
    /// [`DatabaseSession::fetch_batch`] and, once done,
    /// [`DatabaseSession::close_result`].
    pub result: Option<ResultId>,
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
/// A session's command queue is unbounded on purpose (see [`crate::worker`]),
/// so what has to be bounded instead is what a session can *accumulate*. These
/// are deliberately simple caps with clear errors rather than a policy engine:
/// the point is that a runaway caller gets a reportable failure instead of an
/// unbounded allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionLimits {
    max_open_results: NonZeroUsize,
    max_lob_chunk_bytes: NonZeroUsize,
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

    /// The default limits.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_open_results: Self::DEFAULT_MAX_OPEN_RESULTS,
            max_lob_chunk_bytes: Self::DEFAULT_MAX_LOB_CHUNK_BYTES,
        }
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
/// worker strictly in the order they were sent — see [`crate::worker`] for the
/// queueing policy, including what happens to requests queued behind a blocked
/// statement.
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
}

impl DatabaseSession {
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
    /// reflected yet. That is a reporting limitation only —
    /// [`DatabaseSession::close`] re-checks on the worker thread once the queue
    /// has drained, so it cannot act on a stale answer.
    #[must_use]
    pub fn has_possibly_active_transaction(&self) -> bool {
        self.shared.has_possibly_active_transaction()
    }

    fn send(&self, command: Command) -> bool {
        self.command_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .send(command)
            .is_ok()
    }

    fn submit<T>(
        &self,
        make_command: impl FnOnce(mpsc::Sender<DbResult<T>>) -> Command,
    ) -> Completion<T> {
        let (tx, rx) = mpsc::channel();
        let command = make_command(tx.clone());
        if !self.send(command) {
            let _ = tx.send(Err(self.shared.terminal_error()));
        }
        Completion { rx }
    }

    /// Executes one statement. Binds, deadlines and the fetch-size hint live
    /// on [`Statement`] itself.
    #[must_use]
    pub fn execute(&self, statement: Statement) -> Completion<ExecuteOutcome> {
        self.submit(|reply| Command::Execute { statement, reply })
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
        self.submit(|reply| Command::FetchBatch {
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
        self.submit(|reply| Command::CloseResult { result, reply })
    }

    /// Commits the open transaction.
    #[must_use]
    pub fn commit(&self) -> Completion<()> {
        self.submit(|reply| Command::Commit { reply })
    }

    /// Rolls the open transaction back.
    #[must_use]
    pub fn rollback(&self) -> Completion<()> {
        self.submit(|reply| Command::Rollback { reply })
    }

    /// Establishes a savepoint.
    #[must_use]
    pub fn savepoint(&self, name: SavepointName) -> Completion<()> {
        self.submit(|reply| Command::Savepoint { name, reply })
    }

    /// Rolls back to a savepoint, leaving the transaction open.
    #[must_use]
    pub fn rollback_to_savepoint(&self, name: SavepointName) -> Completion<()> {
        self.submit(|reply| Command::RollbackToSavepoint { name, reply })
    }

    /// Validates that the session is still alive. Used after
    /// [`SessionLifecycle::NeedsValidation`] and on mobile resume (`SPEC.md`
    /// §18) — though `db-core` already validates automatically before the
    /// next request when needed; callers rarely need to invoke this directly.
    #[must_use]
    pub fn ping(&self) -> Completion<()> {
        self.submit(|reply| Command::Ping { reply })
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
        self.submit(|reply| Command::ReadLobChunk {
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
        self.submit(|reply| Command::CloseLob { lob, reply })
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
            return Ok(());
        }
        let (tx, rx) = mpsc::channel();
        if !self.send(Command::Close {
            intent: CloseIntent::Explicit(disposition),
            reply: tx,
        }) {
            if let Some(handle) = worker.take() {
                let _ = handle.join();
            }
            return Ok(());
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
                Ok(())
            }
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
    fn drop(&mut self) {
        let worker = self
            .worker
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(handle) = worker.take() else {
            return;
        };
        // Best effort, and honest about it: on a driver that cannot interrupt a
        // running call this does nothing at all, which is why the wait below is
        // bounded.
        let _ = self.cancel();
        let (tx, rx) = mpsc::channel();
        if !self.send(Command::Close {
            intent: CloseIntent::Abandon,
            reply: tx,
        }) {
            let _ = handle.join();
            return;
        }
        match rx.recv_timeout(DROP_SHUTDOWN_TIMEOUT) {
            Ok(_) => {
                let _ = handle.join();
            }
            Err(_) => drop(handle),
        }
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
    /// (`SPEC.md` §11/§19). Making the *wait* asynchronous too is Phase 1 FFI
    /// work (ADR-0002, amendments after the db-core review).
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
        let handle = worker::spawn(driver, params, id, self.limits)?;
        Ok(DatabaseSession {
            id,
            connection_id: handle.connection_id,
            command_tx: Mutex::new(handle.command_tx),
            worker: Mutex::new(Some(handle.join)),
            shared: handle.shared,
            cancel_handle: handle.cancel_handle,
            cancel_kind: handle.cancel_kind,
            connect_warnings: handle.connect_warnings,
        })
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
