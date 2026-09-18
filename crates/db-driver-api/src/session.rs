//! The driver and connection traits, capabilities, and the cancellation
//! contract (ADR-0002 D1, D2, D4, D7).
//!
//! # Threading contract
//!
//! - [`DatabaseDriver`] is `Send + Sync`: one instance may be shared and used to
//!   open connections from anywhere.
//! - [`DatabaseConnection`] is `Send` and deliberately **not** `Sync`. `db-core`
//!   moves it onto the worker thread that owns the session for the session's
//!   lifetime, and every statement-issuing method takes `&mut self`, so "one
//!   session serializes its own calls" is enforced by the borrow checker rather
//!   than by documentation.
//! - [`CancelHandle`] is the one `Send + Sync` escape hatch. It is obtained
//!   before a call blocks and is handed to whatever control path may need to
//!   stop it (`SPEC.md` §24.8, `phase-0.md` Workstream C).
//!
//! There is no `DatabaseSession` trait here on purpose. A session
//! (`SPEC.md` §6, `ARCHITECTURE.md` §3) is the `db-core` type that owns a
//! connection, its worker thread, its cancel handle and its conservative
//! transaction tracking; the driver contract stops at the connection.
//!
//! Nothing in this crate starts a thread or an async runtime. The traits block,
//! and `db-core` decides how blocking work is scheduled.

use std::sync::Arc;

use crate::error::DbResult;
use crate::ids::{ConnectionId, SavepointName};
use crate::params::ConnectionParams;
use crate::result::ExecutionOutcome;
use crate::statement::Statement;

/// How a driver implements statement cancellation.
///
/// Reported through [`Capabilities::cancel`] so the UI can disable Cancel rather
/// than offer something that will not work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CancelKind {
    /// The driver cannot stop a running statement.
    ///
    /// [`CancelHandle::request_cancel`] returns
    /// [`crate::ErrorKind::Unsupported`].
    #[default]
    Unsupported,
    /// The driver stops a statement by arming a short call timeout.
    ///
    /// Cancellation is coarse: it takes effect at the next protocol wait, and
    /// the connection may be left mid-exchange, so the resulting error reports
    /// [`crate::SessionState::NeedsValidation`]. The error is still
    /// [`crate::ErrorKind::Cancelled`], never `Timeout`, because a cancel is
    /// what the user asked for.
    CallTimeout,
    /// The driver sends a protocol-level interrupt on a separate control path.
    Native,
}

/// What a driver can do.
///
/// `Default` supports nothing: a driver opts in explicitly, so a capability can
/// never be advertised by omission. The set will grow as Phase 0 finds out what
/// is worth asking about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Capabilities {
    /// How statement cancellation is implemented.
    pub cancel: CancelKind,
    /// `SAVEPOINT` and `ROLLBACK TO SAVEPOINT` are supported.
    pub savepoints: bool,
    /// Binds may be addressed by name.
    pub named_binds: bool,
    /// OUT and IN OUT binds are supported.
    pub out_binds: bool,
    /// Nested cursors (`REF CURSOR`) are supported.
    pub ref_cursor: bool,
    /// Statements may return result sets implicitly.
    pub implicit_results: bool,
    /// Large objects are streamed rather than materialized.
    pub lob_streaming: bool,
    /// Encrypted transport is supported.
    pub tls: bool,
    /// [`DatabaseConnection::transaction_state`] is exact; it never returns
    /// [`TransactionState::Unknown`].
    pub exact_transaction_state: bool,
    /// Errors carry a position in the statement text.
    pub error_position: bool,
}

impl Capabilities {
    /// A driver that supports nothing; the starting point for a real set.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            cancel: CancelKind::Unsupported,
            savepoints: false,
            named_binds: false,
            out_binds: false,
            ref_cursor: false,
            implicit_results: false,
            lob_streaming: false,
            tls: false,
            exact_transaction_state: false,
            error_position: false,
        }
    }

    /// Whether [`CancelHandle::request_cancel`] can do anything at all.
    #[must_use]
    pub const fn supports_cancel(self) -> bool {
        !matches!(self.cancel, CancelKind::Unsupported)
    }
}

/// What the driver knows about the connection's transaction.
///
/// `SPEC.md` §10 requires a prompt when a worksheet closes with an active
/// transaction, so the honest answer matters more than a confident one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TransactionState {
    /// The driver knows no transaction is open.
    #[default]
    Inactive,
    /// The driver knows a transaction is open.
    Active,
    /// The driver cannot tell.
    ///
    /// A driver that cannot observe server-side transaction state must report
    /// this after any statement that could have opened one, and
    /// [`TransactionState::Inactive`] only straight after a successful commit or
    /// rollback. `db-core` treats `Unknown` as *may be open*: over-prompting is
    /// acceptable, a silent commit is not.
    Unknown,
}

impl TransactionState {
    /// Whether the core must behave as though a transaction were open.
    #[must_use]
    pub const fn may_be_open(self) -> bool {
        matches!(self, Self::Active | Self::Unknown)
    }
}

/// A `Send + Sync` handle for stopping whatever the connection is doing.
///
/// Obtained from [`DatabaseConnection::cancel_handle`] before a call blocks, and
/// normally held as `Arc<dyn CancelHandle>` so it can be cloned to whichever
/// control path needs it.
///
/// The contract, stated so no layer has to guess:
///
/// 1. **Best effort.** `Ok` means the request was delivered or armed, not that
///    anything stopped.
/// 2. **Idempotent.** Repeated calls, and calls while nothing is running, are a
///    successful no-op.
/// 3. **The outcome travels through the blocked call**, which returns
///    [`crate::ErrorKind::Cancelled`] — never through this method's return value.
/// 4. **Races are the caller's to handle.** If the statement finished first it
///    returns normally and the cancel is discarded.
/// 5. **Session state afterwards is reported, not assumed.** The resulting error
///    carries [`crate::SessionState`]; the core revalidates or surfaces the loss
///    accordingly.
/// 6. **No transaction is implicitly resolved.** A cancelled statement neither
///    commits nor rolls back; [`DatabaseConnection::transaction_state`] is
///    authoritative afterwards.
pub trait CancelHandle: Send + Sync {
    /// Requests that the connection stop its current operation.
    ///
    /// # Errors
    ///
    /// [`crate::ErrorKind::Unsupported`] when [`CancelHandle::kind`] is
    /// [`CancelKind::Unsupported`]; otherwise any error raised while delivering
    /// the request.
    fn request_cancel(&self) -> DbResult<()>;

    /// How this handle implements cancellation.
    fn kind(&self) -> CancelKind;
}

/// The entry point of a driver implementation.
///
/// One driver instance may serve many connections and may be shared between
/// threads, which is why it is `Send + Sync` and `connect` takes `&self`.
pub trait DatabaseDriver: Send + Sync {
    /// A short, stable, vendor-neutral driver name for diagnostics.
    fn name(&self) -> &str;

    /// What this driver can do. Constant for the lifetime of the driver.
    fn capabilities(&self) -> Capabilities;

    /// Opens one connection.
    ///
    /// The connection must be opened with auto-commit **off**
    /// (`SPEC.md` §10). A driver that cannot do so must fail here with
    /// [`crate::ErrorKind::Unsupported`] rather than open a connection that
    /// commits behind the user's back.
    ///
    /// # Errors
    ///
    /// [`crate::ErrorKind::Configuration`] for unusable parameters,
    /// [`crate::ErrorKind::Authentication`] for a rejected login,
    /// [`crate::ErrorKind::Connection`] if the server could not be reached.
    fn connect(&self, params: &ConnectionParams) -> DbResult<Box<dyn DatabaseConnection>>;
}

/// One physical/logical connection: the unit a `db-core` session owns.
///
/// All statement-issuing methods take `&mut self`, so a connection's calls are
/// serialized by construction. Cancellation is the one thing that reaches a busy
/// connection from elsewhere, through [`CancelHandle`].
pub trait DatabaseConnection: Send {
    /// This connection's identifier.
    fn id(&self) -> ConnectionId;

    /// What this connection can do. Constant for the connection's lifetime.
    fn capabilities(&self) -> Capabilities;

    /// A handle that can stop the current operation from another control path.
    ///
    /// Callable while the connection is busy on its worker thread, which is why
    /// it is obtained before the blocking call starts.
    fn cancel_handle(&self) -> Arc<dyn CancelHandle>;

    /// Executes one statement and returns everything it produced.
    ///
    /// There is a single entry point because a worksheet cannot know in advance
    /// whether arbitrary user text returns rows, and the core must not parse SQL
    /// to find out. A result set arrives as
    /// [`ExecutionOutcome::take_cursor`].
    ///
    /// A driver must reject [`crate::Value::Lob`] and [`crate::Value::Cursor`]
    /// used as IN binds with [`crate::ErrorKind::Unsupported`].
    ///
    /// # Errors
    ///
    /// Any [`crate::DbError`]; [`crate::ErrorKind::Cancelled`] if the call was
    /// cancelled.
    fn execute(&mut self, statement: &Statement) -> DbResult<ExecutionOutcome>;

    /// Commits the open transaction.
    ///
    /// # Errors
    ///
    /// Any [`crate::DbError`].
    fn commit(&mut self) -> DbResult<()>;

    /// Rolls the open transaction back.
    ///
    /// # Errors
    ///
    /// Any [`crate::DbError`].
    fn rollback(&mut self) -> DbResult<()>;

    /// Establishes a savepoint.
    ///
    /// # Errors
    ///
    /// [`crate::ErrorKind::Unsupported`] if [`Capabilities::savepoints`] is
    /// false; otherwise any [`crate::DbError`].
    fn savepoint(&mut self, name: &SavepointName) -> DbResult<()>;

    /// Rolls back to a savepoint, leaving the transaction open.
    ///
    /// # Errors
    ///
    /// [`crate::ErrorKind::Unsupported`] if [`Capabilities::savepoints`] is
    /// false, [`crate::ErrorKind::Transaction`] if the savepoint does not exist.
    fn rollback_to_savepoint(&mut self, name: &SavepointName) -> DbResult<()>;

    /// What the driver knows about the transaction right now.
    ///
    /// Cheap and non-blocking: it reports driver-side knowledge and must not
    /// issue a round trip.
    fn transaction_state(&self) -> TransactionState;

    /// Checks that the session is still alive.
    ///
    /// Used after an error reporting [`crate::SessionState::NeedsValidation`]
    /// and on mobile resume (`SPEC.md` §18).
    ///
    /// # Errors
    ///
    /// Any [`crate::DbError`]; a dead session reports
    /// [`crate::SessionState::Lost`].
    fn ping(&mut self) -> DbResult<()>;

    /// Closes the connection.
    ///
    /// The driver must not commit as part of closing (`SPEC.md` §10): an open
    /// transaction is rolled back by the server, and the core is responsible for
    /// prompting before it gets here.
    ///
    /// # Errors
    ///
    /// Any [`crate::DbError`] the close produced.
    fn close(self: Box<Self>) -> DbResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DbError;
    use crate::result::{Cursor, RowBatch};
    use crate::value::Value;

    #[allow(clippy::extra_unused_type_parameters)]
    const fn assert_send<T: Send>() {}

    #[allow(clippy::extra_unused_type_parameters)]
    const fn assert_sync<T: Sync>() {}

    #[test]
    fn contract_types_have_the_intended_thread_bounds() {
        // Drivers are shared; connections are owned by one worker thread.
        assert_send::<Box<dyn DatabaseDriver>>();
        assert_sync::<Box<dyn DatabaseDriver>>();
        assert_send::<Box<dyn DatabaseConnection>>();

        // The cancel handle is the escape hatch that reaches a busy connection.
        assert_send::<Arc<dyn CancelHandle>>();
        assert_sync::<Arc<dyn CancelHandle>>();

        // Results and errors move from the worker thread to the core.
        assert_send::<Box<dyn Cursor>>();
        assert_send::<RowBatch>();
        assert_send::<Value>();
        assert_send::<DbError>();
        assert_sync::<DbError>();
        assert_send::<ExecutionOutcome>();
        assert_send::<ConnectionParams>();
        assert_sync::<ConnectionParams>();
        assert_send::<Statement>();
    }

    #[test]
    fn default_capabilities_support_nothing() {
        let capabilities = Capabilities::default();
        assert_eq!(capabilities, Capabilities::none());
        assert_eq!(capabilities.cancel, CancelKind::Unsupported);
        assert!(!capabilities.supports_cancel());
        assert!(!capabilities.savepoints);
        assert!(!capabilities.exact_transaction_state);
    }

    #[test]
    fn cancel_kinds_that_do_something_are_flagged() {
        for kind in [CancelKind::Native, CancelKind::CallTimeout] {
            let capabilities = Capabilities {
                cancel: kind,
                ..Capabilities::none()
            };
            assert!(capabilities.supports_cancel());
        }
    }

    #[test]
    fn unknown_transaction_state_is_treated_as_open() {
        assert!(TransactionState::Active.may_be_open());
        assert!(TransactionState::Unknown.may_be_open());
        assert!(!TransactionState::Inactive.may_be_open());
        assert_eq!(TransactionState::default(), TransactionState::Inactive);
    }
}
