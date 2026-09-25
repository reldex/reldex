//! [`ResultStore`]: one open result's retained prefix, its fetch policy and
//! its honest state (ADR-0004 RS1–RS3).
//!
//! # Where it lives and who touches it
//!
//! The store is consumer-side plain data. It is built from an
//! [`crate::ExecuteOutcome`] and fed the replies to the fetches it asked for,
//! on the thread that drains the session's events — in the product the Qt
//! main thread, through `crates/ffi`'s hub (ADR-0004 RS1, RS5 "Threads"). It
//! never blocks and never talks to a database: it decides *what* to fetch,
//! and hands each decision to its caller ([`ResultStore::pump`]), which
//! submits it. The expensive part — turning a batch into a compact
//! [`ResultSegment`] — has already happened on the session's worker thread by
//! the time a reply reaches it, so appending is a pointer push.
//!
//! # The fetch policy (ADR-0004 RS2)
//!
//! 1. The first fetch is due at once, whatever the demand, sized by the
//!    describe's declared column widths against the round-trip budget. No
//!    other fetch is submitted until it is answered, so only one request is
//!    ever sized blind, and every later one by the width observed.
//! 2. After that the store keeps one fetch of read-ahead: a fetch is due
//!    while `retained + requested-and-unanswered < demand + rows per round
//!    trip`, with at most `fetches_in_flight` outstanding, and only inside
//!    the caps. Rows per round trip is
//!    `clamp(budget / row width, 1, fetch_rows)`, where the row width is the
//!    declared one until rows arrive and then the observed average, never
//!    above the declared one.
//! 3. [`ResultStore::fetch_all`] streams to the caps.
//! 4. [`ResultStore::stop`] stops *submitting*, at once and until
//!    [`ResultStore::resume`], [`ResultStore::fetch_all`] or
//!    [`ResultStore::fetch_more`]. Fetches already submitted still run and
//!    their rows are kept. It is not a cancel.
//!
//! **What the round-trip budget does not bound yet** (ADR-0004 accepted
//! limitation 11). It sizes each *request* — how many rows the store asks
//! for, and therefore each segment and the byte cap's overshoot. It cannot
//! size the driver's *wire* array: `oracle-thin` fixes that at execute, from
//! `Statement::with_fetch_rows`, before the describe, and `oracledb` has no
//! setter after it. A request smaller than the wire array is served from rows
//! the driver has already received, so on Oracle 19c a round trip still costs
//! what `results.fetch_rows` rows cost (ADR-0004 Table 3a).
//!
//! # Caps (ADR-0004 RS3)
//!
//! The row cap is exact: the request that would reach it asks for one row
//! more, kept as a lookahead that is counted in bytes and not shown, so the
//! store knows whether more rows exist. The byte cap is checked before every
//! submit and requests shrink to fit it; what fetches already in flight carry
//! when it is reached is the overshoot ADR-0004 accepted limitation 3 bounds.
//!
//! # Replies and sequence
//!
//! Every fetch carries a per-result sequence number ([`FetchTicket`]). A
//! session's worker runs commands in order and its replies come back in
//! production order, so replies arrive in the order they were requested; one
//! that does not is a core bug — asserted in a debug build, reported as the
//! result's failure in a release build, never appended. A reply that arrives
//! after the store stopped accepting rows — it failed, ended or was
//! discarded — is **stale**: counted off and dropped ([`Fetched::Stale`]).
//!
//! # Every transaction end ends the store (ADR-0004 RS2, ADR-0002 X1)
//!
//! A successful commit, rollback or rollback-to-savepoint command, a
//! successful typed `TransactionControl` statement, and a statement that
//! committed implicitly all release the session's cursors and parked LOBs on
//! the worker. The store follows: no further fetch, the prefix kept, LOB
//! cells unavailable, state [`EndCause::TransactionEnded`]. Production order
//! delivers the reply that ends it before the reply of any fetch submitted
//! after it, so that fetch's "invalidated handle" error is stale, never a
//! failure.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::Arc;

use reldex_db_driver_api::{ColumnMetadata, DbError, DbResult, StatementKind};

use crate::events::RequestId;
use crate::ids::{LobHandle, ResultId};
use crate::session::{DatabaseSession, ExecuteOutcome};
use crate::store::policy::{GRID_ROW_CEILING, ResultCaps, ResultPolicy, declared_row_width};
use crate::store::segment::{CellValue, ResultSegment, SegmentData};

/// Which fetch of which result a reply answers: the result and the
/// per-result sequence number the store gave the fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FetchTicket {
    result: ResultId,
    sequence: u64,
}

impl FetchTicket {
    /// The result.
    #[must_use]
    pub const fn result(self) -> ResultId {
        self.result
    }

    /// The fetch's place in the result's sequence, from 0.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

/// One fetch a store decided on: submit it with
/// [`DatabaseSession::submit_fetch_segment`] (or
/// [`DatabaseSession::fetch_segment`]). Only a store makes one, so a fetch
/// always carries the sequence number its store expects back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FetchRequest {
    ticket: FetchTicket,
    max_rows: NonZeroUsize,
}

impl FetchRequest {
    /// The ticket its reply will carry.
    #[must_use]
    pub const fn ticket(self) -> FetchTicket {
        self.ticket
    }

    /// The result to fetch from.
    #[must_use]
    pub const fn result(self) -> ResultId {
        self.ticket.result
    }

    /// The most rows to fetch.
    #[must_use]
    pub const fn max_rows(self) -> NonZeroUsize {
        self.max_rows
    }
}

/// Something a store needs its caller to submit on its session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StoreAction {
    /// Fetch the next segment.
    Fetch(FetchRequest),
    /// Close the result's cursor: it reached a cap with
    /// `results.close_cursor_at_limit` on (ADR-0004 RS3).
    CloseResult(ResultId),
}

/// The reply to one segment fetch.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SegmentReply {
    /// The rows, compacted on the worker. Empty when the result was already
    /// exhausted.
    pub segment: Arc<ResultSegment>,
    /// Whether the result is known to hold no further row: the segment was
    /// empty, or the driver already knew it had read the last one.
    pub exhausted: bool,
}

impl SegmentReply {
    pub(crate) const fn new(segment: Arc<ResultSegment>, exhausted: bool) -> Self {
        Self { segment, exhausted }
    }
}

/// Which cap a result stopped at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LimitKind {
    /// `results.max_rows`.
    Rows,
    /// `results.max_bytes`.
    Bytes,
    /// The grid's own ceiling ([`crate::GRID_ROW_CEILING`]), reached with
    /// the row cap at "no limit" or above it.
    RowCeiling,
}

/// Whether a result stopped at a cap has more rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MoreRows {
    /// It does: the store holds one row past the cap (the row cap's
    /// lookahead).
    Yes,
    /// Nobody knows without fetching: the byte cap stopped it.
    Unknown,
}

/// Why no further fetch will happen for a result that did not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EndCause {
    /// The session was lost, closed or abandoned.
    SessionEnded,
    /// A commit, rollback, rollback to a savepoint, typed transaction-control
    /// statement or implicitly committing statement ended the transaction,
    /// and with it every cursor of the session.
    TransactionEnded,
    /// The result was discarded ([`ResultStore::discard`]).
    Discarded,
}

/// Why a large-object cell can no longer be read. Never NULL: the database
/// held a value there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LobUnavailable {
    /// The transaction ended, and every parked LOB of the session with it.
    TransactionEnded,
    /// The session ended.
    SessionEnded,
    /// The result's cursor was closed — at a cap, after a failed fetch, or
    /// because the result was discarded — releasing its LOBs.
    ResultClosed,
}

/// One large-object cell, as the store can serve it now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LobCell {
    /// Parked on the session's worker; read it with
    /// [`DatabaseSession::read_lob_chunk`].
    Readable(LobHandle),
    /// No longer readable, and why.
    Unavailable(LobUnavailable),
}

/// Where a result stands. See [`ResultState`].
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum ResultPhase<'a> {
    /// A fetch is outstanding.
    Fetching,
    /// The cursor is open and idle; more rows may exist. Scrolling (demand),
    /// "Fetch all" or [`ResultStore::resume`] after a stop fetches more.
    Open,
    /// The cursor is exhausted: every row of the result is in the store.
    Complete,
    /// Stopped at a cap.
    AtLimit {
        /// Which cap.
        limit: LimitKind,
        /// Whether more rows exist.
        more: MoreRows,
        /// Whether the cursor is still open, so "Fetch more"
        /// ([`ResultStore::fetch_more`]) can continue it. `false` with
        /// `results.close_cursor_at_limit` on: only running the query again
        /// continues, on a new snapshot.
        cursor_open: bool,
    },
    /// A fetch failed after `after` rows. The rows are kept; the result is
    /// incomplete.
    Failed {
        /// How many rows were fetched before the failure.
        after: usize,
        /// Why, with the driver's classification and native code.
        error: &'a DbError,
    },
    /// No further fetch will happen, and the result is not complete.
    Ended {
        /// Why.
        cause: EndCause,
    },
}

/// A result's honest state: where it stands, how much it holds, and the caps
/// in force with their provenance (ADR-0004 RS2). Typed numbers and enums
/// only; the UI composes and translates the sentence (M6.3).
#[derive(Debug, Clone, Copy)]
pub struct ResultState<'a> {
    phase: ResultPhase<'a>,
    rows: usize,
    retained_rows: usize,
    retained_bytes: usize,
    caps: ResultCaps,
    fetches_in_flight: usize,
    stopped: bool,
    lobs: Option<LobUnavailable>,
}

impl<'a> ResultState<'a> {
    /// Where the result stands.
    #[must_use]
    pub const fn phase(&self) -> ResultPhase<'a> {
        self.phase
    }

    /// The rows the result shows: every retained row, except the row cap's
    /// lookahead.
    #[must_use]
    pub const fn rows(&self) -> usize {
        self.rows
    }

    /// The rows the store holds, the lookahead included.
    #[must_use]
    pub const fn retained_rows(&self) -> usize {
        self.retained_rows
    }

    /// The bytes the store holds, as the byte cap counts them: every
    /// segment's accounted bytes and the store's own index (ADR-0004 RS3).
    #[must_use]
    pub const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// The caps in force, with where each came from.
    #[must_use]
    pub const fn caps(&self) -> ResultCaps {
        self.caps
    }

    /// How many fetches are outstanding.
    #[must_use]
    pub const fn fetches_in_flight(&self) -> usize {
        self.fetches_in_flight
    }

    /// Whether "Stop fetching" is in force.
    #[must_use]
    pub const fn stopped(&self) -> bool {
        self.stopped
    }

    /// Why the result's LOB cells can no longer be read, if they cannot.
    #[must_use]
    pub const fn lobs_unavailable(&self) -> Option<LobUnavailable> {
        self.lobs
    }
}

/// What became of one reply handed to [`ResultStore::on_fetched`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Fetched {
    /// The rows were appended (possibly none, for an exhausted result).
    Appended {
        /// How many.
        rows: usize,
    },
    /// The fetch failed; the store is now failed.
    Failed,
    /// The reply arrived after the store stopped accepting rows — it had
    /// failed, ended or been discarded — and was dropped.
    Stale,
    /// The reply is for another result. Nothing changed.
    NotThisResult,
    /// The reply is not the one the store expected next. A core bug:
    /// asserted in a debug build; in a release build the store fails rather
    /// than append rows out of order.
    OutOfSequence,
}

/// Whether "Fetch more" raises the caps by one more step or lifts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FetchMore {
    /// One more step of the same size as each cap's setting.
    Step,
    /// No limit, for this result only.
    Unlimited,
}

/// One fetch submitted and not yet answered.
#[derive(Debug, Clone, Copy)]
struct InFlight {
    sequence: u64,
    rows: usize,
    bytes: usize,
}

#[derive(Debug)]
enum Phase {
    Active,
    AtLimit { limit: LimitKind, more: MoreRows },
    Complete,
    Failed { after: usize, error: DbError },
    Ended(EndCause),
}

/// One open result's retained prefix, fetch policy and state. See the module
/// documentation.
#[derive(Debug)]
pub struct ResultStore {
    result: ResultId,
    columns: Box<[ColumnMetadata]>,
    policy: ResultPolicy,
    caps: ResultCaps,
    declared_width: usize,

    segments: Vec<Arc<ResultSegment>>,
    /// The first row of each segment.
    starts: Vec<usize>,
    /// Whether every segment between the first and the last holds as many
    /// rows as the second, which makes finding a row a division (ADR-0004
    /// accepted limitation 10). The first is exempt: it is sized blind, by
    /// declared widths, and commonly differs from the rest.
    uniform: bool,
    retained_rows: usize,
    segment_bytes: usize,

    next_sequence: u64,
    in_flight: VecDeque<InFlight>,
    in_flight_rows: usize,
    in_flight_bytes: usize,
    first_submitted: bool,

    demand: usize,
    fetch_all: bool,
    stopped: bool,
    transaction_ends_pending: usize,
    exhausted: bool,
    phase: Phase,
    close_due: bool,
    cursor_closed: bool,
    lobs: Option<LobUnavailable>,
}

impl ResultStore {
    /// A store for `result`, whose select list is `columns` (from the same
    /// [`ExecuteOutcome`]), running under `policy`. Its first fetch is due at
    /// once ([`ResultStore::pump`]).
    #[must_use]
    pub fn new(result: ResultId, columns: &[ColumnMetadata], policy: ResultPolicy) -> Self {
        Self {
            result,
            declared_width: declared_row_width(columns),
            columns: columns.into(),
            caps: policy.caps(),
            policy,
            segments: Vec::new(),
            starts: Vec::new(),
            uniform: true,
            retained_rows: 0,
            segment_bytes: 0,
            next_sequence: 0,
            in_flight: VecDeque::new(),
            in_flight_rows: 0,
            in_flight_bytes: 0,
            first_submitted: false,
            demand: 0,
            fetch_all: false,
            stopped: false,
            transaction_ends_pending: 0,
            exhausted: false,
            phase: Phase::Active,
            close_due: false,
            cursor_closed: false,
            lobs: None,
        }
    }

    /// The result this store holds.
    #[must_use]
    pub const fn result(&self) -> ResultId {
        self.result
    }

    /// The select list.
    #[must_use]
    pub fn columns(&self) -> &[ColumnMetadata] {
        &self.columns
    }

    /// The policy the store was opened with. The caps in force can differ
    /// after "Fetch more"; see [`ResultState::caps`].
    #[must_use]
    pub const fn policy(&self) -> &ResultPolicy {
        &self.policy
    }

    // ---------------------------------------------------------------- state

    /// The result's state now.
    #[must_use]
    pub fn state(&self) -> ResultState<'_> {
        let phase = match &self.phase {
            Phase::Active if self.in_flight.is_empty() => ResultPhase::Open,
            Phase::Active => ResultPhase::Fetching,
            Phase::AtLimit { limit, more } => ResultPhase::AtLimit {
                limit: *limit,
                more: *more,
                cursor_open: !self.cursor_closed && !self.close_due,
            },
            Phase::Complete => ResultPhase::Complete,
            Phase::Failed { after, error } => ResultPhase::Failed {
                after: *after,
                error,
            },
            Phase::Ended(cause) => ResultPhase::Ended { cause: *cause },
        };
        ResultState {
            phase,
            rows: self.row_count(),
            retained_rows: self.retained_rows,
            retained_bytes: self.retained_bytes(),
            caps: self.caps,
            fetches_in_flight: self.in_flight.len(),
            stopped: self.stopped,
            lobs: self.lobs,
        }
    }

    /// The rows the result shows: every retained row except the row cap's
    /// lookahead.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.retained_rows.min(self.row_limit())
    }

    /// The bytes the store holds, as the byte cap counts them.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.segment_bytes
            + self.segments.capacity() * size_of::<Arc<ResultSegment>>()
            + self.starts.capacity() * size_of::<usize>()
    }

    /// Whether finding a row is a division (every segment between the first
    /// and the last holds the same number of rows) rather than a binary
    /// search over the segments' first rows (ADR-0004 accepted limitation
    /// 10). The first segment may differ: the first request is sized by the
    /// describe's declared widths, the rest by the rows observed.
    #[must_use]
    pub const fn lookup_is_constant_time(&self) -> bool {
        self.uniform
    }

    // ----------------------------------------------------------------- reads

    /// The segment holding `row`, and that segment's first row. `None` past
    /// the rows the result shows.
    ///
    /// O(1) while [`ResultStore::lookup_is_constant_time`], O(log segments)
    /// otherwise.
    #[must_use]
    pub fn segment_for_row(&self, row: usize) -> Option<(&Arc<ResultSegment>, usize)> {
        if row >= self.row_count() {
            return None;
        }
        let index = self.segment_index(row)?;
        Some((self.segments.get(index)?, *self.starts.get(index)?))
    }

    /// Every segment, in row order, with its first row.
    pub fn segments(&self) -> impl Iterator<Item = (&Arc<ResultSegment>, usize)> {
        self.segments.iter().zip(self.starts.iter().copied())
    }

    /// Reads one cell of the rows the result shows, or `None` out of range.
    ///
    /// Allocation-free. A LOB cell reads as
    /// [`CellValue::LobUnavailable`] once its transaction, its session or its
    /// cursor has ended — never as NULL.
    #[must_use]
    pub fn value(&self, row: usize, column: usize) -> Option<CellValue<'_>> {
        let (segment, first) = self.segment_for_row(row)?;
        let cell = segment.value(row - first, column)?;
        Some(match (cell, self.lobs) {
            (CellValue::Lob(_), Some(reason)) => CellValue::LobUnavailable(reason),
            (cell, _) => cell,
        })
    }

    /// The large object at `(row, column)`, as it can be served now. `None`
    /// for a cell that is NULL, not a LOB, or out of range.
    #[must_use]
    pub fn lob(&self, row: usize, column: usize) -> Option<LobCell> {
        let (segment, first) = self.segment_for_row(row)?;
        let data = segment.column(column)?;
        if !matches!(data.data(), SegmentData::Lob(_)) || data.is_null(row - first) {
            return None;
        }
        match (segment.value(row - first, column)?, self.lobs) {
            (CellValue::Lob(_), Some(reason)) => Some(LobCell::Unavailable(reason)),
            (CellValue::Lob(handle), None) => Some(LobCell::Readable(handle)),
            _ => None,
        }
    }

    fn segment_index(&self, row: usize) -> Option<usize> {
        if row >= self.retained_rows {
            return None;
        }
        let last = self.segments.len().checked_sub(1)?;
        if self.uniform {
            let first = self.segments.first()?.row_count();
            if row < first {
                return Some(0);
            }
            let rows = self.segments.get(1)?.row_count();
            return Some((1 + (row - first) / rows).min(last));
        }
        // How many segments start at or before `row`, less one.
        self.starts
            .partition_point(|start| *start <= row)
            .checked_sub(1)
    }

    // ---------------------------------------------------------------- inputs

    /// The view's demand: how many rows it wants resident, typically the last
    /// visible row plus one. Never blocks and never fetches by itself;
    /// [`ResultStore::pump`] submits what it made due. Does not undo a
    /// [`ResultStore::stop`].
    pub fn set_demand(&mut self, rows: usize) {
        self.demand = rows;
    }

    /// "Fetch all": stream to the caps. Clears a stop.
    pub fn fetch_all(&mut self) {
        self.fetch_all = true;
        self.stopped = false;
    }

    /// "Stop fetching": submit nothing more until [`ResultStore::resume`],
    /// [`ResultStore::fetch_all`] or [`ResultStore::fetch_more`]. Takes effect
    /// at once. Fetches already submitted still run, and their rows are kept:
    /// this is **not** a cancel and must never be presented as one.
    pub fn stop(&mut self) {
        self.stopped = true;
        self.fetch_all = false;
    }

    /// Undoes a stop: fetching follows the demand again.
    pub fn resume(&mut self) {
        self.stopped = false;
    }

    /// "Fetch more" at a cap: raises this result's caps by one more step of
    /// the same size, or lifts them, for this result only, and continues the
    /// same cursor. Returns whether it applied: only a result stopped at a
    /// cap with its cursor still open can continue.
    pub fn fetch_more(&mut self, more: FetchMore) -> bool {
        if !matches!(self.phase, Phase::AtLimit { .. }) || self.cursor_closed || self.close_due {
            return false;
        }
        self.caps = match more {
            FetchMore::Step => self.caps.raised_by(&self.policy.caps()),
            FetchMore::Unlimited => self.caps.lifted(),
        };
        self.phase = Phase::Active;
        self.stopped = false;
        self.settle();
        true
    }

    /// Discards the result: the store takes no further rows, its LOB cells
    /// become unavailable, and replies still in flight are dropped as they
    /// arrive. The caller then submits `close_result` for it
    /// ([`DatabaseSession::submit_close_result`]) — which is also where every
    /// pointer into its segments stops being promised (ADR-0003 A20) — and
    /// drops the store.
    pub fn discard(&mut self) {
        self.phase = Phase::Ended(EndCause::Discarded);
        self.close_due = false;
        self.cursor_closed = true;
        self.lobs = Some(self.lobs.unwrap_or(LobUnavailable::ResultClosed));
    }

    /// A commit, rollback or rollback-to-savepoint **command** was submitted
    /// on the store's session: stop submitting until it is answered
    /// ([`ResultStore::transaction_end_answered`]). Fetches already submitted
    /// run first and are kept. Announce only a command the session accepted:
    /// see [`crate::SessionResults::transaction_end_submitted`].
    pub fn transaction_end_submitted(&mut self) {
        self.transaction_ends_pending += 1;
    }

    /// The answer to a command [`ResultStore::transaction_end_submitted`]
    /// announced: on success the store ends
    /// ([`EndCause::TransactionEnded`]); on failure the worker released
    /// nothing and the store resumes.
    pub fn transaction_end_answered(&mut self, succeeded: bool) {
        self.transaction_ends_pending = self.transaction_ends_pending.saturating_sub(1);
        if succeeded {
            self.end_transaction();
        }
    }

    /// Another statement of the store's session was executed — never the one
    /// that produced this result. A statement that ended the transaction (a
    /// typed `TransactionControl`, or one that committed implicitly) ends the
    /// store, exactly as the worker released its cursor (ADR-0002 X1).
    pub fn statement_executed(&mut self, outcome: &ExecuteOutcome) {
        if outcome.committed_implicitly
            || outcome.statement_kind == StatementKind::TransactionControl
        {
            self.end_transaction();
        }
    }

    /// The store's session ended — closed, lost or abandoned. The prefix
    /// stays readable (ADR-0004 RS2); LOB cells become unavailable.
    pub fn session_ended(&mut self) {
        self.lobs = Some(self.lobs.unwrap_or(LobUnavailable::SessionEnded));
        self.cursor_closed = true;
        self.close_due = false;
        if matches!(self.phase, Phase::Active | Phase::AtLimit { .. }) {
            self.phase = Phase::Ended(EndCause::SessionEnded);
        }
    }

    fn end_transaction(&mut self) {
        self.lobs = Some(self.lobs.unwrap_or(LobUnavailable::TransactionEnded));
        let cursor_was_open = !self.cursor_closed && !self.close_due;
        // The worker has released the cursor; there is nothing left to close.
        self.cursor_closed = true;
        self.close_due = false;
        match self.phase {
            Phase::Active => self.phase = Phase::Ended(EndCause::TransactionEnded),
            // "Keep the cursor open" lasts only until the transaction ends.
            // A result whose cursor was already closed at its cap keeps
            // saying which cap stopped it.
            Phase::AtLimit { .. } if cursor_was_open => {
                self.phase = Phase::Ended(EndCause::TransactionEnded);
            }
            _ => {}
        }
    }

    // --------------------------------------------------------------- fetching

    /// Hands every action now due to `submit`, in order, and returns how many
    /// it took. Stops at the first `submit` that fails, leaving that action
    /// due, and returns its error; nothing is recorded for an action that was
    /// not submitted.
    ///
    /// # Errors
    ///
    /// Whatever `submit` returned — typically
    /// [`reldex_db_driver_api::ErrorKind::Resource`] when the session's
    /// outstanding-request limit is reached. Pump again after draining.
    pub fn pump(&mut self, mut submit: impl FnMut(StoreAction) -> DbResult<()>) -> DbResult<usize> {
        let mut submitted = 0;
        while let Some(action) = self.due() {
            submit(action)?;
            self.record(action);
            submitted += 1;
        }
        Ok(submitted)
    }

    /// [`ResultStore::pump`] onto `session`'s event path, taking a fresh
    /// request id from `next_request` for each action.
    ///
    /// # Errors
    ///
    /// As [`ResultStore::pump`].
    pub fn submit_events(
        &mut self,
        session: &DatabaseSession,
        mut next_request: impl FnMut() -> RequestId,
    ) -> DbResult<usize> {
        self.pump(|action| match action {
            StoreAction::Fetch(fetch) => session.submit_fetch_segment(next_request(), fetch),
            StoreAction::CloseResult(result) => session.submit_close_result(next_request(), result),
        })
    }

    /// The next action due, without recording it.
    fn due(&self) -> Option<StoreAction> {
        if self.close_due {
            return Some(StoreAction::CloseResult(self.result));
        }
        if !matches!(self.phase, Phase::Active)
            || self.stopped
            || self.exhausted
            || self.transaction_ends_pending > 0
            || self.in_flight.len() >= self.policy.fetches_in_flight().get()
            // Until the first reply there is no observed width, and a second
            // request would be sized blind too.
            || (self.retained_rows == 0 && !self.in_flight.is_empty())
        {
            return None;
        }
        let max_rows = self.next_request_rows()?;
        Some(StoreAction::Fetch(FetchRequest {
            ticket: FetchTicket {
                result: self.result,
                sequence: self.next_sequence,
            },
            max_rows,
        }))
    }

    /// Records an action its caller submitted.
    fn record(&mut self, action: StoreAction) {
        match action {
            StoreAction::Fetch(fetch) => {
                let rows = fetch.max_rows.get();
                let bytes = rows.saturating_mul(self.row_width());
                self.in_flight.push_back(InFlight {
                    sequence: fetch.ticket.sequence,
                    rows,
                    bytes,
                });
                self.in_flight_rows += rows;
                self.in_flight_bytes = self.in_flight_bytes.saturating_add(bytes);
                self.next_sequence += 1;
                self.first_submitted = true;
            }
            StoreAction::CloseResult(_) => {
                self.close_due = false;
                self.cursor_closed = true;
                self.lobs = Some(self.lobs.unwrap_or(LobUnavailable::ResultClosed));
            }
        }
    }

    /// The row cap in force, bounded by the grid's ceiling.
    fn row_limit(&self) -> usize {
        self.caps
            .max_rows()
            .value
            .limit()
            .map_or(GRID_ROW_CEILING, |rows| rows.min(GRID_ROW_CEILING))
    }

    /// The width a row is assumed to take: declared until rows arrive, then
    /// the observed average, never above the declared one (ADR-0004 RS2).
    fn row_width(&self) -> usize {
        if self.retained_rows == 0 {
            return self.declared_width;
        }
        (self.segment_bytes / self.retained_rows).clamp(1, self.declared_width)
    }

    /// Rows per round trip: `clamp(budget / row width, 1, fetch_rows)`.
    fn rows_per_round_trip(&self) -> usize {
        (self.policy.round_trip_bytes().get() / self.row_width())
            .clamp(1, self.policy.fetch_rows().get())
    }

    fn next_request_rows(&self) -> Option<NonZeroUsize> {
        let per_trip = self.rows_per_round_trip();
        let pending = self.retained_rows + self.in_flight_rows;
        let first = !self.first_submitted;
        if !first {
            let target = if self.fetch_all {
                usize::MAX
            } else {
                self.demand.saturating_add(per_trip)
            };
            if pending >= target {
                return None;
            }
        }
        // One row past the cap: the lookahead that says whether more exist.
        let row_room = self.row_limit().saturating_add(1).saturating_sub(pending);
        let byte_room = match self.caps.max_bytes().value.limit() {
            None => usize::MAX,
            Some(cap) => {
                cap.saturating_sub(self.retained_bytes().saturating_add(self.in_flight_bytes))
                    / self.row_width()
            }
        };
        let rows = per_trip.min(row_room).min(byte_room);
        // A cap smaller than one declared row must not stop a result before
        // its first row: the first request always asks for one.
        NonZeroUsize::new(rows).or_else(|| if first { NonZeroUsize::new(1) } else { None })
    }

    // ---------------------------------------------------------------- replies

    /// Takes the reply to one of this store's fetches. See [`Fetched`].
    pub fn on_fetched(&mut self, fetch: FetchTicket, reply: DbResult<SegmentReply>) -> Fetched {
        if fetch.result != self.result {
            return Fetched::NotThisResult;
        }
        let expected = self.in_flight.front().map(|entry| entry.sequence);
        if expected != Some(fetch.sequence) {
            if cfg!(debug_assertions) {
                panic!(
                    "reldex-db-core: {} answered fetch #{} while #{expected:?} was expected; a \
                     session's replies arrive in production order (ADR-0004 RS2)",
                    self.result, fetch.sequence
                );
            }
            if let Some(position) = self
                .in_flight
                .iter()
                .position(|entry| entry.sequence == fetch.sequence)
                && let Some(entry) = self.in_flight.remove(position)
            {
                self.forget(entry);
            }
            if matches!(self.phase, Phase::Active | Phase::AtLimit { .. }) {
                self.phase = Phase::Failed {
                    after: self.row_count(),
                    error: DbError::internal(format!(
                        "reldex-db-core: {} received fetch #{} out of sequence; the rows were \
                         not appended, and the result is incomplete",
                        self.result, fetch.sequence
                    )),
                };
            }
            return Fetched::OutOfSequence;
        }
        if let Some(entry) = self.in_flight.pop_front() {
            self.forget(entry);
        }
        if !matches!(self.phase, Phase::Active) {
            return Fetched::Stale;
        }
        let outcome = match reply {
            Err(error) => {
                // The worker closed the cursor after the failed fetch
                // (ADR-0002 D2), and its LOBs with it.
                self.lobs = Some(self.lobs.unwrap_or(LobUnavailable::ResultClosed));
                self.cursor_closed = true;
                self.phase = Phase::Failed {
                    after: self.row_count(),
                    error,
                };
                return Fetched::Failed;
            }
            Ok(reply) => reply,
        };
        let segment = outcome.segment;
        let rows = segment.row_count();
        if rows > 0 && segment.column_count() != self.columns.len() {
            self.phase = Phase::Failed {
                after: self.row_count(),
                error: DbError::internal(format!(
                    "reldex-db-core: a segment of {} has {} columns where the result has {}",
                    self.result,
                    segment.column_count(),
                    self.columns.len()
                )),
            };
            return Fetched::Failed;
        }
        if outcome.exhausted {
            self.exhausted = true;
        }
        if rows > 0 {
            self.append(segment);
        }
        self.settle();
        Fetched::Appended { rows }
    }

    fn forget(&mut self, entry: InFlight) {
        self.in_flight_rows -= entry.rows;
        self.in_flight_bytes = self.in_flight_bytes.saturating_sub(entry.bytes);
    }

    fn append(&mut self, segment: Arc<ResultSegment>) {
        // The segment that was last is about to become one that is not. If
        // it is not the first, and it is shorter or longer than the second,
        // rows no longer map to segments by division.
        if self.segments.len() >= 3
            && let (Some(second), Some(last)) = (self.segments.get(1), self.segments.last())
            && second.row_count() != last.row_count()
        {
            self.uniform = false;
        }
        self.starts.push(self.retained_rows);
        self.retained_rows += segment.row_count();
        self.segment_bytes += segment.accounted_bytes();
        self.segments.push(segment);
    }

    /// Moves an active store to its resting phase once nothing is in flight.
    fn settle(&mut self) {
        if !matches!(self.phase, Phase::Active) || !self.in_flight.is_empty() {
            return;
        }
        let row_limit = self.row_limit();
        self.phase = if self.retained_rows > row_limit {
            let from_ceiling = self
                .caps
                .max_rows()
                .value
                .limit()
                .is_none_or(|rows| rows > GRID_ROW_CEILING);
            Phase::AtLimit {
                limit: if from_ceiling {
                    LimitKind::RowCeiling
                } else {
                    LimitKind::Rows
                },
                more: MoreRows::Yes,
            }
        } else if self.exhausted {
            Phase::Complete
        } else if self
            .caps
            .max_bytes()
            .value
            .limit()
            .is_some_and(|cap| self.retained_bytes().saturating_add(self.row_width()) > cap)
        {
            Phase::AtLimit {
                limit: LimitKind::Bytes,
                more: MoreRows::Unknown,
            }
        } else {
            return;
        };
        if matches!(self.phase, Phase::AtLimit { .. })
            && self.caps.close_cursor_at_limit().value
            && !self.cursor_closed
        {
            self.close_due = true;
        }
    }
}

#[cfg(test)]
mod tests;
