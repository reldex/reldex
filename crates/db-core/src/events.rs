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
//!
//!    **Production order is not acceptance order.** Requests are accepted on
//!    caller threads and answered by the worker, and at a terminal transition
//!    a submit can synthesise its own failure (its command could not be
//!    delivered) while the worker is draining and failing the commands it
//!    already had. Those failures interleave. A consumer must therefore route
//!    strictly by [`RequestId`] and must **not** infer "every earlier request
//!    of this session has been answered" from a reply it just took; the only
//!    event that means a session will produce nothing further is
//!    [`SessionEvent::Terminal`], which is where per-session state should be
//!    retired.
//! 2. **Every accepted request produces exactly one reply event** — never
//!    zero, never two. The reply channel is consumed by answering it, and if
//!    it is *dropped* unanswered (the worker exited, the command could not be
//!    delivered, a core bug unwound the worker) its `Drop` emits the one
//!    failure instead. "Accepted" means the submit returned `Ok`; the one
//!    synchronous failure — [`crate::SessionLimits::max_outstanding_requests`]
//!    — accepts nothing and produces no event.
//!
//!    The reply is produced either way; whether anyone *sees* it is a separate
//!    question. Once the [`EventQueue`] has been dropped there is no consumer,
//!    and events are discarded as they arrive (see [`EventQueue`]) — so rule 2
//!    is a promise about production, and a consumer that wants a request's
//!    answer keeps its queue alive until it has drained it.
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
//! The queue itself has no length limit, and what bounds it is stated rather
//! than hoped for. Per session, at any instant:
//!
//! * **Reply events** are bounded by
//!   [`crate::SessionLimits::max_outstanding_requests`] **+ 1**. A request
//!   reserves a slot when it is accepted and releases it when the **consumer
//!   takes its reply out of the queue** — not when the worker produces it — so
//!   a consumer that stops draining stops the submitter too. Going over the
//!   limit is the one synchronous failure the submit API has
//!   ([`reldex_db_driver_api::ErrorKind::Resource`]); reporting it as an event
//!   would be circular. The `+ 1` is
//!   [`crate::DatabaseSession::submit_close`]: a session at its limit must
//!   still be able to end, so a close reserves against one more than the
//!   limit — once, not repeatedly.
//!
//!   [`crate::SessionRegistry::open`] is inside that `R`, not above it: the
//!   open reserves an ordinary slot, and it does so on a session that has
//!   nothing outstanding, so it is never refused and never adds to the bound.
//!   [`crate::SessionRegistry::abandon`] reserves **nothing** — it answers the
//!   open's already-reserved request and emits a `Terminal`, neither of which
//!   is a new reply — so it can never be refused for lack of a slot, which is
//!   what makes it a teardown path rather than another thing that can fail.
//! * **[`SessionEvent::Executing`]** is bounded by the same number: the worker
//!   emits at most one per outstanding execute, and it is never dropped.
//! * **Unsolicited events** ([`SessionEvent::ServerOutput`],
//!   [`SessionEvent::TransactionStateChanged`]) are bounded by
//!   [`EventCaps::max_unsolicited_per_session`] plus one — a transaction state
//!   is never dropped; see [`EventCaps`] for the exact policy and for how a
//!   drop is reported rather than hidden.
//! * **A `ServerOutput` that carries a `failure`** (M2.7) is never dropped
//!   either, and is bounded like `Executing`: the worker emits at most one
//!   per execute, always *before* that execute's reply, and the reply holds a
//!   request slot until the consumer drains it — which, the queue being a
//!   FIFO, it cannot do without draining the failure first. So at most `R` of
//!   them are ever waiting. At the cap such an event is admitted **without its
//!   lines**, which are counted as dropped like any refused output.
//! * **[`SessionEvent::Terminal`]** is one per session, ever, and is never
//!   dropped.
//!
//! So one session can hold at most `3 × max_outstanding_requests +
//! max_unsolicited_per_session + 3` events in the queue — `R + 1` replies, `R`
//! `Executing`s, `R` failed-read `ServerOutput`s, `U + 1` other unsolicited
//! events and one `Terminal` — and no producer can exceed that however fast
//! it runs. (M2.5 published `2R + U + 3`; M2.7's failed reads are the third
//! `R`, and they only exist on a session whose output pane is on.) M2.6's
//! open and abandon do not change that arithmetic: the open is one of the
//! `R`, abandon reserves nothing, and `Terminal`'s `transaction_possibly_lost`
//! is a field on an event that was already counted rather than a new event.
//! The [`SessionEvent::ServerOutputConfigured`] reply is one of the `R`.
//!
//! Dropping the [`EventQueue`] ends the stream: see [`EventQueue`] for what
//! happens to the events and the slots that were still in it.

use std::cell::Cell;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use reldex_db_driver_api::{
    CancelKind, ConnectionId, DbError, DbResult, ErrorKind, ServerOutputSetting, Warning,
};

use crate::ids::{LobHandle, ResultId, SessionId};
use crate::session::{CloseError, ExecuteOutcome, FetchedBatch};
use crate::shared::SessionLifecycle;
use crate::store::{FetchTicket, SegmentReply};

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
/// * **It must return promptly and must not block.** It is usually called on a
///   session's worker thread, and blocking there stalls that session. It can
///   also run **on the thread that submitted a request**: a submit whose
///   command cannot be delivered synthesises its own failure event (ordering
///   rule 2), and if that fills an empty queue the submitting thread is the one
///   that wakes the consumer. A consumer whose waker posts to its own event
///   loop must therefore tolerate being called from a thread it does not own,
///   including — once an adapter is on the other side of an FFI frame — the UI
///   thread itself.
/// * **It must not call [`EventQueue::set_waker`].** That would take the
///   registration lock for writing while this call holds it for reading, which
///   self-deadlocks.
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
/// * [`SessionEvent::TransactionStateChanged`] is **coalesced, never dropped**,
///   and the cap does not apply to it: it reports a *state*, not an
///   occurrence, so an undelivered one for that session is updated in place to
///   the newer value, and when the session has none queued the new one is
///   admitted even at the cap. Both halves are needed — coalescing alone would
///   still lose the first change after a burst of `ServerOutput` filled the
///   session's allowance, and "the transaction is open" is exactly the fact
///   that must not be lost. The class therefore adds at most **one** event per
///   session over the cap. What it costs is position: a coalesced value is
///   delivered at the earlier of the two slots. It is advisory (`close`
///   re-decides on the worker, ADR-0002 K4), and
///   [`crate::DatabaseSession::has_possibly_active_transaction`] is always
///   authoritative.
/// * [`SessionEvent::ServerOutput`] is **dropped, and the drop is counted**.
///   The incoming event is the one dropped, so what is already queued — where
///   a PL/SQL error usually is — survives. Its line count is added to that
///   session's pending drop count and reported as `dropped` on the next
///   `ServerOutput` that session actually delivers, so the UI can say "output
///   truncated" instead of silently lying. A count that has no later
///   `ServerOutput` to ride on stays readable through
///   [`EventQueue::pending_dropped_lines`]. [`EventQueue::dropped_unsolicited`]
///   counts the refused *events* for diagnostics, and never resets.
/// * A `ServerOutput` whose `failure` is set — reading the server's output
///   failed — is **never dropped**: at the cap its lines are dropped and
///   counted exactly as above, and the event itself is admitted carrying only
///   the failure and the drop count, because "the output is incomplete, and
///   this is why" is the one thing about output a UI must not lose. It is not
///   counted by `dropped_unsolicited`, since the event was not refused. The
///   class is bounded by the reply slots, not by this cap; see the module
///   documentation.
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
    /// Produced by [`crate::SessionRegistry::open`], on the session's own
    /// worker thread the moment `connect` returns. It is a session's **first**
    /// event: nothing else is emitted for a session before the open is
    /// answered, so a consumer can create its per-session state here and
    /// retire it on [`SessionEvent::Terminal`].
    ///
    /// That is structural, not a convention. The registry records the session
    /// and emits this from the worker thread *before* that worker enters its
    /// command loop, so the only producer of any later event for this session
    /// cannot run until this one is queued — even for a consumer that ignores
    /// events and polls [`crate::SessionRegistry::get`] to submit the instant a
    /// handle appears.
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
    /// The connect failed, or was abandoned before it finished.
    ///
    /// The session id in it was allocated and is now dead; a reconnect is a
    /// new [`crate::SessionRegistry::open`] with a new id (`SPEC.md` §18).
    /// [`SessionEvent::Terminal`] always follows, with
    /// [`SessionLifecycle::Lost`] when the connect failed and
    /// [`SessionLifecycle::Closed`] when it was abandoned.
    OpenFailed {
        /// The session id that was allocated and is now dead.
        session: SessionId,
        /// The request that asked for it.
        request: RequestId,
        /// Why. [`reldex_db_driver_api::ErrorKind::Cancelled`] means
        /// [`crate::SessionRegistry::abandon`] gave up on the connect;
        /// anything else is the driver's own classification of the failure,
        /// with its native code and cause chain intact.
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
    /// The reply to [`crate::DatabaseSession::submit_fetch_segment`]: the
    /// next rows of a result store's result, already compacted on the worker
    /// (ADR-0004 RS1). Hand it to the store with
    /// [`crate::SessionResults::observe`] or [`crate::ResultStore::on_fetched`].
    FetchedSegment {
        /// The session.
        session: SessionId,
        /// The request.
        request: RequestId,
        /// Which fetch of which result this answers — set on the failure path
        /// too, so the store can check the sequence and name the result.
        fetch: FetchTicket,
        /// The segment, or why it could not be fetched.
        segment: DbResult<SegmentReply>,
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
    /// The reply to [`crate::DatabaseSession::submit_set_server_output`].
    ///
    /// On success it carries the setting **actually in force**, which a
    /// driver may have adjusted — a buffer size the server does not accept is
    /// clamped into its range, and this says to what — so the UI shows the
    /// real limit rather than the one it asked for. On failure nothing
    /// changed: the session keeps reading output exactly as it did before.
    ServerOutputConfigured {
        /// The session.
        session: SessionId,
        /// The request.
        request: RequestId,
        /// The setting now in force, or why it could not be changed.
        result: DbResult<ServerOutputSetting>,
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
    /// Lines the server produced out of band — `DBMS_OUTPUT` and its kind —
    /// read back by the worker after a statement, on a session whose output
    /// was turned on with [`crate::DatabaseSession::submit_set_server_output`]
    /// (ADR-0002 amendment T).
    ///
    /// **Where it sits in the stream.** The worker reads output after the
    /// statement that produced it and emits it **before that statement's
    /// [`SessionEvent::Executed`]** — so every `ServerOutput` lies between an
    /// execute's [`SessionEvent::Executing`] and its `Executed`. Normally it
    /// belongs to that execute. There are two exceptions, and in both the
    /// lines arrive inside the **next** execute's window, ahead of that
    /// execute's own lines:
    ///
    /// * output written while rows were being *fetched* (a function in a
    ///   select list that prints), because a fetch is not followed by a read;
    /// * output of a statement that failed and left the session needing
    ///   validation (for example a timeout), because no read is attempted on
    ///   such a session. The next command's ping restores it, and the next
    ///   execute's read returns those lines. Reading straight away would need
    ///   a ping first, and would hold back the error reply on a connection
    ///   that may be dead. On the Oracle driver today this case is mostly
    ///   moot: a call timeout during a blocked call loses the session, and a
    ///   lost session's output is gone with it.
    ///
    /// One statement's output may arrive as several events, each bounded by
    /// [`crate::SessionLimits::server_output_chunk_lines`] and
    /// [`crate::SessionLimits::server_output_chunk_bytes`].
    ///
    /// `#[non_exhaustive]`, unlike the other variants: its fields are still
    /// expected to grow (a per-statement truncation report, follow-up M2.13),
    /// so a consumer — M2.11's mapping first — must match it with `..`.
    #[non_exhaustive]
    ServerOutput {
        /// The session.
        session: SessionId,
        /// The lines, in order. An empty line is an empty string.
        lines: Vec<Box<str>>,
        /// How many lines were dropped for this session since the previous
        /// delivered `ServerOutput`, because the session was at
        /// [`EventCaps::max_unsolicited_per_session`]. Zero normally; non-zero
        /// means the UI must say "output truncated".
        dropped: u32,
        /// Reading the output failed, and this read ended. Lines read before
        /// the failure went out on earlier events; this one carries none. The
        /// lines the failed read had taken are lost, and what the server still
        /// held may be (Oracle discards it at the next `PUT`), so the UI must
        /// say the output is incomplete and why.
        ///
        /// The statement's own result is untouched: it arrives on its
        /// `Executed` as usual, success or failure. A read that found the
        /// connection gone is also the session's loss, reported on its
        /// [`SessionEvent::Terminal`] like any other. An event carrying a
        /// failure is **never dropped**, even at the cap; see [`EventCaps`].
        failure: Option<DbError>,
        /// How many of `lines` were not valid UTF-8 on the wire and were
        /// delivered with U+FFFD in place of the invalid bytes rather than
        /// dropped — the same count
        /// [`reldex_db_driver_api::ServerOutputChunk::invalid_utf8_lines`]
        /// reports for the chunk this event carries. Zero normally; the
        /// affected lines are still present in `lines`, so the UI can mark
        /// them rather than show mojibake with no explanation. Added in
        /// M2.12's fix round: previously read off the chunk and then
        /// silently discarded at this boundary
        /// (`docs/exec-plans/active/phase-1-m2-5-event-queue.md` §7.5) —
        /// M2.11 still owns mapping it across the C ABI.
        invalid_utf8_lines: u32,
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
    ///
    /// "Per session" includes one that never opened: a connect that failed and
    /// an open that [`crate::SessionRegistry::abandon`] gave up on both
    /// announce one, after their single [`SessionEvent::OpenFailed`]. Every
    /// session the registry ever named produces exactly one of these, which is
    /// what makes "retire per-session state on `Terminal`, never on anything
    /// else" a complete rule rather than a usually-true one.
    Terminal {
        /// The session.
        session: SessionId,
        /// [`SessionLifecycle::Lost`] or [`SessionLifecycle::Closed`]. `Lost`
        /// wins when both apply, because *why* a session ended matters more
        /// than that it ended. A connect that failed is `Lost` with the
        /// failure as `cause`; an open that was abandoned is `Closed`, because
        /// the abandon won and nothing failed.
        lifecycle: SessionLifecycle,
        /// The failure that lost the session, with its original kind, native
        /// code and cause chain. `None` for a deliberate close.
        cause: Option<DbError>,
        /// Whether this session ended while it may still have held an
        /// unresolved transaction — in other words, whether work the user did
        /// may have been rolled back by the server when the connection went
        /// away.
        ///
        /// **This is the authoritative answer and a consumer must surface it**
        /// (`SPEC.md` §10: never silently commit or hide transaction loss). It
        /// is computed on the worker thread at the point the session actually
        /// ends — after every command queued ahead of the close has run, which
        /// is where ADR-0002 K4 already decides — so unlike any answer a
        /// control thread can read, no queued statement can still invalidate
        /// it.
        ///
        /// * `false` for a `close` whose [`crate::CloseDisposition`] succeeded:
        ///   the user said commit (or rollback) and it happened. A rollback the
        ///   user chose is not a loss.
        /// * `false` for a session that never opened. Nothing was connected, so
        ///   there was no transaction to lose.
        /// * `true` for [`crate::SessionRegistry::abandon`],
        ///   `DatabaseSession::drop` and the registry's teardown whenever a
        ///   transaction may have been open: none of them resolve anything
        ///   (ADR-0002 K5), so the server rolls back.
        /// * For a lost connection, `true` whenever a transaction may have been
        ///   open — which includes the case where the call that *died* is the
        ///   one that may have opened it: the driver's cached transaction state
        ///   describes the world before that call, so it is not trusted and
        ///   [`reldex_db_driver_api::TransactionState::Unknown`] is recorded
        ///   instead. A session lost while **idle** with nothing open still
        ///   reports `false`; losing a connection is not by itself a loss.
        /// * `true` whenever the driver cannot rule a transaction out
        ///   ([`reldex_db_driver_api::TransactionState::Unknown`]), because the
        ///   conservative answer is the only safe one (ADR-0002 K7).
        transaction_possibly_lost: bool,
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
            | Self::FetchedSegment { session, .. }
            | Self::LobChunk { session, .. }
            | Self::Completed { session, .. }
            | Self::SessionClosed { session, .. }
            | Self::ServerOutputConfigured { session, .. }
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
            | Self::FetchedSegment { request, .. }
            | Self::LobChunk { request, .. }
            | Self::Completed { request, .. }
            | Self::SessionClosed { request, .. }
            | Self::ServerOutputConfigured { request, .. }
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
                | Self::FetchedSegment { .. }
                | Self::LobChunk { .. }
                | Self::Completed { .. }
                | Self::SessionClosed { .. }
                | Self::ServerOutputConfigured { .. }
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

/// One session's reply slots: reserved when a request is accepted, released
/// when the consumer takes that request's reply **out of** the queue.
///
/// Shared, so that both ends can reach it: the session reserves and reads it,
/// and the queue — which carries an `Arc` of it on the queued reply itself —
/// releases from inside [`EventQueue::next`] / [`EventQueue::drain_into`].
/// That is what makes [`crate::SessionLimits::max_outstanding_requests`] bound
/// the *queue* and not merely the number of requests in flight on the worker.
///
/// Every operation is a single atomic, so the release can run under the queue's
/// mutex without introducing a second lock or a lock-order edge.
#[derive(Debug, Default)]
pub(crate) struct RequestSlots {
    outstanding: AtomicUsize,
}

impl RequestSlots {
    /// Takes one slot, or refuses because `limit` are already taken.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Resource`] at the limit. Refusing accepts nothing, so
    /// ordering rule 2 is untouched.
    pub(crate) fn reserve(&self, limit: usize) -> DbResult<()> {
        let mut current = self.outstanding.load(Ordering::Acquire);
        loop {
            if current >= limit {
                return Err(DbError::new(
                    ErrorKind::Resource,
                    format!(
                        "reldex-db-core: this session already has {limit} requests outstanding \
                         (its configured limit); drain the event queue before submitting more"
                    ),
                ));
            }
            match self.outstanding.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(seen) => current = seen,
            }
        }
    }

    /// Gives one slot back.
    ///
    /// Releasing more than was reserved is a core bug — the slot rides on its
    /// own event, so it can only happen if some path answers a request twice.
    /// In a debug build this says so; in release it saturates at zero, because
    /// a wrong count that never underflows is the harmless way to be wrong and
    /// panicking here would take out a worker thread over bookkeeping.
    pub(crate) fn release(&self) {
        let released =
            self.outstanding
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |taken| {
                    if taken == 0 { None } else { Some(taken - 1) }
                });
        debug_assert!(
            released.is_ok(),
            "reldex-db-core: a reply slot was released that was never reserved; a request has \
             been answered twice (ordering rule 2)"
        );
    }

    /// How many slots are taken right now.
    pub(crate) fn outstanding(&self) -> usize {
        self.outstanding.load(Ordering::Acquire)
    }
}

/// What one [`Shared::push`] did with an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pushed {
    /// The queue went empty → non-empty: call the waker, after releasing every
    /// lock.
    pub(crate) wake: bool,
    /// The queue took the event, and with it responsibility for the reply slot
    /// it holds. `false` means the caller still owns that slot and must
    /// release it — the event was coalesced, refused by the cap, or pushed
    /// into a queue whose consumer is gone.
    pub(crate) kept: bool,
}

impl Pushed {
    const DISCARDED: Self = Self {
        wake: false,
        kept: false,
    };
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

/// One queued event, with the reply slot it is carrying.
///
/// The slot travels **with** its own event rather than in a per-session
/// tally, which is both cheaper — the reply path, the hot one, touches no hash
/// map at either end — and harder to get wrong: a reply cannot release a slot
/// it was not holding, and a slot cannot outlive the event that owns it
/// whichever way the queue ends.
struct Queued {
    event: SessionEvent,
    /// `Some` for a request's single reply: released the moment a consumer
    /// takes this event out, or the queue is dropped.
    slot: Option<Arc<RequestSlots>>,
}

impl Queued {
    /// Takes the event out, releasing the slot it held.
    fn take(self) -> SessionEvent {
        if let Some(slot) = self.slot {
            slot.release();
        }
        self.event
    }
}

struct Inner {
    queue: VecDeque<Queued>,
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
    /// Set by `Drop for EventQueue`: there is no consumer any more, so events
    /// are discarded on arrival instead of accumulating in a queue nobody can
    /// read.
    closed: bool,
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

    /// Enqueues one event and says what became of it; see [`Pushed`].
    ///
    /// `slots` is `Some` exactly when the event is a request's single reply,
    /// and carries the counter whose slot that request reserved: the queue
    /// takes ownership of the slot when it keeps the event, and releases it
    /// again in [`Shared::pop`].
    fn push(&self, event: SessionEvent, slots: Option<&Arc<RequestSlots>>) -> Pushed {
        let mut inner = self.lock();
        if inner.closed {
            // Nobody will ever read this. Keeping it would grow without bound
            // and hold the submitter's slot for a reply it can never see.
            return Pushed::DISCARDED;
        }
        let was_empty = inner.queue.is_empty();
        let mut event = event;
        if event.is_unsolicited() {
            match inner.admit_unsolicited(&event, self.caps) {
                Admission::Enqueue => {}
                Admission::EnqueueWithoutLines => {
                    // The lines were counted as dropped; the failure the event
                    // carries is what must not be lost, and it costs no lines.
                    if let SessionEvent::ServerOutput { lines, .. } = &mut event {
                        *lines = Vec::new();
                    }
                }
                Admission::Coalesced => return Pushed::DISCARDED,
                Admission::Dropped => {
                    self.dropped_unsolicited.fetch_add(1, Ordering::Relaxed);
                    return Pushed::DISCARDED;
                }
            }
        }
        let event = inner.stamp_unsolicited(event);
        inner.queue.push_back(Queued {
            event,
            slot: slots.map(Arc::clone),
        });
        inner.next_seq += 1;
        if inner.waiters > 0 {
            // Held across the notify on purpose: a waiter registered itself
            // under this same lock, so no wakeup can be lost.
            self.ready.notify_one();
        }
        Pushed {
            wake: was_empty,
            kept: true,
        }
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

    /// Takes the front event, releasing the reply slot it carried — the bound
    /// is on what this queue holds, so a slot is freed here and not when the
    /// worker produced the reply.
    fn pop(&self, inner: &mut Inner) -> Option<SessionEvent> {
        let event = inner.queue.pop_front()?.take();
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
    /// The session is at its cap, but the event reports a failure that must
    /// not be lost: its lines are dropped and counted like any refused
    /// `ServerOutput`, and the event itself — now carrying only the failure
    /// and the drop count — is admitted over the cap.
    EnqueueWithoutLines,
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
                }) = self.queue.get_mut(slot).map(|queued| &mut queued.event)
            {
                *queued = *possibly_active;
                return Admission::Coalesced;
            }
            // There is nothing to fold into, so the cap does not apply: this
            // class is what tells a UI whether a transaction is open, and
            // dropping it because a `ServerOutput` burst used up the session's
            // allowance would leave the user looking at a stale answer with a
            // transaction still on the connection. Admitting it costs one
            // event per session, because the next one coalesces into it.
            return Admission::Enqueue;
        }
        let at_cap = self
            .sessions
            .get(&session)
            .is_some_and(|state| state.unsolicited >= caps.max_unsolicited_per_session().get());
        if !at_cap {
            // Deliberately no `entry().or_default()` here: a session that is
            // not at its cap needs no bookkeeping until `stamp_unsolicited`
            // creates it, and a refused event must not leave one behind.
            return Admission::Enqueue;
        }
        if let SessionEvent::ServerOutput { lines, failure, .. } = event
            && let Some(state) = self.sessions.get_mut(&session)
        {
            let lost = u32::try_from(lines.len()).unwrap_or(u32::MAX);
            state.dropped_lines = state.dropped_lines.saturating_add(lost);
            if failure.is_some() {
                // A failed read is news the UI must show ("output incomplete,
                // because …"), and a drop would hide it. At most one such
                // event exists per execute, and it is always followed by that
                // execute's reply, which holds a request slot until drained —
                // so the class is bounded by `max_outstanding_requests`.
                return Admission::EnqueueWithoutLines;
            }
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
                failure,
                invalid_utf8_lines,
            } => {
                let dropped = dropped.saturating_add(state.dropped_lines);
                state.dropped_lines = 0;
                SessionEvent::ServerOutput {
                    session,
                    lines,
                    dropped,
                    failure,
                    invalid_utf8_lines,
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
/// producers either — their sessions keep working — but from then on every
/// event they push is discarded rather than accumulated; see [`EventQueue`].
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
    /// Queues one event that holds no reply slot, and returns whether the
    /// consumer must be woken.
    ///
    /// Split from [`EventSink::wake`] so that a caller holding a per-session
    /// lock — which is what makes ordering rule 1 true — can release it before
    /// the waker runs. The waker is never called with a `db-core` lock held.
    pub(crate) fn push(&self, event: SessionEvent) -> bool {
        self.shared.push(event, None).wake
    }

    /// Queues a request's single reply, handing the queue the slot that
    /// request reserved. See [`Pushed`] for what the caller must do with a
    /// reply the queue did not keep.
    pub(crate) fn push_reply(&self, event: SessionEvent, slots: &Arc<RequestSlots>) -> Pushed {
        debug_assert!(
            event.is_reply(),
            "only a request's reply carries a slot; {event:?} is not one"
        );
        self.shared.push(event, Some(slots))
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
///
/// # Dropping it ends the stream
///
/// Sessions bound to this queue keep working when it is dropped — they must,
/// because one of them may be in the middle of a close — but the stream is
/// over:
///
/// * every event still in the queue is discarded, and every reply slot those
///   events held is released;
/// * every later event is discarded on arrival, and a reply's slot is released
///   immediately instead of being handed to the queue.
///
/// So [`crate::DatabaseSession::outstanding_requests`] falls back to zero, no
/// submit is refused because a consumer that no longer exists is not draining,
/// and nothing accumulates in memory nobody can reach. What a consumer wanted
/// to see, it must drain **before** dropping the queue.
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
    ///
    /// # Panics
    ///
    /// Never, but calling this **from inside a [`Waker::wake`] call** is a
    /// self-deadlock, not a panic: the wake holds the registration lock for
    /// reading and this takes it for writing. A waker that wants to unregister
    /// itself must post that to its own loop.
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
    ///
    /// A `timeout` the monotonic clock cannot represent — [`Duration::MAX`] —
    /// means "no deadline". **This then blocks indefinitely**, until an event
    /// arrives, rather than giving up immediately on an arithmetic overflow as
    /// it used to. Nothing else wakes it: dropping every [`EventSink`] does not
    /// close the queue, so a caller that may outlive its producers should pass
    /// a real timeout.
    #[must_use]
    pub fn wait_timeout(&self, timeout: Duration) -> Option<SessionEvent> {
        let deadline = std::time::Instant::now().checked_add(timeout);
        let mut inner = self.shared.lock();
        loop {
            if let Some(event) = self.shared.pop(&mut inner) {
                return Some(event);
            }
            let remaining = match deadline {
                Some(deadline) => {
                    let now = std::time::Instant::now();
                    if now >= deadline {
                        return None;
                    }
                    Some(deadline - now)
                }
                None => None,
            };
            inner.waiters += 1;
            inner = match remaining {
                Some(remaining) => {
                    self.shared
                        .ready
                        .wait_timeout(inner, remaining)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0
                }
                None => self
                    .shared
                    .ready
                    .wait(inner)
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            };
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
        self.shared.lock().queue.is_empty()
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

    /// How many [`SessionEvent::ServerOutput`] lines this session has lost
    /// that have not yet been reported on a delivered event.
    ///
    /// The count normally rides out on the next `ServerOutput` that session
    /// delivers, which is where a UI reads it. This is the accessor for the
    /// case where there is no next one — the session went quiet, or ended,
    /// with lines still owed — so that "output truncated" can still be shown.
    /// Zero for a session that has lost nothing, including one this queue has
    /// never heard of.
    #[must_use]
    pub fn pending_dropped_lines(&self, session: SessionId) -> u32 {
        self.shared
            .lock()
            .sessions
            .get(&session)
            .map_or(0, |state| state.dropped_lines)
    }

    /// The caps this queue was built with.
    #[must_use]
    pub fn caps(&self) -> EventCaps {
        self.shared.caps
    }
}

impl Drop for EventQueue {
    /// Ends the stream: see [`EventQueue`]. Everything still queued is
    /// discarded and every reply slot it held is released, so a session whose
    /// consumer went away is not left unable to submit.
    fn drop(&mut self) {
        let mut inner = self.shared.lock();
        inner.closed = true;
        while self.shared.pop(&mut inner).is_some() {}
        // Only `dropped_lines` can be left, and nobody can read it now.
        inner.sessions.clear();
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
            closed: false,
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
            failure: None,
            invalid_utf8_lines: 0,
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

    fn failed_output(session: SessionId, lines: usize) -> SessionEvent {
        SessionEvent::ServerOutput {
            session,
            lines: (0..lines).map(|i| format!("line {i}").into()).collect(),
            dropped: 0,
            failure: Some(reldex_db_driver_api::DbError::internal("the read failed")),
            invalid_utf8_lines: 0,
        }
    }

    #[test]
    fn a_failed_read_is_never_dropped_even_at_the_cap() {
        // M2.7: "the output is incomplete, and this is why" must reach the UI
        // whatever the queue's state. At the cap the event loses its lines —
        // counted, by the existing mechanism — and keeps its failure.
        let (sink, queue) = event_channel(caps(1));
        let session = SessionId::allocate();
        emit(&sink, output(session, 2));
        emit(&sink, output(session, 5)); // refused: 5 lines owed
        emit(&sink, failed_output(session, 3)); // admitted without its 3 lines
        assert_eq!(queue.len(), 2, "the failure was admitted over the cap");
        assert_eq!(
            queue.dropped_unsolicited(),
            1,
            "only the refused event counts as refused; the stripped one was delivered"
        );

        match queue.next().expect("the first output") {
            SessionEvent::ServerOutput {
                lines,
                dropped,
                failure,
                ..
            } => {
                assert_eq!(lines.len(), 2);
                assert_eq!(dropped, 0);
                assert!(failure.is_none());
            }
            other => panic!("expected ServerOutput, got {other:?}"),
        }
        match queue.next().expect("the failure") {
            SessionEvent::ServerOutput {
                lines,
                dropped,
                failure,
                ..
            } => {
                assert!(lines.is_empty(), "its lines were shed at the cap");
                assert_eq!(
                    dropped,
                    5 + 3,
                    "the refused event's lines and its own shed lines are both reported"
                );
                assert!(failure.is_some(), "the failure survived the cap");
            }
            other => panic!("expected ServerOutput, got {other:?}"),
        }
        assert_eq!(
            queue.pending_dropped_lines(session),
            0,
            "nothing still owed"
        );
    }

    #[test]
    fn a_failed_read_under_the_cap_keeps_its_lines() {
        let (sink, queue) = event_channel(caps(4));
        let session = SessionId::allocate();
        emit(&sink, failed_output(session, 3));
        match queue.next().expect("the failure") {
            SessionEvent::ServerOutput {
                lines,
                dropped,
                failure,
                ..
            } => {
                assert_eq!(lines.len(), 3, "nothing is shed below the cap");
                assert_eq!(dropped, 0);
                assert!(failure.is_some());
            }
            other => panic!("expected ServerOutput, got {other:?}"),
        }
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

    /// The case the two single-class tests above each miss: one session, at
    /// its cap on `ServerOutput`, with **no** transaction state queued to
    /// coalesce into. The cap must not swallow the state change — a UI that
    /// misses it shows Commit and Rollback disabled while a transaction is
    /// open on the connection, which is exactly the silent transaction loss
    /// `SPEC.md` §10 forbids.
    #[test]
    fn a_transaction_state_change_is_never_lost_to_a_server_output_burst() {
        let (sink, queue) = event_channel(caps(4));
        let session = SessionId::allocate();
        for _ in 0..4 {
            emit(&sink, output(session, 1));
        }
        emit(&sink, output(session, 3));
        assert_eq!(queue.len(), 4, "the fifth output is over the cap");
        assert_eq!(queue.dropped_unsolicited(), 1);

        emit(
            &sink,
            SessionEvent::TransactionStateChanged {
                session,
                possibly_active: true,
            },
        );
        assert_eq!(
            queue.len(),
            5,
            "a transaction state is admitted over the cap when there is none to fold into"
        );
        assert_eq!(
            queue.dropped_unsolicited(),
            1,
            "and admitting it is not a drop"
        );

        // A second one folds into the first, so the class costs one event.
        emit(
            &sink,
            SessionEvent::TransactionStateChanged {
                session,
                possibly_active: false,
            },
        );
        assert_eq!(queue.len(), 5, "the class is bounded at one over the cap");

        let states: Vec<bool> = std::iter::from_fn(|| queue.next())
            .filter_map(|event| match event {
                SessionEvent::TransactionStateChanged {
                    possibly_active, ..
                } => Some(possibly_active),
                _ => None,
            })
            .collect();
        assert_eq!(states, vec![false], "with the newest value it computed");
    }

    /// Lines lost with no later `ServerOutput` to ride out on stay readable,
    /// so a consumer can still say "output truncated" for a session that went
    /// quiet or ended.
    #[test]
    fn dropped_lines_with_no_later_output_stay_readable_per_session() {
        let (sink, queue) = event_channel(caps(1));
        let session = SessionId::allocate();
        emit(&sink, output(session, 2));
        emit(&sink, output(session, 5));
        emit(&sink, output(session, 4));

        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pending_dropped_lines(session), 9);
        assert_eq!(
            queue.pending_dropped_lines(SessionId::allocate()),
            0,
            "a session this queue has never heard of has lost nothing"
        );

        // Draining the survivor does not clear the debt: it was recorded after
        // that event was already stamped.
        let _ = queue.next();
        assert_eq!(queue.pending_dropped_lines(session), 9);

        emit(&sink, output(session, 1));
        match queue.next().expect("the next delivered output") {
            SessionEvent::ServerOutput { dropped, .. } => assert_eq!(dropped, 9),
            other => panic!("expected ServerOutput, got {other:?}"),
        }
        assert_eq!(queue.pending_dropped_lines(session), 0);
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
