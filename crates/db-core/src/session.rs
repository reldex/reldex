//! The public session API: [`SessionManager`], [`DatabaseSession`] and the
//! request/completion shape built on top of the worker thread in
//! [`crate::worker`].

use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

use reldex_db_driver_api::{
    CancelKind, CancelOutcome, ConnectionId, ConnectionParams, DatabaseDriver, DbError, DbResult,
    LobLocator, ResultSetId, RowBatch, SavepointName, SessionState, Statement, StatementKind,
    Warning,
};

use crate::ids::SessionId;
use crate::shared::SessionShared;
use crate::worker::{self, Command};

/// One value a statement wrote back through an output bind.
///
/// Mirrors [`reldex_db_driver_api::Value`] with one substitution: a nested
/// `REF CURSOR` is a handle derived from the connection, so it never leaves the
/// worker thread. `db-core` registers it as a result and reports its
/// [`ResultSetId`], exactly like the cursor an ordinary query produces
/// (ADR-0002 D1/D2).
#[derive(Debug)]
pub enum OutValue {
    /// Plain data, or a [`reldex_db_driver_api::LobLocator`] to read through
    /// [`DatabaseSession::read_lob_chunk`]. Never
    /// `reldex_db_driver_api::Value::Cursor`.
    Value(reldex_db_driver_api::Value),
    /// A nested result set, open on this session's worker thread. Fetch it with
    /// [`DatabaseSession::fetch_batch`] and release it with
    /// [`DatabaseSession::close_result`], like any other result.
    Result(ResultSetId),
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
/// cursor replaced by the [`ResultSetId`] `db-core` now owns on the worker
/// thread — the cursor itself never leaves it (ADR-0002 D1/D2). A nested
/// `REF CURSOR` returned through an output bind is handled the same way; see
/// [`OutValue`].
#[derive(Debug)]
pub struct ExecuteOutcome {
    /// The result set's id, if the statement produced one. Pass this to
    /// [`DatabaseSession::fetch_batch`] and, once done,
    /// [`DatabaseSession::close_result`].
    pub result: Option<ResultSetId>,
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
    /// `REF CURSOR` among them is already registered as a result of this
    /// session; see [`OutValue`].
    pub out_values: OutValues,
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
#[derive(Debug)]
pub enum CloseError {
    /// A transaction may still be open
    /// ([`DatabaseSession::has_possibly_active_transaction`] was true) and no
    /// [`CloseDisposition`] was given. `SPEC.md` §10: Reldex never silently
    /// commits or hides a transaction. The session is unchanged — call
    /// `close` again with `Some(CloseDisposition::Commit)` or
    /// `Some(CloseDisposition::Rollback))`.
    DecisionRequired,
    /// The driver reported a failure while resolving the transaction or
    /// closing the connection. The session is closed (best-effort) either
    /// way; per the driver contract it must not be reused.
    Failed(DbError),
}

impl fmt::Display for CloseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DecisionRequired => f.write_str(
                "reldex-db-core: a transaction may be open; close needs an explicit disposition",
            ),
            Self::Failed(err) => write!(f, "reldex-db-core: close failed: {err}"),
        }
    }
}

impl std::error::Error for CloseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::DecisionRequired => None,
            Self::Failed(err) => Some(err),
        }
    }
}

/// A pending request's result, from a per-request reply channel
/// (`docs/decisions/0002-driver-api-and-concurrency-model.md` D1: "a later
/// FFI/Qt adapter can turn completions into events").
///
/// [`Completion::wait`] blocks the calling thread until the worker replies;
/// [`Completion::poll`] never blocks, so an event loop (the eventual C++/Qt
/// adapter, or a test) can check completions between other work instead of
/// dedicating a thread to each one.
pub struct Completion<T> {
    rx: mpsc::Receiver<DbResult<T>>,
}

impl<T> Completion<T> {
    /// Blocks until the request completes.
    ///
    /// # Errors
    ///
    /// Whatever [`DbError`] the request produced, or a
    /// [`reldex_db_driver_api::ErrorKind::DriverInternal`] error if the
    /// worker thread ended without replying at all (a bug, not an expected
    /// outcome; ordinary session loss always replies).
    pub fn wait(self) -> DbResult<T> {
        self.rx.recv().unwrap_or_else(|_| {
            Err(DbError::internal(
                "reldex-db-core: the session worker ended without replying",
            ))
        })
    }

    /// Checks whether the request has completed yet, without blocking.
    ///
    /// Returns `None` while the request is still queued or running.
    #[must_use]
    pub fn poll(&self) -> Option<DbResult<T>> {
        match self.rx.try_recv() {
            Ok(value) => Some(value),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(DbError::internal(
                "reldex-db-core: the session worker ended without replying",
            ))),
        }
    }
}

/// A stable, stateful database session (`SPEC.md` §6, §9).
///
/// Owns one dedicated worker thread that owns the driver's
/// `Box<dyn DatabaseConnection>` and every cursor derived from it for the
/// session's whole lifetime; no database or network call this type makes
/// ever runs on the calling thread (ADR-0002 D1/D2). Requests submitted from
/// any thread are processed by that worker strictly in the order they were
/// sent — see [`crate::worker`] for the queueing policy, including what
/// happens to requests queued behind a blocked statement.
///
/// Cloning is deliberately not offered: a session is owned once, matching
/// "a worksheet owns a stable database session" (`SPEC.md` §9). Multiple
/// threads may still submit work concurrently through a shared `&DatabaseSession`
/// (for example behind an `Arc`); every method here takes `&self`.
pub struct DatabaseSession {
    id: SessionId,
    connection_id: ConnectionId,
    command_tx: mpsc::Sender<Command>,
    worker: Option<thread::JoinHandle<()>>,
    shared: Arc<SessionShared>,
    cancel_handle: Arc<dyn reldex_db_driver_api::CancelHandle>,
    cancel_kind: CancelKind,
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

    /// Requests that the currently running statement, if any, stop.
    ///
    /// Callable from any thread while another thread is blocked in
    /// [`Completion::wait`] on this session's `execute` or `fetch_batch`; it
    /// never goes through the request queue, which is the point — a queue a
    /// blocked call can stall would defeat cancellation
    /// (`docs/decisions/0002-driver-api-and-concurrency-model.md` D2).
    ///
    /// # Errors
    ///
    /// Whatever the driver's [`reldex_db_driver_api::CancelHandle::request_cancel`]
    /// returns.
    pub fn cancel(&self) -> DbResult<CancelOutcome> {
        self.cancel_handle.request_cancel()
    }

    /// The session-loss state as of the last completed request
    /// (`SPEC.md` §18).
    #[must_use]
    pub fn session_state(&self) -> SessionState {
        self.shared.session_state()
    }

    /// Whether the session is terminally lost. Once true it stays true:
    /// `db-core` never silently reconnects or replaces a session — open a new
    /// one instead (`SPEC.md` §18).
    #[must_use]
    pub fn is_lost(&self) -> bool {
        self.shared.is_lost()
    }

    /// Whether a transaction may still be open, combining the driver's own
    /// report with core-side tracking (ADR-0002 D4/D6). Conservative by
    /// design: this can be true when nothing is actually open, but is never
    /// false while something might be.
    ///
    /// Reflects the state as of the last request this handle has observed
    /// complete; a request submitted concurrently and not yet awaited is not
    /// reflected yet. Await outstanding [`Completion`]s before calling
    /// [`DatabaseSession::close`] with `None` if that matters.
    #[must_use]
    pub fn has_possibly_active_transaction(&self) -> bool {
        self.shared.has_possibly_active_transaction()
    }

    fn submit<T>(
        &self,
        make_command: impl FnOnce(mpsc::Sender<DbResult<T>>) -> Command,
    ) -> Completion<T> {
        let (tx, rx) = mpsc::channel();
        let command = make_command(tx.clone());
        if self.command_tx.send(command).is_err() {
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
    #[must_use]
    pub fn fetch_batch(&self, result: ResultSetId, max_rows: NonZeroUsize) -> Completion<RowBatch> {
        self.submit(|reply| Command::FetchBatch {
            result,
            max_rows,
            reply,
        })
    }

    /// Releases a result's resources. Safe to call even if the result was
    /// already invalidated by a commit, rollback or a fetch error.
    #[must_use]
    pub fn close_result(&self, result: ResultSetId) -> Completion<()> {
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
    /// [`SessionState::NeedsValidation`] and on mobile resume (`SPEC.md`
    /// §18) — though `db-core` already validates automatically before the
    /// next request when needed; callers rarely need to invoke this directly.
    #[must_use]
    pub fn ping(&self) -> Completion<()> {
        self.submit(|reply| Command::Ping { reply })
    }

    /// Reads the next chunk of a large object, on this session's worker
    /// thread, and hands `locator` back so the caller can keep reading. An
    /// empty `Vec` means the object is exhausted, matching
    /// [`reldex_db_driver_api::LobStream::read_chunk`] returning `0`.
    ///
    /// A [`LobLocator`] taken out of a fetched [`RowBatch`] (see
    /// [`reldex_db_driver_api::Column::take_lob`]) is, like a cursor, a
    /// handle derived from this session's connection: it must be used only
    /// on the worker thread that owns it (ADR-0002 D1/D2). Calling
    /// [`LobLocator::read_chunk`] directly from any other thread would
    /// violate that runtime rule; this method is what keeps the read on the
    /// right thread.
    #[must_use]
    pub fn read_lob_chunk(
        &self,
        locator: LobLocator,
        max_bytes: NonZeroUsize,
    ) -> Completion<(LobLocator, Vec<u8>)> {
        self.submit(|reply| Command::ReadLobChunk {
            locator,
            max_bytes,
            reply,
        })
    }

    /// Closes the session, resolving its transaction first.
    ///
    /// `disposition` must be `Some` when
    /// [`DatabaseSession::has_possibly_active_transaction`] is true —
    /// otherwise this returns [`CloseError::DecisionRequired`] and the
    /// session is left open and usable, so the caller can ask the user and
    /// call this again (`SPEC.md` §10: never silently commit or hide a
    /// transaction). Idempotent: closing an already-closed session succeeds.
    ///
    /// # Errors
    ///
    /// [`CloseError::DecisionRequired`] or [`CloseError::Failed`]; see their
    /// documentation.
    pub fn close(&mut self, disposition: Option<CloseDisposition>) -> Result<(), CloseError> {
        if self.worker.is_none() {
            return Ok(());
        }
        if disposition.is_none() && self.shared.has_possibly_active_transaction() {
            return Err(CloseError::DecisionRequired);
        }
        let (tx, rx) = mpsc::channel();
        let sent = self
            .command_tx
            .send(Command::Close {
                disposition,
                reply: tx,
            })
            .is_ok();
        let outcome = if sent { rx.recv().ok() } else { None };
        self.join();
        match outcome {
            Some(Ok(())) => Ok(()),
            Some(Err(err)) => Err(CloseError::Failed(err)),
            None => Ok(()),
        }
    }

    fn join(&mut self) {
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for DatabaseSession {
    /// Releases the worker thread and its connection without hanging.
    ///
    /// Deliberately **never** commits or rolls back, whatever
    /// [`DatabaseSession::has_possibly_active_transaction`] says: that
    /// decision belongs to the explicit [`DatabaseSession::close`] method.
    /// Dropping a session without closing it first is equivalent to the
    /// connection dying — the server's own rollback-on-disconnect is what
    /// protects data, not a choice Reldex made, which keeps `SPEC.md` §10's
    /// "never silently commit" intact even on this path. This is what
    /// guarantees a session never leaks its thread on drop.
    fn drop(&mut self) {
        if self.worker.is_some() {
            let (tx, _rx) = mpsc::channel();
            let _ = self.command_tx.send(Command::Close {
                disposition: None,
                reply: tx,
            });
            self.join();
        }
    }
}

/// Opens [`DatabaseSession`]s from a driver.
///
/// Stateless by design: nothing here pools or replaces sessions
/// (`SPEC.md` §9). It exists as the documented entry point and the natural
/// home for whatever session-opening policy Phase 1 adds.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionManager {
    _private: (),
}

impl SessionManager {
    /// A new, stateless session manager.
    #[must_use]
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Opens a session against `driver` with `params`.
    ///
    /// Spawns the session's dedicated worker thread and blocks the calling
    /// thread only on that thread's reply that the connection is ready — the
    /// actual `connect` call runs on the worker thread, never on the caller's
    /// (`SPEC.md` §11/§19).
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
        let handle = worker::spawn(driver, params, id)?;
        Ok(DatabaseSession {
            id,
            connection_id: handle.connection_id,
            command_tx: handle.command_tx,
            worker: Some(handle.join),
            shared: handle.shared,
            cancel_handle: handle.cancel_handle,
            cancel_kind: handle.cancel_kind,
        })
    }
}
