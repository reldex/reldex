//! The outbound event path: a shared, unbounded FIFO of typed
//! [`SessionEvent`]s with an edge-triggered [`Waker`]
//! (`docs/exec-plans/active/phase-1.md` §B2; ADR-0002's "a per-session
//! outbound completion/event queue for the FFI adapter", ADR-0003 D5).
//!
//! # Why this exists
//!
//! [`crate::Completion`] hands one reply to one waiting caller, which is the
//! right shape for a test or a blocking tool and the wrong shape for a UI: an
//! event loop that wants N replies has to park N threads on them (ADR-0003 A5
//! measured the cost — one extra thread per open session). This module is the
//! other shape: every worker pushes its replies, as plain data, into **one**
//! queue, and the consumer is told once, from a worker thread, that the queue
//! stopped being empty.
//!
//! Both shapes are the same [`crate::worker::Command`] with a different
//! `ReplyTo`, so nothing about a request's semantics depends on which one the
//! caller chose.
//!
//! # Thread safety
//!
//! * [`EventSink`] is `Send + Sync + Clone`. Workers hold it; cloning is an
//!   atomic increment.
//! * [`EventQueue`] is `Send` and deliberately **not** `Sync`: there is one
//!   consumer. Moving it to another thread is fine; draining it from two at
//!   once is not, and the type system says so.
//! * [`SessionEvent`] is `Send`. By construction it carries only plain data
//!   and core-owned handles — a [`crate::FetchedBatch`] has already had its
//!   large-object locators parked on the worker thread — so ADR-0002's K1
//!   ("only plain data crosses threads") holds for this path exactly as it
//!   does for [`crate::Completion`].
//!
//! # Ordering guarantees
//!
//! These are the contract a consumer may rely on; each has a test named after
//! it in `crates/db-core/tests/event_ordering.rs`.
//!
//! 1. **Per session, events are delivered in the order they were produced.**
//!    One session has one worker thread, and every other producer for that
//!    session (a submit whose command could not be delivered, and — from M2.6
//!    — the registry) takes the same per-session emit lock before touching the
//!    queue. The queue itself is a single FIFO under one mutex, so a session's
//!    subsequence of it is exactly its production order.
//! 2. **Every accepted request produces exactly one reply event** — never
//!    zero, never two. The reply channel is consumed by answering it, and if
//!    it is *dropped* unanswered (the worker exited, the command could not be
//!    delivered, a core bug unwound the worker) its `Drop` emits the one
//!    failure instead. "Accepted" means the submit returned `Ok`; the one
//!    synchronous failure — [`crate::SessionLimits::max_outstanding_requests`]
//!    — accepts nothing and produces no event.
//! 3. **[`SessionEvent::Terminal`] is delivered exactly once per session**,
//!    after the reply of every request that was queued when the transition was
//!    observed. A request submitted after that still gets its one failure
//!    reply, which may follow `Terminal`.
//! 4. **[`SessionEvent::Executing`] precedes its `Executed`** and follows any
//!    earlier request's reply on that session: the worker emits it as the
//!    first thing it does for that command, before the driver call.
//! 5. **No ordering is promised across sessions.** Sessions interleave
//!    freely; a session whose worker is blocked delays only itself.
//!
//! # Back-pressure
//!
//! The queue is unbounded, and what bounds it is stated rather than hoped for:
//!
//! * **Reply events** are bounded by the number of outstanding requests, which
//!   is bounded by [`crate::SessionLimits::max_outstanding_requests`]. Going
//!   over it is the one synchronous failure the submit API has
//!   ([`reldex_db_driver_api::ErrorKind::Resource`]) — reporting it as an
//!   event would be circular.
//! * **Unsolicited events** ([`SessionEvent::ServerOutput`],
//!   [`SessionEvent::TransactionStateChanged`]) are bounded per session by
//!   [`EventCaps::max_unsolicited_per_session`]. A terminal reply is never
//!   dropped, and neither is [`SessionEvent::Terminal`] or
//!   [`SessionEvent::Executing`] — see [`EventCaps`] for the exact policy and
//!   for how a drop is reported rather than hidden.

use std::cell::Cell;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use reldex_db_driver_api::{CancelKind, ConnectionId, DbError, DbResult, Warning};

use crate::ids::{LobHandle, ResultId, SessionId};
use crate::session::{CloseError, ExecuteOutcome, FetchedBatch};
use crate::shared::SessionLifecycle;

/// A caller-chosen correlation id, opaque to the core.
///
/// The core never interprets it, never allocates one and never reuses one on
/// the caller's behalf: it is echoed on the single reply event for the request
/// it was submitted with, which is how an adapter finds the object that asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(pub u64);

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RequestId#{}", self.0)
    }
}

/// Told, once, that the queue stopped being empty.
///
/// # Contract
///
/// * **Edge-triggered and coalesced.** [`Waker::wake`] is called only on the
///   queue's empty → non-empty transition, so a burst produces one call. A
///   consumer that stops draining while [`EventQueue::next`] is still
///   returning `Some` gets no further wake and must re-post its own drain.
/// * **It must return promptly and must not block.** It is called on a
///   session's worker thread, and blocking there stalls that session.
/// * **It must not call back into `db-core`.** Nothing here is re-entrant; a
///   wake is never issued from inside [`EventQueue::next`] or
///   [`EventQueue::drain_into`], and the wake runs while the waker
///   registration is read-locked so that [`EventQueue::set_waker`] can wait
///   for it.
/// * **It should not panic.** One that does is caught and counted
///   ([`EventQueue::waker_panics`]) rather than being allowed to poison the
///   queue or kill the worker that was only trying to report a row; the waker
///   stays registered. A panic that escaped here would eventually unwind
///   across an FFI frame, which is undefined behaviour.
///
/// # Thread safety
///
/// `Send + Sync`: [`Waker::wake`] is called from whichever worker thread
/// happened to fill an empty queue, and from more than one over time.
pub trait Waker: Send + Sync {
    /// Called when the queue goes from empty to non-empty. See the trait's
    /// contract.
    fn wake(&self);
}

/// What a queue is allowed to accumulate.
///
/// Only *unsolicited* events are capped here. Reply events are capped by
/// [`crate::SessionLimits::max_outstanding_requests`] on the submitting side,
/// because a reply that was refused after the request was accepted would break
/// ordering rule 2.
///
/// # The drop policy, exactly
///
/// Per session, at most [`EventCaps::max_unsolicited_per_session`] unsolicited
/// events may be waiting in the queue at once. At the cap:
///
/// * [`SessionEvent::TransactionStateChanged`] is **coalesced, never dropped**:
///   it reports a *state*, not an occurrence, so an undelivered one for that
///   session is updated in place to the newer value. Nothing is lost — the
///   consumer always ends up with the most recent value the worker computed —
///   and this class therefore cannot reach the cap at all. The one thing it
///   costs is position: a coalesced value is delivered at the earlier of the
///   two slots. It is advisory (`close` re-decides on the worker, ADR-0002 K4),
///   and [`crate::DatabaseSession::has_possibly_active_transaction`] is always
///   authoritative.
/// * [`SessionEvent::ServerOutput`] is **dropped, and the drop is counted**.
///   The incoming event is the one dropped, so what is already queued — where
///   a PL/SQL error usually is — survives. Its line count is added to that
///   session's pending drop count and reported as `dropped` on the next
///   `ServerOutput` that session actually delivers, so the UI can say "output
///   truncated" instead of silently lying. [`EventQueue::dropped_unsolicited`]
///   counts the refused *events* for diagnostics, and never resets.
///
/// A reply event, [`SessionEvent::Executing`] and [`SessionEvent::Terminal`]
/// are never dropped and never coalesced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventCaps {
    max_unsolicited_per_session: NonZeroUsize,
}

impl EventCaps {
    /// How many unsolicited events one session may have waiting before the
    /// policy in [`EventCaps`] applies.
    ///
    /// 256 is chosen to be far above what a draining consumer ever holds (the
    /// adapter's budgeted drain is 256 events *in total*, ADR-0003 D5) and far
    /// below anything that could hide a runaway `DBMS_OUTPUT` loop.
    pub const DEFAULT_MAX_UNSOLICITED_PER_SESSION: NonZeroUsize = match NonZeroUsize::new(256) {
        Some(value) => value,
        None => unreachable!(),
    };

    /// The default caps.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_unsolicited_per_session: Self::DEFAULT_MAX_UNSOLICITED_PER_SESSION,
        }
    }

    /// Sets how many unsolicited events one session may have waiting.
    #[must_use]
    pub const fn with_max_unsolicited_per_session(mut self, events: NonZeroUsize) -> Self {
        self.max_unsolicited_per_session = events;
        self
    }

    /// How many unsolicited events one session may have waiting.
    #[must_use]
    pub const fn max_unsolicited_per_session(self) -> NonZeroUsize {
        self.max_unsolicited_per_session
    }
}

impl Default for EventCaps {
    fn default() -> Self {
        Self::new()
    }
}

/// Which operation a [`SessionEvent::Completed`] is the reply to.
///
/// §B2 folds six operations into one event because they all answer
/// "`Ok(())` or why not". Naming which one — and, for the two that release a
/// handle, *which* handle — is what keeps an adapter from having to keep its
/// own request-to-operation map, and is the core-side form of ADR-0003 A21
/// ("a reply names what it was about, on the failure path too").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompletedOperation {
    /// [`crate::DatabaseSession::submit_commit`].
    Commit,
    /// [`crate::DatabaseSession::submit_rollback`].
    Rollback,
    /// [`crate::DatabaseSession::submit_savepoint`].
    Savepoint,
    /// [`crate::DatabaseSession::submit_rollback_to_savepoint`].
    RollbackToSavepoint,
    /// [`crate::DatabaseSession::submit_ping`].
    Ping,
    /// [`crate::DatabaseSession::submit_close_result`]. The id names what
    /// **ended**: it is no longer valid by the time this event is drained.
    CloseResult(ResultId),
    /// [`crate::DatabaseSession::submit_close_lob`]. As above.
    CloseLob(LobHandle),
}

/// One thing that happened on one session.
///
/// `#[non_exhaustive]`: M2.6 (the session registry) produces
/// [`SessionEvent::Opened`]/[`SessionEvent::OpenFailed`] and M2.7 produces
/// [`SessionEvent::ServerOutput`]; a consumer must already handle a variant it
/// does not know.
///
/// Every variant names its session. Every variant that answers a request also
/// names that request, and there is exactly one such event per accepted
/// request (ordering rule 2).
#[derive(Debug)]
#[non_exhaustive]
pub enum SessionEvent {
    // --- exactly one of these per accepted request, ever ---
    /// The session's connection is open and usable.
    ///
    /// Produced by the session registry (M2.6); `db-core` has no other
    /// non-blocking open today.
    Opened {
        /// The session that opened.
        session: SessionId,
        /// The request that asked for it.
        request: RequestId,
        /// The driver connection the session owns.
        connection: ConnectionId,
        /// What a cancel on this session can actually do (`SPEC.md` §24.8).
        cancel_kind: CancelKind,
        /// Non-fatal findings the driver reported while connecting
        /// (ADR-0002 W1/W2).
        warnings: Vec<Warning>,
    },
    /// The connect failed, or was abandoned before it finished (M2.6).
    OpenFailed {
        /// The session id that was allocated and is now dead.
        session: SessionId,
        /// The request that asked for it.
        request: RequestId,
        /// Why.
        error: DbError,
    },
    /// The reply to [`crate::DatabaseSession::submit_execute`].
    Executed {
        /// The session.
        session: SessionId,
        /// The request.
        request: RequestId,
        /// What the statement produced, or why it failed. On success
        /// [`ExecuteOutcome::result`] names the result set it opened and
        /// [`ExecuteOutcome::columns`] describes its columns — available here,
        /// before any row, which is what a grid needs for its header.
        outcome: DbResult<ExecuteOutcome>,
    },
    /// The reply to [`crate::DatabaseSession::submit_fetch`].
    Fetched {
        /// The session.
        session: SessionId,
        /// The request.
        request: RequestId,
        /// Which result was fetched — set on the failure path too, because
        /// "which result could not be read" is exactly what an error report
        /// needs (ADR-0003 A21).
        result: ResultId,
        /// The rows, or why they could not be read. An empty batch means the
        /// result is exhausted.
        batch: DbResult<FetchedBatch>,
    },
    /// The reply to [`crate::DatabaseSession::submit_read_lob_chunk`].
    LobChunk {
        /// The session.
        session: SessionId,
        /// The request.
        request: RequestId,
        /// Which large object was read; set on the failure path too.
        lob: LobHandle,
        /// The bytes, or why they could not be read. An empty `Vec` means the
        /// object is exhausted.
        bytes: DbResult<Vec<u8>>,
    },
    /// The reply to the six operations that answer only "did it work?"; see
    /// [`CompletedOperation`].
    Completed {
        /// The session.
        session: SessionId,
        /// The request.
        request: RequestId,
        /// Which operation this answers.
        operation: CompletedOperation,
        /// Whether it worked.
        result: DbResult<()>,
    },
    /// The reply to [`crate::DatabaseSession::submit_close`].
    ///
    /// Three of [`CloseError`]'s four variants leave the session **open**, so
    /// this is not necessarily the end of it; `SessionEvent::Terminal` is.
    SessionClosed {
        /// The session.
        session: SessionId,
        /// The request.
        request: RequestId,
        /// What the close did — see [`CloseError::session_is_still_open`].
        result: Result<(), CloseError>,
    },

    // --- progress / unsolicited ---
    /// The worker has **started** this statement: it is no longer queued, and
    /// `deadline` is the limit actually armed on it.
    ///
    /// This is what an honest UI shows on a driver that cannot interrupt a
    /// running statement — "running, with this limit" rather than a Cancel
    /// button that cannot work.
    Executing {
        /// The session.
        session: SessionId,
        /// The request whose statement started.
        request: RequestId,
        /// The deadline the statement carried
        /// ([`reldex_db_driver_api::Statement::with_deadline`]), echoed so the
        /// UI shows the real limit rather than the one it hoped for.
        deadline: Option<Duration>,
    },
    /// Lines the server produced out of band (M2.7).
    ServerOutput {
        /// The session.
        session: SessionId,
        /// The lines, in order.
        lines: Vec<Box<str>>,
        /// How many lines were dropped for this session since the previous
        /// delivered `ServerOutput`, because the session was at
        /// [`EventCaps::max_unsolicited_per_session`]. Zero normally; non-zero
        /// means the UI must say "output truncated".
        dropped: u32,
    },
    /// [`crate::DatabaseSession::has_possibly_active_transaction`] flipped.
    ///
    /// Advisory, so that a worksheet's Commit/Rollback affordances do not have
    /// to poll. `close` still re-decides on the worker after the queue drains
    /// (ADR-0002 K4) and can still answer [`CloseError::DecisionRequired`].
    TransactionStateChanged {
        /// The session.
        session: SessionId,
        /// The new value.
        possibly_active: bool,
    },

    /// The session ended. Exactly once per session, at the transition; never
    /// twice, never absent (ordering rule 3).
    Terminal {
        /// The session.
        session: SessionId,
        /// [`SessionLifecycle::Lost`] or [`SessionLifecycle::Closed`]. `Lost`
        /// wins when both apply, because *why* a session ended matters more
        /// than that it ended.
        lifecycle: SessionLifecycle,
        /// The failure that lost the session, with its original kind, native
        /// code and cause chain. `None` for a deliberate close.
        cause: Option<DbError>,
    },
}

impl SessionEvent {
    /// The session this event belongs to.
    #[must_use]
    pub const fn session(&self) -> SessionId {
        match self {
            Self::Opened { session, .. }
            | Self::OpenFailed { session, .. }
            | Self::Executed { session, .. }
            | Self::Fetched { session, .. }
            | Self::LobChunk { session, .. }
            | Self::Completed { session, .. }
            | Self::SessionClosed { session, .. }
            | Self::Executing { session, .. }
            | Self::ServerOutput { session, .. }
            | Self::TransactionStateChanged { session, .. }
            | Self::Terminal { session, .. } => *session,
        }
    }

    /// The request this event refers to, if any.
    #[must_use]
    pub const fn request(&self) -> Option<RequestId> {
        match self {
            Self::Opened { request, .. }
            | Self::OpenFailed { request, .. }
            | Self::Executed { request, .. }
            | Self::Fetched { request, .. }
            | Self::LobChunk { request, .. }
            | Self::Completed { request, .. }
            | Self::SessionClosed { request, .. }
            | Self::Executing { request, .. } => Some(*request),
            Self::ServerOutput { .. }
            | Self::TransactionStateChanged { .. }
            | Self::Terminal { .. } => None,
        }
    }

    /// Whether this is *the* reply to its request — the one event ordering
    /// rule 2 promises. [`SessionEvent::Executing`] is progress, not a reply.
    #[must_use]
    pub const fn is_reply(&self) -> bool {
        matches!(
            self,
            Self::Opened { .. }
                | Self::OpenFailed { .. }
                | Self::Executed { .. }
                | Self::Fetched { .. }
                | Self::LobChunk { .. }
                | Self::Completed { .. }
                | Self::SessionClosed { .. }
        )
    }

    /// Whether this is the session's [`SessionEvent::Terminal`].
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminal { .. })
    }

    /// Whether the queue's per-session cap applies to this event; see
    /// [`EventCaps`].
    const fn is_unsolicited(&self) -> bool {
        matches!(
            self,
            Self::ServerOutput { .. } | Self::TransactionStateChanged { .. }
        )
    }
}

/// Per-session bookkeeping for the drop policy in [`EventCaps`].
#[derive(Default)]
struct SessionQueueState {
    /// How many unsolicited events for this session are waiting in the queue.
    unsolicited: usize,
    /// Lines dropped since the last delivered `ServerOutput`.
    dropped_lines: u32,
    /// The sequence number of this session's undelivered
    /// `TransactionStateChanged`, for coalescing.
    transaction_seq: Option<u64>,
}

impl SessionQueueState {
    const fn is_idle(&self) -> bool {
        self.unsolicited == 0 && self.dropped_lines == 0 && self.transaction_seq.is_none()
    }
}

struct Inner {
    queue: VecDeque<SessionEvent>,
    /// The sequence number of `queue.front()`. Sequence numbers exist only so
    /// that a coalescing target can be found in O(1): the queue is popped only
    /// from the front, so `index = seq - front_seq`.
    front_seq: u64,
    /// The sequence number the next enqueued event will get.
    next_seq: u64,
    sessions: HashMap<SessionId, SessionQueueState>,
    /// How many consumers are parked in [`EventQueue::wait_timeout`]. Read
    /// under the same lock a push holds, so the notify below cannot be lost,
    /// and checked so the ordinary path never touches the condvar at all.
    waiters: usize,
}

struct Shared {
    inner: Mutex<Inner>,
    ready: Condvar,
    /// Read-locked for the duration of a wake, so [`EventQueue::set_waker`] —
    /// which takes it exclusively — cannot return while one is running. That
    /// is the whole of the "do not free the consumer under a wake" contract.
    waker: RwLock<Option<Arc<dyn Waker>>>,
    caps: EventCaps,
    dropped_unsolicited: AtomicU64,
    waker_panics: AtomicU64,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Enqueues one event. Returns whether the queue went empty → non-empty,
    /// which is the caller's cue to [`Shared::wake`] — **after** it has let go
    /// of every lock it holds.
    fn push(&self, event: SessionEvent) -> bool {
        let mut inner = self.lock();
        let was_empty = inner.queue.is_empty();
        if event.is_unsolicited() {
            match inner.admit_unsolicited(&event, self.caps) {
                Admission::Enqueue => {}
                Admission::Coalesced => return false,
                Admission::Dropped => {
                    self.dropped_unsolicited.fetch_add(1, Ordering::Relaxed);
                    return false;
                }
            }
        }
        let event = inner.stamp_unsolicited(event);
        inner.queue.push_back(event);
        inner.next_seq += 1;
        if inner.waiters > 0 {
            // Held across the notify on purpose: a waiter registered itself
            // under this same lock, so no wakeup can be lost.
            self.ready.notify_one();
        }
        was_empty
    }

    /// Calls the waker, if one is registered, with no queue lock held.
    fn wake(&self) {
        let slot = self
            .waker
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(waker) = slot.as_ref() else {
            return;
        };
        // A waker that panics must not poison this queue, kill the worker that
        // was only reporting a row, or — once an adapter is on the other side
        // of an FFI frame — unwind into C++. Catch it, count it, keep going.
        if panic::catch_unwind(AssertUnwindSafe(|| waker.wake())).is_err() {
            self.waker_panics.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn pop(&self, inner: &mut Inner) -> Option<SessionEvent> {
        let event = inner.queue.pop_front()?;
        let seq = inner.front_seq;
        inner.front_seq += 1;
        if event.is_unsolicited() {
            inner.release_unsolicited(event.session(), seq);
        }
        Some(event)
    }
}

/// What [`Inner::admit_unsolicited`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    /// Put it in the queue.
    Enqueue,
    /// Its value was folded into one already queued. Nothing was lost, so
    /// this is not a drop and is not counted as one.
    Coalesced,
    /// The session is at its cap. What it carried is counted and reported on
    /// the next event of its kind that session delivers.
    Dropped,
}

impl Inner {
    /// Applies the per-session cap; see [`EventCaps`] for the policy.
    fn admit_unsolicited(&mut self, event: &SessionEvent, caps: EventCaps) -> Admission {
        let session = event.session();
        if let SessionEvent::TransactionStateChanged {
            possibly_active, ..
        } = event
        {
            // Coalesce whenever an undelivered one is still queued, not only
            // at the cap: this reports a state, and the newest value is the
            // only one that is true.
            if let Some(slot) = self.queued_transaction_slot(session)
                && let Some(SessionEvent::TransactionStateChanged {
                    possibly_active: queued,
                    ..
                }) = self.queue.get_mut(slot)
            {
                *queued = *possibly_active;
                return Admission::Coalesced;
            }
        }
        let state = self.sessions.entry(session).or_default();
        if state.unsolicited < caps.max_unsolicited_per_session().get() {
            return Admission::Enqueue;
        }
        if let SessionEvent::ServerOutput { lines, .. } = event {
            let lost = u32::try_from(lines.len()).unwrap_or(u32::MAX);
            state.dropped_lines = state.dropped_lines.saturating_add(lost);
        }
        Admission::Dropped
    }

    /// The index in `queue` of this session's undelivered
    /// `TransactionStateChanged`, if it still has one.
    fn queued_transaction_slot(&self, session: SessionId) -> Option<usize> {
        let seq = self.sessions.get(&session)?.transaction_seq?;
        usize::try_from(seq.checked_sub(self.front_seq)?).ok()
    }

    /// Records the bookkeeping an unsolicited event needs once it is certain
    /// to be enqueued, and folds this session's pending drop count into a
    /// `ServerOutput`'s `dropped` field.
    fn stamp_unsolicited(&mut self, event: SessionEvent) -> SessionEvent {
        if !event.is_unsolicited() {
            return event;
        }
        let seq = self.next_seq;
        let session = event.session();
        let state = self.sessions.entry(session).or_default();
        state.unsolicited += 1;
        match event {
            SessionEvent::TransactionStateChanged { .. } => {
                state.transaction_seq = Some(seq);
                event
            }
            SessionEvent::ServerOutput {
                session,
                lines,
                dropped,
            } => {
                let dropped = dropped.saturating_add(state.dropped_lines);
                state.dropped_lines = 0;
                SessionEvent::ServerOutput {
                    session,
                    lines,
                    dropped,
                }
            }
            other => other,
        }
    }

    fn release_unsolicited(&mut self, session: SessionId, seq: u64) {
        let Some(state) = self.sessions.get_mut(&session) else {
            return;
        };
        state.unsolicited = state.unsolicited.saturating_sub(1);
        if state.transaction_seq == Some(seq) {
            state.transaction_seq = None;
        }
        if state.is_idle() {
            self.sessions.remove(&session);
        }
    }
}

/// The producer half: cheap to clone, `Send + Sync`, held by workers.
///
/// Dropping every sink does **not** close the queue: a consumer can still
/// drain what is already in it. Dropping the [`EventQueue`] does not stop the
/// producers either — they keep pushing into a queue nobody will read, which
/// is bounded by the same caps as ever and freed when the last sink goes.
#[derive(Clone)]
pub struct EventSink {
    shared: Arc<Shared>,
}

impl fmt::Debug for EventSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventSink").finish_non_exhaustive()
    }
}

impl EventSink {
    /// Queues one event and returns whether the consumer must be woken.
    ///
    /// Split from [`EventSink::wake`] so that a caller holding a per-session
    /// lock — which is what makes ordering rule 1 true — can release it before
    /// the waker runs. The waker is never called with a `db-core` lock held.
    pub(crate) fn push(&self, event: SessionEvent) -> bool {
        self.shared.push(event)
    }

    /// Calls the registered waker. Only ever called after
    /// [`EventSink::push`] returned `true`.
    pub(crate) fn wake(&self) {
        self.shared.wake();
    }
}

/// The consumer half: one per application (or one per adapter).
///
/// `Send` but deliberately not `Sync` — there is exactly one consumer, and
/// [`EventQueue::next`] is a *take*, so a second concurrent drainer would be a
/// race the type system can rule out for free.
pub struct EventQueue {
    shared: Arc<Shared>,
    /// Makes the type `!Sync` without making it `!Send`.
    not_sync: PhantomData<Cell<()>>,
}

impl fmt::Debug for EventQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventQueue")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl EventQueue {
    /// Registers, replaces or (with `None`) clears the wake callback.
    ///
    /// **Does not return while a wake is in progress.** That is the whole
    /// point: a consumer that is about to be destroyed clears its waker and
    /// knows, when this returns, that no thread is inside it
    /// (ADR-0003 D5 rule 2).
    ///
    /// Registering does not wake anything for events already queued — the
    /// waker is edge-triggered on empty → non-empty. A consumer that registers
    /// late drains once itself.
    pub fn set_waker(&self, waker: Option<Arc<dyn Waker>>) {
        let mut slot = self
            .shared
            .waker
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = waker;
    }

    /// Takes the next event, or `None` if there is none. Never blocks and
    /// never calls the waker.
    #[must_use]
    pub fn next(&self) -> Option<SessionEvent> {
        let mut inner = self.shared.lock();
        self.shared.pop(&mut inner)
    }

    /// Takes up to `max` events into `out` under one lock, and returns how
    /// many were appended. Never blocks and never calls the waker.
    ///
    /// The budgeted drain ADR-0003 D5 describes: take a bounded number, let
    /// the event loop breathe, re-post.
    pub fn drain_into(&self, max: usize, out: &mut Vec<SessionEvent>) -> usize {
        let mut inner = self.shared.lock();
        let mut taken = 0;
        while taken < max {
            let Some(event) = self.shared.pop(&mut inner) else {
                break;
            };
            out.push(event);
            taken += 1;
        }
        taken
    }

    /// Blocks for at most `timeout` waiting for an event.
    ///
    /// For headless tests and command-line tools. A UI registers a
    /// [`Waker`] instead and never blocks at all.
    #[must_use]
    pub fn wait_timeout(&self, timeout: Duration) -> Option<SessionEvent> {
        let deadline = std::time::Instant::now().checked_add(timeout)?;
        let mut inner = self.shared.lock();
        loop {
            if let Some(event) = self.shared.pop(&mut inner) {
                return Some(event);
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return None;
            }
            inner.waiters += 1;
            let (guard, _) = self
                .shared
                .ready
                .wait_timeout(inner, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inner = guard;
            inner.waiters -= 1;
        }
    }

    /// How many events are waiting. A snapshot: a worker may push another
    /// before the caller acts on it.
    #[must_use]
    pub fn len(&self) -> usize {
        self.shared.lock().queue.len()
    }

    /// Whether nothing is waiting, as a snapshot.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many unsolicited events this queue has refused, over its whole
    /// life, across every session. Never resets.
    ///
    /// Diagnostics: a non-zero value means a session produced unsolicited
    /// events faster than this consumer drained them, and the per-session
    /// count of what that cost is reported on the next
    /// [`SessionEvent::ServerOutput`]. Coalescing a
    /// [`SessionEvent::TransactionStateChanged`] is not a drop and is not
    /// counted here.
    #[must_use]
    pub fn dropped_unsolicited(&self) -> u64 {
        self.shared.dropped_unsolicited.load(Ordering::Relaxed)
    }

    /// How many times a registered [`Waker`] panicked and was contained.
    ///
    /// Always zero for a waker that honours its contract. A non-zero value is
    /// a bug in the consumer, reported rather than hidden: the queue keeps
    /// working and the waker stays registered.
    #[must_use]
    pub fn waker_panics(&self) -> u64 {
        self.shared.waker_panics.load(Ordering::Relaxed)
    }

    /// The caps this queue was built with.
    #[must_use]
    pub fn caps(&self) -> EventCaps {
        self.shared.caps
    }
}

/// Builds one queue and the sink that feeds it.
///
/// One per application is the intended shape: every session's worker pushes
/// into the same queue, so the consumer has one thing to drain and one waker
/// to register. Nothing forbids more.
#[must_use]
pub fn event_channel(caps: EventCaps) -> (EventSink, EventQueue) {
    let shared = Arc::new(Shared {
        inner: Mutex::new(Inner {
            queue: VecDeque::new(),
            front_seq: 0,
            next_seq: 0,
            sessions: HashMap::new(),
            waiters: 0,
        }),
        ready: Condvar::new(),
        waker: RwLock::new(None),
        caps,
        dropped_unsolicited: AtomicU64::new(0),
        waker_panics: AtomicU64::new(0),
    });
    (
        EventSink {
            shared: Arc::clone(&shared),
        },
        EventQueue {
            shared,
            not_sync: PhantomData,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{EventCaps, EventQueue, EventSink, SessionEvent, Waker, event_channel};
    use crate::ids::SessionId;
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[allow(clippy::extra_unused_type_parameters)]
    const fn assert_send<T: Send>() {}
    #[allow(clippy::extra_unused_type_parameters)]
    const fn assert_sync<T: Sync>() {}

    /// What every producer does: push, and wake only on the edge.
    fn emit(sink: &EventSink, event: SessionEvent) {
        if sink.push(event) {
            sink.wake();
        }
    }

    fn caps(limit: usize) -> EventCaps {
        EventCaps::new().with_max_unsolicited_per_session(
            NonZeroUsize::new(limit).expect("test cap must be non-zero"),
        )
    }

    fn output(session: SessionId, lines: usize) -> SessionEvent {
        SessionEvent::ServerOutput {
            session,
            lines: (0..lines).map(|i| format!("line {i}").into()).collect(),
            dropped: 0,
        }
    }

    struct Counting(AtomicUsize);

    impl Waker for Counting {
        fn wake(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct Panicking;

    impl Waker for Panicking {
        fn wake(&self) {
            panic!("a waker that breaks its contract");
        }
    }

    #[test]
    fn the_sink_is_shareable_and_the_queue_has_one_consumer() {
        assert_send::<EventSink>();
        assert_sync::<EventSink>();
        assert_send::<EventQueue>();
        assert_send::<SessionEvent>();
        // `EventQueue: !Sync` is what makes "one consumer" a type error rather
        // than a comment; it cannot be asserted positively here, so the
        // `PhantomData<Cell<()>>` field carries the note instead.
    }

    #[test]
    fn the_waker_is_edge_triggered_on_empty_to_non_empty() {
        let (sink, queue) = event_channel(EventCaps::new());
        let waker = Arc::new(Counting(AtomicUsize::new(0)));
        queue.set_waker(Some(Arc::clone(&waker) as Arc<dyn Waker>));
        let session = SessionId::allocate();

        emit(
            &sink,
            SessionEvent::TransactionStateChanged {
                session,
                possibly_active: true,
            },
        );
        emit(&sink, output(session, 1));
        emit(&sink, output(session, 1));
        assert_eq!(
            waker.0.load(Ordering::SeqCst),
            1,
            "a burst into an empty queue must produce exactly one wake"
        );

        while queue.next().is_some() {}
        emit(&sink, output(session, 1));
        assert_eq!(
            waker.0.load(Ordering::SeqCst),
            2,
            "the queue going empty re-arms the edge"
        );
    }

    #[test]
    fn draining_never_calls_the_waker() {
        let (sink, queue) = event_channel(EventCaps::new());
        let session = SessionId::allocate();
        emit(&sink, output(session, 1));
        let waker = Arc::new(Counting(AtomicUsize::new(0)));
        queue.set_waker(Some(Arc::clone(&waker) as Arc<dyn Waker>));
        let mut out = Vec::new();
        assert_eq!(queue.drain_into(16, &mut out), 1);
        assert!(queue.next().is_none());
        assert_eq!(
            waker.0.load(Ordering::SeqCst),
            0,
            "a pop must never re-enter the consumer"
        );
    }

    #[test]
    fn a_panicking_waker_is_caught_counted_and_survived() {
        let (sink, queue) = event_channel(EventCaps::new());
        queue.set_waker(Some(Arc::new(Panicking)));
        let session = SessionId::allocate();
        emit(&sink, output(session, 1));
        while queue.next().is_some() {}
        emit(&sink, output(session, 2));
        assert_eq!(queue.waker_panics(), 2);
        assert_eq!(queue.len(), 1, "the event was queued despite the panic");
        queue.set_waker(None);
    }

    #[test]
    fn server_output_over_the_cap_is_dropped_and_the_lines_are_reported() {
        let (sink, queue) = event_channel(caps(2));
        let session = SessionId::allocate();
        emit(&sink, output(session, 1));
        emit(&sink, output(session, 1));
        // At the cap: these are refused, and their lines counted.
        emit(&sink, output(session, 3));
        emit(&sink, output(session, 4));
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.dropped_unsolicited(), 2);

        // Draining one frees a slot; the next accepted event carries the count
        // of everything lost since the last delivered one.
        for _ in 0..2 {
            let taken = queue.next().expect("queued output");
            match taken {
                SessionEvent::ServerOutput { dropped, .. } => assert_eq!(dropped, 0),
                other => panic!("expected ServerOutput, got {other:?}"),
            }
        }
        emit(&sink, output(session, 1));
        match queue.next().expect("output after the drain") {
            SessionEvent::ServerOutput { dropped, .. } => assert_eq!(
                dropped, 7,
                "the 3 + 4 lines that were dropped must be reported, not hidden"
            ),
            other => panic!("expected ServerOutput, got {other:?}"),
        }
        assert_eq!(
            queue.dropped_unsolicited(),
            2,
            "the diagnostic counter never resets"
        );
    }

    #[test]
    fn one_session_hitting_the_cap_does_not_affect_another() {
        let (sink, queue) = event_channel(caps(1));
        let a = SessionId::allocate();
        let b = SessionId::allocate();
        emit(&sink, output(a, 1));
        emit(&sink, output(a, 5));
        emit(&sink, output(b, 1));
        assert_eq!(queue.len(), 2, "b is nowhere near its own cap");
        assert_eq!(queue.dropped_unsolicited(), 1);
    }

    #[test]
    fn transaction_state_is_coalesced_in_place_and_never_dropped() {
        let (sink, queue) = event_channel(caps(1));
        let session = SessionId::allocate();
        for possibly_active in [true, false, true, false] {
            emit(
                &sink,
                SessionEvent::TransactionStateChanged {
                    session,
                    possibly_active,
                },
            );
        }
        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue.dropped_unsolicited(),
            0,
            "coalescing is not dropping: nothing was lost"
        );
        match queue.next().expect("the coalesced state") {
            SessionEvent::TransactionStateChanged {
                possibly_active, ..
            } => assert!(
                !possibly_active,
                "the most recent value the worker computed must survive"
            ),
            other => panic!("expected TransactionStateChanged, got {other:?}"),
        }
    }

    #[test]
    fn a_coalescing_slot_is_released_once_its_event_is_taken() {
        let (sink, queue) = event_channel(EventCaps::new());
        let session = SessionId::allocate();
        emit(
            &sink,
            SessionEvent::TransactionStateChanged {
                session,
                possibly_active: true,
            },
        );
        let _ = queue.next();
        emit(
            &sink,
            SessionEvent::TransactionStateChanged {
                session,
                possibly_active: false,
            },
        );
        assert_eq!(
            queue.len(),
            1,
            "a delivered state must not be coalesced into after the fact"
        );
    }

    #[test]
    fn wait_timeout_returns_what_a_producer_pushes_and_gives_up_otherwise() {
        let (sink, queue) = event_channel(EventCaps::new());
        let session = SessionId::allocate();
        let producer = std::thread::spawn(move || {
            emit(&sink, output(session, 1));
        });
        let event = queue.wait_timeout(Duration::from_secs(30));
        assert!(event.is_some());
        producer.join().expect("producer");
        assert!(queue.wait_timeout(Duration::from_millis(20)).is_none());
    }
}
