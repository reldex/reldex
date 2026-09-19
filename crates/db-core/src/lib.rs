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
//! dedicating a thread to each one. See [`mod@worker`] for the queueing policy
//! and for what a caught driver panic does.
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

mod ids;
mod session;
mod shared;
mod worker;

pub use ids::{LobHandle, ResultId, SessionId};
pub use session::{
    CloseDisposition, CloseError, Completion, DROP_SHUTDOWN_TIMEOUT, DatabaseSession,
    ExecuteOutcome, FetchedBatch, OutValue, OutValues, SessionLimits, SessionManager,
};
pub use shared::SessionLifecycle;

// Re-exported so most callers need only this crate for session-level work,
// without reaching into `reldex-db-driver-api` directly for vendor-neutral
// contract types this API's own signatures already use.
//
// `LobLocator` is deliberately **not** re-exported: a locator never leaves a
// session's worker thread, so nothing above `db-core` should be able to name
// one. [`LobHandle`] is what callers get instead.
pub use reldex_db_driver_api::{
    CancelKind, CancelOutcome, Column, ConnectionId, ConnectionParams, DatabaseDriver, DbError,
    DbResult, ErrorKind, RowBatch, SavepointName, SessionState, Statement, StatementKind,
    TransactionState, ValueRef, Warning,
};
