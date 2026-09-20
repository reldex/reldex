//! What a completed request looks like on the way out (ADR-0003 D5).
//!
//! One flat `#[repr(C)]` struct rather than a tagged union: the adapter
//! switches on `kind` and reads the fields that kind documents. A union would
//! save a few dozen bytes per event and cost every C++ reader a cast.

use reldex_db_core::{CloseError, ExecuteOutcome, StatementKind};

use crate::batch::ReldexBatch;
use crate::error::{ReldexError, ReldexSessionState};
use crate::strings::CStruct;

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
    /// `Executed`: the result set's id, when `has_result`.
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
    /// `Executed`: how many columns the result has; read their names with
    /// `reldex_batch_column_info` on any batch from it.
    pub column_count: u32,
    /// `Opened`: how many connect-time warnings the session reported. Read
    /// them with [`crate::reldex_session_connect_warnings`].
    pub warning_count: u32,
    /// `Executed`: whether the statement produced a result set.
    pub has_result: bool,
    /// `Executed`: whether `rows_affected` is meaningful.
    pub has_rows_affected: bool,
    /// `Executed`: whether the server committed the transaction as a side
    /// effect, whatever the client asked for (typically DDL). Reldex could
    /// neither prevent nor undo it; the user must be told.
    pub committed_implicitly: bool,
    /// `SessionClosed`: whether the session is still open and usable.
    pub session_still_open: bool,
}

// SAFETY: `#[repr(C)]`, `struct_size` first, every field an integer, a `bool`
// or a raw pointer — all valid as zero.
unsafe impl CStruct for ReldexEvent {
    const MIN_SIZE: usize = size_of::<Self>();
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
    pub(crate) column_count: u32,
    pub(crate) warning_count: u32,
    pub(crate) committed_implicitly: bool,
    pub(crate) session_still_open: bool,
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
        }
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

    /// Sets the lifecycle state reported on the event. Called last, because
    /// the session registry knows more than any single error does.
    pub(crate) const fn with_session_state(mut self, state: ReldexSessionState) -> Self {
        self.session_state = state;
        self
    }

    pub(crate) fn with_execute_outcome(
        mut self,
        outcome: &ExecuteOutcome,
        result: Option<u64>,
    ) -> Self {
        self.result = result;
        self.rows_affected = outcome.rows_affected;
        self.statement_kind = outcome.statement_kind.into();
        self.committed_implicitly = outcome.committed_implicitly;
        self.column_count = u32::try_from(outcome.columns.len()).unwrap_or(u32::MAX);
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
            ..ReldexEvent::default()
        }
    }
}
