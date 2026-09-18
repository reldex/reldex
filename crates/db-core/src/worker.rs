//! The per-session worker thread: the only thread that ever touches a
//! `Box<dyn DatabaseConnection>` (ADR-0002 D1/D2).
//!
//! Commands arrive over an [`std::sync::mpsc`] channel and are processed
//! strictly in the order they were sent — the channel's own FIFO order is the
//! whole mechanism, so a statement that blocks (a long query, a mock
//! `Action::Block`) simply makes every command queued behind it on *this
//! session* wait its turn, exactly like a single-threaded worksheet would;
//! cancellation reaches a blocked call through the driver's
//! `Arc<dyn CancelHandle>` instead, which is why [`crate::DatabaseSession::cancel`]
//! never goes through this channel at all.

use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc;
use std::sync::{Arc, mpsc::Sender};
use std::thread;

use reldex_db_driver_api::{
    CancelHandle, CancelKind, ConnectionId, ConnectionParams, Cursor, DatabaseConnection,
    DatabaseDriver, DbError, DbResult, LobLocator, ResultSetId, RowBatch, SavepointName,
    SessionState, Statement,
};

use crate::ids::SessionId;
use crate::session::{CloseDisposition, ExecuteOutcome, OutValue, OutValues};
use crate::shared::SessionShared;

/// Moves the driver's output values onto the core side, registering any nested
/// `REF CURSOR` as a result of this session.
///
/// A `Value::Cursor` is a handle derived from the connection, so it must stay
/// on this thread (ADR-0002 D1/D2). It is put in the same `cursors` map an
/// ordinary query's cursor goes into, and the caller gets its
/// [`ResultSetId`] — so fetching a REF CURSOR is the same API call as fetching
/// anything else, and closing the session releases it the same way.
fn take_out_values(
    outcome: &mut reldex_db_driver_api::ExecutionOutcome,
    cursors: &mut HashMap<ResultSetId, Box<dyn Cursor>>,
) -> OutValues {
    let mut register = |value: reldex_db_driver_api::Value| match value {
        reldex_db_driver_api::Value::Cursor(cursor) => {
            let id = cursor.id();
            cursors.insert(id, cursor);
            OutValue::Result(id)
        }
        other => OutValue::Value(other),
    };
    match outcome.take_out_values() {
        reldex_db_driver_api::OutValues::None => OutValues::None,
        reldex_db_driver_api::OutValues::Positional(values) => OutValues::Positional(
            values
                .into_iter()
                .map(|slot| slot.map(&mut register))
                .collect(),
        ),
        reldex_db_driver_api::OutValues::Named(values) => OutValues::Named(
            values
                .into_iter()
                .map(|(name, value)| (name, register(value)))
                .collect(),
        ),
        // `OutValues` is `#[non_exhaustive]`: a shape this core does not know
        // about must not be silently reported as "no output binds", but there
        // is nothing useful it can say about one either.
        _ => OutValues::None,
    }
}

/// One outstanding request's reply channel. A plain [`mpsc::channel`] used
/// once as a oneshot: see [`crate::Completion`].
pub(crate) type Reply<T> = Sender<DbResult<T>>;

/// A message sent to a session's worker thread. Requests are processed in the
/// order they arrive; see the module documentation.
pub(crate) enum Command {
    /// Execute a statement.
    Execute {
        statement: Statement,
        reply: Reply<ExecuteOutcome>,
    },
    /// Fetch the next batch of an open result.
    FetchBatch {
        result: ResultSetId,
        max_rows: std::num::NonZeroUsize,
        reply: Reply<RowBatch>,
    },
    /// Release a result's resources.
    CloseResult {
        result: ResultSetId,
        reply: Reply<()>,
    },
    /// Commit the open transaction.
    Commit { reply: Reply<()> },
    /// Roll the open transaction back.
    Rollback { reply: Reply<()> },
    /// Establish a savepoint.
    Savepoint {
        name: SavepointName,
        reply: Reply<()>,
    },
    /// Roll back to a savepoint, leaving the transaction open.
    RollbackToSavepoint {
        name: SavepointName,
        reply: Reply<()>,
    },
    /// Validate that the session is still alive.
    Ping { reply: Reply<()> },
    /// Reads the next chunk of a LOB, on this worker thread, and hands the
    /// locator back so the caller can read further (ADR-0002 D1/D2: a
    /// derived handle is used only on the connection's owning worker
    /// thread). An empty `Vec` means the object is exhausted.
    ReadLobChunk {
        locator: LobLocator,
        max_bytes: std::num::NonZeroUsize,
        reply: Reply<(LobLocator, Vec<u8>)>,
    },
    /// Resolve the transaction (if a disposition is given) and close the
    /// connection. Always the last command a worker processes.
    Close {
        disposition: Option<CloseDisposition>,
        reply: Reply<()>,
    },
}

/// Everything [`spawn`] hands back once the connection is open.
pub(crate) struct WorkerHandle {
    pub(crate) command_tx: Sender<Command>,
    pub(crate) join: thread::JoinHandle<()>,
    pub(crate) shared: Arc<SessionShared>,
    pub(crate) cancel_handle: Arc<dyn CancelHandle>,
    pub(crate) cancel_kind: CancelKind,
    pub(crate) connection_id: ConnectionId,
}

struct Ready {
    cancel_handle: Arc<dyn CancelHandle>,
    cancel_kind: CancelKind,
    connection_id: ConnectionId,
}

/// Calls into driver code, converting a panic into
/// [`reldex_db_driver_api::ErrorKind::DriverInternal`] instead of unwinding
/// across the worker boundary (`AGENTS.md` — no panics escape the worker
/// thread; a panicking driver call marks the session lost).
fn call<T>(f: impl FnOnce() -> DbResult<T>) -> DbResult<T> {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => Err(panic_to_error(&payload)),
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

/// Spawns the dedicated worker thread for one session and connects it,
/// blocking the calling thread only on the reply channel — the connect call
/// itself always runs on the new worker thread, never on the caller's
/// (`SPEC.md` §11/§19).
pub(crate) fn spawn(
    driver: Arc<dyn DatabaseDriver>,
    params: ConnectionParams,
    session_id: SessionId,
) -> DbResult<WorkerHandle> {
    let (command_tx, command_rx) = mpsc::channel::<Command>();
    let (ready_tx, ready_rx) = mpsc::channel::<DbResult<Ready>>();
    let shared = Arc::new(SessionShared::new());
    let shared_for_worker = Arc::clone(&shared);

    let join = thread::Builder::new()
        .name(format!("reldex-session-{}", session_id.get()))
        .spawn(move || worker_main(&driver, &params, command_rx, &ready_tx, &shared_for_worker))
        .map_err(|err| {
            DbError::internal(format!(
                "reldex-db-core: failed to spawn session worker thread: {err}"
            ))
        })?;

    match ready_rx.recv() {
        Ok(Ok(ready)) => Ok(WorkerHandle {
            command_tx,
            join,
            shared,
            cancel_handle: ready.cancel_handle,
            cancel_kind: ready.cancel_kind,
            connection_id: ready.connection_id,
        }),
        Ok(Err(err)) => {
            let _ = join.join();
            Err(err)
        }
        Err(_) => {
            let panicked = join.join().is_err();
            Err(DbError::internal(if panicked {
                "reldex-db-core: session worker thread panicked while connecting"
            } else {
                "reldex-db-core: session worker exited without connecting"
            }))
        }
    }
}

fn worker_main(
    driver: &Arc<dyn DatabaseDriver>,
    params: &ConnectionParams,
    command_rx: mpsc::Receiver<Command>,
    ready_tx: &Sender<DbResult<Ready>>,
    shared: &Arc<SessionShared>,
) {
    let connection = match call(|| driver.connect(params)) {
        Ok(connection) => connection,
        Err(err) => {
            let _ = ready_tx.send(Err(err));
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
    shared.note_driver_transaction_state(connection.transaction_state());
    let ready = Ready {
        cancel_handle: Arc::clone(&cancel_handle),
        cancel_kind: cancel_handle.kind(),
        connection_id: connection.id(),
    };
    if ready_tx.send(Ok(ready)).is_err() {
        // Nobody is waiting for this session any more (the caller gave up).
        // Still close cleanly so nothing leaks.
        let _ = call(|| connection.close());
        return;
    }

    let mut cursors: HashMap<ResultSetId, Box<dyn Cursor>> = HashMap::new();
    let mut connection_slot = Some(connection);
    for command in command_rx {
        if matches!(
            dispatch(&mut connection_slot, &mut cursors, shared, command),
            Action::Exit
        ) {
            break;
        }
    }

    if let Some(connection) = connection_slot.take() {
        let _ = call(|| connection.close());
    }
}

enum Action {
    Continue,
    Exit,
}

/// Runs one command, updating `shared` and, once the session becomes
/// terminally lost, dropping the connection so every later command takes the
/// cheap "already gone" path instead of touching driver code again.
fn dispatch(
    connection_slot: &mut Option<Box<dyn DatabaseConnection>>,
    cursors: &mut HashMap<ResultSetId, Box<dyn Cursor>>,
    shared: &Arc<SessionShared>,
    command: Command,
) -> Action {
    if shared.needs_validation() && !matches!(command, Command::Close { .. }) {
        if let Some(connection) = connection_slot.as_mut() {
            match call(|| connection.ping()) {
                Ok(()) => shared.mark_validated(),
                Err(err) => shared.mark_lost_from(&err),
            }
        }
    }

    let action = run(connection_slot, cursors, shared, command);

    if shared.is_lost() {
        if let Some(connection) = connection_slot.take() {
            let _ = call(|| connection.close());
        }
        cursors.clear();
    }

    action
}

fn run(
    connection_slot: &mut Option<Box<dyn DatabaseConnection>>,
    cursors: &mut HashMap<ResultSetId, Box<dyn Cursor>>,
    shared: &Arc<SessionShared>,
    command: Command,
) -> Action {
    match command {
        Command::Execute { statement, reply } => {
            let Some(connection) = connection_slot.as_mut() else {
                let _ = reply.send(Err(shared.terminal_error()));
                return Action::Continue;
            };
            match call(|| connection.execute(&statement)) {
                Ok(mut outcome) => {
                    let result = outcome.take_cursor().map(|cursor| {
                        let id = cursor.id();
                        cursors.insert(id, cursor);
                        id
                    });
                    let statement_kind = outcome.statement_kind();
                    let committed_implicitly = outcome.committed_implicitly();
                    if committed_implicitly {
                        shared.note_commit_or_rollback();
                        // A server that commits around DDL commonly closes
                        // cursors too; treat every open result as invalidated
                        // (ADR-0002 D2, "Lifecycle of derived handles").
                        cursors.clear();
                    } else {
                        shared.note_statement_kind(statement_kind);
                    }
                    let out_values = take_out_values(&mut outcome, cursors);
                    shared.note_driver_transaction_state(connection.transaction_state());
                    let _ = reply.send(Ok(ExecuteOutcome {
                        result,
                        rows_affected: outcome.rows_affected(),
                        statement_kind,
                        committed_implicitly,
                        warnings: outcome.warnings().to_vec(),
                        out_values,
                    }));
                }
                Err(err) => {
                    shared.note_error(&err);
                    shared.note_driver_transaction_state(connection.transaction_state());
                    let _ = reply.send(Err(err));
                }
            }
            Action::Continue
        }
        Command::FetchBatch {
            result,
            max_rows,
            reply,
        } => {
            match cursors.get_mut(&result) {
                None => {
                    // If the session is already lost, that is *why* the
                    // result is gone (session loss invalidates every open
                    // result) — say so, rather than a generic "unknown
                    // handle" that reports a merely-`Usable` session state
                    // and would let a caller keep treating this session as
                    // fine.
                    let error = if shared.is_lost() {
                        shared.terminal_error()
                    } else {
                        DbError::internal("reldex-db-core: unknown or invalidated result handle")
                    };
                    let _ = reply.send(Err(error));
                }
                Some(cursor) => match call(|| cursor.fetch_batch(max_rows)) {
                    Ok(batch) => {
                        let _ = reply.send(Ok(batch));
                    }
                    Err(err) => {
                        shared.note_error(&err);
                        cursors.remove(&result);
                        let _ = reply.send(Err(err));
                    }
                },
            }
            Action::Continue
        }
        Command::CloseResult { result, reply } => {
            let outcome = match cursors.remove(&result) {
                Some(cursor) => call(|| cursor.close()),
                None => Ok(()),
            };
            if let Err(err) = &outcome {
                shared.note_error(err);
            }
            let _ = reply.send(outcome);
            Action::Continue
        }
        Command::Commit { reply } => {
            let Some(connection) = connection_slot.as_mut() else {
                let _ = reply.send(Err(shared.terminal_error()));
                return Action::Continue;
            };
            let outcome = call(|| connection.commit());
            match &outcome {
                Ok(()) => {
                    shared.note_commit_or_rollback();
                    cursors.clear();
                }
                Err(err) => shared.note_error(err),
            }
            shared.note_driver_transaction_state(connection.transaction_state());
            let _ = reply.send(outcome);
            Action::Continue
        }
        Command::Rollback { reply } => {
            let Some(connection) = connection_slot.as_mut() else {
                let _ = reply.send(Err(shared.terminal_error()));
                return Action::Continue;
            };
            let outcome = call(|| connection.rollback());
            match &outcome {
                Ok(()) => {
                    shared.note_commit_or_rollback();
                    cursors.clear();
                }
                Err(err) => shared.note_error(err),
            }
            shared.note_driver_transaction_state(connection.transaction_state());
            let _ = reply.send(outcome);
            Action::Continue
        }
        Command::Savepoint { name, reply } => {
            let Some(connection) = connection_slot.as_mut() else {
                let _ = reply.send(Err(shared.terminal_error()));
                return Action::Continue;
            };
            let outcome = call(|| connection.savepoint(&name));
            if let Err(err) = &outcome {
                shared.note_error(err);
            }
            let _ = reply.send(outcome);
            Action::Continue
        }
        Command::RollbackToSavepoint { name, reply } => {
            let Some(connection) = connection_slot.as_mut() else {
                let _ = reply.send(Err(shared.terminal_error()));
                return Action::Continue;
            };
            let outcome = call(|| connection.rollback_to_savepoint(&name));
            match &outcome {
                // The transaction remains open: only the cursors it may have
                // invalidated are cleared, not the possibly-active flag.
                Ok(()) => cursors.clear(),
                Err(err) => shared.note_error(err),
            }
            shared.note_driver_transaction_state(connection.transaction_state());
            let _ = reply.send(outcome);
            Action::Continue
        }
        Command::Ping { reply } => {
            let Some(connection) = connection_slot.as_mut() else {
                let _ = reply.send(Err(shared.terminal_error()));
                return Action::Continue;
            };
            let outcome = call(|| connection.ping());
            match &outcome {
                Ok(()) => shared.mark_validated(),
                Err(err) => shared.note_error(err),
            }
            let _ = reply.send(outcome);
            Action::Continue
        }
        Command::ReadLobChunk {
            mut locator,
            max_bytes,
            reply,
        } => {
            let mut buf = vec![0_u8; max_bytes.get()];
            match call(|| locator.read_chunk(&mut buf)) {
                Ok(n) => {
                    buf.truncate(n);
                    let _ = reply.send(Ok((locator, buf)));
                }
                Err(err) => {
                    shared.note_error(&err);
                    let _ = reply.send(Err(err));
                }
            }
            Action::Continue
        }
        Command::Close { disposition, reply } => {
            let disposition_result: DbResult<()> = match (connection_slot.as_mut(), disposition) {
                (Some(connection), Some(CloseDisposition::Commit)) => call(|| connection.commit()),
                (Some(connection), Some(CloseDisposition::Rollback)) => {
                    call(|| connection.rollback())
                }
                (_, None) | (None, Some(_)) => Ok(()),
            };
            let close_result = match connection_slot.take() {
                Some(connection) => call(|| connection.close()),
                None => Ok(()),
            };
            cursors.clear();
            let _ = reply.send(disposition_result.and(close_result));
            Action::Exit
        }
    }
}
