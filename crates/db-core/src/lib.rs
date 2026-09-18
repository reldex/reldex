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
//! driver's `Box<dyn DatabaseConnection>` and every cursor derived from it
//! for the session's whole lifetime. No database or network call ever runs on
//! the caller's thread. Requests are sent over a channel and processed
//! strictly in the order they arrive; each returns a [`Completion`] the
//! caller can [`Completion::wait`] on or [`Completion::poll`] — deliberately
//! shaped so a later FFI/Qt adapter can turn completions into events instead
//! of dedicating a thread to each one. See [`mod@worker`] for the queueing
//! policy.
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
//! [`reldex_db_driver_api::StatementKind`]. [`DatabaseSession::close`]
//! refuses to close over a possibly-active transaction without an explicit
//! [`CloseDisposition`] — it never silently commits or rolls back.
//!
//! When a driver reports [`reldex_db_driver_api::SessionState::Lost`] (or a
//! revalidating `ping` fails), the session moves to a terminal lost state:
//! every later request fails fast, open results are invalidated, and nothing
//! here ever reconnects or replaces the session on its own — a reconnect is
//! the caller explicitly opening a new one, which gets a new [`SessionId`].
//!
//! # LOB handles cross threads inside a `RowBatch`, but reading them must not
//!
//! A fetched [`reldex_db_driver_api::RowBatch`] can carry a
//! [`reldex_db_driver_api::LobLocator`] in a LOB column, and the batch itself
//! is `Send` and does cross to the caller's thread — that part of the driver
//! contract is unavoidable, since delivering rows is the point. But a locator
//! is still a handle derived from the connection, so
//! [`DatabaseSession::read_lob_chunk`] exists to keep the actual read on the
//! worker thread that owns it: take the locator out of the batch
//! ([`reldex_db_driver_api::Column::take_lob`]) and hand it to that method
//! instead of calling [`reldex_db_driver_api::LobLocator::read_chunk`]
//! directly.

mod ids;
mod session;
mod shared;
mod worker;

pub use ids::SessionId;
pub use session::{
    CloseDisposition, CloseError, Completion, DatabaseSession, ExecuteOutcome, SessionManager,
};

// Re-exported so most callers need only this crate for session-level work,
// without reaching into `reldex-db-driver-api` directly for vendor-neutral
// contract types this API's own signatures already use.
pub use reldex_db_driver_api::{
    CancelKind, CancelOutcome, ConnectionId, ConnectionParams, DatabaseDriver, DbError, DbResult,
    ErrorKind, LobLocator, ResultSetId, RowBatch, SavepointName, SessionState, Statement,
    StatementKind, TransactionState, Warning,
};
