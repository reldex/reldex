//! The per-session worker thread: the only thread that ever touches a
//! `Box<dyn DatabaseConnection>`, a `Box<dyn Cursor>` or a `LobLocator`
//! (ADR-0002 D1/D2).
//!
//! # Queue policy
//!
//! Commands arrive over an **unbounded** [`std::sync::mpsc`] channel and are
//! processed strictly in the order they were sent — the channel's own FIFO
//! order is the whole mechanism, so a statement that blocks (a long query, a
//! mock `Action::Block`) simply makes every command queued behind it on *this
//! session* wait its turn, exactly like a single-threaded worksheet would.
//! Nothing reorders, nothing jumps the queue, and nothing times a queued
//! command out: a caller that wants a bound sets one on the statement
//! ([`Statement::with_deadline`](reldex_db_driver_api::Statement::with_deadline))
//! or stops waiting on its own [`crate::Completion`].
//!
//! The channel is unbounded on purpose. A bounded one would make `execute`
//! block the *calling* thread once the queue filled, which is exactly what
//! `SPEC.md` §11/§19 forbids; the queue's real bound is that one session
//! belongs to one worksheet, so its depth is the number of requests a human
//! (or the FFI adapter on their behalf) has outstanding. What is bounded here
//! instead is the *resources* a session can accumulate: open results
//! ([`crate::SessionLimits::max_open_results`]) and the buffer one LOB read may
//! allocate ([`crate::SessionLimits::max_lob_chunk_bytes`]).
//!
//! Cancellation reaches a blocked call through the driver's
//! `Arc<dyn CancelHandle>` instead, which is why [`crate::DatabaseSession::cancel`]
//! never goes through this channel at all.
//!
//! # Panic containment, and its limit
//!
//! Every call into driver code goes through [`call`], which turns a panic into
//! an [`reldex_db_driver_api::ErrorKind::DriverInternal`] error rather than
//! letting it unwind across the worker boundary. A connection that panicked is
//! then considered *torn*: it is dropped on this thread and `close()` is
//! deliberately **not** called on it, because the object's internal state is by
//! definition unknown and a second call into it is as likely to panic again as
//! to release anything.
//!
//! This guarantee only holds for drivers that actually unwind. The intended
//! primary driver does not: `oracledb` 26.0.0-beta.3 locks a poisoned mutex in
//! `impl Drop for StatementHolder`, so a panic inside a round trip panics again
//! during unwinding and **aborts the process** (spike U-4,
//! `docs/exec-plans/active/phase-0-spike-results.md` §5). No wrapper can
//! contain that, and `catch_unwind` does not help.

use std::cell::Cell;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc;
use std::sync::{Arc, mpsc::Sender};
use std::thread;

use reldex_db_driver_api::{
    CancelHandle, CancelKind, ColumnKind, ConnectionId, ConnectionParams, Cursor,
    DatabaseConnection, DatabaseDriver, DbError, DbResult, ErrorKind, LobLocator, ResultSetId,
    RowBatch, SavepointName, SessionState, Statement, TransactionState, Warning,
};

use crate::events::SessionEvent;
use crate::ids::{LobHandle, ResultId, SessionId};
use crate::reply::{CloseReplyTo, ReplyTo};
use crate::session::{
    CloseDisposition, CloseError, ExecuteOutcome, FetchedBatch, OutValue, OutValues, SessionLimits,
};
use crate::shared::SessionShared;

/// Why the worker is being asked to shut down.
pub(crate) enum CloseIntent {
    /// An explicit [`crate::DatabaseSession::close`]. The worker refuses to
    /// close over a possibly-open transaction without a disposition
    /// (`SPEC.md` §10), and that decision is made **here**, after every command
    /// queued ahead of this one has run — not on the caller's thread against a
    /// flag that may still be stale.
    Explicit(Option<CloseDisposition>),
    /// A [`Drop`]. Never commits and never prompts: the connection is released
    /// and the server rolls its transaction back, which is what makes
    /// "dropping a session cannot commit" structural rather than a promise.
    Abandon,
}

/// A message sent to a session's worker thread. Requests are processed in the
/// order they arrive; see the module documentation.
pub(crate) enum Command {
    /// Execute a statement.
    Execute {
        statement: Statement,
        reply: ReplyTo<ExecuteOutcome>,
    },
    /// Fetch the next batch of an open result.
    FetchBatch {
        result: ResultId,
        max_rows: NonZeroUsize,
        reply: ReplyTo<FetchedBatch>,
    },
    /// Release a result's resources.
    CloseResult {
        result: ResultId,
        reply: ReplyTo<()>,
    },
    /// Commit the open transaction.
    Commit { reply: ReplyTo<()> },
    /// Roll the open transaction back.
    Rollback { reply: ReplyTo<()> },
    /// Establish a savepoint.
    Savepoint {
        name: SavepointName,
        reply: ReplyTo<()>,
    },
    /// Roll back to a savepoint, leaving the transaction open.
    RollbackToSavepoint {
        name: SavepointName,
        reply: ReplyTo<()>,
    },
    /// Validate that the session is still alive.
    Ping { reply: ReplyTo<()> },
    /// Read the next chunk of a parked large object, on this worker thread.
    /// An empty `Vec` means the object is exhausted.
    ReadLobChunk {
        lob: LobHandle,
        max_bytes: NonZeroUsize,
        reply: ReplyTo<Vec<u8>>,
    },
    /// Release a parked large object.
    CloseLob { lob: LobHandle, reply: ReplyTo<()> },
    /// Resolve the transaction (if a disposition is given) and close the
    /// connection.
    Close {
        intent: CloseIntent,
        reply: CloseReplyTo,
    },
}

impl Command {
    /// Fails this command with `error` without running it.
    ///
    /// Used when a revalidating `ping` failed, and when the session's
    /// transition to a terminal state has to answer everything already queued
    /// before [`SessionEvent::Terminal`] goes out: the session is gone, so the
    /// command must be *answered*, not executed (`SPEC.md` §18).
    fn fail(self, error: DbError) {
        match self {
            Self::Execute { reply, .. } => {
                reply.answer(Err(error));
            }
            Self::FetchBatch { reply, .. } => {
                reply.answer(Err(error));
            }
            Self::CloseResult { reply, .. }
            | Self::Commit { reply }
            | Self::Rollback { reply }
            | Self::Savepoint { reply, .. }
            | Self::RollbackToSavepoint { reply, .. }
            | Self::Ping { reply }
            | Self::CloseLob { reply, .. } => {
                reply.answer(Err(error));
            }
            Self::ReadLobChunk { reply, .. } => {
                reply.answer(Err(error));
            }
            Self::Close { reply, .. } => {
                reply.answer(Err(CloseError::Failed(error)));
            }
        }
    }
}

/// Everything [`spawn`] hands back, the instant the thread exists.
///
/// Deliberately *not* the connection: [`spawn`] no longer waits for
/// `connect`, so what it can promise is only the two things that exist before
/// it — the channel commands go down, and the handle that owns the thread. The
/// connect's own outcome arrives later, through the [`ReportOpen`] the caller
/// supplied.
pub(crate) struct Spawned {
    pub(crate) command_tx: Sender<Command>,
    pub(crate) join: thread::JoinHandle<()>,
}

/// Everything a successful `connect` produced, handed to whoever asked for the
/// session.
pub(crate) struct Ready {
    pub(crate) cancel_handle: Arc<dyn CancelHandle>,
    pub(crate) cancel_kind: CancelKind,
    pub(crate) connection_id: ConnectionId,
    pub(crate) connect_warnings: Vec<Warning>,
}

/// Whether anyone still wants the session whose `connect` just returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Adoption {
    /// Somebody took the session; the worker enters its command loop and owns
    /// the connection for the rest of its life.
    Adopted,
    /// Nobody did — the caller gave up, or
    /// [`crate::SessionRegistry::abandon`] won the race. The worker
    /// **closes the connection on its own thread**, which is the only thread
    /// allowed to touch it (ADR-0002 D1/D2, H2), and exits. Nothing is adopted
    /// late and no live database session is left behind.
    Abandoned,
}

/// How a worker reports that its `connect` finished.
///
/// Called exactly once, on the worker thread, the moment `connect` returns —
/// which is the one point at which a session can still be given up on without
/// anything having been adopted. The blocking
/// [`crate::SessionManager::open_session`] passes a closure that sends down a
/// channel it is parked on; [`crate::SessionRegistry`] passes one that hands
/// the outcome to the registry, which decides under its own lock whether the
/// session is adopted or was abandoned while the connect ran.
///
/// It must not block for long: the worker is holding a live connection while
/// it runs, and on the registry path it runs under the registry's mutex.
pub(crate) type ReportOpen = Box<dyn FnOnce(DbResult<Ready>) -> Adoption + Send>;

/// Calls into driver code, converting a panic into
/// [`reldex_db_driver_api::ErrorKind::DriverInternal`] instead of unwinding
/// across the worker boundary, and recording in `torn` that the object it was
/// called on can no longer be trusted (see the module documentation).
fn call<T>(torn: &Cell<bool>, f: impl FnOnce() -> DbResult<T>) -> DbResult<T> {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            torn.set(true);
            // `payload.as_ref()`, not `&payload`: a `&Box<dyn Any + Send>`
            // unsizes to `dyn Any + Send` as the *box*, so the downcasts below
            // never match the panic's own payload and every contained panic
            // would be reported as "a non-string payload" — throwing away the
            // one thing that says what went wrong.
            Err(panic_to_error(payload.as_ref()))
        }
    }
}

fn panic_to_error(payload: &(dyn std::any::Any + Send)) -> DbError {
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "driver call panicked with a non-string payload".to_owned());
    DbError::internal(format!("reldex-db-core: driver call panicked: {message}"))
        .with_session_state(SessionState::Lost)
}

/// Spawns the dedicated worker thread for one session. **Returns as soon as
/// the thread exists**; it does not wait for `connect`.
///
/// `shared` is built and — on the event path — bound to its sink by the
/// caller, before the thread exists, so that whoever reports the open emits
/// through the same per-session emit lock as everything else and ordering
/// rule 1 holds for a session's very first events
/// (`phase-1-m2-5-event-queue.md` §6).
///
/// The connect itself always runs on the new worker thread, never on the
/// caller's (`SPEC.md` §11/§19); its outcome reaches the caller through
/// `report`.
pub(crate) fn spawn(
    driver: Arc<dyn DatabaseDriver>,
    params: ConnectionParams,
    session_id: SessionId,
    limits: SessionLimits,
    shared: Arc<SessionShared>,
    report: ReportOpen,
) -> DbResult<Spawned> {
    let (command_tx, command_rx) = mpsc::channel::<Command>();

    let join = thread::Builder::new()
        .name(format!("reldex-session-{}", session_id.get()))
        .spawn(move || {
            worker_main(
                &driver, &params, command_rx, report, shared, session_id, limits,
            );
        })
        .map_err(|err| {
            DbError::internal(format!(
                "reldex-db-core: failed to spawn session worker thread: {err}"
            ))
        })?;

    Ok(Spawned { command_tx, join })
}

fn worker_main(
    driver: &Arc<dyn DatabaseDriver>,
    params: &ConnectionParams,
    command_rx: mpsc::Receiver<Command>,
    report: ReportOpen,
    shared: Arc<SessionShared>,
    session: SessionId,
    limits: SessionLimits,
) {
    let torn = Cell::new(false);
    let mut connection = match call(&torn, || driver.connect(params)) {
        Ok(connection) => connection,
        Err(err) => {
            // Nothing was opened, so there is nothing to close and nothing to
            // adopt; the report's own answer is the session's one reply.
            let _ = report(Err(err));
            return;
        }
    };

    // Exactly once, here, where `connect` has just returned: the driver holds
    // its connect-time findings until somebody asks, and this is the only
    // moment at which "somebody" is defined (ADR-0002, C-6 amendment). They
    // travel out with the successful open through `WorkerHandle`, so a session
    // that never executes a statement still reports them.
    let connect_warnings = match call(&torn, || Ok(connection.take_connect_warnings())) {
        Ok(warnings) => warnings,
        Err(err) => {
            // A driver that panicked is *torn*: its internal state is unknown,
            // so it is dropped rather than closed (see the module
            // documentation), and the session never opens.
            drop(connection);
            let _ = report(Err(err));
            return;
        }
    };

    let cancel_handle = connection.cancel_handle();
    // Seed the driver's real transaction state before anything runs, rather
    // than leaving `SessionShared`'s own conservative construction default in
    // place until the first command happens to refresh it: a driver that
    // genuinely knows nothing is open yet (`Capabilities::exact_transaction_state`)
    // should say so immediately, so closing an untouched session does not
    // demand a disposition it cannot need.
    //
    // Silently, and that matters on the event path: this is the session's
    // initial value, not a change, and announcing it here would put an
    // unsolicited event ahead of the session's own `Opened`.
    shared.seed_driver_transaction_state(connection.transaction_state());
    let ready = Ready {
        cancel_handle: Arc::clone(&cancel_handle),
        cancel_kind: cancel_handle.kind(),
        connection_id: connection.id(),
        connect_warnings,
    };
    // `report` is what records the session and emits its `Opened` — and it runs
    // **here**, on this worker, *before* the command loop below. That ordering
    // is the whole reason "`Opened` is a session's first event" is structural:
    // this thread is the only producer of a reply for this session, and it
    // cannot produce one until `report` has returned, so nothing this session
    // ever says can overtake the `Opened`. It holds even for a consumer that
    // ignores events and polls `SessionRegistry::get` instead, because the
    // handle it would poll does not exist until `report` has recorded it.
    // Moving any part of this after the loop starts would break that silently
    // (see `registry_open.rs`, "a submit made the instant the session appears").
    if matches!(report(Ok(ready)), Adoption::Abandoned) {
        // Nobody is waiting for this session any more: the blocking caller
        // gave up, or `SessionRegistry::abandon` won the race with this
        // connect. Close the connection **here**, on the thread that opened
        // it, so a late success never leaves a live database session behind
        // and nothing is adopted after the fact (ADR-0002 H2, §B3).
        let _ = call(&torn, || connection.close());
        // And emit **nothing**. Whoever refused this session has already
        // answered its open and announced its `Terminal`, in that order
        // (ADR-0002 R3/R4); a "backstop" announcement here would race that
        // pair and could deliver `Terminal` *before* the `OpenFailed` it must
        // follow, which is exactly what ordering rule 3 forbids. The blocking
        // path reaches here only when its caller gave up, and has no sink
        // bound, so there is nothing to announce to either.
        return;
    }

    let mut worker = Worker {
        session,
        shared,
        limits,
        connection_id: connection.id(),
        exact_transaction_state: connection.capabilities().exact_transaction_state(),
        connection: Some(connection),
        cursors: HashMap::new(),
        lobs: HashMap::new(),
        torn,
    };

    while let Ok(command) = command_rx.recv() {
        if matches!(worker.dispatch(command, &command_rx), Flow::Exit) {
            break;
        }
    }

    // Whatever is still queued was accepted before the session ended, so it
    // must be answered *before* `Terminal` (ordering rule 3). Dropping the
    // receiver is what answers it: an unanswered `ReplyTo` emits the one
    // failure it owes as it goes (see `crate::reply`), so there is no separate
    // bookkeeping set of owed replies to keep in step.
    drop(command_rx);
    worker.release_results();
    let _ = worker.close_connection();
    // Covers the path no command took: the session handle was dropped without
    // an explicit close, so the channel simply ended.
    worker.shared.mark_closed();
    // Computed here, on the worker thread, once every command that was queued
    // ahead of the end has run: this is the only point at which the answer
    // cannot still be invalidated by work the caller had already submitted.
    let transaction_possibly_lost = worker.shared.transaction_lost_at_end();
    worker.shared.emit_terminal(transaction_possibly_lost);
}

enum Flow {
    Continue,
    Exit,
}

/// A large object taken out of a batch and parked on the worker thread.
struct ParkedLob {
    /// The result it came from, so closing that result releases it too. `None`
    /// for a locator delivered through an output bind, which belongs to the
    /// session rather than to any one cursor.
    result: Option<ResultSetId>,
    locator: LobLocator,
}

/// Everything one session's worker thread owns.
struct Worker {
    session: SessionId,
    shared: Arc<SessionShared>,
    limits: SessionLimits,
    connection_id: ConnectionId,
    exact_transaction_state: bool,
    connection: Option<Box<dyn DatabaseConnection>>,
    cursors: HashMap<ResultSetId, Box<dyn Cursor>>,
    lobs: HashMap<LobHandle, ParkedLob>,
    /// Set once a driver call panicked; see the module documentation.
    torn: Cell<bool>,
}

impl Worker {
    /// Runs one driver call through the connection, or `None` when there is no
    /// connection left.
    fn with_connection<T>(
        &mut self,
        f: impl FnOnce(&mut dyn DatabaseConnection) -> DbResult<T>,
    ) -> Option<DbResult<T>> {
        let torn = &self.torn;
        let connection = self.connection.as_mut()?;
        Some(call(torn, || f(connection.as_mut())))
    }

    fn transaction_state(&self) -> TransactionState {
        self.connection.as_ref().map_or(
            // No connection left: the conservative answer, not a confident one.
            TransactionState::Unknown,
            |connection| connection.transaction_state(),
        )
    }

    /// Runs one command, revalidating first when the last error asked for it,
    /// and releasing everything once the session becomes terminally lost.
    fn dispatch(&mut self, command: Command, queued: &mpsc::Receiver<Command>) -> Flow {
        self.shared.enter_driver_call();
        let flow = self.dispatch_inner(command, queued);
        self.shared.leave_driver_call();
        flow
    }

    fn dispatch_inner(&mut self, command: Command, queued: &mpsc::Receiver<Command>) -> Flow {
        if self.shared.needs_validation() && !matches!(command, Command::Close { .. }) {
            match self.with_connection(|connection| connection.ping()) {
                Some(Ok(())) => self.shared.mark_validated(),
                Some(Err(err)) => self.shared.mark_lost_from(&err),
                None => {}
            }
            if self.shared.is_lost() {
                // The session did not survive revalidation. The command must be
                // *answered*, not run: executing it would issue traffic on a
                // connection the core has already declared gone, and a driver
                // that happens to succeed would report `Ok` for a session
                // `is_lost()` says is dead.
                self.discard_connection();
                self.release_results();
                command.fail(self.shared.terminal_error());
                return self.announce_terminal(Flow::Continue, queued);
            }
        }

        let flow = self.run(command);

        if self.shared.is_lost() {
            self.discard_connection();
            self.release_results();
        }

        self.announce_terminal(flow, queued)
    }

    /// Emits [`SessionEvent::Terminal`] once the session has reached a
    /// terminal state, after answering everything that was already queued.
    ///
    /// Ordering rule 3 makes `Terminal` follow the reply of every request
    /// accepted before the transition, and a request is accepted the moment
    /// its command is queued — so the queue is drained and failed here, before
    /// the announcement. A `Close` found in that queue is *run* rather than
    /// failed: failing it would leave the worker looping while
    /// [`crate::DatabaseSession::close`] waited to join it.
    ///
    /// When the worker is on its way out (`Flow::Exit`) this does nothing:
    /// `worker_main`'s shutdown drops the command channel first, which answers
    /// the rest, and announces afterwards.
    fn announce_terminal(&mut self, flow: Flow, queued: &mpsc::Receiver<Command>) -> Flow {
        if matches!(flow, Flow::Exit)
            || self.shared.terminal_emitted()
            || !self.shared.lifecycle().is_terminal()
        {
            return flow;
        }
        while let Ok(command) = queued.try_recv() {
            if matches!(command, Command::Close { .. }) {
                // Runs the close, which answers it and ends the worker; the
                // shutdown path then answers whatever is left and announces.
                let close = self.run(command);
                debug_assert!(
                    matches!(close, Flow::Exit),
                    "a close reached at the terminal transition always ends the worker: the \
                     session is terminal, so its connection has already been discarded and \
                     `Worker::close` takes the `connection.is_none()` branch"
                );
                // Propagated rather than assumed: were a close ever to leave
                // the session open here, the loop would carry on and the next
                // command would drain and announce again, instead of exiting
                // with `Terminal` unsent.
                return close;
            }
            command.fail(self.shared.terminal_error());
        }
        // The session was lost rather than closed, so nothing resolved the
        // transaction: whatever may have been open went with the connection.
        let transaction_possibly_lost = self.shared.transaction_lost_at_end();
        self.shared.emit_terminal(transaction_possibly_lost);
        flow
    }

    fn run(&mut self, command: Command) -> Flow {
        match command {
            Command::Execute { statement, reply } => self.execute(statement, reply),
            Command::FetchBatch {
                result,
                max_rows,
                reply,
            } => self.fetch_batch(result, max_rows, reply),
            Command::CloseResult { result, reply } => {
                let outcome = match self.owned_result(result) {
                    Ok(id) => self.close_result(id),
                    Err(err) => Err(err),
                };
                if let Err(err) = &outcome {
                    self.shared.note_error(err);
                }
                reply.answer(outcome);
                Flow::Continue
            }
            Command::Commit { reply } => self.resolve_transaction(CloseDisposition::Commit, reply),
            Command::Rollback { reply } => {
                self.resolve_transaction(CloseDisposition::Rollback, reply)
            }
            Command::Savepoint { name, reply } => {
                let Some(outcome) = self.with_connection(|connection| connection.savepoint(&name))
                else {
                    reply.answer(Err(self.shared.terminal_error()));
                    return Flow::Continue;
                };
                if let Err(err) = &outcome {
                    self.shared.note_error(err);
                }
                reply.answer(outcome);
                Flow::Continue
            }
            Command::RollbackToSavepoint { name, reply } => {
                let Some(outcome) =
                    self.with_connection(|connection| connection.rollback_to_savepoint(&name))
                else {
                    reply.answer(Err(self.shared.terminal_error()));
                    return Flow::Continue;
                };
                match &outcome {
                    // The transaction remains open: only the handles it may
                    // have invalidated are released, not the possibly-active
                    // flag.
                    Ok(()) => self.release_results(),
                    Err(err) => self.shared.note_error(err),
                }
                let state = self.transaction_state();
                self.shared.note_driver_transaction_state(state);
                reply.answer(outcome);
                Flow::Continue
            }
            Command::Ping { reply } => {
                let Some(outcome) = self.with_connection(|connection| connection.ping()) else {
                    reply.answer(Err(self.shared.terminal_error()));
                    return Flow::Continue;
                };
                match &outcome {
                    Ok(()) => self.shared.mark_validated(),
                    Err(err) => self.shared.note_error(err),
                }
                reply.answer(outcome);
                Flow::Continue
            }
            Command::ReadLobChunk {
                lob,
                max_bytes,
                reply,
            } => self.read_lob_chunk(lob, max_bytes, reply),
            Command::CloseLob { lob, reply } => {
                let outcome = self.owned_lob(lob).map(|handle| {
                    // Dropping the locator *here* is the whole point: it is a
                    // handle derived from the connection, so it must be
                    // released on the thread that owns it.
                    self.lobs.remove(&handle);
                });
                reply.answer(outcome);
                Flow::Continue
            }
            Command::Close { intent, reply } => self.close(intent, reply),
        }
    }

    // ---------------------------------------------------------------- execute

    fn execute(&mut self, statement: Statement, reply: ReplyTo<ExecuteOutcome>) -> Flow {
        // Ordering rule 4: the statement has left the queue and this is the
        // limit actually armed on it, which is what an honest "running" state
        // shows on a driver that cannot interrupt a running call. Emitted
        // before the driver call, from the one thread that produces this
        // session's events, so it always precedes the matching `Executed` and
        // always follows the previous request's reply.
        if let Some(request) = reply.request() {
            self.shared.emit(SessionEvent::Executing {
                session: self.session,
                request,
                deadline: statement.deadline(),
            });
        }

        let Some(outcome) = self.with_connection(|connection| connection.execute(&statement))
        else {
            reply.answer(Err(self.shared.terminal_error()));
            return Flow::Continue;
        };

        let mut outcome = match outcome {
            Ok(outcome) => outcome,
            Err(err) => {
                self.shared.note_error(&err);
                let state = self.transaction_state();
                self.shared.note_driver_transaction_state(state);
                reply.answer(Err(err));
                return Flow::Continue;
            }
        };

        let statement_kind = outcome.statement_kind();
        let committed_implicitly = outcome.committed_implicitly();
        if committed_implicitly {
            self.shared.note_commit_or_rollback();
            // A server that commits around DDL commonly closes cursors too;
            // treat every open result as invalidated (ADR-0002 D2, "Lifecycle
            // of derived handles").
            self.release_results();
        }

        // Everything this command registered, so it can be released again if the
        // caller is no longer listening.
        let mut registered: Vec<ResultSetId> = Vec::new();

        let mut columns = Vec::new();
        let result = match outcome.take_cursor() {
            None => None,
            Some(cursor) => match self.register_cursor(cursor) {
                Ok(id) => {
                    registered.push(id.result());
                    // Plain data, copied here on the worker thread: the cursor
                    // itself never leaves it, so this is the only chance to
                    // learn the column names the caller's grid needs.
                    if let Some(cursor) = self.cursors.get(&id.result()) {
                        // Through `call` like every other driver call: the
                        // contract says `columns()` is a cheap accessor, but a
                        // panic here must not unwind the worker. A torn cursor
                        // reports no columns rather than failing a statement
                        // that already ran on the server.
                        columns =
                            call(&self.torn, || Ok(cursor.columns().to_vec())).unwrap_or_default();
                    }
                    Some(id)
                }
                Err(err) => {
                    self.shared.note_error(&err);
                    reply.answer(Err(err));
                    return Flow::Continue;
                }
            },
        };

        let out_values = match self.take_out_values(&mut outcome, &mut registered) {
            Ok(out_values) => out_values,
            Err(err) => {
                self.release_registered(registered);
                self.shared.note_error(&err);
                reply.answer(Err(err));
                return Flow::Continue;
            }
        };

        let driver_state = self.transaction_state();
        if committed_implicitly {
            self.shared.note_driver_transaction_state(driver_state);
        } else {
            self.shared
                .note_statement(statement_kind, driver_state, self.exact_transaction_state);
        }

        let value = ExecuteOutcome {
            result,
            columns,
            rows_affected: outcome.rows_affected(),
            statement_kind,
            committed_implicitly,
            warnings: outcome.warnings().to_vec(),
            out_values,
        };
        if !reply.answer(Ok(value)) {
            // The caller dropped its `Completion` before the reply landed.
            // Whatever this command registered is unreachable now, so it must be
            // released here rather than living until the session closes.
            self.release_registered(registered);
        }
        Flow::Continue
    }

    /// Moves the driver's output values onto the core side, replacing every
    /// driver-owned handle with a core-owned one.
    ///
    /// A `Value::Cursor` and a `Value::Lob` are both handles derived from the
    /// connection, so neither may leave this thread (ADR-0002 D1/D2). The cursor
    /// joins the same `cursors` map an ordinary query's cursor goes into and the
    /// caller gets a [`ResultId`]; the locator is parked and the caller gets a
    /// [`LobHandle`].
    fn take_out_values(
        &mut self,
        outcome: &mut reldex_db_driver_api::ExecutionOutcome,
        registered: &mut Vec<ResultSetId>,
    ) -> DbResult<OutValues> {
        let taken = outcome.take_out_values();
        match taken {
            reldex_db_driver_api::OutValues::None => Ok(OutValues::None),
            reldex_db_driver_api::OutValues::Positional(values) => {
                let mut out = Vec::with_capacity(values.len());
                for slot in values {
                    out.push(match slot {
                        None => None,
                        Some(value) => Some(self.adopt_out_value(value, registered)?),
                    });
                }
                Ok(OutValues::Positional(out))
            }
            reldex_db_driver_api::OutValues::Named(values) => {
                let mut out = Vec::with_capacity(values.len());
                for (name, value) in values {
                    out.push((name, self.adopt_out_value(value, registered)?));
                }
                Ok(OutValues::Named(out))
            }
            // `OutValues` is `#[non_exhaustive]`: a shape this core does not
            // know about must not be silently reported as "no output binds",
            // but there is nothing useful it can say about one either.
            _ => Ok(OutValues::None),
        }
    }

    fn adopt_out_value(
        &mut self,
        value: reldex_db_driver_api::Value,
        registered: &mut Vec<ResultSetId>,
    ) -> DbResult<OutValue> {
        match value {
            reldex_db_driver_api::Value::Cursor(cursor) => {
                let id = self.register_cursor(cursor)?;
                registered.push(id.result());
                Ok(OutValue::Result(id))
            }
            reldex_db_driver_api::Value::Lob(locator) => {
                Ok(OutValue::Lob(self.park_lob(None, locator)?))
            }
            other => Ok(OutValue::Value(other)),
        }
    }

    // ------------------------------------------------------------------ fetch

    fn fetch_batch(
        &mut self,
        result: ResultId,
        max_rows: NonZeroUsize,
        reply: ReplyTo<FetchedBatch>,
    ) -> Flow {
        let id = match self.owned_result(result) {
            Ok(id) => id,
            Err(err) => {
                reply.answer(Err(err));
                return Flow::Continue;
            }
        };
        let Some(cursor) = self.cursors.get_mut(&id) else {
            reply.answer(Err(self.unknown_result_error()));
            return Flow::Continue;
        };
        let torn = &self.torn;
        let outcome = call(torn, || cursor.fetch_batch(max_rows));

        match outcome {
            Ok(batch) => {
                let fetched = match self.park_batch_lobs(id, batch) {
                    Ok(fetched) => fetched,
                    Err(err) => {
                        self.shared.note_error(&err);
                        reply.answer(Err(err));
                        return Flow::Continue;
                    }
                };
                let handles: Vec<LobHandle> = fetched.lobs().map(|(_, _, lob)| lob).collect();
                if !reply.answer(Ok(fetched)) {
                    // Nobody is listening, so nothing will ever read — or close
                    // — these locators. Drop them here, on the thread that owns
                    // them.
                    for lob in handles {
                        self.lobs.remove(&lob);
                    }
                }
            }
            Err(err) => {
                self.shared.note_error(&err);
                // After any error from `fetch_batch`, the only legal call on a
                // cursor is `close()` (ADR-0002 D2). Make it, here, rather than
                // dropping the cursor and hoping.
                let _ = self.close_result(id);
                reply.answer(Err(err));
            }
        }
        Flow::Continue
    }

    /// Takes every large-object locator out of `batch` and parks it, so only
    /// plain data reaches the caller (ADR-0002 D1/D2).
    fn park_batch_lobs(
        &mut self,
        result: ResultSetId,
        mut batch: RowBatch,
    ) -> DbResult<FetchedBatch> {
        let mut lobs = Vec::new();
        for index in 0..batch.column_count() {
            let rows = match batch.column(index) {
                Some(column) if column.kind() == ColumnKind::Lob => column.len(),
                _ => continue,
            };
            for row in 0..rows {
                let taken = batch
                    .column_mut(index)
                    .and_then(|column| column.take_lob(row));
                if let Some(locator) = taken {
                    lobs.push((row, index, self.park_lob(Some(result), locator)?));
                }
            }
        }
        Ok(FetchedBatch::new(batch, lobs))
    }

    fn park_lob(
        &mut self,
        result: Option<ResultSetId>,
        locator: LobLocator,
    ) -> DbResult<LobHandle> {
        if locator.connection_id() != self.connection_id {
            return Err(DbError::internal(format!(
                "reldex-db-core: a driver returned a large object belonging to {} on the \
                 session that owns {}",
                locator.connection_id(),
                self.connection_id
            )));
        }
        let handle = LobHandle::allocate(self.session);
        self.lobs.insert(handle, ParkedLob { result, locator });
        Ok(handle)
    }

    fn read_lob_chunk(
        &mut self,
        lob: LobHandle,
        max_bytes: NonZeroUsize,
        reply: ReplyTo<Vec<u8>>,
    ) -> Flow {
        let handle = match self.owned_lob(lob) {
            Ok(handle) => handle,
            Err(err) => {
                reply.answer(Err(err));
                return Flow::Continue;
            }
        };
        // Bound the damage a single request can do: the caller asks for a chunk
        // size, but it must not be able to ask this process for an arbitrary
        // allocation.
        let capacity = max_bytes.get().min(self.limits.max_lob_chunk_bytes().get());
        let Some(parked) = self.lobs.get_mut(&handle) else {
            reply.answer(Err(Self::closed_lob_error(handle)));
            return Flow::Continue;
        };
        let mut buf = vec![0_u8; capacity];
        let torn = &self.torn;
        let outcome = call(torn, || parked.locator.read_chunk(&mut buf));
        match outcome {
            Ok(read) => {
                buf.truncate(read);
                reply.answer(Ok(buf));
            }
            Err(err) => {
                self.shared.note_error(&err);
                // "After any error from `read_chunk`, the only legal action is
                // to drop it" (ADR-0002 D2). Drop it here, on the owning thread.
                self.lobs.remove(&handle);
                reply.answer(Err(err));
            }
        }
        Flow::Continue
    }

    // ------------------------------------------------------------- handle map

    fn register_cursor(&mut self, cursor: Box<dyn Cursor>) -> DbResult<ResultId> {
        if cursor.connection_id() != self.connection_id {
            let torn = &self.torn;
            let foreign = cursor.connection_id();
            let _ = call(torn, || cursor.close());
            return Err(DbError::internal(format!(
                "reldex-db-core: a driver returned a cursor belonging to {foreign} on the \
                 session that owns {}",
                self.connection_id
            )));
        }
        let limit = self.limits.max_open_results().get();
        if self.cursors.len() >= limit {
            let torn = &self.torn;
            let _ = call(torn, || cursor.close());
            return Err(DbError::new(
                ErrorKind::Resource,
                format!(
                    "reldex-db-core: this session already has {limit} open results (its \
                     configured limit); close one before opening another"
                ),
            ));
        }
        let id = cursor.id();
        self.cursors.insert(id, cursor);
        Ok(ResultId::new(self.session, id))
    }

    /// Checks that a result handle belongs to *this* session before looking it
    /// up, so a handle from another session is reported as foreign rather than
    /// as one that was closed.
    fn owned_result(&self, result: ResultId) -> DbResult<ResultSetId> {
        if result.owner() == self.session {
            return Ok(result.result());
        }
        Err(DbError::internal(format!(
            "reldex-db-core: {result} belongs to another session; a result handle is only \
             valid on the session that produced it"
        )))
    }

    fn owned_lob(&self, lob: LobHandle) -> DbResult<LobHandle> {
        if lob.owner() == self.session {
            return Ok(lob);
        }
        Err(DbError::internal(format!(
            "reldex-db-core: {lob} belongs to another session; a large-object handle is only \
             valid on the session that produced it"
        )))
    }

    fn unknown_result_error(&self) -> DbError {
        // If the session is already lost, that is *why* the result is gone
        // (session loss invalidates every open result) — say so, rather than a
        // generic "unknown handle" that would let a caller keep treating this
        // session as fine.
        if self.shared.is_lost() {
            self.shared.terminal_error()
        } else {
            DbError::internal("reldex-db-core: unknown, closed or invalidated result handle")
        }
    }

    fn closed_lob_error(lob: LobHandle) -> DbError {
        DbError::internal(format!(
            "reldex-db-core: {lob} is closed; its result or session released it"
        ))
    }

    /// Closes one result on this thread, together with every large object taken
    /// from it.
    fn close_result(&mut self, id: ResultSetId) -> DbResult<()> {
        self.lobs.retain(|_, parked| parked.result != Some(id));
        match self.cursors.remove(&id) {
            None => Ok(()),
            Some(cursor) => {
                if self.torn.get() {
                    drop(cursor);
                    return Ok(());
                }
                let torn = &self.torn;
                call(torn, || cursor.close())
            }
        }
    }

    /// Releases handles this command registered but could not hand over.
    fn release_registered(&mut self, registered: Vec<ResultSetId>) {
        for id in registered {
            let _ = self.close_result(id);
        }
    }

    /// Releases every open result and parked large object, on this thread.
    ///
    /// Cursors are **closed**, not merely dropped: `Cursor::close` is the
    /// driver's chance to release server-side state, and the contract makes it
    /// idempotent and safe after the connection is gone (ADR-0002 D2).
    fn release_results(&mut self) {
        self.lobs.clear();
        let cursors: Vec<Box<dyn Cursor>> =
            self.cursors.drain().map(|(_, cursor)| cursor).collect();
        for cursor in cursors {
            if self.torn.get() {
                drop(cursor);
                continue;
            }
            let torn = &self.torn;
            let _ = call(torn, || cursor.close());
        }
    }

    // ------------------------------------------------------------- lifecycle

    fn resolve_transaction(&mut self, disposition: CloseDisposition, reply: ReplyTo<()>) -> Flow {
        let Some(outcome) = self.with_connection(|connection| match disposition {
            CloseDisposition::Commit => connection.commit(),
            CloseDisposition::Rollback => connection.rollback(),
        }) else {
            reply.answer(Err(self.shared.terminal_error()));
            return Flow::Continue;
        };
        match &outcome {
            Ok(()) => {
                self.shared.note_commit_or_rollback();
                self.release_results();
            }
            Err(err) => self.shared.note_error(err),
        }
        let state = self.transaction_state();
        self.shared.note_driver_transaction_state(state);
        reply.answer(outcome);
        Flow::Continue
    }

    fn close(&mut self, intent: CloseIntent, reply: CloseReplyTo) -> Flow {
        if self.connection.is_none() {
            // The session is already gone. Closing it is not a silent success:
            // whatever transaction it held went with it, and a `Commit`
            // disposition committed nothing (`SPEC.md` §10). Report that, and
            // still release everything.
            let error = self.shared.lost_transaction_error();
            self.release_results();
            self.shared.mark_closed();
            self.shared.mark_ended();
            reply.answer(Err(CloseError::Failed(error)));
            return Flow::Exit;
        }

        let disposition = match intent {
            CloseIntent::Explicit(disposition) => {
                if disposition.is_none() && self.shared.has_possibly_active_transaction() {
                    // Decided here, on the worker, *after* every command queued
                    // ahead of this one has run — the flag the caller's thread
                    // can see is by definition a snapshot from before them.
                    reply.answer(Err(CloseError::DecisionRequired));
                    return Flow::Continue;
                }
                disposition
            }
            CloseIntent::Abandon => None,
        };

        if let Some(disposition) = disposition {
            let outcome = self.with_connection(|connection| match disposition {
                CloseDisposition::Commit => connection.commit(),
                CloseDisposition::Rollback => connection.rollback(),
            });
            if let Some(Err(err)) = outcome {
                self.shared.note_error(&err);
                // The transaction is still there. Destroying it by closing
                // anyway would be exactly the silent data loss `SPEC.md` §10
                // forbids, so the session stays open and the caller can retry,
                // roll back, or close with the other disposition.
                let close_error = match disposition {
                    CloseDisposition::Commit => CloseError::CommitFailed(err),
                    CloseDisposition::Rollback => CloseError::RollbackFailed(err),
                };
                reply.answer(Err(close_error));
                return Flow::Continue;
            }
            self.shared.note_commit_or_rollback();
            // The disposition the caller asked for succeeded, so this session
            // is about to end with its transaction *resolved*: `Terminal` must
            // say `transaction_possibly_lost: false` even for a driver that
            // cannot rule a transaction out on its own
            // (`TransactionState::Unknown` reads as "may be open" forever).
            // Recorded as a fact about what happened rather than re-derived
            // from the driver, which also keeps this path from emitting one
            // last `TransactionStateChanged` between the reply and `Terminal`.
            self.shared.mark_transaction_resolved_at_end();
        }

        self.release_results();
        let outcome = self.close_connection();
        self.shared.mark_closed();
        self.shared.mark_ended();
        reply.answer(outcome.map_err(CloseError::Failed));
        Flow::Exit
    }

    /// Drops the connection after the session became terminally lost.
    fn discard_connection(&mut self) {
        let _ = self.close_connection();
    }

    fn close_connection(&mut self) -> DbResult<()> {
        let Some(connection) = self.connection.take() else {
            return Ok(());
        };
        if self.torn.get() {
            // A driver call panicked. The object's internal state is unknown,
            // so calling `close()` on it is as likely to panic again as to
            // release anything; drop it here instead and say so. See the module
            // documentation for why this guarantee stops at drivers that unwind.
            drop(connection);
            return Ok(());
        }
        let torn = &self.torn;
        call(torn, || connection.close())
    }
}
