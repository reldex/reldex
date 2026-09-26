//! What an event looks like on the way out (ADR-0003 D5, A35).
//!
//! Replies to requests, progress (`EXECUTING`) and notifications
//! (`SERVER_OUTPUT`, `TRANSACTION_STATE`, `TERMINAL`) all cross as one flat
//! `#[repr(C)]` struct rather than a tagged union: the adapter switches on
//! `kind` and reads the fields that kind documents. A union would save a few
//! dozen bytes per event and cost every C++ reader a cast.
//!
//! Each one is built by `session::translate` from a `db-core` `SessionEvent`,
//! on the thread that drains the hub, at the moment it is handed out. The
//! objects an event owns — its batch, its error, its server-output lines —
//! are created then too, not when the event was queued (A36).

use reldex_db_core::{CloseError, ExecuteOutcome, StatementKind};

use crate::batch::ReldexBatch;
use crate::error::{ReldexError, ReldexSessionState};
use crate::strings::{CStruct, OwnedStr, ReldexStr};

/// The lines of one `RELDEX_EVENT_KIND_SERVER_OUTPUT` event.
///
/// Owned by the caller from the moment the event is drained; release with
/// [`reldex_server_output_lines_release`]. Each line is NUL-terminated like
/// every outbound [`ReldexStr`] (`len` is authoritative even for a line that
/// legitimately contains an embedded NUL byte — the same rule the rest of
/// this boundary already promises).
pub struct ReldexServerOutputLines {
    lines: Vec<OwnedStr>,
}

impl Drop for ReldexServerOutputLines {
    fn drop(&mut self) {
        crate::counters::destroyed(crate::counters::Kind::ServerOutputLines);
    }
}

impl ReldexServerOutputLines {
    pub(crate) fn new(lines: Vec<Box<str>>) -> Self {
        crate::counters::created(crate::counters::Kind::ServerOutputLines);
        Self {
            lines: lines.into_iter().map(OwnedStr::new).collect(),
        }
    }
}

/// How many lines `lines` holds.
///
/// # Safety
///
/// `lines` must be null (reported as 0) or a live
/// [`ReldexServerOutputLines`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_server_output_lines_count(
    lines: *const ReldexServerOutputLines,
) -> usize {
    crate::status::entry_value(0, || {
        if lines.is_null() {
            return 0;
        }
        // SAFETY: delegated to this function's contract.
        unsafe { &*lines }.lines.len()
    })
}

/// Line `index`, or the empty string when `index` is out of range.
///
/// # Safety
///
/// `lines` must be null (reported as empty) or a live
/// [`ReldexServerOutputLines`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_server_output_lines_get(
    lines: *const ReldexServerOutputLines,
    index: usize,
) -> ReldexStr {
    crate::status::entry_value(ReldexStr::empty(), || {
        if lines.is_null() {
            return ReldexStr::empty();
        }
        // SAFETY: delegated to this function's contract.
        unsafe { &*lines }
            .lines
            .get(index)
            .map_or_else(ReldexStr::empty, OwnedStr::as_reldex_str)
    })
}

/// Releases the lines of a drained `RELDEX_EVENT_KIND_SERVER_OUTPUT` event.
///
/// # Safety
///
/// `lines` must be null (a no-op) or a pointer this library handed out that
/// has not already been released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_server_output_lines_release(lines: *mut ReldexServerOutputLines) {
    crate::status::entry_value((), || {
        if lines.is_null() {
            return;
        }
        // SAFETY: delegated to this function's contract.
        drop(unsafe { Box::from_raw(lines) });
    });
}

/// Which request an event is the reply to.
///
/// `0` is reserved for a kind this header predates (ADR-0003 D7). Exactly one
/// event is delivered per accepted request; an event whose `error` is non-null
/// is that request's *failure*, still delivered under its own kind so the
/// adapter never has to guess which operation failed.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexEventKind {
    /// A kind this header does not know.
    Unknown = 0,
    /// Reply to `reldex_hub_open_session`.
    Opened = 1,
    /// Reply to `reldex_session_execute`.
    Executed = 2,
    /// Reply to `reldex_session_fetch`. On success `batch` is non-null and the
    /// caller owns it.
    Fetched = 3,
    /// Reply to `reldex_session_close_result`.
    ResultClosed = 4,
    /// Reply to `reldex_session_close`. Read `close_outcome`: a close can fail
    /// and leave the session **open**.
    SessionClosed = 5,
    /// Reply to `reldex_session_commit`, `reldex_session_rollback`,
    /// `reldex_session_savepoint`, `reldex_session_rollback_to_savepoint` or
    /// `reldex_session_ping`. Read `completed_operation` to know which one.
    Completed = 6,
    /// Reply to `reldex_session_set_server_output`. `server_output_mode` and
    /// `server_output_buffer_bytes` carry the setting **actually in force**,
    /// which a driver may have adjusted.
    ServerOutputConfigured = 7,
    /// Unsolicited: lines the server produced out of band (M2.7) —
    /// `DBMS_OUTPUT` and its kind — read by the session's worker after the
    /// statement that wrote them and delivered **before that statement's
    /// `EXECUTED`**, so every `SERVER_OUTPUT` lies between an execute's
    /// `EXECUTING` and its `EXECUTED` (two documented exceptions land in the
    /// *next* execute's window: output written during a fetch, and output of
    /// a statement that failed and left the session needing validation). One
    /// statement's output may arrive as several events. `request` is `0`.
    /// See `server_output_lines`, `server_output_dropped` and
    /// `server_output_invalid_utf8_lines`; a non-null `error` is a read that
    /// failed, so the output is incomplete and the pane must say why.
    ServerOutput = 8,
    /// The session ended. Exactly once per session, unsolicited, carrying no
    /// request id of its own (`request` is `0`): after a close that actually
    /// closed, after a connect that failed or was abandoned, and — since ABI
    /// 3.2 — the moment a session is **lost** mid-statement, without waiting
    /// for the caller to close it. `session_state` is `LOST` or `CLOSED`;
    /// `error` is the failure that lost it (native code and cause intact), or
    /// null for a deliberate end. Read `transaction_possibly_lost` and
    /// surface it (`SPEC.md` §10: never hide a transaction loss); `abandoned`
    /// says the session ended because the caller called
    /// `reldex_session_abandon`; `server_output_dropped` carries output lines
    /// the session lost that no earlier `SERVER_OUTPUT` reported.
    ///
    /// Replies to requests the caller submitted **after** the session ended
    /// may still follow it — each accepted request gets its one reply — but
    /// once this event has been drained the session id is retired: every
    /// later call naming it reports `RELDEX_STATUS_NOT_FOUND`, and it no
    /// longer counts in `reldex_hub_session_count`. Not delivered for
    /// sessions still open when the hub is destroyed: by then nothing can
    /// observe it.
    Terminal = 9,
    /// Progress, not a reply (ABI 3.2): the worker has **started** the
    /// statement submitted as `executing_request` — it has left the queue,
    /// and `deadline_ms`/`has_deadline` echo the limit actually armed on it.
    /// This is the honest "running, with this limit" state a UI shows on a
    /// driver that cannot cancel (`SPEC.md` §24.8). The one reply to that
    /// request is still its `EXECUTED`, which always follows.
    ///
    /// `request` is `0`, as on every other kind that answers nothing: exactly
    /// one event per accepted request carries that request's id, its reply —
    /// the 3.1 contract, unchanged. Like the other two kinds 3.2 added, it is
    /// never delivered to a caller whose `struct_size` predates 3.2 (see
    /// `reldex_hub_next_event`).
    Executing = 10,
    /// Unsolicited (ABI 3.2): whether a transaction may be open on this
    /// session flipped; read `transaction_possibly_active`. Advisory — it
    /// drives Commit/Rollback affordances without polling, while a close still
    /// re-decides on the worker. Never dropped: a value not yet drained is
    /// updated in place to the newest one. `request` is `0`.
    TransactionState = 11,
    /// A result store's segment fetch was answered (M5.2 Stage A; ABI 3.2).
    /// **Not produced by this build** — nothing in this ABI submits a segment
    /// fetch yet (that is M5.2 Stage B) — and delivered as an opaque
    /// notification when it is: `request`, `result`/`has_result` when the
    /// result is one this session reported, `row_count`, and `error`. The
    /// segment's rows are not reachable through this event.
    FetchedSegment = 12,
}

/// Which operation a `RELDEX_EVENT_KIND_COMPLETED` event answers.
///
/// `0` is reserved for an operation this header predates.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexCompletedOperation {
    /// An operation this header does not know.
    Unknown = 0,
    /// `reldex_session_commit`.
    Commit = 1,
    /// `reldex_session_rollback`.
    Rollback = 2,
    /// `reldex_session_savepoint`.
    Savepoint = 3,
    /// `reldex_session_rollback_to_savepoint`.
    RollbackToSavepoint = 4,
    /// `reldex_session_ping`.
    Ping = 5,
}

/// Whether server output is on for a session, and with what buffer — the
/// setting `RELDEX_EVENT_KIND_SERVER_OUTPUT_CONFIGURED` reports **in force**.
///
/// `0` is reserved for a mode this header predates.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexServerOutputMode {
    /// A mode this header does not know.
    Unknown = 0,
    /// The server does not buffer output for this session.
    Disabled = 1,
    /// The server buffers output with no limit other than its own memory.
    EnabledUnlimited = 2,
    /// The server buffers output up to `server_output_buffer_bytes`.
    EnabledBytes = 3,
}

/// What kind of statement the server ran, as far as the driver could tell.
///
/// `0` is reserved for a kind this header predates.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexStatementKind {
    /// A kind this header does not know.
    Unknown = 0,
    /// A query that returns rows.
    Query = 1,
    /// Data manipulation.
    Dml = 2,
    /// Data definition — note `committed_implicitly`.
    Ddl = 3,
    /// An anonymous PL/SQL block or a call into stored code.
    PlSqlBlock = 4,
    /// Transaction control submitted as text.
    TransactionControl = 5,
    /// Session control (`ALTER SESSION` and similar).
    SessionControl = 6,
    /// Something else, or the driver could not classify it.
    Other = 7,
}

impl From<StatementKind> for ReldexStatementKind {
    fn from(kind: StatementKind) -> Self {
        match kind {
            StatementKind::Query => Self::Query,
            StatementKind::Dml => Self::Dml,
            StatementKind::Ddl => Self::Ddl,
            StatementKind::PlSqlBlock => Self::PlSqlBlock,
            StatementKind::TransactionControl => Self::TransactionControl,
            StatementKind::SessionControl => Self::SessionControl,
            StatementKind::Other => Self::Other,
            // `StatementKind` is `#[non_exhaustive]`.
            _ => Self::Unknown,
        }
    }
}

/// How a session close ended.
///
/// Three of these leave the session **open and usable**, so the adapter can
/// ask the user and try again; only `CLOSED` and `FAILED` end the session
/// (`SPEC.md` §10 — Reldex never silently commits, and never hides a
/// transaction it could not resolve).
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexCloseOutcome {
    /// An outcome this header does not know.
    Unknown = 0,
    /// The session is closed.
    Closed = 1,
    /// A transaction may be open and no disposition was given. **Still open.**
    DecisionRequired = 2,
    /// The requested commit failed, so the transaction is unchanged.
    /// **Still open.**
    CommitFailed = 3,
    /// The requested rollback failed, so the transaction is unchanged.
    /// **Still open.**
    RollbackFailed = 4,
    /// The transaction was resolved (or there was none) but closing failed, or
    /// the session was already lost — in which case nothing was committed. The
    /// session is closed either way and must not be reused.
    Failed = 5,
}

/// One event, filled in by [`crate::reldex_hub_next_event`]: a request's reply,
/// `Executing` progress, or one of the unsolicited kinds.
///
/// The caller sets `struct_size` before each call
/// (`ReldexEvent ev = { .struct_size = sizeof ev };`) and **owns** `error` and
/// `batch` when they come back non-null: release them with
/// [`crate::reldex_error_free`] and [`crate::reldex_batch_release`].
///
/// # Integer widths, so the rule is not guessed at
///
/// * A **count of things in this process** — rows, columns, warnings — is
///   `size_t`, because that is what the caller will loop with and what
///   `reldex_batch_row_count` / `reldex_batch_column_count` already return.
///   Mixing `uint32_t` and `size_t` counts in one struct is how a `-Wsign-
///   compare` warning gets silenced with a cast that is wrong on one platform.
/// * A **quantity the database reported** — `rows_affected` — is `uint64_t`,
///   deliberately *not* `size_t`: an `UPDATE` can change more rows than a
///   32-bit host can address, and the number must not change meaning when the
///   adapter is built for one.
/// * An **id** is `uint64_t`, and an **enum** is `int32_t` (ADR-0003 D6/D7).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReldexEvent {
    /// `sizeof(ReldexEvent)` on the way in; how much of it is valid on the way
    /// out.
    pub struct_size: u32,
    /// A [`ReldexEventKind`].
    pub kind: i32,
    /// The session this reply belongs to.
    pub session: u64,
    /// The `request_id` the caller passed to the submitting call, on a reply:
    /// the request it answers. Exactly one event per accepted request carries
    /// that request's id. `0` on every kind that answers nothing — progress
    /// (`Executing`, which names its statement in `executing_request`
    /// instead) and the unsolicited kinds (`ServerOutput`, `TransactionState`,
    /// `Terminal`).
    pub request: u64,
    /// The result set this event is about, when `has_result`:
    ///
    /// * `Executed` — the id the statement opened, to fetch and close with;
    /// * `Fetched` — the id the fetch was submitted for, so a caller does not
    ///   have to keep its own request-to-result map;
    /// * `ResultClosed` — the id that has just been closed. It is **no longer
    ///   valid** by the time this event is drained: it names what ended.
    ///
    /// Zero and `has_result == false` on every other kind, and on a failure of
    /// an `Executed` that never opened one.
    pub result: u64,
    /// `Executed`: rows changed, when `has_rows_affected`.
    pub rows_affected: u64,
    /// `Opened`: the driver connection's id.
    pub connection_id: u64,
    /// `Fetched`: the batch's row count, mirrored here so a caller that only
    /// needs "is the result exhausted?" does not have to touch the batch. Zero
    /// means exhausted.
    pub row_count: usize,
    /// The failure, or null on success. The caller owns it.
    pub error: *mut ReldexError,
    /// `Fetched`: the batch, or null. The caller owns it.
    pub batch: *mut ReldexBatch,
    /// The session's lifecycle as of this event: a [`ReldexSessionState`].
    ///
    /// A successful reply reports `USABLE` (the request succeeded, so the
    /// session was usable when it answered) even if the session was lost
    /// while the reply waited in the queue; that loss is the `TERMINAL`
    /// behind it. A failed reply, a progress event and a notification report
    /// the session's state when the event was taken.
    pub session_state: i32,
    /// `Executed`: a [`ReldexStatementKind`].
    pub statement_kind: i32,
    /// `Opened`: a [`crate::ReldexCancelKind`] — what a cancel on this session
    /// can actually do (`SPEC.md` §24.8).
    pub cancel_kind: i32,
    /// `SessionClosed`: a [`ReldexCloseOutcome`].
    pub close_outcome: i32,
    /// `Executed`: how many columns the result has. Their names and types are
    /// available from this moment with `reldex_session_result_column`, without
    /// waiting for a batch — a result with columns and no rows never produces
    /// one to ask. `reldex_batch_column_info` reports the same description per
    /// batch.
    pub column_count: usize,
    /// `Opened`: how many connect-time warnings the session reported. Read
    /// them with [`crate::reldex_session_connect_warnings`].
    pub warning_count: usize,
    /// Whether `result` names a result set; see that field.
    pub has_result: bool,
    /// `Executed`: whether `rows_affected` is meaningful.
    pub has_rows_affected: bool,
    /// `Executed`: whether the server committed the transaction as a side
    /// effect, whatever the client asked for (typically DDL). Reldex could
    /// neither prevent nor undo it; the user must be told.
    pub committed_implicitly: bool,
    /// `SessionClosed`: whether the session is still open and usable.
    pub session_still_open: bool,
    /// `Completed`: a [`ReldexCompletedOperation`].
    pub completed_operation: i32,
    /// `ServerOutputConfigured`: a [`ReldexServerOutputMode`] — the mode
    /// **actually in force**.
    pub server_output_mode: i32,
    /// `ServerOutputConfigured`, when `server_output_mode` is
    /// `RELDEX_SERVER_OUTPUT_MODE_ENABLED_BYTES`: the buffer size actually in
    /// force, which the server may have clamped from what was requested.
    pub server_output_buffer_bytes: u64,
    /// `ServerOutput`: how many lines were dropped for this session since the
    /// previous delivered `ServerOutput`, because the session already had the
    /// most unsolicited events the queue holds for one session waiting
    /// undrained (256). `Terminal`: lines dropped that no delivered
    /// `ServerOutput` reported. Zero normally; non-zero means the UI must say
    /// "output truncated".
    pub server_output_dropped: u32,
    /// `ServerOutput`: how many of `server_output_lines` were not valid UTF-8
    /// on the wire and were delivered with U+FFFD in place of the invalid
    /// bytes rather than dropped (M2.12).
    pub server_output_invalid_utf8_lines: u32,
    /// `ServerOutput`: the lines, in order. The caller owns them; release with
    /// [`reldex_server_output_lines_release`]. Null when there are none (which
    /// still happens with `error` set, or `server_output_dropped` non-zero, or
    /// both — a `ServerOutput` event is never produced with nothing to say).
    pub server_output_lines: *mut ReldexServerOutputLines,
    /// `Terminal`: whether this session ended while it may still have held an
    /// unresolved transaction. **This is the authoritative answer and a
    /// consumer must surface it** (`SPEC.md` §10: never silently commit or
    /// hide transaction loss).
    pub transaction_possibly_lost: bool,
    /// `Executing` (ABI 3.2): whether `deadline_ms` is meaningful — false
    /// means the statement runs with no time limit.
    pub has_deadline: bool,
    /// `TransactionState` (ABI 3.2): whether a transaction may now be open.
    /// Conservative: it can be true with nothing open, never false while
    /// something might be.
    pub transaction_possibly_active: bool,
    /// `Terminal` (ABI 3.2): the session ended because the caller called
    /// `reldex_session_abandon` — so a loss reported by
    /// `transaction_possibly_lost` is the rollback that abandoning implies,
    /// not a failure.
    pub abandoned: bool,
    /// `Executing` (ABI 3.2): the deadline armed on the statement, in
    /// milliseconds, when `has_deadline`.
    pub deadline_ms: u64,
    /// `Executing` (ABI 3.2): the `request_id` of the execute that started.
    /// Its `EXECUTED` — the one event that carries this id in `request` — is
    /// still to come. `0` on every other kind.
    pub executing_request: u64,
}

/// How much of [`ReldexEvent`] a caller built against ABI 3.2 or later
/// declares: everything up to and including `executing_request`. A smaller
/// `struct_size` comes from an older header, whose caller is never handed a
/// kind that header predates (see [`introduced_in_3_2`]).
pub(crate) const EVENT_SIZE_3_2: usize =
    std::mem::offset_of!(ReldexEvent, executing_request) + size_of::<u64>();

/// Whether `event` translates to a kind ABI 3.2 introduced (`EXECUTING`,
/// `TRANSACTION_STATE`, `FETCHED_SEGMENT`). None of them owns anything a
/// caller must release, and none answers a request a pre-3.2 caller can
/// make, so a caller whose header predates them is simply never given one:
/// its event stream keeps 3.1's shape (ADR-0003 A26, A35).
pub(crate) const fn introduced_in_3_2(event: &reldex_db_core::SessionEvent) -> bool {
    matches!(
        event,
        reldex_db_core::SessionEvent::Executing { .. }
            | reldex_db_core::SessionEvent::TransactionStateChanged { .. }
            | reldex_db_core::SessionEvent::FetchedSegment { .. }
    )
}

// SAFETY: `#[repr(C)]`, `struct_size` first, every field an integer, a `bool`
// or a raw pointer — all valid as zero.
unsafe impl CStruct for ReldexEvent {
    // The size before M2.11 appended `completed_operation` through
    // `transaction_possibly_lost` — see `ReldexLiveCounts`'s `MIN_SIZE` for
    // why this is computed with `offset_of!` rather than hard-coded.
    const MIN_SIZE: usize = std::mem::offset_of!(Self, completed_operation);
}

impl Default for ReldexEvent {
    /// An empty event with `struct_size` set, which is the shape a caller is
    /// expected to pass in.
    fn default() -> Self {
        Self {
            struct_size: u32::try_from(size_of::<Self>()).unwrap_or(u32::MAX),
            kind: ReldexEventKind::Unknown as i32,
            session: 0,
            request: 0,
            result: 0,
            rows_affected: 0,
            connection_id: 0,
            row_count: 0,
            error: std::ptr::null_mut(),
            batch: std::ptr::null_mut(),
            session_state: ReldexSessionState::Unknown as i32,
            statement_kind: ReldexStatementKind::Unknown as i32,
            cancel_kind: crate::ReldexCancelKind::Unknown as i32,
            close_outcome: ReldexCloseOutcome::Unknown as i32,
            column_count: 0,
            warning_count: 0,
            has_result: false,
            has_rows_affected: false,
            committed_implicitly: false,
            session_still_open: false,
            completed_operation: ReldexCompletedOperation::Unknown as i32,
            server_output_mode: ReldexServerOutputMode::Unknown as i32,
            server_output_buffer_bytes: 0,
            server_output_dropped: 0,
            server_output_invalid_utf8_lines: 0,
            server_output_lines: std::ptr::null_mut(),
            transaction_possibly_lost: false,
            has_deadline: false,
            transaction_possibly_active: false,
            abandoned: false,
            deadline_ms: 0,
            executing_request: 0,
        }
    }
}

/// An event while it is still Rust's: the owning form of [`ReldexEvent`].
///
/// Holds the error and the batch as boxes, so an event that is still queued
/// when the hub goes away frees them instead of leaking them. Ownership moves
/// to the caller — and only then — in
/// [`crate::reldex_hub_next_event`].
pub(crate) struct QueuedEvent {
    pub(crate) kind: ReldexEventKind,
    pub(crate) session: u64,
    pub(crate) request: u64,
    pub(crate) session_state: ReldexSessionState,
    pub(crate) error: Option<Box<ReldexError>>,
    pub(crate) batch: Option<Box<ReldexBatch>>,
    pub(crate) result: Option<u64>,
    pub(crate) rows_affected: Option<u64>,
    pub(crate) connection_id: u64,
    pub(crate) row_count: usize,
    pub(crate) statement_kind: ReldexStatementKind,
    pub(crate) cancel_kind: crate::ReldexCancelKind,
    pub(crate) close_outcome: ReldexCloseOutcome,
    pub(crate) column_count: usize,
    pub(crate) warning_count: usize,
    pub(crate) committed_implicitly: bool,
    pub(crate) session_still_open: bool,
    pub(crate) completed_operation: ReldexCompletedOperation,
    pub(crate) server_output_mode: ReldexServerOutputMode,
    pub(crate) server_output_buffer_bytes: u64,
    pub(crate) server_output_dropped: u32,
    pub(crate) server_output_invalid_utf8_lines: u32,
    pub(crate) server_output_lines: Option<Box<ReldexServerOutputLines>>,
    pub(crate) transaction_possibly_lost: bool,
    pub(crate) deadline_ms: Option<u64>,
    pub(crate) executing_request: u64,
    pub(crate) transaction_possibly_active: bool,
    pub(crate) abandoned: bool,
}

impl QueuedEvent {
    pub(crate) fn new(kind: ReldexEventKind, session: u64, request: u64) -> Self {
        Self {
            kind,
            session,
            request,
            session_state: ReldexSessionState::Unknown,
            error: None,
            batch: None,
            result: None,
            rows_affected: None,
            connection_id: 0,
            row_count: 0,
            statement_kind: ReldexStatementKind::Unknown,
            cancel_kind: crate::ReldexCancelKind::Unknown,
            close_outcome: ReldexCloseOutcome::Unknown,
            column_count: 0,
            warning_count: 0,
            committed_implicitly: false,
            session_still_open: false,
            completed_operation: ReldexCompletedOperation::Unknown,
            server_output_mode: ReldexServerOutputMode::Unknown,
            server_output_buffer_bytes: 0,
            server_output_dropped: 0,
            server_output_invalid_utf8_lines: 0,
            server_output_lines: None,
            transaction_possibly_lost: false,
            deadline_ms: None,
            executing_request: 0,
            transaction_possibly_active: false,
            abandoned: false,
        }
    }

    /// `Completed`: which operation this answers.
    pub(crate) const fn with_completed_operation(
        mut self,
        operation: ReldexCompletedOperation,
    ) -> Self {
        self.completed_operation = operation;
        self
    }

    /// `ServerOutputConfigured`: the setting actually in force.
    pub(crate) fn with_server_output_setting(
        mut self,
        setting: reldex_db_driver_api::ServerOutputSetting,
    ) -> Self {
        use reldex_db_driver_api::{ServerOutputBuffer, ServerOutputSetting};
        match setting {
            ServerOutputSetting::Disabled => {
                self.server_output_mode = ReldexServerOutputMode::Disabled;
            }
            ServerOutputSetting::Enabled(ServerOutputBuffer::Unlimited) => {
                self.server_output_mode = ReldexServerOutputMode::EnabledUnlimited;
            }
            ServerOutputSetting::Enabled(ServerOutputBuffer::Bytes(bytes)) => {
                self.server_output_mode = ReldexServerOutputMode::EnabledBytes;
                self.server_output_buffer_bytes = u64::from(bytes.get());
            }
        }
        self
    }

    /// `ServerOutput`: one event's lines, what was dropped ahead of them, and
    /// a failed read.
    pub(crate) fn with_server_output(
        mut self,
        lines: Vec<Box<str>>,
        dropped: u32,
        invalid_utf8_lines: u32,
        failure: Option<&reldex_db_core::DbError>,
    ) -> Self {
        self.server_output_dropped = dropped;
        self.server_output_invalid_utf8_lines = invalid_utf8_lines;
        if !lines.is_empty() {
            self.server_output_lines = Some(Box::new(ReldexServerOutputLines::new(lines)));
        }
        if let Some(failure) = failure {
            self.error = Some(Box::new(ReldexError::from_db_error(failure)));
        }
        self
    }

    /// `Terminal`: whether the session ended with a transaction possibly
    /// still unresolved, and whether the caller abandoned it.
    pub(crate) const fn with_terminal(mut self, lost: bool, abandoned: bool) -> Self {
        self.transaction_possibly_lost = lost;
        self.abandoned = abandoned;
        self
    }

    /// `Executing`: the deadline armed on the statement.
    /// `Executing`: the statement that started, and the limit armed on it.
    pub(crate) fn with_executing(
        mut self,
        request: u64,
        deadline: Option<std::time::Duration>,
    ) -> Self {
        self.executing_request = request;
        self.deadline_ms =
            deadline.map(|limit| u64::try_from(limit.as_millis()).unwrap_or(u64::MAX));
        self
    }

    /// `TransactionState`: the new value.
    pub(crate) const fn with_transaction_possibly_active(mut self, active: bool) -> Self {
        self.transaction_possibly_active = active;
        self
    }

    pub(crate) fn with_error(mut self, error: ReldexError) -> Self {
        self.session_state = error.session_state_for_event();
        self.error = Some(Box::new(error));
        self
    }

    /// Attaches the fetched batch, which the caller will own once the event is
    /// handed out.
    pub(crate) fn with_batch(mut self, batch: Box<ReldexBatch>, row_count: usize) -> Self {
        self.batch = Some(batch);
        self.row_count = row_count;
        self
    }

    /// Names the result set this reply is about, on a kind other than
    /// `Executed` — where [`Self::with_execute_outcome`] sets it instead.
    ///
    /// Set even when the request failed: which result failed to fetch is
    /// exactly what the caller needs in order to report it.
    pub(crate) const fn with_result(mut self, result: u64) -> Self {
        self.result = Some(result);
        self
    }

    /// Sets the lifecycle state reported on the event. Called last, because
    /// the session registry knows more than any single error does.
    pub(crate) const fn with_session_state(mut self, state: ReldexSessionState) -> Self {
        self.session_state = state;
        self
    }

    /// `column_count` is passed in rather than read from `outcome`: the
    /// columns themselves are *moved* out of the outcome into the result's
    /// shared [`crate::batch::ResultColumns`] before this is called, so
    /// `outcome.columns` is empty by now.
    pub(crate) fn with_execute_outcome(
        mut self,
        outcome: &ExecuteOutcome,
        result: Option<u64>,
        column_count: usize,
    ) -> Self {
        self.result = result;
        self.rows_affected = outcome.rows_affected;
        self.statement_kind = outcome.statement_kind.into();
        self.committed_implicitly = outcome.committed_implicitly;
        self.column_count = column_count;
        self
    }

    pub(crate) fn with_close_outcome(mut self, result: &Result<(), CloseError>) -> Self {
        let (outcome, still_open) = match result {
            Ok(()) => (ReldexCloseOutcome::Closed, false),
            Err(CloseError::DecisionRequired) => (ReldexCloseOutcome::DecisionRequired, true),
            Err(CloseError::CommitFailed(_)) => (ReldexCloseOutcome::CommitFailed, true),
            Err(CloseError::RollbackFailed(_)) => (ReldexCloseOutcome::RollbackFailed, true),
            Err(CloseError::Failed(_)) => (ReldexCloseOutcome::Failed, false),
        };
        self.close_outcome = outcome;
        self.session_still_open = still_open;
        self
    }

    /// Converts to the C shape, transferring ownership of the error and the
    /// batch to the caller.
    pub(crate) fn into_c(self) -> ReldexEvent {
        ReldexEvent {
            kind: self.kind as i32,
            session: self.session,
            request: self.request,
            result: self.result.unwrap_or(0),
            rows_affected: self.rows_affected.unwrap_or(0),
            connection_id: self.connection_id,
            row_count: self.row_count,
            error: self.error.map_or(std::ptr::null_mut(), Box::into_raw),
            batch: self.batch.map_or(std::ptr::null_mut(), Box::into_raw),
            session_state: self.session_state as i32,
            statement_kind: self.statement_kind as i32,
            cancel_kind: self.cancel_kind as i32,
            close_outcome: self.close_outcome as i32,
            column_count: self.column_count,
            warning_count: self.warning_count,
            has_result: self.result.is_some(),
            has_rows_affected: self.rows_affected.is_some(),
            committed_implicitly: self.committed_implicitly,
            session_still_open: self.session_still_open,
            completed_operation: self.completed_operation as i32,
            server_output_mode: self.server_output_mode as i32,
            server_output_buffer_bytes: self.server_output_buffer_bytes,
            server_output_dropped: self.server_output_dropped,
            server_output_invalid_utf8_lines: self.server_output_invalid_utf8_lines,
            server_output_lines: self
                .server_output_lines
                .map_or(std::ptr::null_mut(), Box::into_raw),
            transaction_possibly_lost: self.transaction_possibly_lost,
            has_deadline: self.deadline_ms.is_some(),
            transaction_possibly_active: self.transaction_possibly_active,
            abandoned: self.abandoned,
            deadline_ms: self.deadline_ms.unwrap_or(0),
            executing_request: self.executing_request,
            ..ReldexEvent::default()
        }
    }
}
