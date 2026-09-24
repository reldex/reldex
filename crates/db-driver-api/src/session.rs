//! The driver and connection traits, capabilities, and the cancellation
//! contract (ADR-0002 D1, D2, D4, D7).
//!
//! # Threading contract
//!
//! - [`DatabaseDriver`] is `Send + Sync`: one instance may be shared and used to
//!   open connections from anywhere.
//! - [`DatabaseConnection`] is `Send` and deliberately **not** `Sync`. `db-core`
//!   moves it onto the worker thread that owns the session for the session's
//!   lifetime, and every statement-issuing method takes `&mut self`.
//! - [`CancelHandle`] is the one `Send + Sync` escape hatch. It is obtained
//!   before a call blocks and is handed to whatever control path may need to
//!   stop it (`SPEC.md` §24.8, `phase-0.md` Workstream C).
//!
//! ## What `&mut self` does and does not prove
//!
//! `&mut self` serializes calls **through the connection value itself**. It does
//! *not* make "one session's database traffic is serialized" a compile-time
//! property, and this crate must not claim that it does. A connection hands out
//! independent `Send` handles — a [`crate::Cursor`], a [`crate::LobLocator`], a
//! nested cursor inside a [`crate::Value`] — each of which issues its own
//! protocol traffic without borrowing the connection at all. Two cursors on one
//! connection can exist at once, and the borrow checker has nothing to say about
//! where they are used.
//!
//! The real invariant is a runtime one, and `db-core` is what enforces it:
//!
//! > Every object derived from a connection is used **only on that connection's
//! > owning worker thread**. Of everything a fetch produces, only
//! > [`crate::RowBatch`] — plain data, no handles — crosses a thread boundary.
//!
//! [`crate::Cursor::connection_id`] and [`crate::LobStream::connection_id`]
//! exist so that this can be asserted rather than assumed.
//!
//! There is no `DatabaseSession` trait here on purpose. A session
//! (`SPEC.md` §6, `ARCHITECTURE.md` §3) is the `db-core` type that owns a
//! connection, its worker thread, its cancel handle and its conservative
//! transaction tracking; the driver contract stops at the connection.
//!
//! Nothing in this crate starts a thread or an async runtime. The traits block,
//! and `db-core` decides how blocking work is scheduled.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use crate::error::{DbError, DbResult};
use crate::ids::{ConnectionId, SavepointName};
use crate::params::ConnectionParams;
use crate::result::{ExecutionOutcome, Warning};
use crate::server_output::{ServerOutputChunk, ServerOutputSetting};
use crate::statement::Statement;

/// How a driver implements statement cancellation.
///
/// Reported through [`Capabilities::cancel`] so the UI can tell the user the
/// truth *before* they press Cancel, rather than offering something that will
/// not work (`SPEC.md` §24.8; "never hide limitations").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum CancelKind {
    /// The driver cannot stop a running statement at all.
    ///
    /// [`CancelHandle::request_cancel`] returns
    /// [`crate::ErrorKind::Unsupported`], and the UI disables Cancel.
    #[default]
    Unsupported,
    /// The driver can only enforce a deadline **armed before the call started**
    /// ([`Statement::with_deadline`]).
    ///
    /// This is not "cancel implemented with a timeout". A driver in this class
    /// cannot interrupt a call that is already running — typically because
    /// setting its timeout takes the same internal lock the running call holds
    /// for the whole round trip, so an on-demand attempt would block until the
    /// statement finished anyway, which is the opposite of cancelling. This is
    /// the verified shape of the primary driver today (ADR-0001 C1).
    ///
    /// Consequences the rest of the system must respect:
    ///
    /// - [`CancelHandle::request_cancel`] returns
    ///   [`CancelOutcome::NotInterruptible`] carrying the time left on the
    ///   pre-armed deadline, if the driver knows it. It never blocks and never
    ///   claims the statement will stop now.
    /// - The UI must say what will actually happen ("this statement cannot be
    ///   interrupted; it will stop by 14:32:07"), not show a spinner implying a
    ///   cancel is in flight.
    /// - When the deadline fires the call fails with
    ///   [`crate::ErrorKind::Timeout`], because that is what happened. A driver
    ///   must not relabel it `Cancelled` to make the UI look better.
    ///
    /// A driver in this class does **not** satisfy `SPEC.md` §10/§24.8 "cancel a
    /// running statement". ADR-0001 spike S4 decides whether the primary driver
    /// can leave this class.
    PreArmedDeadline,
    /// The driver interrupts a running statement over a separate control path.
    ///
    /// [`CancelHandle::request_cancel`] returns [`CancelOutcome::Requested`]
    /// promptly, without waiting for the statement, and the blocked call fails
    /// with [`crate::ErrorKind::Cancelled`]. This is the only class that meets
    /// `SPEC.md` §24.8.
    Native,
}

impl CancelKind {
    /// Whether this driver can stop a statement that is *already running*.
    ///
    /// The question the UI must ask before it offers a Cancel button, and the
    /// one `SPEC.md` §24.8 is about. Only [`CancelKind::Native`] answers yes;
    /// [`CancelKind::PreArmedDeadline`] can bound a call in advance but cannot
    /// interrupt one.
    #[must_use]
    pub const fn interrupts_running_call(self) -> bool {
        matches!(self, Self::Native)
    }

    /// Whether [`Statement::with_deadline`] is the only way to bound a call on
    /// this driver, so `db-core` must arm one before every execute it may need
    /// to stop.
    #[must_use]
    pub const fn needs_deadline_armed_up_front(self) -> bool {
        matches!(self, Self::PreArmedDeadline)
    }
}

/// What a cancellation request actually achieved.
///
/// [`CancelHandle::request_cancel`] returns this instead of a bare `()` so that
/// "I asked, and nothing can come of it" is a value the UI can render, rather
/// than an `Ok` that looks like success. `SPEC.md` §2 ranks correctness above
/// convenience; an honest "not interruptible" is worth more than a spinner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CancelOutcome {
    /// An interrupt was delivered or armed on a separate control path.
    ///
    /// Best effort, as ever: the statement may still finish normally, and the
    /// outcome arrives through the blocked call. Also the correct answer when
    /// nothing was running, since cancellation is idempotent.
    Requested,
    /// This driver cannot interrupt a call that is already running
    /// ([`CancelKind::PreArmedDeadline`]).
    ///
    /// Nothing was sent and nothing will stop early. The call ends when it ends,
    /// or when the deadline armed before it started fires.
    NotInterruptible {
        /// How long is left on that pre-armed deadline, if the driver can work
        /// it out.
        ///
        /// `None` means either that no deadline was armed — in which case the
        /// statement will run to completion — or that the driver cannot measure
        /// the remainder. Either way the caller must not invent a number: show
        /// "cannot be interrupted" without a time.
        deadline_remaining: Option<Duration>,
    },
}

impl CancelOutcome {
    /// Whether anything is actually going to try to stop the statement.
    ///
    /// False for [`CancelOutcome::NotInterruptible`], which is precisely the
    /// case the UI must not present as a cancel in progress.
    #[must_use]
    pub const fn is_requested(self) -> bool {
        matches!(self, Self::Requested)
    }
}

/// What a driver can do.
///
/// `Default` supports nothing: a driver opts in explicitly, so a capability can
/// never be advertised by omission. The set will grow as Phase 0 finds out what
/// is worth asking about, which is why the fields are private and reached
/// through a builder — adding one must not break every driver that constructs
/// this struct.
///
/// ```
/// use reldex_db_driver_api::{Capabilities, CancelKind};
///
/// let capabilities = Capabilities::none()
///     .with_cancel(CancelKind::PreArmedDeadline)
///     .with_savepoints(true)
///     .with_named_binds(true)
///     .with_lob_streaming(true);
///
/// assert!(capabilities.savepoints());
/// // It can bound a call in advance, but it cannot interrupt one.
/// assert!(!capabilities.can_interrupt_running_call());
/// assert!(!capabilities.out_binds(), "not opted in, so not advertised");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Capabilities {
    cancel: CancelKind,
    savepoints: bool,
    named_binds: bool,
    out_binds: bool,
    ref_cursor: bool,
    lob_streaming: bool,
    tls: bool,
    exact_transaction_state: bool,
    error_position: bool,
    server_output: bool,
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
            lob_streaming: false,
            tls: false,
            exact_transaction_state: false,
            error_position: false,
            server_output: false,
        }
    }

    /// Declares how statement cancellation is implemented.
    #[must_use]
    pub const fn with_cancel(mut self, cancel: CancelKind) -> Self {
        self.cancel = cancel;
        self
    }

    /// Declares support for `SAVEPOINT` and `ROLLBACK TO SAVEPOINT`.
    #[must_use]
    pub const fn with_savepoints(mut self, supported: bool) -> Self {
        self.savepoints = supported;
        self
    }

    /// Declares support for binds addressed by name.
    #[must_use]
    pub const fn with_named_binds(mut self, supported: bool) -> Self {
        self.named_binds = supported;
        self
    }

    /// Declares support for OUT and IN OUT binds.
    #[must_use]
    pub const fn with_out_binds(mut self, supported: bool) -> Self {
        self.out_binds = supported;
        self
    }

    /// Declares support for nested cursors (`REF CURSOR`).
    #[must_use]
    pub const fn with_ref_cursor(mut self, supported: bool) -> Self {
        self.ref_cursor = supported;
        self
    }

    /// Declares that large objects are streamed rather than materialized.
    #[must_use]
    pub const fn with_lob_streaming(mut self, supported: bool) -> Self {
        self.lob_streaming = supported;
        self
    }

    /// Declares support for encrypted transport.
    #[must_use]
    pub const fn with_tls(mut self, supported: bool) -> Self {
        self.tls = supported;
        self
    }

    /// Declares that [`DatabaseConnection::transaction_state`] is exact.
    #[must_use]
    pub const fn with_exact_transaction_state(mut self, exact: bool) -> Self {
        self.exact_transaction_state = exact;
        self
    }

    /// Declares that errors carry a position in the statement text.
    #[must_use]
    pub const fn with_error_position(mut self, supported: bool) -> Self {
        self.error_position = supported;
        self
    }

    /// Declares that the connection can collect server output
    /// ([`DatabaseConnection::set_server_output`],
    /// [`DatabaseConnection::take_server_output`]).
    #[must_use]
    pub const fn with_server_output(mut self, supported: bool) -> Self {
        self.server_output = supported;
        self
    }

    /// How statement cancellation is implemented.
    #[must_use]
    pub const fn cancel(self) -> CancelKind {
        self.cancel
    }

    /// Whether `SAVEPOINT` and `ROLLBACK TO SAVEPOINT` are supported.
    #[must_use]
    pub const fn savepoints(self) -> bool {
        self.savepoints
    }

    /// Whether binds may be addressed by name.
    #[must_use]
    pub const fn named_binds(self) -> bool {
        self.named_binds
    }

    /// Whether OUT and IN OUT binds are supported.
    #[must_use]
    pub const fn out_binds(self) -> bool {
        self.out_binds
    }

    /// Whether nested cursors (`REF CURSOR`) are supported.
    #[must_use]
    pub const fn ref_cursor(self) -> bool {
        self.ref_cursor
    }

    /// Whether large objects are streamed rather than materialized.
    #[must_use]
    pub const fn lob_streaming(self) -> bool {
        self.lob_streaming
    }

    /// Whether encrypted transport is supported.
    #[must_use]
    pub const fn tls(self) -> bool {
        self.tls
    }

    /// Whether [`DatabaseConnection::transaction_state`] is exact; it never
    /// returns [`TransactionState::Unknown`].
    #[must_use]
    pub const fn exact_transaction_state(self) -> bool {
        self.exact_transaction_state
    }

    /// Whether errors carry a position in the statement text.
    #[must_use]
    pub const fn error_position(self) -> bool {
        self.error_position
    }

    /// Whether the connection can collect server output — text a session
    /// writes out of band, such as `DBMS_OUTPUT` (ADR-0002 amendment T).
    ///
    /// When this is false, `db-core` answers a request to enable it with
    /// [`crate::ErrorKind::Unsupported`] without calling the driver, and the UI
    /// does not offer the output pane.
    #[must_use]
    pub const fn server_output(self) -> bool {
        self.server_output
    }

    /// Whether a statement that is **already running** can be stopped.
    ///
    /// This, not [`Capabilities::cancel`] being non-`Unsupported`, is what
    /// `SPEC.md` §24.8 asks for and what decides whether the UI offers Cancel.
    /// See [`CancelKind::PreArmedDeadline`].
    #[must_use]
    pub const fn can_interrupt_running_call(self) -> bool {
        self.cancel.interrupts_running_call()
    }
}

/// What the driver knows about the connection's transaction.
///
/// `SPEC.md` §10 requires a prompt when a worksheet closes with an active
/// transaction, so the honest answer matters more than a confident one.
///
/// Deliberately **not** `#[non_exhaustive]`: three states is the whole truth
/// table (open / not open / do not know), and `db-core` must handle each
/// explicitly, so exhaustive matching is a feature (ADR-0002, amendment S1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TransactionState {
    /// The driver knows no transaction is open.
    Inactive,
    /// The driver knows a transaction is open.
    Active,
    /// The driver cannot tell.
    ///
    /// The **default**, because it is the only safe thing to assume about a
    /// connection nobody has said anything about: [`TransactionState::may_be_open`]
    /// is true, so a default-constructed value makes the core prompt rather than
    /// discard. A driver that cannot observe server-side transaction state must
    /// report this after any statement that could have opened one, and
    /// [`TransactionState::Inactive`] only straight after a successful commit or
    /// rollback. Over-prompting is acceptable; a silent commit is not.
    #[default]
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
/// 1. **It must never block on the connection's own call lock, and must return
///    promptly.** This is a hard requirement, not advice. A "cancel" that waits
///    for the statement it is cancelling is worse than no cancel at all: it
///    freezes the control path that was meant to stay responsive, and
///    `SPEC.md` §24.17 requires the rest of the application to keep working.
///    A driver that cannot signal without taking the lock the running call holds
///    must report [`CancelKind::PreArmedDeadline`] or
///    [`CancelKind::Unsupported`] and return immediately — never emulate a
///    cancel by blocking.
/// 2. **Best effort.** [`CancelOutcome::Requested`] means the request was
///    delivered or armed, not that anything stopped.
/// 3. **Honest about impotence.** A driver that cannot interrupt a running call
///    returns [`CancelOutcome::NotInterruptible`] rather than a bare `Ok` that
///    reads as success.
/// 4. **Idempotent.** Repeated calls, and calls while nothing is running, are a
///    successful no-op.
/// 5. **The outcome travels through the blocked call**, which returns
///    [`crate::ErrorKind::Cancelled`] — never through this method's return value.
/// 6. **Races are the caller's to handle.** If the statement finished first it
///    returns normally and the cancel is discarded.
/// 7. **Session state afterwards is reported, not assumed.** The resulting error
///    carries [`crate::SessionState`]; the core revalidates or surfaces the loss
///    accordingly.
/// 8. **No transaction is implicitly resolved.** A cancelled statement neither
///    commits nor rolls back; [`DatabaseConnection::transaction_state`] is
///    authoritative afterwards.
/// 9. **Safe after the connection is closed.** A handle outlives its connection
///    — it is `Arc`-shared precisely so a control path can hold one while the
///    worker owns the connection — so `request_cancel` can arrive after
///    [`DatabaseConnection::close`] has run. It must return without panicking
///    and without touching the closed connection. Returning an error is fine
///    and so is [`CancelOutcome::Requested`]; doing nothing is expected. A
///    driver that cannot make this safe keeps whatever state the handle needs
///    alive independently of the connection. The core narrows this window but
///    cannot close it, for the same reason rule 6 exists
///    (`docs/decisions/0002-driver-api-and-concurrency-model.md`, amendment
///    R7).
pub trait CancelHandle: Send + Sync {
    /// Requests that the connection stop its current operation.
    ///
    /// **Must not block.** See rule 1 on the trait: this is called from a
    /// control path while another thread is inside a blocking `execute` or
    /// `fetch_batch`, and taking that call's lock would deadlock the very
    /// responsiveness the method exists to provide.
    ///
    /// The returned [`CancelOutcome`] says what the request achieved, so the UI
    /// can tell the user the truth instead of showing a cancel that is not
    /// happening.
    ///
    /// # Errors
    ///
    /// [`crate::ErrorKind::Unsupported`] when [`CancelHandle::kind`] is
    /// [`CancelKind::Unsupported`]; otherwise any error raised while delivering
    /// the request. "I cannot interrupt this" is **not** an error — it is
    /// [`CancelOutcome::NotInterruptible`].
    fn request_cancel(&self) -> DbResult<CancelOutcome>;

    /// How this handle implements cancellation.
    ///
    /// Constant for the handle's lifetime, so the UI can decide what to offer
    /// before anything is running.
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
/// All statement-issuing methods take `&mut self`, so calls *through the
/// connection value* are serialized by construction. Objects derived from it —
/// [`crate::Cursor`], [`crate::LobLocator`] — are independent handles that
/// borrow nothing, so their thread affinity is a runtime rule `db-core`
/// enforces; see the module documentation. Cancellation is the one thing that
/// reaches a busy connection from elsewhere, through [`CancelHandle`].
///
/// # Lifetime of derived handles
///
/// One rule, stated once, so no driver has to invent it:
///
/// > A cursor or LOB stream never outlives the usefulness of its connection.
/// > Once the connection is closed, every derived handle **reports** and does
/// > not panic, does not block, and does not touch a socket that is gone.
///
/// In detail:
///
/// - After [`DatabaseConnection::close`], every outstanding [`crate::Cursor`]
///   and [`crate::LobStream`] returns
///   [`crate::DbError::connection_closed`] from every operation — with one
///   deliberate exception: [`crate::Cursor::close`] is idempotent and reports
///   `Ok(())` when there is nothing left to release. A release path that fails
///   because there is nothing to release only teaches callers to ignore its
///   result. `close` consumes the connection, so `db-core` is expected to have
///   dropped or closed its handles first; a driver must survive it not having
///   done so.
/// - After [`DatabaseConnection::commit`] or
///   [`DatabaseConnection::rollback`], open cursors and LOB locators may be
///   invalid — see those methods.
/// - After any error from a cursor, the only legal call on it is
///   [`crate::Cursor::close`]; after any error from a LOB stream, the only legal
///   action is to drop it. Drivers enforce this by reporting, not by trusting —
///   and specifically, a cursor that has failed must keep reporting that
///   failure rather than returning the empty batch that means "exhausted",
///   which would turn a partial result into one that looks complete.
/// - Dropping a derived handle is itself driver work, so it happens on the
///   owning worker thread like everything else — including dropping a
///   [`crate::RowBatch`] whose locators have not been taken out; see that type.
pub trait DatabaseConnection: Send {
    /// This connection's identifier.
    fn id(&self) -> ConnectionId;

    /// What this connection can do. Constant for the connection's lifetime.
    fn capabilities(&self) -> Capabilities;

    /// A handle that can stop the current operation from another control path.
    ///
    /// Callable while the connection is busy on its worker thread, which is why
    /// it is obtained before the blocking call starts. Cheap and non-blocking:
    /// it must not take any lock the running call holds.
    fn cancel_handle(&self) -> Arc<dyn CancelHandle>;

    /// Non-fatal findings produced while this connection was being *opened*.
    ///
    /// [`DatabaseDriver::connect`] returns a connection or an error, with
    /// nothing in between, so a driver that notices something while opening a
    /// session — a transport parameter it cannot honour, a profile setting that
    /// does nothing — could previously only refuse the session or stay silent.
    /// Neither is right for a finding that does not weaken the session but that
    /// the user still has to be told about, and attaching it to the first
    /// statement that happens to run loses it entirely for a session that is
    /// opened, pinged and closed.
    ///
    /// **Taken, not borrowed**, so each finding is reported once: the caller
    /// collects them immediately after `connect` returns and the connection
    /// keeps nothing. A second call returns an empty vector.
    ///
    /// The default is empty, for the many drivers with nothing to say, so
    /// adding this to the contract costs an existing driver no change. Cheap
    /// and non-blocking: it reports what `connect` already discovered and must
    /// not issue a round trip.
    ///
    /// Anything that makes the session *worse* than the caller asked for is an
    /// error from `connect`, not a warning here. This channel is for "you
    /// configured something that did nothing", not for "your session is less
    /// safe than you think".
    fn take_connect_warnings(&mut self) -> Vec<Warning> {
        Vec::new()
    }

    /// Executes one statement and returns everything it produced.
    ///
    /// There is a single entry point because a worksheet cannot know in advance
    /// whether arbitrary user text returns rows, and the core must not parse SQL
    /// to find out. A result set arrives as [`ExecutionOutcome::take_cursor`],
    /// and what kind of statement it turned out to be as
    /// [`ExecutionOutcome::statement_kind`] — which the driver must classify
    /// itself, because the core will not.
    ///
    /// The driver applies [`Statement::deadline`] before the call starts and
    /// [`Statement::fetch_rows`] when it sizes its fetch; neither can be
    /// supplied later.
    ///
    /// # Errors
    ///
    /// Any [`crate::DbError`]; [`crate::ErrorKind::Cancelled`] if the call was
    /// cancelled, or [`crate::ErrorKind::Timeout`] if a deadline set through
    /// [`Statement::with_deadline`] fired.
    fn execute(&mut self, statement: &Statement) -> DbResult<ExecutionOutcome>;

    /// Commits the open transaction.
    ///
    /// Committing may invalidate handles derived from this connection: most
    /// servers scope a LOB locator to the transaction that produced it, and some
    /// close open cursors. A driver must not paper over this. If a cursor or
    /// locator is no longer usable afterwards, its next operation returns a
    /// [`crate::DbError`] — [`crate::ErrorKind::Transaction`] when the server
    /// says so — rather than blocking, panicking, or returning a short result
    /// that looks complete. `db-core` therefore treats an open cursor or locator
    /// as transaction-scoped and must not promise the user otherwise.
    ///
    /// # Errors
    ///
    /// Any [`crate::DbError`].
    fn commit(&mut self) -> DbResult<()>;

    /// Rolls the open transaction back.
    ///
    /// The same handle-invalidation rule as [`DatabaseConnection::commit`]
    /// applies, and more forcefully: rolling back commonly closes open cursors.
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
    /// Like a full rollback, this may invalidate cursors and LOB locators opened
    /// since the savepoint; see [`DatabaseConnection::commit`].
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

    /// Turns server output on or off for this connection (ADR-0002
    /// amendment T), and reports the setting actually in force.
    ///
    /// One round trip. It changes what the **server** buffers for this
    /// session; it reads nothing. Turning output off may discard whatever the
    /// server was still holding — that is the server's behaviour (Oracle's
    /// `DBMS_OUTPUT.DISABLE` purges its buffer), and the caller must drain
    /// first if it wants those lines.
    ///
    /// The returned value is what the caller shows: a driver whose server
    /// accepts only a range of buffer sizes clamps
    /// [`crate::ServerOutputBuffer::Bytes`] into that range and returns the
    /// size it used, rather than refusing or pretending. It never answers a
    /// different enabled/disabled state than it was asked for.
    ///
    /// Must not touch the transaction: enabling or disabling output is not a
    /// statement the user ran, and it neither opens nor resolves anything.
    ///
    /// The default answers [`crate::ErrorKind::Unsupported`] and issues
    /// nothing, so a driver without the capability needs no code at all.
    ///
    /// # Errors
    ///
    /// [`crate::ErrorKind::Unsupported`] when
    /// [`Capabilities::server_output`] is false; otherwise any
    /// [`crate::DbError`] the round trip produced, classified like any other
    /// call's (a lost connection reports [`crate::SessionState::Lost`]).
    fn set_server_output(&mut self, setting: ServerOutputSetting) -> DbResult<ServerOutputSetting> {
        let _ = setting;
        Err(DbError::unsupported("server output"))
    }

    /// Takes up to `max_lines` buffered lines of server output, oldest first.
    ///
    /// **One round trip per call, whatever it returns** — a driver must never
    /// read one line per round trip, and must not loop internally until the
    /// buffer is empty: the caller decides how many chunks to ask for, which
    /// is what keeps one call's memory bounded. The lines leave the server's
    /// buffer as they are taken.
    ///
    /// `max_bytes` bounds the text in one chunk. It is a target, not a
    /// splitter: a line is the unit the server produced, and cutting one
    /// would be inventing a line break, so a line is always returned whole.
    /// A chunk holds at most `max_lines` lines, and lines totalling at most
    /// `max_bytes` bytes **plus at most one more line** — the one that did
    /// not fit. That last line exists because a driver may learn a line's
    /// size only by taking it from the server, and a line taken cannot be put
    /// back; returning it is the only alternative to losing it. A caller
    /// bounding memory therefore allows `max_bytes` plus one maximum-length
    /// line (32,767 bytes for `DBMS_OUTPUT`).
    ///
    /// What the server has not finished — for `DBMS_OUTPUT`, a `PUT` with no
    /// line end yet — is not a line and is not returned. Empty lines are
    /// lines, and are returned as empty strings.
    ///
    /// Must not touch the transaction, for the same reason as
    /// [`DatabaseConnection::set_server_output`].
    ///
    /// The default answers [`crate::ErrorKind::Unsupported`] and issues
    /// nothing.
    ///
    /// # Errors
    ///
    /// [`crate::ErrorKind::Unsupported`] when
    /// [`Capabilities::server_output`] is false; otherwise any
    /// [`crate::DbError`] the round trip produced. A failure may lose the
    /// lines that round trip had already taken from the server's buffer —
    /// the caller reports the failure rather than assuming the output is
    /// complete.
    fn take_server_output(
        &mut self,
        max_lines: NonZeroUsize,
        max_bytes: NonZeroUsize,
    ) -> DbResult<ServerOutputChunk> {
        let _ = (max_lines, max_bytes);
        Err(DbError::unsupported("server output"))
    }

    /// Closes the connection.
    ///
    /// The driver must not commit as part of closing (`SPEC.md` §10): an open
    /// transaction is rolled back by the server, and the core is responsible for
    /// prompting before it gets here.
    ///
    /// Any cursor or LOB stream still alive afterwards must return
    /// [`crate::DbError::connection_closed`] from every operation except
    /// [`crate::Cursor::close`], which reports `Ok(())` because there is
    /// nothing left to release — never a panic, never a block on a socket that
    /// is gone. See the trait's "Lifetime of derived handles".
    ///
    /// # Errors
    ///
    /// Any [`crate::DbError`] the close produced.
    fn close(self: Box<Self>) -> DbResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{DbError, ErrorKind};
    use crate::result::{Cursor, RowBatch};
    use crate::value::Value;

    #[allow(clippy::extra_unused_type_parameters)]
    const fn assert_send<T: Send>() {}

    #[allow(clippy::extra_unused_type_parameters)]
    const fn assert_sync<T: Sync>() {}

    /// A connection that implements only what the contract *requires*, so the
    /// defaulted methods are exercised as a driver author would inherit them.
    struct StubConnection;

    struct StubCancel;

    impl CancelHandle for StubCancel {
        fn request_cancel(&self) -> DbResult<CancelOutcome> {
            Err(DbError::new(ErrorKind::Unsupported, "stub"))
        }

        fn kind(&self) -> CancelKind {
            CancelKind::Unsupported
        }
    }

    impl StubConnection {
        fn unsupported<T>() -> DbResult<T> {
            Err(DbError::new(ErrorKind::Unsupported, "stub"))
        }
    }

    impl DatabaseConnection for StubConnection {
        fn id(&self) -> ConnectionId {
            ConnectionId::allocate()
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::none()
        }

        fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
            Arc::new(StubCancel)
        }

        fn execute(&mut self, _statement: &Statement) -> DbResult<ExecutionOutcome> {
            Self::unsupported()
        }

        fn commit(&mut self) -> DbResult<()> {
            Self::unsupported()
        }

        fn rollback(&mut self) -> DbResult<()> {
            Self::unsupported()
        }

        fn savepoint(&mut self, _name: &SavepointName) -> DbResult<()> {
            Self::unsupported()
        }

        fn rollback_to_savepoint(&mut self, _name: &SavepointName) -> DbResult<()> {
            Self::unsupported()
        }

        fn transaction_state(&self) -> TransactionState {
            TransactionState::Unknown
        }

        fn ping(&mut self) -> DbResult<()> {
            Self::unsupported()
        }

        fn close(self: Box<Self>) -> DbResult<()> {
            Ok(())
        }
    }

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
    fn a_driver_with_nothing_to_say_about_connecting_needs_no_code_at_all() {
        // The point of the default body: adding a connect-time warning channel
        // to the contract must cost a driver that has no findings nothing, and
        // it must stay usable through `dyn` like every other method.
        let mut connection: Box<dyn DatabaseConnection> = Box::new(StubConnection);
        assert!(connection.take_connect_warnings().is_empty());
        assert!(
            connection.take_connect_warnings().is_empty(),
            "taking twice is a no-op, not a repeat"
        );
    }

    #[test]
    fn a_driver_without_server_output_needs_no_code_and_says_unsupported() {
        // Additive contract (ADR-0002 amendment T): the defaults must compile
        // for every existing driver and answer in a typed way, not with an
        // `Ok` that reads as "enabled" or an empty chunk that reads as
        // "nothing was printed".
        let mut connection: Box<dyn DatabaseConnection> = Box::new(StubConnection);
        let set = connection
            .set_server_output(ServerOutputSetting::Enabled(
                crate::ServerOutputBuffer::Unlimited,
            ))
            .expect_err("a driver without the capability refuses");
        assert_eq!(set.kind(), ErrorKind::Unsupported);
        let n = NonZeroUsize::new(16).expect("non-zero");
        let take = connection
            .take_server_output(n, n)
            .expect_err("a driver without the capability refuses");
        assert_eq!(take.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn default_capabilities_support_nothing() {
        let capabilities = Capabilities::default();
        assert_eq!(capabilities, Capabilities::none());
        assert_eq!(capabilities.cancel(), CancelKind::Unsupported);
        assert!(!capabilities.can_interrupt_running_call());
        assert!(!capabilities.savepoints());
        assert!(!capabilities.named_binds());
        assert!(!capabilities.out_binds());
        assert!(!capabilities.ref_cursor());
        assert!(!capabilities.lob_streaming());
        assert!(!capabilities.tls());
        assert!(!capabilities.exact_transaction_state());
        assert!(!capabilities.error_position());
        assert!(!capabilities.server_output());
    }

    #[test]
    fn capabilities_are_opted_into_one_at_a_time() {
        // Private fields plus a builder: adding a capability must not break
        // every driver that constructs this.
        let capabilities = Capabilities::none()
            .with_cancel(CancelKind::Native)
            .with_savepoints(true)
            .with_named_binds(true)
            .with_out_binds(true)
            .with_ref_cursor(true)
            .with_lob_streaming(true)
            .with_tls(true)
            .with_exact_transaction_state(true)
            .with_error_position(true)
            .with_server_output(true);

        assert_eq!(capabilities.cancel(), CancelKind::Native);
        assert!(capabilities.server_output());
        assert!(capabilities.savepoints());
        assert!(capabilities.exact_transaction_state());
        assert!(capabilities.error_position());
        assert!(capabilities.can_interrupt_running_call());
    }

    #[test]
    fn only_a_native_cancel_can_stop_a_running_statement() {
        // The distinction the UI needs: a pre-armed deadline bounds a call in
        // advance but cannot interrupt one, so offering "Cancel" for it would be
        // a lie (`SPEC.md` §24.8).
        assert!(CancelKind::Native.interrupts_running_call());
        assert!(!CancelKind::PreArmedDeadline.interrupts_running_call());
        assert!(!CancelKind::Unsupported.interrupts_running_call());

        assert!(CancelKind::PreArmedDeadline.needs_deadline_armed_up_front());
        assert!(!CancelKind::Native.needs_deadline_armed_up_front());
        assert!(!CancelKind::Unsupported.needs_deadline_armed_up_front());

        assert!(
            !Capabilities::none()
                .with_cancel(CancelKind::PreArmedDeadline)
                .can_interrupt_running_call()
        );
    }

    #[test]
    fn a_cancel_that_cannot_work_says_so_instead_of_returning_bare_ok() {
        let requested = CancelOutcome::Requested;
        assert!(requested.is_requested());

        let impotent = CancelOutcome::NotInterruptible {
            deadline_remaining: Some(Duration::from_secs(12)),
        };
        assert!(
            !impotent.is_requested(),
            "the UI must not present this as a cancel in progress"
        );
        let CancelOutcome::NotInterruptible { deadline_remaining } = impotent else {
            panic!("expected NotInterruptible");
        };
        assert_eq!(deadline_remaining, Some(Duration::from_secs(12)));

        // No deadline armed and no interrupt possible: the statement runs to
        // completion, and the caller must not invent a time.
        let unbounded = CancelOutcome::NotInterruptible {
            deadline_remaining: None,
        };
        assert!(!unbounded.is_requested());
    }

    #[test]
    fn unknown_transaction_state_is_the_default_and_is_treated_as_open() {
        assert!(TransactionState::Active.may_be_open());
        assert!(TransactionState::Unknown.may_be_open());
        assert!(!TransactionState::Inactive.may_be_open());
        // The safe default: a connection nobody has classified must make the
        // core prompt, not discard (`SPEC.md` §10).
        assert_eq!(TransactionState::default(), TransactionState::Unknown);
        assert!(TransactionState::default().may_be_open());
    }
}
