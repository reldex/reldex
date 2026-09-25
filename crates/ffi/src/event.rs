//! What a completed request looks like on the way out (ADR-0003 D5).
//!
//! One flat `#[repr(C)]` struct rather than a tagged union: the adapter
//! switches on `kind` and reads the fields that kind documents. A union would
//! save a few dozen bytes per event and cost every C++ reader a cast.

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
    /// Unsolicited: lines the server produced out of band (M2.7), collected
    /// after the statement that produced them and delivered before that
    /// statement's own `Executed`/`Completed`/`SessionClosed` reply. See
    /// `server_output_lines`, `server_output_dropped` and
    /// `server_output_invalid_utf8_lines`; `error` carries a failed read.
    ///
    /// M2.11 delivers this only on the **completion path**
    /// (`reldex-db-core`'s `DatabaseSession::take_server_output`, drained
    /// after every reply while output is on) — never as a fully unsolicited,
    /// mid-statement event, which needs the event-queue switch this crate has
    /// not made yet (see the crate's module documentation, "What is
    /// interim here"). A caller sees a session's output attributed to the
    /// request whose reply immediately follows it, which is correct for
    /// every case except the two rare mid-statement exceptions
    /// `docs/exec-plans/active/phase-1-m2-5-event-queue.md` §7.5 documents.
    ServerOutput = 8,
    /// The session ended. Delivered for a connect that failed
    /// (`RELDEX_EVENT_KIND_OPENED` with an error) and for a
    /// `reldex_session_close` that actually closed the session — **not**
    /// for the hub being destroyed while a session's statement cannot be
    /// interrupted (ADR-0003 A17: the caller has already released its hub
    /// pointer by then, so nothing could observe it). Read
    /// `transaction_possibly_lost` (`SPEC.md` §10: never hide a transaction
    /// loss).
    Terminal = 9,
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

/// One reply, filled in by [`crate::reldex_hub_next_event`].
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
    /// The `request_id` the caller passed to the submitting call.
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
    /// previous delivered `ServerOutput`, because the session's completion-path
    /// log was full (`reldex-db-core`'s `ServerOutputLog::MAX_RETAINED_LINES` /
    /// `MAX_RETAINED_BYTES`). Zero normally; non-zero means the UI must say
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

    /// `ServerOutput`: the lines collected on the completion path, plus what
    /// was dropped or failed.
    pub(crate) fn with_server_output_log(mut self, log: reldex_db_core::ServerOutputLog) -> Self {
        self.server_output_dropped = log.dropped;
        self.server_output_invalid_utf8_lines = log.invalid_utf8_lines;
        if !log.lines.is_empty() {
            self.server_output_lines = Some(Box::new(ReldexServerOutputLines::new(log.lines)));
        }
        if let Some(failure) = log.failure {
            self.error = Some(Box::new(ReldexError::from_db_error(&failure)));
        }
        self
    }

    /// `Terminal`: whether the session ended with a transaction possibly
    /// still unresolved.
    pub(crate) const fn with_transaction_possibly_lost(mut self, lost: bool) -> Self {
        self.transaction_possibly_lost = lost;
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
            ..ReldexEvent::default()
        }
    }
}
