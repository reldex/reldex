//! Vendor-neutral database driver contract for Reldex.
//!
//! This crate is the only contract between `reldex-db-core` and any database
//! vendor (`docs/architecture/ARCHITECTURE.md` §2, §4). It defines the traits a
//! driver implements, the normalized [`DbError`], the value and type model, and
//! the shape of batched results. It contains no vendor code, no I/O, no threads,
//! no async runtime and **no production dependencies**.
//!
//! The decisions behind every type here are recorded in
//! `docs/decisions/0002-driver-api-and-concurrency-model.md`.
//!
//! # Shape of the contract
//!
//! ```text
//! DatabaseDriver ──connect──▶ DatabaseConnection ──execute──▶ ExecutionOutcome
//!                                   │                              │
//!                             cancel_handle                    take_cursor
//!                                   ▼                              ▼
//!                          Arc<dyn CancelHandle>           Cursor ──fetch_batch──▶ RowBatch
//! ```
//!
//! # Threading
//!
//! The traits block. [`DatabaseDriver`] is `Send + Sync`;
//! [`DatabaseConnection`] is `Send` but not `Sync` and is owned by one worker
//! thread in `db-core`; [`CancelHandle`] is `Send + Sync` so a control path
//! other than the blocked one can stop a running statement. See
//! [`mod@session`] for the full contract.
//!
//! # What lives elsewhere
//!
//! `DatabaseSession` (`SPEC.md` §6) is a `db-core` type, not a trait here:
//! session ownership, worker threads, conservative transaction tracking and
//! reconnect policy are core concerns. Connection pooling, metadata providers,
//! script splitting and the FFI surface are deliberately out of scope
//! (ADR-0002 D8; `AGENTS.md`, "keep public APIs small").

pub mod error;
pub mod ids;
pub mod params;
pub mod result;
pub mod session;
pub mod statement;
pub mod types;
pub mod value;

pub use crate::error::{DbError, DbResult, ErrorKind, NativeError, SessionState, SqlPosition};
pub use crate::ids::{
    ConnectionId, MAX_SAVEPOINT_NAME_LEN, ResultSetId, SavepointName, SavepointNameError,
    SessionId, StatementId,
};
pub use crate::params::{
    ConnectionParams, Credentials, Endpoint, ExtensionValue, Extensions, Secret, SessionRole,
    TlsMode,
};
pub use crate::result::{
    BytesColumn, Column, ColumnData, ColumnKind, Cursor, DEFAULT_FETCH_ROWS, ExecutionOutcome,
    NullMask, OutValues, RowBatch, TextColumn, Warning, WarningKind,
};
pub use crate::session::{
    CancelHandle, CancelKind, Capabilities, DatabaseConnection, DatabaseDriver, TransactionState,
};
pub use crate::statement::{Bind, BindDirection, Binds, NamedBind, OutBindSpec, Statement};
pub use crate::types::{ColumnMetadata, SqlType};
pub use crate::value::{
    LobKind, LobLocator, LobStream, MAX_EXPONENT, MAX_SIGNIFICANT_DIGITS, MIN_EXPONENT, Number,
    NumberError, TemporalError, TimeZone, Timestamp, Value, ValueRef,
};
