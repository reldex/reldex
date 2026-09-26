//! Sessions: the calls that submit work to one, and the translation of what
//! `db-core` reports back into the events the adapter drains (ADR-0003 D3/D5).
//!
//! # How a request travels
//!
//! A submitting call runs on the caller's thread and never blocks. It records
//! what the reply will need that `db-core`'s event does not carry — the
//! caller's own request id and, for a fetch, the result's key and column
//! description ([`crate::hub::Pending`]) — allocates a `db-core` request id,
//! and hands the request to the session's worker with one
//! `DatabaseSession::submit_*` call. The worker pushes its events straight
//! into the hub's `db-core` event queue, and [`crate::reldex_hub_next_event`]
//! takes them out, again on the caller's thread, and [`translate`]s each one.
//! No thread of this crate's sits in between
//! (`docs/exec-plans/active/phase-1-m2-5-event-queue.md` §3.1, §6, §7.2).
//!
//! Every ordering guarantee ADR-0003 D5 makes is therefore `db-core`'s own:
//! one session's events in the order it produced them; exactly one reply per
//! accepted request, even when a driver call panics (the worker contains the
//! panic, and a reply channel dropped unanswered still answers); exactly one
//! `Terminal` per session, after the replies of every request queued ahead of
//! it. A submit `db-core` refuses — the session's reply slots are all taken —
//! was never accepted and produces no event.
//!
//! # Per-session state ends on `TERMINAL`, and only there
//!
//! A session's entry here lives from `reldex_hub_open_session` until its
//! `TERMINAL` event is drained, whatever ended it: a close, a loss, an
//! abandon, or a connect that never succeeded. Draining that event removes
//! the entry and retires the session from `db-core`'s registry — the one
//! retirement rule `db-core` documents, and what keeps
//! `reldex_hub_session_count` and `reldex_live_counts` exact.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use reldex_db_core::{
    CancelKind, CancelOutcome, CloseDisposition, CloseError, CompletedOperation, DatabaseSession,
    DbError, DbResult, ErrorKind, RequestId, ResultId, SessionEvent, SessionId, SessionRegistry,
    Statement,
};
use reldex_db_driver_api::{SavepointName, ServerOutputBuffer, ServerOutputSetting};

use crate::batch::{ReldexBatch, ReldexColumnInfo, ResultColumns};
use crate::error::{ReldexError, ReldexSessionState, set_last_argument_error, set_last_error};
use crate::event::{
    QueuedEvent, ReldexCompletedOperation, ReldexEventKind, ReldexServerOutputMode,
};
use crate::format::ReldexTextArena;
use crate::hub::{Pending, ReldexHub, lock, with_hub};
use crate::mock::{BlockControl, ReldexMockScenarioConfig, build_driver, release_block};
use crate::status::{ReldexStatus, entry, entry_value};
use crate::strings::{CStruct, ReldexStr, read_in_struct, write_out_struct};

/// Identifies one session. Never reused, in this process.
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

/// What [`reldex_session_abandon`] found (ABI 3.2).
///
/// `0` is reserved for an outcome this header predates.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexAbandonOutcome {
    /// An outcome this header does not know.
    Unknown = 0,
    /// The connect had not finished. Its `OPENED` event arrives with a
    /// `RELDEX_ERROR_KIND_CANCELLED` error, then its `TERMINAL`; a connection
    /// that arrives late is closed, never adopted.
    Connecting = 1,
    /// An open session was released **without committing**: the server rolls
    /// back whatever transaction it held. `*out_transaction_possibly_lost` is
    /// a lower bound available now; the authoritative answer is the
    /// `TERMINAL` event's `transaction_possibly_lost`, which the UI must
    /// surface either way (`SPEC.md` §10).
    Open = 2,
    /// The session had already ended — closed, lost, or abandoned before.
    /// Nothing happened; its `TERMINAL` is, or was, delivered as usual.
    AlreadyEnded = 3,
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
    /// Connecting; nothing may be submitted until `db-core` hands out the
    /// session, which it does from the moment the connect succeeds.
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
    columns: Arc<ResultColumns>,
}

struct SessionSlot {
    phase: Phase,
    /// This crate's handle, cached from the registry once the session opens
    /// and released before the registry's own, so that the registry's is the
    /// last one and its release — which joins the worker — happens at a known
    /// point: the retirement on `TERMINAL`.
    session: Option<Arc<DatabaseSession>>,
    results: HashMap<u64, ResultEntry>,
    next_result_id: u64,
    /// The caller submitted a close or called abandon, which ends the
    /// documented lifetime of every column description this session handed
    /// out; see [`reldex_session_result_column`].
    caller_ended: bool,
    /// [`reldex_session_abandon`] ended the session.
    abandoned: bool,
}

/// One session's entry in its hub.
pub(crate) struct SessionEntry {
    id: SessionId,
    slot: Mutex<SessionSlot>,
    /// The mock's statement gate and connect gate, when the mock is linked.
    block: Option<BlockControl>,
    connect_block: Option<BlockControl>,
}

impl Drop for SessionEntry {
    fn drop(&mut self) {
        crate::counters::destroyed(crate::counters::Kind::Session);
    }
}

/// What retiring an entry leaves behind.
struct Ended {
    abandoned: bool,
    caller_ended: bool,
    columns: Vec<Arc<ResultColumns>>,
}

impl SessionEntry {
    fn new(
        id: SessionId,
        block: Option<BlockControl>,
        connect_block: Option<BlockControl>,
    ) -> Self {
        crate::counters::created(crate::counters::Kind::Session);
        Self {
            id,
            slot: Mutex::new(SessionSlot {
                phase: Phase::Opening,
                session: None,
                results: HashMap::new(),
                next_result_id: 1,
                caller_ended: false,
                abandoned: false,
            }),
            block,
            connect_block,
        }
    }

    fn lock(&self) -> MutexGuard<'_, SessionSlot> {
        lock(&self.slot)
    }

    /// The handle to submit on, or `None` when nothing may be submitted.
    ///
    /// A session is usable from the moment it connects, not from the moment
    /// the caller drains its `OPENED`: `db-core`'s registry hands the handle
    /// out once the connect has succeeded, and it is cached here then.
    fn handle(&self, registry: &SessionRegistry) -> Option<Arc<DatabaseSession>> {
        {
            let slot = self.lock();
            match slot.phase {
                Phase::Open => return slot.session.clone(),
                Phase::Closed => return None,
                Phase::Opening => {}
            }
        }
        let session = registry.get(self.id)?;
        let mut slot = self.lock();
        if slot.phase != Phase::Opening {
            return slot.session.clone();
        }
        slot.phase = Phase::Open;
        slot.session = Some(Arc::clone(&session));
        Some(session)
    }

    /// The lifecycle state reported on an event that carries no error.
    fn state(&self) -> ReldexSessionState {
        let slot = self.lock();
        match (&slot.session, slot.phase) {
            (Some(session), _) => session.session_state().into(),
            (None, Phase::Closed) => ReldexSessionState::Closed,
            (None, _) => ReldexSessionState::Unknown,
        }
    }

    /// Submits one request, recording what its reply will need first.
    fn submit(
        &self,
        hub: &ReldexHub,
        what: &str,
        pending: Pending,
        send: impl FnOnce(&DatabaseSession, RequestId) -> DbResult<()>,
    ) -> ReldexStatus {
        let Some(session) = self.handle(&hub.registry) else {
            return refused(what);
        };
        let request = hub.begin_request(pending);
        match send(&session, request) {
            Ok(()) => ReldexStatus::Ok,
            Err(error) => {
                // Refused: nothing was accepted, so no reply will come.
                hub.abandon_request(request);
                set_last_error(error);
                ReldexStatus::Error
            }
        }
    }

    /// The `db-core` id and shared columns of the open result `key`.
    fn result(&self, key: u64) -> Option<(ResultId, Arc<ResultColumns>)> {
        self.lock()
            .results
            .get(&key)
            .map(|open| (open.id, Arc::clone(&open.columns)))
    }

    /// Records a result the session just opened and returns its key.
    fn register_result(&self, id: ResultId, columns: Arc<ResultColumns>) -> u64 {
        let mut slot = self.lock();
        let key = slot.next_result_id;
        slot.next_result_id += 1;
        slot.results.insert(key, ResultEntry { id, columns });
        key
    }

    /// The key this crate gave the `db-core` result `id`, if it is open.
    fn result_key(&self, id: ResultId) -> Option<u64> {
        self.lock()
            .results
            .iter()
            .find_map(|(key, open)| (open.id == id).then_some(*key))
    }

    /// Forgets a result the caller closed. The columns are returned so they
    /// are dropped outside the lock.
    fn forget_result(&self, key: u64) -> Option<Arc<ResultColumns>> {
        self.lock().results.remove(&key).map(|open| open.columns)
    }

    /// The session closed at the caller's request: nothing more is accepted,
    /// and the caller's close ended every column description's lifetime.
    fn close(&self) -> Vec<Arc<ResultColumns>> {
        let (session, columns) = {
            let mut slot = self.lock();
            slot.phase = Phase::Closed;
            slot.caller_ended = true;
            let columns = slot.results.drain().map(|(_, open)| open.columns);
            let columns: Vec<_> = columns.collect();
            (slot.session.take(), columns)
        };
        drop(session);
        columns
    }

    /// Takes everything the entry still holds, on its `TERMINAL`.
    fn end(&self) -> Ended {
        let (session, ended) = {
            let mut slot = self.lock();
            slot.phase = Phase::Closed;
            let columns = slot.results.drain().map(|(_, open)| open.columns);
            let columns: Vec<_> = columns.collect();
            let ended = Ended {
                abandoned: slot.abandoned,
                caller_ended: slot.caller_ended,
                columns,
            };
            (slot.session.take(), ended)
        };
        drop(session);
        self.release_gates();
        ended
    }

    /// [`reldex_session_abandon`]: never blocks, never commits.
    fn abandon(&self, registry: &SessionRegistry) -> (ReldexAbandonOutcome, bool) {
        let session = {
            let mut slot = self.lock();
            slot.phase = Phase::Closed;
            slot.caller_ended = true;
            slot.session.take()
        };
        // Before the registry's abandon, so the registry's handle is the last.
        drop(session);
        let (outcome, lost) = match registry.abandon(self.id) {
            reldex_db_core::Abandoned::Connecting => (ReldexAbandonOutcome::Connecting, false),
            reldex_db_core::Abandoned::Open {
                transaction_possibly_lost,
            } => (ReldexAbandonOutcome::Open, transaction_possibly_lost),
            // `Unknown` cannot happen while this entry exists (the registry
            // retires a session only when this crate asks, on `TERMINAL`), and
            // `Abandoned` is `#[non_exhaustive]`.
            _ => (ReldexAbandonOutcome::AlreadyEnded, false),
        };
        if outcome != ReldexAbandonOutcome::AlreadyEnded {
            self.lock().abandoned = true;
        }
        (outcome, lost)
    }

    /// Hub teardown: abandon, never commit, never wait.
    pub(crate) fn tear_down(&self, registry: &SessionRegistry) {
        let columns = self.close();
        let _ = registry.abandon(self.id);
        // After the abandon, so a parked connect that is released now finds it
        // abandoned and closes its connection instead of adopting it.
        self.release_gates();
        drop(columns);
    }

    fn release_gates(&self) {
        for gate in [&self.block, &self.connect_block].into_iter().flatten() {
            release_block(gate);
        }
    }

    /// Releases a parked mock statement or connect; see
    /// [`crate::reldex_mock_release_block`].
    pub(crate) fn release_block(&self) -> ReldexStatus {
        if self.block.is_none() && self.connect_block.is_none() {
            return ReldexStatus::InvalidState;
        }
        self.release_gates();
        ReldexStatus::Ok
    }
}

/// Records why a submit was refused before reaching `db-core`.
fn refused(what: &str) -> ReldexStatus {
    set_last_error(DbError::internal(format!(
        "reldex-ffi: {what} was refused because the session is not open: it is still \
         connecting, its connect failed, or it was closed or abandoned"
    )));
    ReldexStatus::InvalidState
}

/// Records that a call named a result the session does not have open.
fn unknown_result(what: &str) -> ReldexStatus {
    set_last_error(DbError::internal(format!(
        "reldex-ffi: {what} names a result that this session does not have open; it was \
         closed, or it belongs to another session"
    )));
    ReldexStatus::NotFound
}

/// Runs `body` with the hub and the entry for `session`, reporting a stale or
/// unknown id rather than dereferencing anything.
///
/// # Safety
///
/// `hub` must be null, or a live hub.
pub(crate) unsafe fn with_session(
    hub: *mut ReldexHub,
    session: ReldexSessionId,
    body: impl FnOnce(&ReldexHub, &Arc<SessionEntry>) -> ReldexStatus,
) -> ReldexStatus {
    // SAFETY: delegated to this function's contract.
    let found = unsafe {
        with_hub(hub.cast_const(), |hub| {
            hub.session_entry(session).map(|entry| body(hub, &entry))
        })
    };
    match found {
        None => set_last_argument_error("the hub pointer is null or unaligned"),
        Some(None) => {
            set_last_error(DbError::internal(format!(
                "reldex-ffi: session {session} is not on this hub; its TERMINAL event has been \
                 drained, or it belongs to another hub"
            )));
            ReldexStatus::NotFound
        }
        Some(Some(status)) => status,
    }
}

/// Turns one `db-core` event into the event the caller drains.
///
/// Runs on the caller's thread, inside `reldex_hub_next_event`, with no lock
/// of the queue's held. Everything that changes this crate's per-session
/// state in response to what a session did happens here, in the order the
/// session did it.
pub(crate) fn translate(hub: &ReldexHub, event: SessionEvent) -> QueuedEvent {
    let pending = hub.pending_for(&event);
    let caller = pending.as_ref().map_or(0, |pending| pending.caller);
    // Only a reply carries the caller's id in `request` — exactly one event per
    // accepted request does (D5 rule 5, and the 3.1 contract a 3.1 adapter
    // still relies on). Progress names its request elsewhere.
    let answered = if event.is_reply() { caller } else { 0 };
    let id = event.session();
    let key = id.get();
    // Only the kinds that change this crate's per-session state look the
    // entry up; a FETCHED — the one kind a result stream is made of — does not.
    let entry = if matches!(event, SessionEvent::Fetched { .. }) {
        None
    } else {
        hub.session_entry(key)
    };
    let entry = entry.as_deref();
    // A successful reply reports the state as of that reply: a request that
    // succeeded left the session usable. Progress and notifications report
    // the session's state now.
    let usable = ReldexSessionState::Usable;
    let state = || entry.map_or(ReldexSessionState::Unknown, SessionEntry::state);
    let reply = |kind| QueuedEvent::new(kind, key, answered);
    let failed =
        |event: QueuedEvent, error: &DbError| event.with_error(ReldexError::from_db_error(error));
    match event {
        SessionEvent::Opened {
            connection,
            cancel_kind,
            warnings,
            ..
        } => {
            if let Some(entry) = entry {
                let _ = entry.handle(&hub.registry);
            }
            let mut opened = reply(ReldexEventKind::Opened);
            opened.connection_id = connection.get();
            opened.cancel_kind = cancel_kind.into();
            opened.warning_count = warnings.len();
            opened.with_session_state(usable)
        }
        SessionEvent::OpenFailed { error, .. } => {
            if let Some(entry) = entry {
                entry.lock().phase = Phase::Closed;
            }
            // Mirrors the `TERMINAL` that follows: an abandoned connect ended
            // deliberately, a failed one was lost.
            let ended = if error.kind() == ErrorKind::Cancelled {
                ReldexSessionState::Closed
            } else {
                ReldexSessionState::Lost
            };
            failed(reply(ReldexEventKind::Opened), &error).with_session_state(ended)
        }
        SessionEvent::Executed { outcome, .. } => match outcome {
            Ok(mut outcome) => {
                let column_count = outcome.columns.len();
                let result = match (outcome.result, entry) {
                    // Moved, not cloned: the names are copied into their
                    // NUL-terminated form exactly once per result set and
                    // shared by every batch of it.
                    (Some(result), Some(entry)) => Some(entry.register_result(
                        result,
                        ResultColumns::new(std::mem::take(&mut outcome.columns)),
                    )),
                    _ => None,
                };
                reply(ReldexEventKind::Executed)
                    .with_execute_outcome(&outcome, result, column_count)
                    .with_session_state(usable)
            }
            Err(error) => failed(reply(ReldexEventKind::Executed), &error),
        },
        SessionEvent::Fetched { batch, .. } => {
            let (result, columns) =
                pending.map_or((None, None), |pending| (pending.result, pending.columns));
            let event = match batch {
                Ok(batch) => {
                    let rows = batch.row_count();
                    // Every fetch records its result's columns when it is
                    // submitted; the fallback only keeps an impossible case
                    // from being undefined.
                    let columns = columns.unwrap_or_else(|| ResultColumns::new(Vec::new()));
                    reply(ReldexEventKind::Fetched)
                        .with_batch(Box::new(ReldexBatch::new(batch, columns)), rows)
                        .with_session_state(usable)
                }
                Err(error) => failed(reply(ReldexEventKind::Fetched), &error),
            };
            with_optional_result(event, result)
        }
        SessionEvent::FetchedSegment { fetch, segment, .. } => {
            let event = with_optional_result(
                reply(ReldexEventKind::FetchedSegment),
                entry.and_then(|entry| entry.result_key(fetch.result())),
            );
            match segment {
                Ok(segment) => {
                    let mut event = event.with_session_state(usable);
                    event.row_count = segment.segment.row_count();
                    event
                }
                Err(error) => failed(event, &error),
            }
        }
        SessionEvent::Completed {
            operation, result, ..
        } => {
            let event = if let CompletedOperation::CloseResult(_) = operation {
                let closed = pending.and_then(|pending| pending.result);
                // The caller submitted this close, which ended the strings'
                // documented lifetime; dropped here, on the caller's own
                // thread, whatever the outcome.
                drop(closed.and_then(|closed| entry?.forget_result(closed)));
                with_optional_result(reply(ReldexEventKind::ResultClosed), closed)
            } else {
                reply(ReldexEventKind::Completed)
                    .with_completed_operation(completed_operation(operation))
            };
            match result {
                Ok(()) => event.with_session_state(usable),
                Err(error) => failed(event, &error),
            }
        }
        SessionEvent::SessionClosed { result, .. } => {
            let still_open = result
                .as_ref()
                .err()
                .is_some_and(CloseError::session_is_still_open);
            let mut event = reply(ReldexEventKind::SessionClosed).with_close_outcome(&result);
            if let Err(error) = &result {
                event = event.with_error(close_error_to_reldex(error));
            }
            if still_open {
                event.with_session_state(state())
            } else {
                drop(entry.map(SessionEntry::close));
                event.with_session_state(ReldexSessionState::Closed)
            }
        }
        SessionEvent::ServerOutputConfigured { result, .. } => match result {
            Ok(setting) => reply(ReldexEventKind::ServerOutputConfigured)
                .with_server_output_setting(setting)
                .with_session_state(usable),
            Err(error) => failed(reply(ReldexEventKind::ServerOutputConfigured), &error),
        },
        SessionEvent::Executing { deadline, .. } => {
            QueuedEvent::new(ReldexEventKind::Executing, key, 0)
                .with_executing(caller, deadline)
                .with_session_state(state())
        }
        SessionEvent::ServerOutput {
            lines,
            dropped,
            failure,
            invalid_utf8_lines,
            ..
        } => QueuedEvent::new(ReldexEventKind::ServerOutput, key, 0)
            .with_server_output(lines, dropped, invalid_utf8_lines, failure.as_ref())
            .with_session_state(state()),
        SessionEvent::TransactionStateChanged {
            possibly_active, ..
        } => QueuedEvent::new(ReldexEventKind::TransactionState, key, 0)
            .with_transaction_possibly_active(possibly_active)
            .with_session_state(state()),
        SessionEvent::Terminal {
            lifecycle,
            cause,
            transaction_possibly_lost,
            ..
        } => {
            let mut event = QueuedEvent::new(ReldexEventKind::Terminal, key, 0);
            if let Some(cause) = &cause {
                event = event.with_error(ReldexError::from_db_error(cause));
            }
            event.server_output_dropped = hub.pending_dropped_lines(id);
            let abandoned = retire(hub, id);
            event
                .with_session_state(lifecycle.into())
                .with_terminal(transaction_possibly_lost, abandoned)
        }
        // `LobChunk` (nothing in this ABI reads a LOB yet) and every variant
        // a later `db-core` adds: delivered as `RELDEX_EVENT_KIND_UNKNOWN`,
        // which an adapter already ignores (ADR-0003 D7), rather than dropped
        // — its reply slot is released either way. `request` is the caller's
        // id only if the variant is a reply; a future progress kind carries 0.
        _ => reply(ReldexEventKind::Unknown),
    }
}

fn with_optional_result(event: QueuedEvent, result: Option<u64>) -> QueuedEvent {
    match result {
        Some(result) => event.with_result(result),
        None => event,
    }
}

fn completed_operation(operation: CompletedOperation) -> ReldexCompletedOperation {
    match operation {
        CompletedOperation::Commit => ReldexCompletedOperation::Commit,
        CompletedOperation::Rollback => ReldexCompletedOperation::Rollback,
        CompletedOperation::Savepoint => ReldexCompletedOperation::Savepoint,
        CompletedOperation::RollbackToSavepoint => ReldexCompletedOperation::RollbackToSavepoint,
        CompletedOperation::Ping => ReldexCompletedOperation::Ping,
        // `CloseLob` (not exported yet) and later additions:
        // `CompletedOperation` is `#[non_exhaustive]`.
        _ => ReldexCompletedOperation::Unknown,
    }
}

/// Retires a session on its `TERMINAL`: removes its entry, and releases
/// `db-core`'s registry entry. Returns whether the caller abandoned it.
///
/// Nothing here waits on a statement: `TERMINAL` is emitted after the worker's
/// last driver call has returned (or, for an abandoned connect, by the
/// abandon itself, with the connect's thread detached), so releasing the
/// registry's handle joins a worker that is finishing or already gone.
fn retire(hub: &ReldexHub, id: SessionId) -> bool {
    let entry = lock(&hub.sessions).remove(&id.get());
    // This crate's handle goes first (inside `end`), so the registry's is the
    // last one and the worker is joined here, not by whichever thread happens
    // to drop a clone later.
    let ended = entry.as_deref().map(SessionEntry::end);
    hub.registry.retire(id);
    let Some(ended) = ended else {
        return false;
    };
    if ended.caller_ended {
        drop(ended.columns);
    } else {
        // Nothing the caller did ended these descriptions' documented
        // lifetime, so they stay readable until the hub is destroyed.
        hub.orphan_columns(ended.columns);
    }
    ended.abandoned
}

/// Opens a session and reports its id immediately; the connection itself is
/// made on the session's own worker thread.
///
/// This **never blocks**. The answer arrives as a `RELDEX_EVENT_KIND_OPENED`
/// event carrying `request`; on failure that same request id comes back as an
/// `OPENED` event with a non-null `error`, followed by the session's
/// `TERMINAL` — exactly one reply either way.
///
/// The session id is valid as soon as this returns, but nothing may be
/// submitted on it until the connect has succeeded (see
/// [`reldex_session_execute`] for exactly when that is).
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
            let core = hub.begin_request(Pending::caller(request));
            // Never blocks, never fails: anything that goes wrong — even the
            // worker thread not spawning — arrives as the open's one reply.
            let id = hub.registry.open(choice.driver, choice.params, core);
            let entry = SessionEntry::new(id, choice.block, choice.connect_block);
            lock(&hub.sessions).insert(id.get(), Arc::new(entry));
            if !out_session.is_null() {
                // SAFETY: checked non-null and aligned above; the caller
                // promises it points at a writable `uint64_t`.
                unsafe { out_session.write(id.get()) };
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
/// carrying `request`, preceded by a `RELDEX_EVENT_KIND_EXECUTING` event with
/// the same `request` when the worker starts it, and by any
/// `RELDEX_EVENT_KIND_SERVER_OUTPUT` the statement wrote.
///
/// `deadline_ms` arms a per-statement time limit; `0` means none, with the
/// consequence `SPEC.md` §10 requires the UI to state — on a driver that
/// cannot interrupt a running call, only disconnecting the worksheet can end
/// it, and that loses its transaction.
///
/// # Submitting before the session's `OPENED` event
///
/// There is no window in which a request is silently dropped. Either:
///
/// * the session is still connecting, and this returns
///   `RELDEX_STATUS_INVALID_STATE` having accepted nothing — no event follows;
/// * the connect failed, and this returns `RELDEX_STATUS_INVALID_STATE` for
///   the same reason (the failure itself arrives as the `OPENED` event's
///   error); or
/// * the connect has already succeeded, and the request is accepted and
///   answered normally — **even if the caller has not drained the `OPENED`
///   event yet**. The session becomes usable when it connects, not when the
///   caller notices.
///
/// The simple rule for an adapter is still "wait for `OPENED`", because that
/// is the first moment it can be sure.
///
/// # When the session is already gone
///
/// A request submitted after the session was lost, but before its `TERMINAL`
/// was drained, is still **accepted** and answered with a failure reply — which
/// may arrive after the `TERMINAL`. Once `TERMINAL` has been drained the id is
/// retired and this reports `RELDEX_STATUS_NOT_FOUND`.
///
/// # When the caller does not drain
///
/// A session holds at most 1,024 undrained replies. Past that, this returns
/// `RELDEX_STATUS_ERROR` with a `RELDEX_ERROR_KIND_RESOURCE` last error and
/// accepts nothing: drain the queue, then submit again.
///
/// Binds are not exported yet.
///
/// # Safety
///
/// `hub` must be a live hub, and `sql` must point at `sql.len` readable bytes.
/// It need not be NUL-terminated.
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
            with_session(hub, session, move |hub, entry| {
                entry.submit(hub, "execute", Pending::caller(request), |session, core| {
                    session.submit_execute(core, statement)
                })
            })
        }
    })
}

/// Submits a fetch of at most `max_rows` more rows of `result`.
///
/// The reply is a `RELDEX_EVENT_KIND_FETCHED` event carrying `request` and
/// `result`; on success it owns a `ReldexBatch*`, and a batch with **no rows**
/// means the result is exhausted.
///
/// `max_rows` is `uint32_t`, not `size_t`, and it is the one deliberate
/// exception to the ADR-0003 A11 rule that a count of things in this process
/// is `size_t`: it is not a count of anything that exists, it is a **cap the
/// caller chooses**, and a batch above four billion rows is a bug in the
/// caller rather than a use case. Fixing the width also keeps the meaning of a
/// call identical on a 32-bit host, which the mobile target may be. It must be
/// above zero; the batch that comes back is at most this many rows, and may be
/// smaller — including empty, which is how exhaustion is reported.
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
            with_session(hub, session, |hub, entry| {
                if entry.handle(&hub.registry).is_none() {
                    return refused("fetch");
                }
                let Some((id, columns)) = entry.result(result) else {
                    return unknown_result("fetch");
                };
                let pending = Pending {
                    caller: request,
                    result: Some(result),
                    columns: Some(columns),
                };
                entry.submit(hub, "fetch", pending, |session, core| {
                    session.submit_fetch(core, id, max_rows)
                })
            })
        }
    })
}

/// How many columns the open result `result` has.
///
/// The same number the `EXECUTED` event reported in `column_count`, available
/// again for as long as the result is open.
///
/// # When it returns 0
///
/// Three cases, and **each one records a thread-local last error** you can take
/// with `reldex_last_error_take()` — this call has no status of its own to
/// report them with:
///
/// * `hub` is null or unaligned, or `session` is not on it;
/// * `result` is not an open result of that session — it was never opened, it
///   belongs to another session, or it has been closed;
/// * the call came from inside a waker callback, which the ADR-0003 D5
///   contract forbids (the same refusal `RELDEX_STATUS_REENTRANT` reports
///   elsewhere).
///
/// A result that genuinely has no columns is not one of them: a statement that
/// produces no result set produces no result id either, so there is nothing to
/// ask about.
///
/// # Safety
///
/// `hub` must be null (reported as 0) or a live hub.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_session_result_column_count(
    hub: *mut ReldexHub,
    session: ReldexSessionId,
    result: ReldexResultId,
) -> usize {
    entry_value(0, || {
        let mut count = 0;
        // SAFETY: delegated to this function's contract for `hub`.
        let _ = unsafe {
            with_session(hub, session, |_hub, entry| match entry.result(result) {
                Some((_, columns)) => {
                    count = columns.len();
                    ReldexStatus::Ok
                }
                None => {
                    // Recorded rather than swallowed: without a status to
                    // return, the last error is the only way this call can say
                    // *why* it answered zero (the round-1 rule that every
                    // failure describes itself).
                    set_last_error(DbError::internal(format!(
                        "reldex-ffi: reldex_session_result_column_count: session {session} has no open result {result}"
                    )));
                    ReldexStatus::NotFound
                }
            })
        };
        count
    })
}

/// Describes column `column` of the open result `result` from the metadata the
/// statement reported — **without a batch**.
///
/// # Why this exists
///
/// `reldex_batch_column_info` can only answer once rows have arrived, so a
/// grid that wants headers has to either wait for the first batch or build
/// them twice; and a result with columns and no rows never produces a batch to
/// ask at all. This answers from the moment the `EXECUTED` event naming
/// `result` is drained, which is when a UI wants to put its header row up.
///
/// The description is the one the whole result shares: built once, when the
/// statement's `EXECUTED` was drained, and reported identically by every batch
/// of it. Calling this allocates nothing.
///
/// # The one difference from the batch version
///
/// `kind` here is the storage family the column's **declared** type maps to.
/// `reldex_batch_column_info` reports the storage a *particular batch* actually
/// used. They agree for every type the driver contract can represent; where a
/// driver had to fall back — an unrepresentable native type crossing as text —
/// the batch is the one telling the truth about the bytes, so a cell reader
/// must use the batch's `kind`, never this one.
///
/// # Lifetime of the strings
///
/// `out->name` and `out->native_type_name` point into storage owned by the
/// result set; they are **not** copied into `out`. They stay valid, and
/// NUL-terminated, until the first of:
///
/// * the caller submits `reldex_session_close_result` for this `result`;
/// * the caller submits `reldex_session_close` for this session, or calls
///   `reldex_session_abandon` on it;
/// * the caller calls `reldex_hub_destroy`.
///
/// **Only those.** Nothing Reldex does on its own ends the lifetime: a
/// statement that fails, a fetch that fails, a cancel, and a session *lost*
/// mid-statement all leave the strings readable, because the caller — who may
/// still be holding the pointers — did nothing to say otherwise. A lost
/// session's descriptions are kept until the hub is destroyed, even after its
/// `TERMINAL` has been drained.
///
/// Note what that does **not** promise. On a lost session the result itself is
/// gone — nothing can be fetched from it — and once its `TERMINAL` has been
/// drained this call reports `RELDEX_STATUS_NOT_FOUND` for it. The two are
/// separate on purpose: what was already handed out stays readable, and what
/// was not is not invented.
///
/// A caller that wants the names past those points must copy them, which is
/// what a Qt model does anyway, building its header `QString`s once with
/// `QString::fromUtf8(info.name.ptr, info.name.len)`.
///
/// Two consequences worth stating, because both are easy to get wrong:
///
/// * The rule is "valid **at least** until you submit", not "invalid from the
///   moment you submit". Right after `reldex_session_close_result` this call
///   may still succeed for a while (the description is freed when its
///   `RESULT_CLOSED` is drained); that is not a signal that the close has not
///   landed, and the `RESULT_CLOSED` event remains the only such signal.
/// * `reldex_hub_destroy` frees them **synchronously, on the thread that
///   called it**, before it returns. There is no window after it during which
///   a stale pointer still happens to work.
///
/// A batch held past the close keeps its own claim on the description alive,
/// so `reldex_batch_column_info` on that batch keeps working. These are not
/// the same pointers.
///
/// # A second execute does not close the first result
///
/// Submitting another statement on this session leaves every earlier result
/// **open and describable**. Nothing is closed implicitly, because closing a
/// result the caller might still be paging through is not a decision this
/// library may take on its own. A caller that re-runs a query without calling
/// `reldex_session_close_result` therefore accumulates open results — each
/// holding its cursor and its rows on the server — until the session closes.
///
/// # Safety
///
/// `hub` must be null, or a live hub; `out` must be null, or point at a
/// writable `ReldexColumnInfo` with its `struct_size` set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_session_result_column(
    hub: *mut ReldexHub,
    session: ReldexSessionId,
    result: ReldexResultId,
    column: usize,
    out: *mut ReldexColumnInfo,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract for `hub`.
        unsafe {
            with_session(hub, session, |_hub, entry| {
                let Some((_, columns)) = entry.result(result) else {
                    set_last_error(DbError::internal(format!(
                        "reldex-ffi: reldex_session_result_column: session {session} has no open result {result}"
                    )));
                    return ReldexStatus::NotFound;
                };
                let Some(info) = columns.info(column) else {
                    set_last_error(DbError::internal(format!(
                        "reldex-ffi: reldex_session_result_column: column {column} is out of range; the result has {}",
                        columns.len()
                    )));
                    return ReldexStatus::NotFound;
                };
                // SAFETY: delegated to this function's contract for `out`.
                if write_out_struct(out, info) {
                    ReldexStatus::Ok
                } else {
                    set_last_argument_error(
                        "reldex_session_result_column: `out` is null, unaligned, or too small",
                    )
                }
            })
        }
    })
}

/// Releases a result set and every large object taken from it. The reply is a
/// `RELDEX_EVENT_KIND_RESULT_CLOSED` event carrying `request` and `result`.
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
            with_session(hub, session, |hub, entry| {
                if entry.handle(&hub.registry).is_none() {
                    return refused("close_result");
                }
                let Some((id, _)) = entry.result(result) else {
                    return unknown_result("close_result");
                };
                let pending = Pending {
                    caller: request,
                    result: Some(result),
                    columns: None,
                };
                entry.submit(hub, "close_result", pending, |session, core| {
                    session.submit_close_result(core, id)
                })
            })
        }
    })
}

/// Closes a session, resolving its transaction first.
///
/// The reply is a `RELDEX_EVENT_KIND_SESSION_CLOSED` event carrying `request`.
/// Read its `close_outcome`: `DECISION_REQUIRED`, `COMMIT_FAILED` and
/// `ROLLBACK_FAILED` all leave the session **open and usable**, so the caller
/// can ask the user and close again. A close that closed is followed by the
/// session's `TERMINAL`. This is the only path that can commit; abandoning a
/// session or destroying the hub never does.
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
            with_session(hub, session, |hub, entry| {
                let status =
                    entry.submit(hub, "close", Pending::caller(request), |session, core| {
                        session.submit_close(core, disposition)
                    });
                if status == ReldexStatus::Ok {
                    entry.lock().caller_ended = true;
                }
                status
            })
        }
    })
}

/// Abandons a session: releases it **without committing** and without
/// waiting, whatever it is doing (ABI 3.2).
///
/// This is how a worksheet is disconnected when a close cannot be waited for —
/// a statement that cannot be interrupted, or a connect that has not
/// returned. It never blocks and is never refused for lack of room. The
/// session's `TERMINAL` follows once its worker reaches the abandon —
/// immediately when it is idle or still connecting, when the current driver
/// call returns when it is not — carrying `abandoned = true` and the
/// authoritative `transaction_possibly_lost`. Every request already accepted
/// still gets its one reply.
///
/// `*out_outcome` receives a [`ReldexAbandonOutcome`]; with
/// `RELDEX_ABANDON_OUTCOME_OPEN`, `*out_transaction_possibly_lost` is a
/// conservative early answer the UI may show at once. Nothing may be submitted
/// on the session afterwards (`RELDEX_STATUS_INVALID_STATE`); a second abandon
/// reports `ALREADY_ENDED` and does nothing.
///
/// # Safety
///
/// `hub` must be a live hub; each out pointer must be null or point at a
/// writable value of its type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_session_abandon(
    hub: *mut ReldexHub,
    session: ReldexSessionId,
    out_outcome: *mut i32,
    out_transaction_possibly_lost: *mut bool,
) -> ReldexStatus {
    entry(|| {
        if (!out_outcome.is_null() && !out_outcome.is_aligned())
            || (!out_transaction_possibly_lost.is_null()
                && !out_transaction_possibly_lost.is_aligned())
        {
            return set_last_argument_error("reldex_session_abandon: an out pointer is unaligned");
        }
        // SAFETY: delegated to this function's contract for `hub`.
        unsafe {
            with_session(hub, session, |hub, entry| {
                let (outcome, lost) = entry.abandon(&hub.registry);
                if !out_outcome.is_null() {
                    // SAFETY: checked non-null and aligned above.
                    out_outcome.write(outcome as i32);
                }
                if !out_transaction_possibly_lost.is_null() {
                    // SAFETY: checked non-null and aligned above.
                    out_transaction_possibly_lost.write(lost);
                }
                ReldexStatus::Ok
            })
        }
    })
}

/// Submits a request that is answered by a `RELDEX_EVENT_KIND_COMPLETED`
/// event.
///
/// # Safety
///
/// `hub` must be null, or a live hub.
unsafe fn submit_simple(
    hub: *mut ReldexHub,
    session: ReldexSessionId,
    request: ReldexRequestId,
    what: &str,
    send: impl FnOnce(&DatabaseSession, RequestId) -> DbResult<()>,
) -> ReldexStatus {
    // SAFETY: delegated to this function's contract.
    unsafe {
        with_session(hub, session, |hub, entry| {
            entry.submit(hub, what, Pending::caller(request), send)
        })
    }
}

/// Commits the session's current transaction. The reply is a
/// `RELDEX_EVENT_KIND_COMPLETED` event carrying `request`, with
/// `completed_operation` set to `RELDEX_COMPLETED_OPERATION_COMMIT`.
///
/// # Safety
///
/// `hub` must be a live hub.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_session_commit(
    hub: *mut ReldexHub,
    session: ReldexSessionId,
    request: ReldexRequestId,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract for `hub`.
        unsafe {
            submit_simple(hub, session, request, "commit", |session, core| {
                session.submit_commit(core)
            })
        }
    })
}

/// Rolls back the session's current transaction. The reply is a
/// `RELDEX_EVENT_KIND_COMPLETED` event, `completed_operation`
/// `RELDEX_COMPLETED_OPERATION_ROLLBACK`.
///
/// # Safety
///
/// `hub` must be a live hub.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_session_rollback(
    hub: *mut ReldexHub,
    session: ReldexSessionId,
    request: ReldexRequestId,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract for `hub`.
        unsafe {
            submit_simple(hub, session, request, "rollback", |session, core| {
                session.submit_rollback(core)
            })
        }
    })
}

/// Reads a savepoint name for the two savepoint calls.
///
/// # Safety
///
/// `name` must point at `name.len` readable bytes.
unsafe fn savepoint_name(name: ReldexStr, what: &str) -> Result<SavepointName, ReldexStatus> {
    // SAFETY: delegated to this function's contract.
    let Some(text) = (unsafe { name.as_str() }) else {
        return Err(set_last_argument_error(format!(
            "{what}: `name` is null or is not valid UTF-8"
        )));
    };
    SavepointName::new(text).map_err(|error| {
        set_last_error(DbError::new(ErrorKind::Configuration, error.to_string()));
        ReldexStatus::InvalidArgument
    })
}

/// Marks a savepoint named `name` in the session's current transaction. The
/// reply is a `RELDEX_EVENT_KIND_COMPLETED` event, `completed_operation`
/// `RELDEX_COMPLETED_OPERATION_SAVEPOINT`.
///
/// # Safety
///
/// `hub` must be a live hub, and `name` must point at `name.len` readable
/// bytes of UTF-8 text.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_session_savepoint(
    hub: *mut ReldexHub,
    session: ReldexSessionId,
    request: ReldexRequestId,
    name: ReldexStr,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract for `name`.
        let name = match unsafe { savepoint_name(name, "reldex_session_savepoint") } {
            Ok(name) => name,
            Err(status) => return status,
        };
        // SAFETY: delegated to this function's contract for `hub`.
        unsafe {
            submit_simple(hub, session, request, "savepoint", move |session, core| {
                session.submit_savepoint(core, name)
            })
        }
    })
}

/// Rolls the session's current transaction back to a savepoint named `name`.
/// The reply is a `RELDEX_EVENT_KIND_COMPLETED` event, `completed_operation`
/// `RELDEX_COMPLETED_OPERATION_ROLLBACK_TO_SAVEPOINT`.
///
/// # Safety
///
/// As [`reldex_session_savepoint`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_session_rollback_to_savepoint(
    hub: *mut ReldexHub,
    session: ReldexSessionId,
    request: ReldexRequestId,
    name: ReldexStr,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract for `name`.
        let name = match unsafe { savepoint_name(name, "reldex_session_rollback_to_savepoint") } {
            Ok(name) => name,
            Err(status) => return status,
        };
        // SAFETY: delegated to this function's contract for `hub`.
        unsafe {
            submit_simple(
                hub,
                session,
                request,
                "rollback_to_savepoint",
                move |session, core| session.submit_rollback_to_savepoint(core, name),
            )
        }
    })
}

/// Pings the session: a round trip with no statement, used to validate a
/// connection the driver flagged as needing revalidation. The reply is a
/// `RELDEX_EVENT_KIND_COMPLETED` event, `completed_operation`
/// `RELDEX_COMPLETED_OPERATION_PING`.
///
/// # Safety
///
/// `hub` must be a live hub.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_session_ping(
    hub: *mut ReldexHub,
    session: ReldexSessionId,
    request: ReldexRequestId,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract for `hub`.
        unsafe {
            submit_simple(hub, session, request, "ping", |session, core| {
                session.submit_ping(core)
            })
        }
    })
}

/// Turns this session's server output collection on or off (M2.7; ADR-0002
/// amendment T). The reply is a `RELDEX_EVENT_KIND_SERVER_OUTPUT_CONFIGURED`
/// event carrying `request`; read `server_output_mode` and
/// `server_output_buffer_bytes` there for the setting **actually in force** —
/// a driver may clamp a requested buffer size into the range its server
/// accepts.
///
/// **Off by default, and only the user turns it on.** While on, every
/// statement this session runs pays at least one extra round trip
/// (`docs/exec-plans/active/phase-1-m2-5-event-queue.md` §7.5). A reconnect
/// is a new session, and a new session starts with output off: this library
/// never carries the setting over one.
///
/// `mode` is a [`crate::ReldexServerOutputMode`]:
/// `RELDEX_SERVER_OUTPUT_MODE_DISABLED` turns output off;
/// `RELDEX_SERVER_OUTPUT_MODE_ENABLED_UNLIMITED` turns it on with no buffer
/// limit; `RELDEX_SERVER_OUTPUT_MODE_ENABLED_BYTES` turns it on with
/// `buffer_bytes` as the requested limit (`buffer_bytes` is ignored for the
/// other two modes).
///
/// # Safety
///
/// `hub` must be a live hub.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_session_set_server_output(
    hub: *mut ReldexHub,
    session: ReldexSessionId,
    request: ReldexRequestId,
    mode: i32,
    buffer_bytes: u64,
) -> ReldexStatus {
    entry(|| {
        let setting = if mode == ReldexServerOutputMode::Disabled as i32 {
            ServerOutputSetting::Disabled
        } else if mode == ReldexServerOutputMode::EnabledUnlimited as i32 {
            ServerOutputSetting::Enabled(ServerOutputBuffer::Unlimited)
        } else if mode == ReldexServerOutputMode::EnabledBytes as i32 {
            let Ok(bytes) = u32::try_from(buffer_bytes) else {
                return set_last_argument_error(
                    "reldex_session_set_server_output: `buffer_bytes` does not fit in 32 bits",
                );
            };
            let Some(bytes) = std::num::NonZeroU32::new(bytes) else {
                return set_last_argument_error(
                    "reldex_session_set_server_output: `buffer_bytes` must be above zero for \
                     RELDEX_SERVER_OUTPUT_MODE_ENABLED_BYTES",
                );
            };
            ServerOutputSetting::Enabled(ServerOutputBuffer::Bytes(bytes))
        } else {
            return set_last_argument_error(
                "reldex_session_set_server_output: `mode` is not a ReldexServerOutputMode value \
                 this build accepts as input",
            );
        };
        // SAFETY: delegated to this function's contract for `hub`.
        unsafe {
            submit_simple(
                hub,
                session,
                request,
                "set_server_output",
                move |session, core| session.submit_set_server_output(core, setting),
            )
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
/// # The ordering rule this imposes on teardown
///
/// "Callable from any thread" and "the hub may be destroyed" are two rules
/// that have to be sequenced by the **caller**, because this library has no
/// internal synchronisation for it and the ABI gives it nowhere to put one:
/// `hub` is a raw pointer, and a cancel that arrives after
/// [`crate::reldex_hub_destroy`] has freed it dereferences freed memory. The
/// rule, stated as a contract rather than left implied:
///
/// > Every thread that may call `reldex_session_request_cancel` must have
/// > **returned from that call** — be joined, or otherwise proven quiescent —
/// > before `reldex_hub_destroy` is called.
///
/// A stale *session id* is safe: ids are never reused, so a cancel naming a
/// session that has since been retired reports `RELDEX_STATUS_NOT_FOUND`
/// against a live hub. It is only the **hub pointer** that must be sequenced.
/// In a Qt adapter this falls out naturally — the worker that offers Cancel is
/// stopped before the bridge is torn down — but it must be done on purpose.
///
/// The structural alternative is a refcounted cancel handle
/// (`reldex_session_cancel_handle` / `reldex_cancel_handle_release`) that
/// keeps what it needs alive independently of the hub. That is deliberately
/// **not** built yet: nothing needs it, and ADR-0003's amendment A10 records
/// it as the fix if a real cross-thread cancel appears.
///
/// # Safety
///
/// `hub` must be a live hub that no other thread is concurrently destroying
/// (see above); `out_outcome` must be null or point at a writable `int32_t`.
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
        let cancel = |hub: &ReldexHub, entry: &Arc<SessionEntry>| {
            let Some(session) = entry.handle(&hub.registry) else {
                set_last_error(DbError::internal(
                    "reldex-ffi: reldex_session_request_cancel: the session is still opening or \
                     already closed; there is nothing to cancel",
                ));
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
/// **This appends to the arena**, like the formatter, so it invalidates every
/// `ReldexArenaView` taken from that arena before the call. Either read the
/// warnings into their own arena or re-take the view afterwards.
///
/// # Safety
///
/// `hub` must be a live hub and `arena` a live arena that no other thread is
/// using (it is taken by `&mut`).
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
        let collect = |hub: &ReldexHub, entry: &Arc<SessionEntry>| {
            let Some(session) = entry.handle(&hub.registry) else {
                set_last_error(DbError::internal(
                    "reldex-ffi: reldex_session_connect_warnings: the session is still opening or \
                     already closed",
                ));
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

#[cfg(all(test, feature = "mock-driver"))]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use reldex_db_core::{
        Cap, CapSource, ConnectionParams, OutValue, ResultCaps, ResultPolicy, ResultStore,
        SessionEvent, Sourced, Statement,
    };
    use reldex_db_driver_api::{Credentials, Endpoint, LobKind};
    use reldex_driver_mock::{Action, MockDriver, Scenario};

    use super::{Pending, ReldexHub, ResultColumns, translate, with_session};
    use crate::event::ReldexEventKind;
    use crate::mock::statements;

    /// The next raw event matching `wanted`, discarding the others.
    fn next_raw(hub: &ReldexHub, wanted: impl Fn(&SessionEvent) -> bool) -> SessionEvent {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(event) = hub.take_raw().filter(|event| wanted(event)) {
                return event;
            }
            assert!(Instant::now() < deadline, "no event within the hang guard");
            std::thread::yield_now();
        }
    }

    /// Nothing in ABI 3.2 submits a segment fetch or reads a LOB, so these
    /// two replies are driven straight through `db-core` and only their
    /// translation is under test: `FetchedSegment` crosses as an opaque
    /// notification naming the caller's request and result, and `LobChunk` —
    /// standing in for every variant this build does not know — as
    /// `RELDEX_EVENT_KIND_UNKNOWN`, not as nothing.
    #[test]
    fn a_segment_reply_and_an_unknown_reply_cross_as_notifications() {
        let raw = crate::reldex_hub_create();
        let mut id = 0_u64;
        // SAFETY: `raw` is the live hub just created; `id` is a real local.
        let status =
            unsafe { crate::reldex_hub_open_session(raw, std::ptr::null(), 1, &raw mut id) };
        assert_eq!(status, crate::ReldexStatus::Ok);
        let check = |hub: &ReldexHub, entry: &Arc<super::SessionEntry>| {
            let opened = next_raw(hub, |event| matches!(event, SessionEvent::Opened { .. }));
            assert_eq!(translate(hub, opened).kind, ReldexEventKind::Opened);
            let session = entry.handle(&hub.registry).expect("the session is open");

            let query = statements::text(statements::GENERATED_QUERY);
            let request = hub.begin_request(Pending::caller(2));
            session
                .submit_execute(request, Statement::new(query))
                .expect("accepted");
            let SessionEvent::Executed { outcome, .. } =
                next_raw(hub, |event| matches!(event, SessionEvent::Executed { .. }))
            else {
                unreachable!("filtered on Executed");
            };
            let outcome = outcome.expect("the query runs");
            let result = outcome.result.expect("the query opens a result");
            let policy = ResultPolicy::new(ResultCaps::new(
                Sourced::new(Cap::Unlimited, CapSource::Application),
                Sourced::new(Cap::Unlimited, CapSource::BuiltIn),
            ));
            let mut store = ResultStore::new(result, &outcome.columns, policy);
            let key = entry.register_result(result, ResultColumns::new(outcome.columns));
            let fetches = store
                .submit_events(&session, || hub.begin_request(Pending::caller(3)))
                .expect("the store asks for its first segment");
            assert!(fetches >= 1);
            let segment = next_raw(hub, |event| {
                matches!(event, SessionEvent::FetchedSegment { .. })
            });
            let segment = translate(hub, segment);
            assert_eq!(segment.kind, ReldexEventKind::FetchedSegment);
            assert_eq!(segment.request, 3);
            assert_eq!(segment.result, Some(key));
            assert!(segment.row_count > 0);
            assert!(segment.error.is_none());
            crate::ReldexStatus::Ok
        };
        // SAFETY: `raw` is live until destroyed below.
        let checked = unsafe { with_session(raw, id, check) };
        assert_eq!(checked, crate::ReldexStatus::Ok);

        // A world that can hand out a large object, opened on the same hub.
        let scenario = Scenario::new();
        scenario.on_sql(
            "lob",
            Action::LobOut {
                name: "doc".to_owned(),
                kind: LobKind::Character,
                bytes: b"text".to_vec(),
            },
        );
        let params = ConnectionParams::new(
            Endpoint::ConnectString("mock".to_owned()),
            Credentials::External,
        );
        // SAFETY: as above.
        let unknown = unsafe {
            crate::hub::with_hub(raw, |hub| {
                let request = hub.begin_request(Pending::caller(4));
                let lob_session =
                    hub.registry
                        .open(Arc::new(MockDriver::new(scenario)), params, request);
                next_raw(hub, |event| matches!(event, SessionEvent::Opened { .. }));
                let session = hub.registry.get(lob_session).expect("open");
                let request = hub.begin_request(Pending::caller(5));
                session
                    .submit_execute(request, Statement::new("lob"))
                    .expect("accepted");
                let SessionEvent::Executed { outcome, .. } =
                    next_raw(hub, |event| matches!(event, SessionEvent::Executed { .. }))
                else {
                    unreachable!("filtered on Executed");
                };
                let outcome = outcome.expect("the block runs");
                let Some(OutValue::Lob(lob)) = outcome.out_values.named("doc") else {
                    panic!("expected a large object");
                };
                let chunk = NonZeroUsize::new(16).expect("non-zero");
                let request = hub.begin_request(Pending::caller(6));
                session
                    .submit_read_lob_chunk(request, *lob, chunk)
                    .expect("accepted");
                let read = next_raw(hub, |event| matches!(event, SessionEvent::LobChunk { .. }));
                (translate(hub, read), lob_session.get())
            })
        };
        let (unknown, lob_session) = unknown.expect("the hub is live");
        assert_eq!(unknown.kind, ReldexEventKind::Unknown);
        assert_eq!(unknown.request, 6);
        assert_eq!(unknown.session, lob_session);
        // SAFETY: `raw` is live and destroyed once.
        unsafe { crate::reldex_hub_destroy(raw) };
    }
}
