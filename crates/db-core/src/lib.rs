//! Vendor-neutral core: sessions, transactions, query execution, results,
//! metadata, and workspace state (`docs/architecture/ARCHITECTURE.md` §3).
//!
//! Dependency rules (`docs/architecture/ARCHITECTURE.md` §2):
//! - Allowed dependencies: `reldex-db-driver-api` only.
//! - Forbidden: depending on any concrete `reldex-driver-*` crate, or on
//!   Qt/QML/FFI/UI code. This crate must remain UI-independent.
//!
//! This slice (Phase 0 Workstreams A + B) implements the session layer:
//! [`SessionManager`] opens a [`DatabaseSession`] from an
//! `Arc<dyn DatabaseDriver>`. Query execution, results, metadata and
//! workspace state are later slices; `crates/db-core/tests/dependency_rules.rs`
//! enforces the dependency-direction rule above so it cannot regress
//! silently.
//!
//! # Threading (ADR-0002 D1/D2)
//!
//! Each [`DatabaseSession`] owns one dedicated worker thread, which owns the
//! driver's `Box<dyn DatabaseConnection>`, every cursor derived from it and
//! every large-object locator it produced, for the session's whole lifetime.
//! No database or network call ever runs on the caller's thread. Requests are
//! sent over a channel and processed strictly in the order they arrive; each
//! returns a [`Completion`] the caller can [`Completion::wait`] on,
//! [`Completion::poll`], or [`Completion::wait_timeout`] — deliberately shaped
//! so a later FFI/Qt adapter can turn completions into events instead of
//! dedicating a thread to each one. See the `worker` module for the queueing
//! policy and for what a caught driver panic does.
//!
//! Cancellation is the one thing that reaches a session from outside that
//! queue: [`DatabaseSession::cancel`] calls the driver's
//! `Arc<dyn CancelHandle>` directly from whatever thread calls it, and
//! [`DatabaseSession::cancel_kind`] tells the caller up front whether that can
//! actually interrupt a running call (`reldex_db_driver_api::CancelKind`).
//!
//! # Transactions and session loss (`SPEC.md` §10, §18)
//!
//! [`DatabaseSession::has_possibly_active_transaction`] combines the driver's
//! own (possibly [`reldex_db_driver_api::TransactionState::Unknown`]) report
//! with core-side tracking derived from
//! [`reldex_db_driver_api::StatementKind`], and is conservative: a statement is
//! treated as possibly transaction-opening unless a driver with exact
//! transaction state says otherwise, because `SELECT … FOR UPDATE` and
//! `SET TRANSACTION` open transactions and no `StatementKind` can tell them
//! from a plain query. [`DatabaseSession::close`] refuses to close over a
//! possibly-active transaction without an explicit [`CloseDisposition`], and
//! decides that on the worker thread once the queue has drained. It never
//! silently commits or rolls back, and **dropping** a session never commits at
//! all: `close` is the only path that can.
//!
//! When a driver reports [`reldex_db_driver_api::SessionState::Lost`] (or a
//! revalidating `ping` fails), the session moves to the terminal
//! [`SessionLifecycle::Lost`] state: every later request fails fast with an
//! error that keeps the original kind and native code, open results are
//! released, and nothing here ever reconnects or replaces the session on its
//! own — a reconnect is the caller explicitly opening a new one, which gets a
//! new [`SessionId`].
//!
//! # Only plain data crosses threads
//!
//! A driver puts live `LobLocator`s straight into a fetched batch's LOB
//! columns, and a locator is a handle derived from the connection: using it, or
//! even dropping it, on another thread runs driver code on the wrong thread.
//! So `db-core` takes every locator out of a batch before the batch leaves the
//! worker and parks it beside the cursors; the caller receives a
//! [`FetchedBatch`] — plain data plus opaque [`LobHandle`]s — and reads through
//! [`DatabaseSession::read_lob_chunk`]. With that, the invariant ADR-0002 D1
//! states is literally true: of everything a fetch produces, only plain data
//! crosses a thread boundary.
//!
//! # The Result Store
//!
//! A result a UI shows is retained in a [`ResultStore`] (ADR-0004): the
//! fetched prefix as immutable [`ResultSegment`]s, each one a batch compacted
//! **on the worker thread** — `NUMBER` re-encoded as a scaled `i64` where that
//! is exact, text and byte buffers copied to their exact size, every LOB locator
//! parked and replaced by an id — before its reply leaves the worker
//! ([`DatabaseSession::submit_fetch_segment`],
//! [`SessionEvent::FetchedSegment`]). The store decides what to fetch from
//! the view's demand and the row and byte caps, and reports a typed
//! [`ResultState`]. [`SessionResults`] holds a session's stores and ends
//! them, deterministically, whenever the transaction or the session ends.
//!
//! # Two ways to be answered
//!
//! Every request can be answered either through its own [`Completion`] — the
//! blocking shape, kept for tests, tools and the device-check binary — or as a
//! typed [`SessionEvent`] pushed into a shared [`EventQueue`] with an
//! edge-triggered [`Waker`] (`docs/exec-plans/active/phase-1.md` §B2). Both
//! are the same worker command with a different reply channel, so a request's
//! semantics never depend on which one the caller chose, and no thread is
//! parked per outstanding request on the event path. [`EventQueue`],
//! [`EventCaps`] and [`Waker`] carry the ordering guarantees, the
//! back-pressure policy and the waker contract in full.
//!
//! # Two ways to open a session
//!
//! Opening has the same split. [`SessionManager::open_session`] blocks the
//! caller until the connection is ready and hands back a [`DatabaseSession`];
//! [`SessionRegistry::open`] returns a [`SessionId`] immediately, runs the
//! connect on the session's own worker thread, and answers with one
//! [`SessionEvent::Opened`] or [`SessionEvent::OpenFailed`]. The registry is
//! also what can *stop* an open: [`SessionRegistry::abandon`] answers a
//! pending connect at once and closes the connection that arrives afterwards,
//! because a connect cannot be interrupted and waiting for one is exactly what
//! a UI must not do (`docs/exec-plans/active/phase-1.md` §B3).

#![forbid(unsafe_code)]

mod events;
mod ids;
mod registry;
mod reply;
mod session;
mod shared;
mod store;
mod worker;

pub use events::{
    CompletedOperation, EventCaps, EventQueue, EventSink, RequestId, SessionEvent, Waker,
    event_channel,
};
pub use ids::{LobHandle, ResultId, SessionId};
pub use registry::{Abandoned, RegisteredSession, RegistryCounts, SessionRegistry};
pub use session::{
    CloseDisposition, CloseError, Completion, DROP_SHUTDOWN_TIMEOUT, DatabaseSession,
    ExecuteOutcome, FetchedBatch, OutValue, OutValues, ServerOutputLog, SessionLimits,
    SessionManager,
};
pub use shared::SessionLifecycle;
pub use store::{
    Cap, CapSource, CellValue, DEFAULT_FETCHES_IN_FLIGHT, DEFAULT_ROUND_TRIP_BYTES, EndCause,
    FetchMore, FetchRequest, FetchTicket, Fetched, GRID_ROW_CEILING, LimitKind, LobCell,
    LobUnavailable, MoreRows, NumberValue, Observed, ResultCaps, ResultPhase, ResultPolicy,
    ResultSegment, ResultState, ResultStore, ScaledNumber, SegmentColumn, SegmentData,
    SegmentReply, SessionResults, Sourced, StoreAction,
};

// Re-exported so most callers need only this crate for session-level work,
// without reaching into `reldex-db-driver-api` directly for vendor-neutral
// contract types this API's own signatures already use.
//
// `LobLocator` is deliberately **not** re-exported: a locator never leaves a
// session's worker thread, so nothing above `db-core` should be able to name
// one. [`LobHandle`] is what callers get instead.
pub use reldex_db_driver_api::{
    CancelKind, CancelOutcome, Column, ColumnMetadata, ConnectionId, ConnectionParams,
    DatabaseDriver, DbError, DbResult, ErrorKind, RowBatch, SavepointName, ServerOutputBuffer,
    ServerOutputSetting, SessionState, Statement, StatementKind, TransactionState, ValueRef,
    Warning,
};
