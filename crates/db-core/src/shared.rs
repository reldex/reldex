//! State a [`crate::DatabaseSession`] handle and its worker thread share.
//!
//! Nothing here touches a driver. The worker updates this after every command
//! it processes (`docs/decisions/0002-driver-api-and-concurrency-model.md`);
//! the handle only ever reads it, which is why every method takes `&self`
//! and a plain [`std::sync::Mutex`] is enough — this is not a hot path.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use reldex_db_driver_api::{
    DbError, DbResult, ErrorKind, NativeError, SessionState, SqlPosition, StatementKind,
    TransactionState,
};

use crate::events::{EventSink, RequestSlots, SessionEvent};
use crate::ids::SessionId;
use crate::session::ServerOutputLog;

/// Where a session is in its lifecycle.
///
/// The driver's [`SessionState`] ladder (`Usable`/`NeedsValidation`/`Lost`)
/// describes a *connection*; it has no way to say "this session was closed on
/// purpose". Reporting a closed session as `Usable` — which is what happened
/// before this type existed — tells a caller it can still submit work, so the
/// core keeps its own four-state lifecycle and maps the driver's three rungs
/// onto it.
///
/// Deliberately **not** `#[non_exhaustive]`, for the same reason
/// [`SessionState`] is not (ADR-0002, amendment S1): it is a closed state
/// machine every call site must handle explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionLifecycle {
    /// The session is usable.
    Usable,
    /// The worker must `ping` before the next command runs.
    NeedsValidation,
    /// Terminal: the session is gone and is never silently replaced
    /// (`SPEC.md` §18). Takes precedence over [`SessionLifecycle::Closed`],
    /// because *why* a session ended matters more than that it ended.
    Lost,
    /// Terminal: the session was closed deliberately. Distinct from
    /// [`SessionLifecycle::Lost`] — nothing failed — but equally not usable.
    Closed,
}

impl SessionLifecycle {
    /// Whether work can still be submitted.
    #[must_use]
    pub const fn is_usable(self) -> bool {
        matches!(self, Self::Usable | Self::NeedsValidation)
    }

    /// Whether the session ended, for any reason.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Lost | Self::Closed)
    }
}

impl fmt::Display for SessionLifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Usable => "usable",
            Self::NeedsValidation => "needs-validation",
            Self::Lost => "lost",
            Self::Closed => "closed",
        };
        f.write_str(text)
    }
}

/// The rendered `source` chain of the error that lost a session.
///
/// [`DbError`] is not [`Clone`] and its `source` is a boxed trait object, so the
/// chain cannot be kept as-is for the fail-fast errors every later command gets.
/// Keeping its *rendering* as a real [`std::error::Error`] means the cause still
/// arrives through `Error::source`, where a UI already looks for it, instead of
/// being flattened into the message and lost.
#[derive(Debug)]
struct LostCause(String);

impl fmt::Display for LostCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LostCause {}

/// Everything worth keeping from the error that lost the session.
///
/// The previous implementation kept `error.to_string()` and rebuilt a generic
/// `ErrorKind::Connection` failure from it, so the UI could no longer tell a
/// network loss from a killed session from a driver bug, and the native
/// `ORA-nnnnn` code was reduced to a substring of a sentence.
#[derive(Debug)]
struct LostReason {
    kind: ErrorKind,
    message: String,
    native: Option<NativeError>,
    position: Option<SqlPosition>,
    cause: Option<String>,
}

impl LostReason {
    fn capture(error: &DbError) -> Self {
        Self {
            kind: error.kind(),
            message: error.message().to_owned(),
            native: error
                .native()
                .map(|native| NativeError::new(native.code(), native.message())),
            position: error.position().copied(),
            cause: std::error::Error::source(error).map(ToString::to_string),
        }
    }

    /// Rebuilds a reportable error, keeping the original classification.
    fn to_error(&self, context: &str) -> DbError {
        let mut error = DbError::new(
            self.kind,
            format!("reldex-db-core: {context}: {}", self.message),
        )
        .with_session_state(SessionState::Lost);
        if let Some(native) = &self.native {
            error = error.with_native(NativeError::new(native.code(), native.message()));
        }
        if let Some(position) = self.position {
            error = error.with_position(position);
        }
        if let Some(cause) = &self.cause {
            error = error.with_source(LostCause(cause.clone()));
        }
        error
    }
}

/// How a session ended — the one record every "this session is already over"
/// answer is derived from.
///
/// Recorded once, on the worker thread, at the point the session actually
/// ends. The distinction is the whole point: "a close has run" and "a close
/// succeeded" are not the same thing, and only the second one lets a later
/// close report success (`SPEC.md` §10, ADR-0002 K2/E7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EndedAs {
    /// An explicit [`crate::DatabaseSession::close`] ran on a live connection
    /// and succeeded: the disposition the caller chose was carried out and the
    /// connection was released. This is the only end that makes a later close
    /// the documented no-op success.
    Cleanly = 1,
    /// The session ended without a close resolving anything: it was lost, it
    /// was abandoned (which never commits and never rolls back explicitly,
    /// ADR-0002 K5), or the close itself failed. A later close is told so.
    Unresolved = 2,
}

impl EndedAs {
    /// The stored code for a session that has not ended.
    pub(crate) const NOT_ENDED: u8 = 0;

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Cleanly),
            2 => Some(Self::Unresolved),
            _ => None,
        }
    }
}

struct State {
    /// Where the session is in its lifecycle; see [`SessionLifecycle`].
    lifecycle: SessionLifecycle,
    /// Core-side conservative transaction tracking, derived from
    /// `StatementKind` and the driver's own report (ADR-0002 D4/D6): true once
    /// a statement that may have opened a transaction ran, false again after a
    /// commit/rollback or an implicit commit (DDL).
    core_possibly_active: bool,
    /// The driver's own last-reported [`TransactionState`], which may be
    /// [`TransactionState::Unknown`].
    driver_transaction_state: TransactionState,
    /// Why the session was lost, kept in full so the fail-fast errors every
    /// later command gets can say more than "something went wrong".
    lost_reason: Option<LostReason>,
}

impl State {
    /// The same answer [`SessionShared::has_possibly_active_transaction`]
    /// gives, computed while the lock is already held so that a mutator can
    /// tell whether it flipped.
    fn possibly_active(&self) -> bool {
        self.core_possibly_active || self.driver_transaction_state.may_be_open()
    }
}

/// State shared between a [`crate::DatabaseSession`] and its worker thread.
pub(crate) struct SessionShared {
    /// This session's id, so the events emitted here can name it without the
    /// caller having to pass it in.
    session: SessionId,
    state: Mutex<State>,
    /// How many commands the worker is currently inside. Kept outside the mutex
    /// so [`crate::DatabaseSession::cancel`] — which runs on a control path
    /// while the worker is blocked in driver code — never waits on a lock the
    /// worker might hold.
    in_flight: AtomicUsize,
    /// Where this session's events go, once a caller has bound a sink.
    ///
    /// Held across the push, which is what makes ordering rule 1 structural:
    /// the worker is one thread, and every *other* producer for this session —
    /// a submit whose command could not be delivered, an unanswered reply
    /// channel being dropped, and from M2.6 the registry — passes through this
    /// same lock. The waker is deliberately called *after* it is released.
    events: Mutex<Option<EventSink>>,
    /// Set the first time [`SessionShared::emit_terminal`] runs, so
    /// [`SessionEvent::Terminal`] is emitted exactly once however many paths
    /// observe the transition (ordering rule 3).
    terminal_emitted: AtomicBool,
    /// How this session ended, as an [`EndedAs`] code, or
    /// [`EndedAs::NOT_ENDED`] while it has not.
    ///
    /// The single authoritative record behind every idempotent close, on both
    /// reply paths. It deliberately says *how* rather than merely *whether*:
    /// only a close that actually ran to completion on a live connection makes
    /// a later close the documented no-op success, and a session that was lost
    /// or abandoned owes that later close the truth instead.
    ended: AtomicU8,
    /// Set by [`crate::SessionRegistry::abandon`] when it gives up on a connect
    /// that has not finished. It only ever happens *before* a connection
    /// exists, so there is no ambiguity about what it describes: the open was
    /// cancelled, and [`SessionShared::terminal_error`] says so with
    /// [`ErrorKind::Cancelled`] rather than the generic "this session is
    /// closed" a deliberate close produces.
    open_cancelled: AtomicBool,
    /// Set on the worker thread when a [`crate::CloseDisposition`] the caller
    /// chose actually succeeded, as part of the close that ends the session.
    ///
    /// This is what separates "the session ended and took an unresolved
    /// transaction with it" from "the user said commit (or rollback) and it
    /// worked". Only the second is not a loss, and only an explicit `close`
    /// can produce it — `Drop` and
    /// [`crate::SessionRegistry::abandon`] never resolve anything, which is
    /// exactly why they are lossy (ADR-0002 K5).
    transaction_resolved_at_end: AtomicBool,
    /// Set the moment anything asks this session to go away without a close —
    /// [`crate::SessionRegistry::abandon`], the registry's teardown, or the
    /// handle's `Drop` — so the worker stops reading server output between
    /// round trips instead of finishing a long drain nobody will see
    /// (ADR-0002 amendment T). Never cleared.
    abandon_requested: AtomicBool,
    /// Server output collected for requests answered through a
    /// [`crate::Completion`]; see [`ServerOutputLog`].
    collected_output: Mutex<ServerOutputLog>,
    /// The slots event-path requests hold: taken when a request is accepted,
    /// given back when the consumer takes its reply **out of** the queue. That
    /// is what makes [`crate::SessionLimits::max_outstanding_requests`] bound
    /// the queue itself and not merely what is in flight on the worker, so the
    /// counter is shared with the queue rather than owned here.
    requests: Arc<RequestSlots>,
}

impl SessionShared {
    pub(crate) fn new(session: SessionId) -> Self {
        Self {
            session,
            state: Mutex::new(State {
                lifecycle: SessionLifecycle::Usable,
                core_possibly_active: false,
                // `TransactionState::default()` is `Unknown` for the same
                // reason: nobody has classified this connection yet, so the
                // safe answer is "may be open" (ADR-0002, amendment S2).
                driver_transaction_state: TransactionState::default(),
                lost_reason: None,
            }),
            in_flight: AtomicUsize::new(0),
            events: Mutex::new(None),
            terminal_emitted: AtomicBool::new(false),
            ended: AtomicU8::new(EndedAs::NOT_ENDED),
            open_cancelled: AtomicBool::new(false),
            transaction_resolved_at_end: AtomicBool::new(false),
            abandon_requested: AtomicBool::new(false),
            collected_output: Mutex::new(ServerOutputLog::default()),
            requests: Arc::new(RequestSlots::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    // ------------------------------------------------------------- events

    /// Routes this session's events to `sink`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::DriverInternal`] if a sink is already bound. Rebinding is
    /// refused rather than silently honoured: replies already in flight are
    /// addressed to the first queue, so a second bind would split one
    /// session's stream across two consumers and break ordering rule 1.
    ///
    /// [`ErrorKind::Resource`] if this session has already announced its end.
    /// [`SessionEvent::Terminal`] is emitted once, at the transition, so a
    /// sink bound after it would receive a stream that never terminates —
    /// every later submit failing one by one with no event to say the session
    /// is gone. Refusing is the honest answer, and the caller already has
    /// [`SessionShared::lifecycle`] to see why.
    pub(crate) fn bind_events(&self, sink: EventSink) -> DbResult<()> {
        let mut slot = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            return Err(DbError::internal(
                "reldex-db-core: this session already routes its events to a queue; bind once, \
                 at open",
            ));
        }
        if self.terminal_emitted() {
            return Err(DbError::new(
                ErrorKind::Resource,
                "reldex-db-core: this session has already ended; its Terminal event was emitted \
                 before this queue was bound, so binding now would produce a stream that never \
                 terminates",
            ));
        }
        *slot = Some(sink);
        Ok(())
    }

    /// Whether a sink is bound, so a submit can refuse rather than accept a
    /// request whose reply would have nowhere to go.
    pub(crate) fn has_events(&self) -> bool {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// Emits one event, if this session routes events at all.
    ///
    /// The queue push happens under the emit lock (ordering rule 1); the
    /// waker runs after it is released, so no `db-core` lock is ever held
    /// while a consumer's callback runs.
    pub(crate) fn emit(&self, event: SessionEvent) {
        let wake = {
            let slot = self
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match slot.as_ref() {
                // Cloned only on the rare empty → non-empty edge, so the
                // ordinary path costs one uncontended lock and no refcount
                // traffic.
                Some(sink) if sink.push(event) => Some(sink.clone()),
                Some(_) | None => None,
            }
        };
        if let Some(sink) = wake {
            sink.wake();
        }
    }

    /// Emits a request's single reply.
    ///
    /// The slot the request reserved is handed to the queue along with the
    /// event, and released when the consumer takes it out again — that is what
    /// makes [`crate::SessionLimits::max_outstanding_requests`] bound the
    /// queue's contents. The slot is released **here** only when the queue did
    /// not take the event: there is no sink, or the consumer is gone.
    ///
    /// Every reply event goes through here, including the failures
    /// [`crate::reply::ReplyTo`]'s `Drop` synthesises; nothing else may emit a
    /// [`SessionEvent::is_reply`] event, or the accounting would drift.
    pub(crate) fn emit_reply(&self, event: SessionEvent) {
        let (release, wake) = {
            let slot = self
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match slot.as_ref() {
                Some(sink) => {
                    let pushed = sink.push_reply(event, &self.requests);
                    (!pushed.kept, pushed.wake.then(|| sink.clone()))
                }
                None => (true, None),
            }
        };
        if release {
            self.requests.release();
        }
        if let Some(sink) = wake {
            sink.wake();
        }
    }

    /// Reserves one outstanding-request slot, or refuses.
    ///
    /// The one synchronous failure in the event-path submit API: refusing here
    /// accepts nothing, so ordering rule 2 ("exactly one reply per accepted
    /// request") is untouched. Reporting it as an event would be circular —
    /// the queue is what is full.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Resource`] when `limit` requests are already outstanding.
    pub(crate) fn reserve_request(&self, limit: usize) -> DbResult<()> {
        self.requests.reserve(limit)
    }

    /// How many event-path requests are accepted and whose reply the consumer
    /// has not yet taken out of the queue.
    pub(crate) fn outstanding(&self) -> usize {
        self.requests.outstanding()
    }

    /// Records **how** this session ended, once. The first writer wins.
    ///
    /// Called on the worker thread at the point the session actually ends, and
    /// it is the only thing any later "this session is already over" answer is
    /// derived from — see [`SessionShared::settled_close`].
    pub(crate) fn mark_ended(&self, how: EndedAs) {
        let _ = self.ended.compare_exchange(
            EndedAs::NOT_ENDED,
            how as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// Whether this session has ended.
    pub(crate) fn has_ended(&self) -> bool {
        self.ended.load(Ordering::Acquire) != EndedAs::NOT_ENDED
    }

    /// How this session ended, or `None` while it has not.
    pub(crate) fn ended_as(&self) -> Option<EndedAs> {
        EndedAs::from_code(self.ended.load(Ordering::Acquire))
    }

    /// The answer a close owes when the session is **already over**, or `None`
    /// while it is not.
    ///
    /// Both idempotent-close paths go through this, so there is exactly one
    /// place that decides it and exactly one thing it is decided from: the
    /// record of how the session ended. Deriving it instead from "a close has
    /// run" made a close on a *lost* session report success once the first
    /// close had set that flag — the session really had ended, but nothing had
    /// been committed, which is the loss `SPEC.md` §10 forbids hiding.
    /// Idempotency answers "this session is already over", never "your commit
    /// happened".
    pub(crate) fn settled_close(&self) -> Option<DbResult<()>> {
        match self.ended_as()? {
            EndedAs::Cleanly => Some(Ok(())),
            EndedAs::Unresolved => Some(Err(self.lost_transaction_error())),
        }
    }

    /// Records that the open was given up on before the connect finished.
    ///
    /// Only [`crate::SessionRegistry::abandon`] (and the registry's own
    /// teardown) calls this, and only while the session is still `Opening`, so
    /// it describes exactly one thing: nobody is waiting for this connection
    /// any more. It is what turns the one reply the open owes into
    /// `OpenFailed { ErrorKind::Cancelled }` — including when that reply is
    /// produced by [`crate::reply::ReplyTo`]'s `Drop` rather than by an
    /// explicit answer, which is why the reason lives here rather than at the
    /// call site.
    pub(crate) fn mark_open_cancelled(&self) {
        self.open_cancelled.store(true, Ordering::Release);
    }

    /// Emits [`SessionEvent::Terminal`], at most once for this session.
    ///
    /// Called at the transition, by whichever path observed it. The lifecycle
    /// reported is whatever the session has reached by then, so a session that
    /// was lost and then closed says `Lost`.
    ///
    /// `transaction_possibly_lost` is the session's final word on whether it
    /// took an unresolved transaction with it; see
    /// [`SessionShared::transaction_lost_at_end`] for who may compute it and
    /// when.
    pub(crate) fn emit_terminal(&self, transaction_possibly_lost: bool) {
        if self
            .terminal_emitted
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let (lifecycle, cause) = {
            let state = self.lock();
            let cause = state
                .lost_reason
                .as_ref()
                .map(|reason| reason.to_error("the session was lost"));
            (state.lifecycle, cause)
        };
        self.emit(SessionEvent::Terminal {
            session: self.session,
            lifecycle,
            cause,
            transaction_possibly_lost,
        });
    }

    /// Whether [`SessionShared::emit_terminal`] has already run.
    pub(crate) fn terminal_emitted(&self) -> bool {
        self.terminal_emitted.load(Ordering::Acquire)
    }

    /// Runs `f` on the shared state and emits
    /// [`SessionEvent::TransactionStateChanged`] if it flipped the answer
    /// [`SessionShared::has_possibly_active_transaction`] gives.
    ///
    /// The comparison happens inside the lock and the emit outside it, so the
    /// state lock is never held while the queue's is taken.
    fn with_transaction_watch<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let (out, changed) = {
            let mut state = self.lock();
            let before = state.possibly_active();
            let out = f(&mut state);
            let after = state.possibly_active();
            (out, (before != after).then_some(after))
        };
        if let Some(possibly_active) = changed {
            self.emit(SessionEvent::TransactionStateChanged {
                session: self.session,
                possibly_active,
            });
        }
        out
    }

    /// Where the session is in its lifecycle, as of the last command the worker
    /// processed.
    pub(crate) fn lifecycle(&self) -> SessionLifecycle {
        self.lock().lifecycle
    }

    /// Whether the session is in the terminal `Lost` state.
    pub(crate) fn is_lost(&self) -> bool {
        self.lock().lifecycle == SessionLifecycle::Lost
    }

    /// Whether the worker believes it must `ping` before the next command.
    pub(crate) fn needs_validation(&self) -> bool {
        self.lock().lifecycle == SessionLifecycle::NeedsValidation
    }

    /// A successful `ping` clears `NeedsValidation` back to `Usable`. Never
    /// used to clear `Lost` or `Closed`: `SPEC.md` §18 forbids silently
    /// resurrecting a session once it is gone.
    pub(crate) fn mark_validated(&self) {
        let mut state = self.lock();
        if state.lifecycle == SessionLifecycle::NeedsValidation {
            state.lifecycle = SessionLifecycle::Usable;
        }
    }

    /// Records that the session was closed deliberately.
    ///
    /// `Lost` wins: a session that failed and was then closed still reports why
    /// it failed.
    pub(crate) fn mark_closed(&self) {
        let mut state = self.lock();
        if state.lifecycle != SessionLifecycle::Lost {
            state.lifecycle = SessionLifecycle::Closed;
        }
    }

    /// Marks the session terminally lost, capturing why for later fail-fast
    /// errors. Used when a revalidating `ping` itself fails.
    pub(crate) fn mark_lost_from(&self, error: &DbError) {
        let mut state = self.lock();
        state.lifecycle = SessionLifecycle::Lost;
        state.lost_reason = Some(LostReason::capture(error));
    }

    /// Applies the [`SessionState`] a [`DbError`] reported: worsens the
    /// tracked state, never improves it, and only [`SessionShared::mark_validated`]
    /// (a successful `ping`) moves it back toward `Usable`.
    pub(crate) fn note_error(&self, error: &DbError) {
        let mut state = self.lock();
        if state.lifecycle == SessionLifecycle::Lost {
            return;
        }
        match error.session_state() {
            SessionState::Usable => {}
            SessionState::NeedsValidation => {
                if state.lifecycle == SessionLifecycle::Usable {
                    state.lifecycle = SessionLifecycle::NeedsValidation;
                }
            }
            SessionState::Lost => {
                state.lifecycle = SessionLifecycle::Lost;
                state.lost_reason = Some(LostReason::capture(error));
            }
        }
    }

    /// Core-side conservative update after a statement executed successfully
    /// (ADR-0002 D4/D6).
    ///
    /// `Dml` and `PlSqlBlock` always mark the transaction possibly active.
    /// Everything else — `Query`, `TransactionControl`, `SessionControl`,
    /// `Other` — does too, **unless** the driver reports exact transaction
    /// state and says [`TransactionState::Inactive`] straight afterwards. That
    /// asymmetry is the point: `SELECT … FOR UPDATE`, `SET TRANSACTION` and
    /// `LOCK TABLE` all open a transaction, and neither the core (which must
    /// not parse SQL) nor a `StatementKind` can tell them from a plain
    /// `SELECT`. An extra prompt is acceptable; a silent commit is not
    /// (`SPEC.md` §10).
    ///
    /// No kind ever *clears* the flag. Only a commit, a rollback or an implicit
    /// commit does, through [`SessionShared::note_commit_or_rollback`].
    pub(crate) fn note_statement(
        &self,
        kind: StatementKind,
        driver_state: TransactionState,
        exact: bool,
    ) {
        self.with_transaction_watch(|state| {
            state.driver_transaction_state = driver_state;
            let definitely_inactive = exact && driver_state == TransactionState::Inactive;
            let opens = match kind {
                StatementKind::Dml | StatementKind::PlSqlBlock => true,
                // `StatementKind` is `#[non_exhaustive]`; an unknown kind is
                // treated like `Other`, which is the conservative arm anyway.
                _ => !definitely_inactive,
            };
            if opens {
                state.core_possibly_active = true;
            }
        });
    }

    /// A successful `commit`/`rollback`, or a statement that committed
    /// implicitly (DDL), resolves the transaction.
    pub(crate) fn note_commit_or_rollback(&self) {
        self.with_transaction_watch(|state| state.core_possibly_active = false);
    }

    /// Records the driver's own last-reported transaction state.
    pub(crate) fn note_driver_transaction_state(&self, driver_state: TransactionState) {
        self.with_transaction_watch(|state| state.driver_transaction_state = driver_state);
    }

    /// Records the driver's transaction state at connect time, **without**
    /// announcing it.
    ///
    /// [`SessionEvent::TransactionStateChanged`] reports a *flip* of
    /// [`SessionShared::has_possibly_active_transaction`]
    /// (`docs/exec-plans/active/phase-1.md` §B2), and a session that did not
    /// exist a moment ago has not flipped anything: this is its initial value.
    /// Announcing it would also put an unsolicited event **before** that
    /// session's [`SessionEvent::Opened`], which is the one thing a consumer
    /// should be able to treat as a session's first word.
    ///
    /// A consumer therefore takes the initial value from
    /// [`crate::DatabaseSession::has_possibly_active_transaction`] when it sees
    /// `Opened` — which is authoritative anyway — and the event stream reports
    /// every change from there. Assuming the conservative default until it
    /// asks costs at most an extra close prompt, which ADR-0002 K7 already
    /// accepts; the opposite mistake is a silent commit, which it does not.
    pub(crate) fn seed_driver_transaction_state(&self, driver_state: TransactionState) {
        self.lock().driver_transaction_state = driver_state;
    }

    /// Whether a transaction may still be open, combining the driver's own
    /// report (which may be [`TransactionState::Unknown`]) with core-side
    /// tracking, per ADR-0002.
    pub(crate) fn has_possibly_active_transaction(&self) -> bool {
        self.lock().possibly_active()
    }

    /// Records that the [`crate::CloseDisposition`] the caller chose succeeded,
    /// as part of the close that is ending this session.
    ///
    /// Only [`crate::worker::Worker::close`] calls this, on the worker thread,
    /// after the disposition returned `Ok`. It is what makes a deliberate
    /// `close(Commit)` — or a deliberate `close(Rollback)` — *not* a loss: the
    /// user said what should happen to the transaction and it happened.
    pub(crate) fn mark_transaction_resolved_at_end(&self) {
        self.transaction_resolved_at_end
            .store(true, Ordering::Release);
    }

    /// The value [`SessionEvent::Terminal`] carries as
    /// `transaction_possibly_lost`.
    ///
    /// **Only the worker thread may call this, and only at the point the
    /// session actually ends** — after every command queued ahead of the close
    /// has run. Anywhere else the answer is a snapshot that a queued statement
    /// can still invalidate, which is exactly the under-reporting this field
    /// exists to remove: `abandon` sees "no transaction", the worker then runs
    /// a queued `INSERT`, and the server rolls it back at close.
    ///
    /// Conservative in the only direction that matters (`SPEC.md` §10,
    /// ADR-0002 K7): a driver that cannot rule a transaction out reports
    /// [`TransactionState::Unknown`], which reads as "may be open", so the
    /// answer is `true`. A session that never opened has nothing to lose and is
    /// reported `false` by its caller rather than here.
    pub(crate) fn transaction_lost_at_end(&self) -> bool {
        !self.transaction_resolved_at_end.load(Ordering::Acquire)
            && self.has_possibly_active_transaction()
    }

    /// The worker is about to run a command, so a cancel has something to aim
    /// at.
    pub(crate) fn enter_driver_call(&self) {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
    }

    /// The worker finished a command.
    pub(crate) fn leave_driver_call(&self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }

    /// Whether the worker is currently inside a command.
    ///
    /// Read by [`crate::DatabaseSession::cancel`] so a cancel issued when
    /// nothing is running is answered as the idempotent no-op the contract
    /// allows, instead of being handed to a driver that may latch it onto the
    /// *next* statement. It narrows that race; it cannot close it — see
    /// `DatabaseSession::cancel`.
    pub(crate) fn driver_call_in_flight(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) > 0
    }

    /// Records that something asked this session to go away without a close.
    ///
    /// Read by the worker between server-output reads, so a drain stops at
    /// the next round-trip boundary rather than running to the end of a large
    /// buffer for a session nobody is listening to any more.
    pub(crate) fn request_abandon(&self) {
        self.abandon_requested.store(true, Ordering::Release);
    }

    /// Whether [`SessionShared::request_abandon`] has run.
    pub(crate) fn abandon_requested(&self) -> bool {
        self.abandon_requested.load(Ordering::Acquire)
    }

    /// Appends one read's worth of server output to the completion-path log,
    /// within its bounds; see [`ServerOutputLog`].
    ///
    /// Lines past the bound are refused, newest first, and counted — the same
    /// "keep what is already promised, count the rest" rule the event queue
    /// applies, because the start of a run is where an error usually is.
    pub(crate) fn collect_server_output(&self, lines: Vec<Box<str>>, failure: Option<DbError>) {
        let mut log = self
            .collected_output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut bytes: usize = log.lines.iter().map(|line| line.len()).sum();
        for line in lines {
            let fits = log.lines.len() < ServerOutputLog::MAX_RETAINED_LINES
                && bytes.saturating_add(line.len()) <= ServerOutputLog::MAX_RETAINED_BYTES;
            if fits {
                bytes += line.len();
                log.lines.push(line);
            } else {
                log.dropped = log.dropped.saturating_add(1);
            }
        }
        if let Some(failure) = failure {
            log.failures = log.failures.saturating_add(1);
            if log.failure.is_none() {
                log.failure = Some(failure);
            }
        }
    }

    /// Takes everything the completion-path log holds, leaving it empty.
    pub(crate) fn take_collected_server_output(&self) -> ServerOutputLog {
        std::mem::take(
            &mut *self
                .collected_output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// The error every command gets once the session is lost or closed.
    ///
    /// For a lost session this keeps the original [`ErrorKind`], native code
    /// and text, statement position and cause, so the UI can show *why* the
    /// session went away rather than a flattened sentence. A session whose
    /// *open* was abandoned reports [`ErrorKind::Cancelled`]: nothing failed
    /// and nothing was ever connected, so calling it a connection error would
    /// be wrong in both directions.
    pub(crate) fn terminal_error(&self) -> DbError {
        let state = self.lock();
        match &state.lost_reason {
            Some(reason) => reason.to_error("session is lost; open a new session to reconnect"),
            None if self.open_cancelled.load(Ordering::Acquire) => DbError::new(
                ErrorKind::Cancelled,
                "reldex-db-core: the session was abandoned before its connection was open, so \
                 nothing was connected and nothing is adopted late",
            )
            .with_session_state(SessionState::Lost),
            None => DbError::new(ErrorKind::Connection, "reldex-db-core: session is closed")
                .with_session_state(SessionState::Lost),
        }
    }

    /// The error [`crate::DatabaseSession::close`] reports when the connection
    /// is already gone.
    ///
    /// Closing a lost session is not a silent success: whatever transaction it
    /// held went with it, and a `Commit` disposition in particular committed
    /// nothing. Saying so is the whole point (`SPEC.md` §10, §18).
    pub(crate) fn lost_transaction_error(&self) -> DbError {
        let state = self.lock();
        match &state.lost_reason {
            Some(reason) => reason.to_error(
                "the session was already lost, so nothing was committed and any \
                 transaction it held is gone",
            ),
            None => DbError::new(
                ErrorKind::Connection,
                "reldex-db-core: the connection was already gone, so nothing was committed \
                 and any transaction it held is gone",
            )
            .with_session_state(SessionState::Lost),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_usable_with_no_possibly_active_transaction() {
        let shared = SessionShared::new(SessionId::allocate());
        assert_eq!(shared.lifecycle(), SessionLifecycle::Usable);
        assert!(!shared.is_lost());
        // `TransactionState::default()` is `Unknown`, which `may_be_open()`.
        assert!(shared.has_possibly_active_transaction());
    }

    #[test]
    fn dml_marks_possibly_active_until_commit_or_rollback() {
        let shared = SessionShared::new(SessionId::allocate());
        shared.note_driver_transaction_state(TransactionState::Inactive);
        assert!(!shared.has_possibly_active_transaction());
        shared.note_statement(StatementKind::Dml, TransactionState::Active, true);
        assert!(shared.has_possibly_active_transaction());
        shared.note_commit_or_rollback();
        shared.note_driver_transaction_state(TransactionState::Inactive);
        assert!(!shared.has_possibly_active_transaction());
    }

    #[test]
    fn a_query_is_only_dismissed_when_an_exact_driver_says_inactive() {
        // A plain SELECT on an exact driver: nothing is open.
        let shared = SessionShared::new(SessionId::allocate());
        shared.note_statement(StatementKind::Query, TransactionState::Inactive, true);
        assert!(!shared.has_possibly_active_transaction());

        // `SELECT … FOR UPDATE` on the same driver: the driver reports the lock,
        // and the core must keep the flag.
        let shared = SessionShared::new(SessionId::allocate());
        shared.note_statement(StatementKind::Query, TransactionState::Active, true);
        assert!(shared.has_possibly_active_transaction());

        // The same query on a driver that cannot observe transaction state:
        // conservative, always.
        let shared = SessionShared::new(SessionId::allocate());
        shared.note_statement(StatementKind::Query, TransactionState::Unknown, false);
        assert!(shared.has_possibly_active_transaction());
        shared.note_commit_or_rollback();
        shared.note_driver_transaction_state(TransactionState::Inactive);
        assert!(!shared.has_possibly_active_transaction());
    }

    #[test]
    fn needs_validation_recovers_on_a_successful_ping_but_not_from_lost() {
        let shared = SessionShared::new(SessionId::allocate());
        shared.note_error(&DbError::new(ErrorKind::Timeout, "slow"));
        assert!(shared.needs_validation());
        shared.mark_validated();
        assert_eq!(shared.lifecycle(), SessionLifecycle::Usable);

        shared.note_error(&DbError::new(ErrorKind::NetworkLost, "gone"));
        assert!(shared.is_lost());
        shared.mark_validated();
        assert!(shared.is_lost(), "a successful ping must never clear Lost");
        shared.mark_closed();
        assert!(shared.is_lost(), "closing a lost session must not hide why");
    }

    #[test]
    fn closing_is_a_state_of_its_own_and_is_never_usable() {
        let shared = SessionShared::new(SessionId::allocate());
        shared.mark_closed();
        assert_eq!(shared.lifecycle(), SessionLifecycle::Closed);
        assert!(!shared.lifecycle().is_usable());
        assert!(shared.lifecycle().is_terminal());
        assert!(!shared.is_lost());
    }

    #[test]
    fn terminal_error_keeps_the_classification_of_the_loss() {
        let shared = SessionShared::new(SessionId::allocate());
        let closed = shared.terminal_error();
        assert!(closed.to_string().contains("closed"));
        assert_eq!(closed.kind(), ErrorKind::Connection);

        shared.note_error(
            &DbError::new(ErrorKind::NetworkLost, "connection reset")
                .with_native(NativeError::new(3113, "ORA-03113: end-of-file on channel")),
        );
        let lost = shared.terminal_error();
        assert_eq!(lost.session_state(), SessionState::Lost);
        assert_eq!(
            lost.kind(),
            ErrorKind::NetworkLost,
            "the UI must still be able to tell a network loss from a driver bug"
        );
        assert_eq!(lost.native().map(NativeError::code), Some(3113));
        assert!(lost.to_string().contains("lost"));
    }

    #[test]
    fn in_flight_tracking_is_a_plain_counter() {
        let shared = SessionShared::new(SessionId::allocate());
        assert!(!shared.driver_call_in_flight());
        shared.enter_driver_call();
        assert!(shared.driver_call_in_flight());
        shared.leave_driver_call();
        assert!(!shared.driver_call_in_flight());
    }
}
