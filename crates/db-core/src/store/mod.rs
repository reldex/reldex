//! The Result Store (ADR-0004): the fetched prefix of each open result,
//! retained as immutable compacted segments, fetched on demand, bounded by
//! per-result row and byte caps, with a typed, honest state.
//!
//! * [`ResultSegment`] — one fetched batch, compacted on the session's worker
//!   thread (RS1).
//! * [`ResultStore`] — one result: its segments, its fetch policy and its
//!   [`ResultState`] (RS2, RS3).
//! * [`SessionResults`] — every store of one session, and the rules that
//!   end them when the transaction or the session ends.
//! * [`ScaledNumber`] / [`NumberValue`] — a `NUMBER` kept as a scaled `i64`,
//!   formatting byte-identically to [`reldex_db_driver_api::Number`].
//!
//! Spill, eviction and Arrow are deferred to Phase 3 (RS4): nothing here
//! ever drops a retained row.

mod policy;
mod result_store;
mod scaled;
mod segment;
mod session_results;

pub use policy::{
    Cap, CapSource, DEFAULT_FETCHES_IN_FLIGHT, DEFAULT_ROUND_TRIP_BYTES, GRID_ROW_CEILING,
    ResultCaps, ResultPolicy, Sourced,
};
pub use result_store::{
    EndCause, FetchMore, FetchRequest, FetchTicket, Fetched, LimitKind, LobCell, LobUnavailable,
    MoreRows, ResultPhase, ResultState, ResultStore, SegmentReply, StoreAction,
};
pub use scaled::{NumberValue, ScaledNumber};
pub use segment::{CellValue, ResultSegment, SegmentColumn, SegmentData};
pub use session_results::{Observed, SessionResults};

pub(crate) use segment::compact_batch;
