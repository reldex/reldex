//! Sessions, and the **interim** completion pump that turns `db-core`'s
//! `Completion<T>` into events (ADR-0003 D3/D5).
//!
//! # Why the pump is here and not in `db-core`
//!
//! `phase-1.md` §B2/§B3 design an `EventQueue`/`EventSink`/`SessionRegistry`
//! inside `db-core`, delivered by tasks M2.5/M2.6. Building even a subset of
//! that now would mean doing M2.5's central decision — turning the worker's
//! `Reply<T>` into `enum ReplyTo<T>` — before the task that owns it, against
//! the suite that has to be parameterised over both paths. So this crate keeps
//! its own pump, which is deletable in one commit:
//!
//! * **One thread per session.** It performs the blocking `open_session`, then
//!   loops: take the next submitted request, **block** in `Completion::wait`
//!   for its reply, push exactly one event. Nothing polls and nothing sleeps,
//!   so no interval sits between a reply and the UI hearing about it.
//! * **Order is the channel's.** Submitting takes the session's lock, sends
//!   the request to `db-core` and hands the pump its `Completion` — in that
//!   order, atomically. `db-core` replies to one session's requests strictly
//!   in order (its own FIFO worker queue), so waiting on the oldest
//!   outstanding completion is both correct and exact: per-session ordering
//!   holds without a sequence number anywhere.
//! * **Submitting still happens on the caller's thread**, because
//!   `DatabaseSession::execute` only enqueues. Doing it on the pump would
//!   serialise submission behind the previous reply and destroy the
//!   adapter's ability to have the next fetch already in flight.
//! * **Exactly one reply per accepted request.** A request that is rejected
//!   (any non-`Ok` status) was never accepted and produces no event. Once a
//!   close succeeds the pump drains everything still queued, so requests that
//!   raced the close get their one failure reply too.
//!
//! When M2.5/M2.6 land, this module keeps its exported functions and its
//! registry and loses the threads: it will forward a `db-core` `SessionEvent`
//! instead of waiting on a `Completion`.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

use reldex_db_core::{
    CancelKind, CancelOutcome, CloseDisposition, CloseError, ColumnMetadata, Completion,
    DatabaseSession, DbError, ErrorKind, ExecuteOutcome, FetchedBatch, ResultId, Statement,
};

use crate::batch::ReldexBatch;
use crate::error::{ReldexError, ReldexSessionState, set_last_argument_error, set_last_error};
use crate::event::{QueuedEvent, ReldexEventKind};
use crate::format::ReldexTextArena;
use crate::hub::{ReldexHub, with_hub};
use crate::mock::{BlockControl, ReldexMockScenarioConfig, build_driver, release_block};
use crate::status::{ReldexStatus, entry};
use crate::strings::{CStruct, ReldexStr, read_in_struct};

/// Identifies one session for this hub's lifetime. Never reused.
pub type ReldexSessionId = u64;

/// Identifies one open result set, scoped to the session that produced it.
///
/// An id from another session is simply not found there, so a mixed-up id is
/// *reported* rather than being undefined behaviour (ADR-0003 D3).
pub type ReldexResultId = u64;

/// The caller's own correlation id, opaque to Reldex and echoed back on the
/// one event that replies to the request it was passed to.
pub type ReldexRequestId = u64;

/// What a cancel on this session can actually do (`SPEC.md` §24.8).
///
/// `0` is reserved for a kind this header predates.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexCancelKind {
    /// A kind this header does not know.
    Unknown = 0,
    /// The driver cannot stop a running statement at all. The UI must not
    /// offer Cancel.
    Unsupported = 1,
    /// The driver can only enforce a deadline armed *before* the statement
    /// started. The UI must say when the statement will stop, not pretend a
    /// cancel is in flight.
    PreArmedDeadline = 2,
    /// The driver can interrupt a running statement.
    Native = 3,
}

impl From<CancelKind> for ReldexCancelKind {
    fn from(kind: CancelKind) -> Self {
        match kind {
            CancelKind::Unsupported => Self::Unsupported,
            CancelKind::PreArmedDeadline => Self::PreArmedDeadline,
            CancelKind::Native => Self::Native,
            // `CancelKind` is `#[non_exhaustive]`.
            _ => Self::Unknown,
        }
    }
}

/// What a cancel request achieved.
///
/// `0` is reserved for an outcome this header predates.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexCancelOutcome {
    /// An outcome this header does not know.
    Unknown = 0,
    /// An interrupt was delivered or armed. Best effort: the statement may
    /// still finish normally, and its outcome arrives as its own event.
    Requested = 1,
    /// Nothing was sent and nothing will stop early — this driver cannot
    /// interrupt a running call. The UI must not show a cancel in progress.
    NotInterruptible = 2,
}

/// What to do with a possibly-open transaction when closing a session.
///
/// `RELDEX_CLOSE_DISPOSITION_NONE` is the honest default: if a transaction may
/// be open, the close comes back as
/// `RELDEX_CLOSE_OUTCOME_DECISION_REQUIRED` with the session still open, so
/// the user can be asked. Reldex never silently commits (`SPEC.md` §10).
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexCloseDisposition {
    /// A disposition this header does not know.
    Unknown = 0,
    /// No decision has been made yet.
    None = 1,
    /// Commit the open transaction before closing.
    Commit = 2,
    /// Roll the open transaction back before closing.
    Rollback = 3,
}

/// How a session is opened.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReldexOpenOptions {
    /// `sizeof(ReldexOpenOptions)`.
    pub struct_size: u32,
    /// A [`crate::ReldexDriverKind`]. This build accepts only
    /// `RELDEX_DRIVER_KIND_MOCK`.
    pub driver: i32,
    /// The scripted world, when `driver` is the mock.
    pub mock: ReldexMockScenarioConfig,
}

// SAFETY: `#[repr(C)]`, `struct_size` first, and every field is an integer or
// another `CStruct` that is itself valid when zeroed.
unsafe impl CStruct for ReldexOpenOptions {
    const MIN_SIZE: usize = size_of::<u32>() + size_of::<i32>();
}

impl Default for ReldexOpenOptions {
    /// The mock driver with its default world.
    fn default() -> Self {
        Self {
            struct_size: u32::try_from(size_of::<Self>()).unwrap_or(u32::MAX),
            driver: crate::ReldexDriverKind::Mock as i32,
            mock: ReldexMockScenarioConfig::default(),
        }
    }
}

/// Where a session is in its life, from this crate's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The pump is still inside `open_session`; nothing may be submitted yet.
    Opening,
    /// Open for business.
    Open,
    /// Closed, abandoned, or its open failed. Nothing more is accepted.
    Closed,
}

/// One open result set: what `db-core` calls it, and the columns its batches
/// describe.
struct ResultEntry {
    id: ResultId,
    columns: Arc<[ColumnMetadata]>,
}

struct SessionSlot {
    phase: Phase,
    session: Option<Arc<DatabaseSession>>,
    commands: Option<mpsc::Sender<PumpCommand>>,
    results: HashMap<u64, ResultEntry>,
    next_result_id: u64,
}

/// A request handed to the pump thread, with the completion it must wait on.
enum PumpCommand {
    Execute {
        request: u64,
        completion: Completion<ExecuteOutcome>,
    },
    Fetch {
        request: u64,
        completion: Completion<FetchedBatch>,
        columns: Arc<[ColumnMetadata]>,
    },
    CloseResult {
        request: u64,
        completion: Completion<()>,
        key: u64,
    },
    Close {
        request: u64,
        disposition: Option<CloseDisposition>,
    },
}

/// One session's registry entry: everything the caller's thread and the pump
/// thread share.
pub(crate) struct SessionEntry {
    id: u64,
    /// Guards submission *and* ordering: a submit sends to `db-core` and hands
    /// the pump its completion while holding this, so the pump can never see
    /// two requests in the wrong order.
    slot: Mutex<SessionSlot>,
    block: Option<BlockControl>,
}

impl SessionEntry {
    fn lock(&self) -> std::sync::MutexGuard<'_, SessionSlot> {
        self.slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Closes the session without committing and lets the pump finish on its
    /// own, so hub teardown never blocks on a statement that cannot be
    /// interrupted.
    pub(crate) fn shut_down(&self) {
        let session = {
            let mut slot = self.lock();
            slot.phase = Phase::Closed;
            // Dropping the sender ends the pump's loop once it is idle.
            slot.commands = None;
            slot.results.clear();
            slot.session.take()
        };
        if let Some(block) = &self.block {
            release_block(block);
        }
        if let Some(session) = session {
            // Best effort, and honest about it: on a driver that cannot
            // interrupt a running call this does nothing, which is why nothing
            // here waits for it.
            let _ = session.cancel();
        }
    }

    /// Releases a parked mock statement; see
    /// [`crate::reldex_mock_release_block`].
    pub(crate) fn release_block(&self) -> ReldexStatus {
        match &self.block {
            Some(block) => {
                release_block(block);
                ReldexStatus::Ok
            }
            None => ReldexStatus::InvalidState,
        }
    }

    /// Takes the session and the pump's sender for a submission, refusing if
    /// the session is not open.
    fn submit<T>(
        &self,
        make: impl FnOnce(
            &Arc<DatabaseSession>,
            &mut SessionSlot,
        ) -> Result<(PumpCommand, T), ReldexStatus>,
    ) -> Result<T, ReldexStatus> {
        let mut slot = self.lock();
        match slot.phase {
            Phase::Opening => return Err(ReldexStatus::InvalidState),
            Phase::Closed => return Err(ReldexStatus::InvalidState),
            Phase::Open => {}
        }
        let Some(session) = slot.session.clone() else {
            return Err(ReldexStatus::InvalidState);
        };
        let (command, value) = make(&session, &mut slot)?;
        let Some(sender) = slot.commands.as_ref() else {
            return Err(ReldexStatus::InvalidState);
        };
        // The send cannot block: the channel is unbounded. If the pump has
        // gone, the session is finished and nothing was accepted.
        sender
            .send(command)
            .map_err(|_| ReldexStatus::InvalidState)?;
        Ok(value)
    }
}

/// Runs `body` with the entry for `session`, reporting a stale or unknown id
/// rather than dereferencing anything.
///
/// # Safety
///
/// `hub` must be null, or a live hub.
pub(crate) unsafe fn with_session(
    hub: *mut ReldexHub,
    session: ReldexSessionId,
    body: impl FnOnce(&Arc<SessionEntry>) -> ReldexStatus,
) -> ReldexStatus {
    // SAFETY: delegated to this function's contract.
    let found = unsafe {
        with_hub(hub.cast_const(), |hub| {
            hub.sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&session)
                .map(Arc::clone)
        })
    };
    match found {
        None => set_last_argument_error("the hub pointer is null or unaligned"),
        Some(None) => {
            set_last_error(DbError::internal(format!(
                "reldex-ffi: session {session} is not open on this hub; it was closed, or it \
                 belongs to another hub"
            )));
            ReldexStatus::NotFound
        }
        Some(Some(entry)) => body(&entry),
    }
}

/// Opens a session and reports its id immediately; the connection itself is
/// made on the session's own thread.
///
/// This **never blocks**: `db-core`'s `open_session` waits for the connect to
/// finish, so the wait happens on the session's pump thread and the answer
/// arrives as a `RELDEX_EVENT_KIND_OPENED` event carrying `request`. On
/// failure that same request id comes back as an `OPENED` event with a
/// non-null `error` — exactly one reply either way.
///
/// The session id is valid as soon as this returns, but nothing may be
/// submitted on it until its `OPENED` event arrives without an error.
///
/// # Safety
///
/// `hub` must be a live hub; `options` must be null (for the defaults) or
/// point at a [`ReldexOpenOptions`] with `struct_size` set; `out_session` must
/// be null or point at a writable `uint64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_hub_open_session(
    hub: *mut ReldexHub,
    options: *const ReldexOpenOptions,
    request: ReldexRequestId,
    out_session: *mut ReldexSessionId,
) -> ReldexStatus {
    entry(|| {
        let options = if options.is_null() {
            ReldexOpenOptions::default()
        } else {
            // SAFETY: delegated to this function's contract for `options`.
            match unsafe { read_in_struct(options) } {
                Some(options) => options,
                None => {
                    return set_last_argument_error(
                        "reldex_hub_open_session: `options` is unaligned or its struct_size is \
                         too small",
                    );
                }
            }
        };
        if !out_session.is_null() && !out_session.is_aligned() {
            return set_last_argument_error("reldex_hub_open_session: `out_session` is unaligned");
        }
        let choice = match build_driver(&options) {
            Ok(choice) => choice,
            Err(status) => {
                set_last_error(DbError::new(
                    ErrorKind::Unsupported,
                    "reldex-ffi: this build cannot open a session against the requested driver; \
                     only the mock driver is linked",
                ));
                return status;
            }
        };

        let start = |hub: &Arc<ReldexHub>| {
            let id = hub
                .next_session_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (tx, rx) = mpsc::channel();
            let entry = Arc::new(SessionEntry {
                id,
                slot: Mutex::new(SessionSlot {
                    phase: Phase::Opening,
                    session: None,
                    commands: Some(tx),
                    results: HashMap::new(),
                    next_result_id: 1,
                }),
                block: choice.block,
            });
            hub.sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(id, Arc::clone(&entry));

            let pump_hub = Arc::clone(hub);
            let pump_entry = Arc::clone(&entry);
            let driver = choice.driver;
            let params = choice.params;
            let spawned = std::thread::Builder::new()
                .name(format!("reldex-ffi-session-{id}"))
                .spawn(move || {
                    pump_main(&pump_hub, &pump_entry, &rx, driver, params, request);
                });
            if spawned.is_err() {
                hub.sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id);
                set_last_error(DbError::internal(
                    "reldex-ffi: could not spawn the session's thread",
                ));
                return ReldexStatus::Error;
            }
            if !out_session.is_null() {
                // SAFETY: checked non-null and aligned above; the caller
                // promises it points at a writable `uint64_t`.
                unsafe { out_session.write(id) };
            }
            ReldexStatus::Ok
        };
        // SAFETY: delegated to this function's contract for `hub`.
        unsafe { with_hub(hub.cast_const(), start) }.unwrap_or_else(|| {
            set_last_argument_error("reldex_hub_open_session: `hub` is null or unaligned")
        })
    })
}

/// Submits one statement. The reply is a `RELDEX_EVENT_KIND_EXECUTED` event
/// carrying `request`.
///
/// `deadline_ms` arms a per-statement time limit; `0` means none, with the
/// consequence `SPEC.md` §10 requires the UI to state — on a driver that
/// cannot interrupt a running call, only disconnecting the worksheet can end
/// it, and that loses its transaction.
///
/// Binds are not exported yet (M2.11).
///
/// # Safety
///
/// `hub` must be a live hub, and `sql` must point at `sql.len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_session_execute(
    hub: *mut ReldexHub,
    session: ReldexSessionId,
    request: ReldexRequestId,
    sql: ReldexStr,
    deadline_ms: u64,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract for `sql`.
        let Some(text) = (unsafe { sql.as_str() }) else {
            return set_last_argument_error(
                "reldex_session_execute: `sql` is null or is not valid UTF-8",
            );
        };
        let mut statement = Statement::new(text);
        if deadline_ms > 0 {
            statement = statement.with_deadline(Duration::from_millis(deadline_ms));
        }
        // SAFETY: delegated to this function's contract for `hub`.
        unsafe {
            with_session(hub, session, move |entry| {
                let outcome = entry.submit(move |session, _slot| {
                    let completion = session.execute(statement);
                    Ok((
                        PumpCommand::Execute {
                            request,
                            completion,
                        },
                        (),
                    ))
                });
                report(outcome, "execute")
            })
        }
    })
}

/// Submits a fetch of at most `max_rows` more rows of `result`.
///
/// The reply is a `RELDEX_EVENT_KIND_FETCHED` event carrying `request`; on
/// success it owns a `ReldexBatch*`, and a batch with **no rows** means the
/// result is exhausted.
///
/// # Safety
///
/// `hub` must be a live hub.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_session_fetch(
    hub: *mut ReldexHub,
    session: ReldexSessionId,
    request: ReldexRequestId,
    result: ReldexResultId,
    max_rows: u32,
) -> ReldexStatus {
    entry(|| {
        let Some(max_rows) = NonZeroUsize::new(max_rows as usize) else {
            return set_last_argument_error("reldex_session_fetch: `max_rows` must be above zero");
        };
        // SAFETY: delegated to this function's contract for `hub`.
        unsafe {
            with_session(hub, session, |entry| {
                let outcome = entry.submit(|session, slot| {
                    let Some(open) = slot.results.get(&result) else {
                        return Err(ReldexStatus::NotFound);
                    };
                    let columns = Arc::clone(&open.columns);
                    let completion = session.fetch_batch(open.id, max_rows);
                    Ok((
                        PumpCommand::Fetch {
                            request,
                            completion,
                            columns,
                        },
                        (),
                    ))
                });
                report(outcome, "fetch")
            })
        }
    })
}

/// Releases a result set and every large object taken from it. The reply is a
/// `RELDEX_EVENT_KIND_RESULT_CLOSED` event carrying `request`.
///
/// # Safety
///
/// `hub` must be a live hub.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_session_close_result(
    hub: *mut ReldexHub,
    session: ReldexSessionId,
    request: ReldexRequestId,
    result: ReldexResultId,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract for `hub`.
        unsafe {
            with_session(hub, session, |entry| {
                let outcome = entry.submit(|session, slot| {
                    let Some(open) = slot.results.get(&result) else {
                        return Err(ReldexStatus::NotFound);
                    };
                    let completion = session.close_result(open.id);
                    Ok((
                        PumpCommand::CloseResult {
                            request,
                            completion,
                            key: result,
                        },
                        (),
                    ))
                });
                report(outcome, "close_result")
            })
        }
    })
}

/// Closes a session, resolving its transaction first.
///
/// The reply is a `RELDEX_EVENT_KIND_SESSION_CLOSED` event carrying `request`.
/// Read its `close_outcome`: `DECISION_REQUIRED`, `COMMIT_FAILED` and
/// `ROLLBACK_FAILED` all leave the session **open and usable**, so the caller
/// can ask the user and close again. This is the only path that can commit;
/// destroying the hub never does.
///
/// # Safety
///
/// `hub` must be a live hub.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_session_close(
    hub: *mut ReldexHub,
    session: ReldexSessionId,
    request: ReldexRequestId,
    disposition: i32,
) -> ReldexStatus {
    entry(|| {
        let disposition = if disposition == ReldexCloseDisposition::None as i32 {
            None
        } else if disposition == ReldexCloseDisposition::Commit as i32 {
            Some(CloseDisposition::Commit)
        } else if disposition == ReldexCloseDisposition::Rollback as i32 {
            Some(CloseDisposition::Rollback)
        } else {
            return set_last_argument_error(
                "reldex_session_close: `disposition` is not a ReldexCloseDisposition value",
            );
        };
        // SAFETY: delegated to this function's contract for `hub`.
        unsafe {
            with_session(hub, session, |entry| {
                let outcome = entry.submit(|_session, _slot| {
                    Ok((
                        PumpCommand::Close {
                            request,
                            disposition,
                        },
                        (),
                    ))
                });
                report(outcome, "close")
            })
        }
    })
}

/// Asks the session's driver to stop whatever it is running.
///
/// Deliberately callable from **any** thread, unlike every other function
/// here, and it returns promptly: it never goes through the request queue a
/// blocked statement would stall (ADR-0002 D2). It produces no event — the
/// cancelled statement's own reply is where the outcome shows up. Check
/// `RELDEX_EVENT_KIND_OPENED`'s `cancel_kind` first: on a driver that cannot
/// interrupt a running call this reports `NOT_INTERRUPTIBLE`, and the UI must
/// say so rather than show a cancel in progress.
///
/// # Safety
///
/// `hub` must be a live hub; `out_outcome` must be null or point at a writable
/// `int32_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_session_request_cancel(
    hub: *mut ReldexHub,
    session: ReldexSessionId,
    out_outcome: *mut i32,
) -> ReldexStatus {
    entry(|| {
        if !out_outcome.is_null() && !out_outcome.is_aligned() {
            return set_last_argument_error(
                "reldex_session_request_cancel: `out_outcome` is unaligned",
            );
        }
        let cancel = |entry: &Arc<SessionEntry>| {
            // The lock is held only long enough to clone the handle: a
            // cancel must not wait behind a submission.
            let session = entry.lock().session.clone();
            let Some(session) = session else {
                return ReldexStatus::InvalidState;
            };
            match session.cancel() {
                Ok(outcome) => {
                    let mapped = match outcome {
                        CancelOutcome::Requested => ReldexCancelOutcome::Requested,
                        CancelOutcome::NotInterruptible { .. } => {
                            ReldexCancelOutcome::NotInterruptible
                        }
                        // `CancelOutcome` is `#[non_exhaustive]`.
                        _ => ReldexCancelOutcome::Unknown,
                    };
                    if !out_outcome.is_null() {
                        // SAFETY: checked non-null and aligned above.
                        unsafe { out_outcome.write(mapped as i32) };
                    }
                    ReldexStatus::Ok
                }
                Err(error) => {
                    set_last_error(error);
                    ReldexStatus::Error
                }
            }
        };
        // SAFETY: delegated to this function's contract for `hub`.
        unsafe { with_session(hub, session, cancel) }
    })
}

/// Appends this session's connect-time warnings to `arena`, one string per
/// warning, in the order the driver reported them.
///
/// These are the non-fatal findings from opening the connection — a TLS
/// parameter the driver could not honour, say — and the `OPENED` event's
/// `warning_count` says how many to expect. They are constant for the
/// session's life, so reading them late is not the same as losing them.
///
/// # Safety
///
/// `hub` must be a live hub and `arena` a live arena.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_session_connect_warnings(
    hub: *mut ReldexHub,
    session: ReldexSessionId,
    arena: *mut ReldexTextArena,
) -> ReldexStatus {
    entry(|| {
        if arena.is_null() || !arena.is_aligned() {
            return set_last_argument_error(
                "reldex_session_connect_warnings: `arena` is null or unaligned",
            );
        }
        let collect = |entry: &Arc<SessionEntry>| {
            let session = entry.lock().session.clone();
            let Some(session) = session else {
                return ReldexStatus::InvalidState;
            };
            // SAFETY: the caller promises `arena` is a live arena this library
            // produced, and nothing else may touch it concurrently (D5 rule 3).
            let arena = unsafe { &mut *arena };
            for warning in session.connect_warnings() {
                arena.push(warning.message());
            }
            ReldexStatus::Ok
        };
        // SAFETY: delegated to this function's contract for `hub`.
        unsafe { with_session(hub, session, collect) }
    })
}

/// Turns a submission outcome into a status, recording why when it failed.
fn report(outcome: Result<(), ReldexStatus>, what: &str) -> ReldexStatus {
    match outcome {
        Ok(()) => ReldexStatus::Ok,
        Err(ReldexStatus::NotFound) => {
            set_last_error(DbError::internal(format!(
                "reldex-ffi: {what} names a result that this session does not have open; it was \
                 closed, or it belongs to another session"
            )));
            ReldexStatus::NotFound
        }
        Err(status) => {
            set_last_error(DbError::internal(format!(
                "reldex-ffi: {what} was refused because the session is not open"
            )));
            status
        }
    }
}

/// One session's thread: opens the connection, then turns completions into
/// events until there is nothing left to answer.
fn pump_main(
    hub: &Arc<ReldexHub>,
    entry: &Arc<SessionEntry>,
    rx: &mpsc::Receiver<PumpCommand>,
    driver: Arc<dyn reldex_db_core::DatabaseDriver>,
    params: reldex_db_core::ConnectionParams,
    open_request: u64,
) {
    let id = entry.id;
    let session = match hub.manager.open_session(driver, params) {
        Ok(session) => Arc::new(session),
        Err(error) => {
            {
                let mut slot = entry.lock();
                slot.phase = Phase::Closed;
                slot.commands = None;
            }
            hub.push_event(
                QueuedEvent::new(ReldexEventKind::Opened, id, open_request)
                    .with_error(ReldexError::from_db_error(&error)),
            );
            return;
        }
    };
    {
        let mut slot = entry.lock();
        slot.phase = Phase::Open;
        slot.session = Some(Arc::clone(&session));
    }
    let mut opened = QueuedEvent::new(ReldexEventKind::Opened, id, open_request);
    opened.connection_id = session.connection_id().get();
    opened.cancel_kind = session.cancel_kind().into();
    opened.warning_count = u32::try_from(session.connect_warnings().len()).unwrap_or(u32::MAX);
    opened.session_state = session.session_state().into();
    hub.push_event(opened);

    while let Ok(command) = rx.recv() {
        let finished = run_command(hub, entry, &session, command);
        if finished {
            // The session is gone. Everything still queued was accepted before
            // it went, so each one still gets its single reply — `db-core`
            // answers them with the session's terminal error — and only then
            // does the pump stop.
            while let Ok(pending) = rx.try_recv() {
                run_command(hub, entry, &session, pending);
            }
            break;
        }
    }
}

/// Runs one submitted request and pushes its single reply. Returns whether the
/// session ended.
fn run_command(
    hub: &Arc<ReldexHub>,
    entry: &Arc<SessionEntry>,
    session: &Arc<DatabaseSession>,
    command: PumpCommand,
) -> bool {
    let id = entry.id;
    match command {
        PumpCommand::Execute {
            request,
            completion,
        } => {
            let event = match completion.wait() {
                Ok(outcome) => {
                    let result = outcome.result.map(|result| {
                        let mut slot = entry.lock();
                        let key = slot.next_result_id;
                        slot.next_result_id += 1;
                        slot.results.insert(
                            key,
                            ResultEntry {
                                id: result,
                                columns: Arc::from(outcome.columns.clone()),
                            },
                        );
                        key
                    });
                    QueuedEvent::new(ReldexEventKind::Executed, id, request)
                        .with_execute_outcome(&outcome, result)
                }
                Err(error) => QueuedEvent::new(ReldexEventKind::Executed, id, request)
                    .with_error(ReldexError::from_db_error(&error)),
            };
            hub.push_event(event.with_session_state(session.session_state().into()));
            false
        }
        PumpCommand::Fetch {
            request,
            completion,
            columns,
        } => {
            let event = match completion.wait() {
                Ok(batch) => {
                    let rows = batch.row_count();
                    QueuedEvent::new(ReldexEventKind::Fetched, id, request)
                        .with_batch(Box::new(ReldexBatch::new(batch, columns)), rows)
                }
                Err(error) => QueuedEvent::new(ReldexEventKind::Fetched, id, request)
                    .with_error(ReldexError::from_db_error(&error)),
            };
            hub.push_event(event.with_session_state(session.session_state().into()));
            false
        }
        PumpCommand::CloseResult {
            request,
            completion,
            key,
        } => {
            let outcome = completion.wait();
            entry.lock().results.remove(&key);
            let event = match outcome {
                Ok(()) => QueuedEvent::new(ReldexEventKind::ResultClosed, id, request),
                Err(error) => QueuedEvent::new(ReldexEventKind::ResultClosed, id, request)
                    .with_error(ReldexError::from_db_error(&error)),
            };
            hub.push_event(event.with_session_state(session.session_state().into()));
            false
        }
        PumpCommand::Close {
            request,
            disposition,
        } => {
            let outcome = session.close(disposition);
            let still_open = outcome
                .as_ref()
                .err()
                .is_some_and(CloseError::session_is_still_open);
            let mut event = QueuedEvent::new(ReldexEventKind::SessionClosed, id, request)
                .with_close_outcome(&outcome);
            if let Err(error) = &outcome {
                event = event.with_error(close_error_to_reldex(error));
            }
            if !still_open {
                let mut slot = entry.lock();
                slot.phase = Phase::Closed;
                slot.commands = None;
                slot.results.clear();
            }
            let state = if still_open {
                session.session_state().into()
            } else {
                ReldexSessionState::Closed
            };
            hub.push_event(event.with_session_state(state));
            !still_open
        }
    }
}

/// Renders a close failure as the error the adapter shows.
///
/// `DecisionRequired` has no underlying `DbError` — nothing failed, a decision
/// is missing — so it is reported as a transaction-kind error carrying that
/// explanation, rather than as an empty failure the UI would have to invent
/// wording for.
fn close_error_to_reldex(error: &CloseError) -> ReldexError {
    match error {
        CloseError::DecisionRequired => {
            ReldexError::from_db_error(&DbError::new(ErrorKind::Transaction, error.to_string()))
        }
        CloseError::CommitFailed(inner)
        | CloseError::RollbackFailed(inner)
        | CloseError::Failed(inner) => ReldexError::from_db_error(inner),
    }
}
