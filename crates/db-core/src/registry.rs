//! The session registry: a non-blocking `open`, and `abandon`
//! (`docs/exec-plans/active/phase-1.md` §B3; ADR-0002 amendment R1–R5).
//!
//! # What this adds, and what it deliberately does not
//!
//! [`crate::SessionManager::open_session`] is unchanged and still blocks the
//! caller on the connect. This is the other shape, the one a UI needs:
//! [`SessionRegistry::open`] returns a [`SessionId`] immediately, the connect
//! runs on the session's own worker thread, and its outcome arrives as exactly
//! one [`crate::SessionEvent::Opened`] or [`crate::SessionEvent::OpenFailed`] carrying the
//! caller's [`RequestId`]. Nothing here pools, reuses or replaces a session:
//! a reconnect is a new `open` with a new [`SessionId`] (`SPEC.md` §9, §18).
//!
//! # The state machine, and which thread moves it
//!
//! ```text
//!                 open()                complete_open(Ok)
//!      (nothing) --------> Opening -------------------------> Open
//!                            |  \                              |
//!         abandon()/retire() |   \ complete_open(Err)          | abandon()
//!         registry drop      |    \                            v
//!                            v     `--------------------> Ending
//!                          Ended <--------------------------'  |
//!                                          retire()            | retire()
//!                                                              v
//!                                                          (nothing)
//! ```
//!
//! * **`Opening`** — the worker thread exists and is inside
//!   `DatabaseDriver::connect`. There is no session handle yet, so
//!   [`SessionRegistry::get`] answers `None` and nothing can be submitted.
//! * **`Open`** — the connection was adopted. `get` hands out the
//!   `Arc<DatabaseSession>`; this is the only state in which it does.
//! * **`Ending`** — [`SessionRegistry::abandon`] has told the session to stop.
//!   The handle is kept (so nothing is dropped, and therefore nothing is
//!   *waited on*, inside `abandon`) until the consumer sees
//!   [`crate::SessionEvent::Terminal`] and calls [`SessionRegistry::retire`].
//! * **`Ended`** — an open that was **abandoned while its connect was still
//!   running**. The entry is a tombstone with one job: tell that connect, when
//!   it finishes, that nobody will adopt it. The connect arriving is what
//!   resolves it, and the registry then drops the entry itself — so this state
//!   cannot accumulate even for a consumer that never retires anything.
//!
//! # What the registry keeps, and for how long
//!
//! An entry exists exactly as long as the registry owns something for that
//! session: a worker it must be able to refuse (`Opening`, `Ended`) or a
//! handle it must release (`Open`, `Ending`). A session it owns nothing for
//! keeps **no entry at all** — an open that failed, an open whose thread would
//! not spawn, and an abandoned open once its late connect has arrived — because
//! their `Terminal` has already been emitted and a tombstone with no reader is
//! only a leak waiting for a consumer that forgets to retire. `state` then
//! answers `None` and `retire` `false`; "forgotten" and "never existed" are
//! deliberately indistinguishable, which is safe because `Terminal` — not a
//! poll — is what a consumer routes on (ADR-0003 A18).
//!
//! A session that really opened is the case that still needs the consumer:
//! only [`SessionRegistry::retire`] releases the registry's handle.
//! [`SessionRegistry::counts`] exists so that a consumer which forgets is
//! visible rather than slowly leaking.
//!
//! Every transition happens under this module's one mutex, and that mutex is
//! the single place `open`, `abandon`, `retire`, the registry's own teardown
//! and a worker reporting its connect are serialised against each other. **No
//! event is ever emitted while it is held**, and no driver call is ever made
//! while it is held: each entry point decides under the lock, releases it, and
//! only then emits or calls the driver — the same discipline M2.5 established
//! for `SessionShared` (ADR-0002 E3/E4).
//!
//! The one thing the lock *is* held across is `thread::Builder::spawn`, and
//! that is deliberate: it closes the race where a connect finishes before the
//! entry that owns its reply has been recorded. It is not free — `spawn` is a
//! syscall, so opens serialise on it — and ADR-0002 R2 carries the measured
//! cost at a burst rate no application produces.
//!
//! # Nothing is ever joined on a path that can be parked
//!
//! A `connect` cannot be interrupted — that is what forced the driver's own
//! helper thread (ADR-0002 H1/H2, spike U-15) — so abandoning one cannot mean
//! waiting for it. It means: answer the open now, announce the session's end
//! now, and **detach** the worker thread. When the connect eventually returns,
//! the worker finds no entry to adopt it, closes the connection on its own
//! thread and exits. A late success therefore never produces an `Opened` after
//! its `OpenFailed`, and never leaves a live database session behind
//! (ADR-0003 A17, applied to the connect).

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Instant;

use reldex_db_driver_api::{ConnectionParams, DatabaseDriver, DbResult};

use crate::events::{EventSink, RequestId};
use crate::ids::SessionId;
use crate::reply::{OpenedSession, ReplyTo};
use crate::session::{
    AbandonReply, DROP_SHUTDOWN_TIMEOUT, DatabaseSession, SessionLimits, SessionManager,
};
use crate::shared::SessionShared;
use crate::worker::{self, Adoption, Command, Ready};

/// Where a session the registry knows about is in its lifecycle.
///
/// A coarser thing than [`crate::SessionLifecycle`], and about a different
/// subject: this describes the *registration*, not the connection. A session
/// can be `Open` here while its connection is already `Lost`, until the
/// consumer retires it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RegisteredSession {
    /// The connect is running on the session's worker thread. There is no
    /// handle yet.
    Opening,
    /// The connection is open and [`SessionRegistry::get`] hands out its
    /// handle.
    Open,
    /// [`SessionRegistry::abandon`] has told this session to end. Its handle
    /// is kept until [`SessionRegistry::retire`], but is no longer handed out.
    Ending,
    /// The session ended without ever producing a handle, and the entry is the
    /// tombstone that stops a late connect being adopted.
    Ended,
}

/// What [`SessionRegistry::abandon`] found, and therefore what it did.
///
/// §B3 sketches `abandon` as returning unit. It returns this instead, because
/// exactly one of its cases costs the user something and a caller that cannot
/// see which one cannot report it: abandoning an **open** session releases the
/// connection without committing, so a transaction it held is rolled back by
/// the server. `SPEC.md` §10 forbids hiding that, and the core has no UI to
/// say it with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Abandoned {
    /// No session with that id: it was never opened here, or it has already
    /// been retired.
    Unknown,
    /// The connect had not finished. Its request gets exactly one
    /// [`crate::SessionEvent::OpenFailed`] with
    /// [`reldex_db_driver_api::ErrorKind::Cancelled`], this session's
    /// [`crate::SessionEvent::Terminal`] follows it, and a connection that arrives
    /// late is closed on the worker thread rather than adopted.
    Connecting,
    /// An open session was released without committing. Its
    /// [`crate::SessionEvent::Terminal`] follows once its worker reaches the abandon
    /// — immediately when it is idle, and when the current driver call returns
    /// when it is not.
    Open {
        /// A **lower bound**, available synchronously, on whether this abandon
        /// costs the user work.
        ///
        /// `true` — tell the user now: the session may have been carrying a
        /// transaction, and the server rolls it back as the connection goes.
        /// It is deliberately wide: it is `true` if a transaction may be open,
        /// *or* any request is still outstanding, *or* the worker is inside a
        /// driver call, because each of those can open a transaction after
        /// this call reads it. Like every conservative answer in this crate it
        /// can be `true` with nothing actually open (ADR-0002 K7).
        ///
        /// `false` — **not** "nothing was lost". It means only that nothing
        /// visible from this thread, at this instant, says otherwise; a
        /// statement the consumer submitted a moment ago can still run and
        /// open a transaction before the worker reaches the abandon. The
        /// answer that settles it is `transaction_possibly_lost` on this
        /// session's [`crate::SessionEvent::Terminal`], which is computed on
        /// the worker thread once the queue has drained — the same place
        /// ADR-0002 K4 already decides — and which a consumer must surface
        /// either way.
        transaction_possibly_lost: bool,
    },
    /// The session was already abandoned, or has already ended. Abandoning is
    /// idempotent: the second call changes nothing and produces nothing.
    Ending,
}

/// What [`SessionRegistry::counts`] reports: how many registered sessions are
/// in each [`RegisteredSession`] state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct RegistryCounts {
    /// Sessions whose connect has not returned.
    pub opening: usize,
    /// Open sessions, available from [`SessionRegistry::get`].
    pub open: usize,
    /// Sessions told to end whose entry has not been retired.
    pub ending: usize,
    /// Tombstones for an abandoned open whose late connect has not arrived.
    pub ended: usize,
}

/// One session's registration.
enum Entry {
    Opening(Opening),
    Open(Arc<DatabaseSession>),
    Ending(Arc<DatabaseSession>),
    Ended,
}

impl Entry {
    const fn state(&self) -> RegisteredSession {
        match self {
            Self::Opening(_) => RegisteredSession::Opening,
            Self::Open(_) => RegisteredSession::Open,
            Self::Ending(_) => RegisteredSession::Ending,
            Self::Ended => RegisteredSession::Ended,
        }
    }
}

/// A session whose worker exists and whose connect has not returned.
///
/// It holds the two halves of the session that exist before the connection
/// does — the command channel and the thread handle — and, crucially, **the
/// open's reply channel**. Putting the reply here rather than on the worker
/// thread is what makes `abandon` immediate: whoever takes this entry out from
/// under the registry's lock is the one that answers the open, and there is
/// exactly one such taker.
struct Opening {
    shared: Arc<SessionShared>,
    command_tx: Sender<Command>,
    join: JoinHandle<()>,
    reply: ReplyTo<OpenedSession>,
}

struct Inner {
    sessions: Mutex<HashMap<SessionId, Entry>>,
    events: EventSink,
    limits: SessionLimits,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, HashMap<SessionId, Entry>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Called on the worker thread, once, the moment `connect` returns.
    ///
    /// This is the race's only resolution point. Either the entry is still
    /// `Opening`, in which case this adopts the connection, records the
    /// session and answers the open; or it is not, in which case `abandon` (or
    /// the registry's teardown) already answered the open and this connection
    /// must be closed by the thread that opened it.
    fn complete_open(&self, id: SessionId, outcome: DbResult<Ready>) -> Adoption {
        // `outcome` is moved *into* the decision or left here to be dropped
        // after the lock is released, never inside it: it carries the driver's
        // `Arc<dyn CancelHandle>`, and releasing a driver-owned object under a
        // `db-core` lock is the kind of edge this module exists to not have.
        let mut unadopted = None;
        let decision = {
            let mut sessions = self.lock();
            match sessions.get(&id) {
                Some(Entry::Opening(_)) => {
                    let Some(Entry::Opening(opening)) = sessions.remove(&id) else {
                        unreachable!("the entry was `Opening` a line ago, under this same lock")
                    };
                    let Opening {
                        shared,
                        command_tx,
                        join,
                        reply,
                    } = opening;
                    match outcome {
                        Ok(ready) => {
                            // One clone, not two: the connect warnings go to
                            // the `Opened` event and to the session's own
                            // accessor, and `assemble` takes `Ready` by value
                            // so it can move its copy in.
                            let opened = OpenedSession {
                                connection: ready.connection_id,
                                cancel_kind: ready.cancel_kind,
                                warnings: ready.connect_warnings.clone(),
                            };
                            let session = DatabaseSession::assemble(
                                id,
                                command_tx,
                                join,
                                shared,
                                ready,
                                self.limits,
                            );
                            // Inserted **before** the lock is released and
                            // before `Opened` is emitted, which is what makes
                            // "`Opened` is this session's first event, and the
                            // handle is there as soon as it arrives" true
                            // rather than usually-true. It is safe only because
                            // this runs on the worker thread *before* that
                            // worker enters its command loop: nothing else can
                            // produce an event for this session yet.
                            sessions.insert(id, Entry::Open(Arc::new(session)));
                            Some(OpenDecision::Opened(reply, opened))
                        }
                        Err(error) => {
                            // Removed rather than tombstoned. The registry owns
                            // nothing for this id any more and never will: the
                            // `OpenFailed` and `Terminal` below are its last
                            // words, so keeping an entry would only be a leak
                            // waiting for a consumer that forgets to retire.
                            sessions.remove(&id);
                            // The channel and the thread handle die with this
                            // scope. Dropping the handle detaches, which is
                            // right and costs nothing: a worker whose connect
                            // failed has already returned or is about to.
                            drop(command_tx);
                            drop(join);
                            Some(OpenDecision::Failed(reply, shared, error))
                        }
                    }
                }
                // Abandoned while connecting, or the registry is gone. Nothing
                // to answer — the open's one reply has already been produced —
                // and the worker closes what it opened.
                _ => {
                    // The connect that the `Ended` tombstone was waiting for
                    // has now arrived, so the tombstone has done its job and
                    // the registry is finished with this id. Dropping it here
                    // is what bounds the map for a consumer that abandons
                    // without retiring; `state` and `retire` then answer
                    // `None`/`false`, which is documented on both.
                    if matches!(sessions.get(&id), Some(Entry::Ended)) {
                        sessions.remove(&id);
                    }
                    unadopted = Some(outcome);
                    None
                }
            }
        };

        drop(unadopted);
        match decision {
            None => Adoption::Abandoned,
            Some(OpenDecision::Opened(reply, opened)) => {
                reply.answer(Ok(opened));
                Adoption::Adopted
            }
            Some(OpenDecision::Failed(reply, shared, error)) => {
                // A connect that failed never produced a session, so the
                // lifecycle it announces is `Lost` with the connect's own
                // error as the cause — kind, native code and cause chain
                // intact (ADR-0002 K8).
                shared.mark_lost_from(&error);
                reply.answer(Err(error));
                // Nothing was ever connected, so there was no transaction to
                // lose.
                shared.emit_terminal(false);
                Adoption::Abandoned
            }
        }
    }
}

/// What [`Inner::complete_open`] decided under the lock, to be carried out
/// after releasing it.
enum OpenDecision {
    Opened(ReplyTo<OpenedSession>, OpenedSession),
    Failed(ReplyTo<OpenedSession>, Arc<SessionShared>, crate::DbError),
}

/// Owns every event-bound session, and serialises `open`, `abandon`, `retire`
/// and a worker's connect against each other.
///
/// See the module documentation for the state machine and for what `abandon`
/// guarantees.
pub struct SessionRegistry {
    inner: Arc<Inner>,
    manager: SessionManager,
}

impl SessionRegistry {
    /// Builds a registry whose sessions are opened with `manager`'s
    /// [`SessionLimits`] and whose events all go to `events`.
    ///
    /// One registry per application is the intended shape, feeding the one
    /// [`crate::EventQueue`] the consumer drains. Nothing forbids more.
    #[must_use]
    pub fn new(manager: SessionManager, events: EventSink) -> Self {
        Self {
            inner: Arc::new(Inner {
                sessions: Mutex::new(HashMap::new()),
                events,
                limits: manager.limits(),
            }),
            manager,
        }
    }

    /// The manager this registry opens sessions with.
    #[must_use]
    pub const fn manager(&self) -> SessionManager {
        self.manager
    }

    /// Opens a session. **Never blocks and never fails synchronously.**
    ///
    /// Returns the [`SessionId`] the session will have for its whole life; the
    /// worker thread is spawned here, `connect` runs on it, and its outcome
    /// arrives as exactly one [`crate::SessionEvent::Opened`] or
    /// [`crate::SessionEvent::OpenFailed`] carrying `request`. Everything that could
    /// fail synchronously — the thread not spawning, most obviously — is
    /// reported as that event instead, because a caller that has already been
    /// given a `SessionId` needs one answer, not two shapes of answer.
    ///
    /// The session is **not** available from [`SessionRegistry::get`] until it
    /// has opened, so nothing can be submitted to a connection that does not
    /// exist yet; a consumer routes on `Opened` and asks for the handle then.
    ///
    /// The open reserves one slot against
    /// [`SessionLimits::max_outstanding_requests`], like any other request on
    /// the event path. It can never be refused for it: the session is brand
    /// new, so it has nothing outstanding.
    ///
    /// # Panics
    ///
    /// Never in practice. The two `expect`s below assert properties of a
    /// `SessionShared` built a line earlier — it has never been bound to a
    /// sink, has not ended, and has no outstanding requests — and neither can
    /// be false for a value nothing else has seen yet.
    pub fn open(
        &self,
        driver: Arc<dyn DatabaseDriver>,
        params: ConnectionParams,
        request: RequestId,
    ) -> SessionId {
        let id = SessionId::allocate();
        let shared = Arc::new(SessionShared::new(id));
        // Bound *before* the worker exists, which is the point
        // (`phase-1-m2-5-event-queue.md` §6): this session's very first events
        // — including the ones the registry itself produces, before there is
        // any worker to produce them — go through the same per-session emit
        // lock as everything else, so ordering rule 1 holds from the start.
        shared
            .bind_events(self.inner.events.clone())
            .expect("a SessionShared built a line ago has no sink and has not ended");
        shared
            .reserve_request(self.inner.limits.max_outstanding_requests().get())
            .expect("a session with nothing outstanding always has room for its own open");
        let reply = ReplyTo::event(Arc::clone(&shared), id, request, ());

        let inner = Arc::clone(&self.inner);
        let spawn_failure = {
            // The lock is held across the spawn deliberately: the worker can
            // finish its connect before `spawn` has even returned, and
            // `complete_open` must not be able to look for an entry that this
            // call has not recorded yet.
            let mut sessions = self.inner.lock();
            match worker::spawn(
                driver,
                params,
                id,
                self.inner.limits,
                Arc::clone(&shared),
                Box::new(move |outcome| inner.complete_open(id, outcome)),
            ) {
                Ok(spawned) => {
                    sessions.insert(
                        id,
                        Entry::Opening(Opening {
                            shared,
                            command_tx: spawned.command_tx,
                            join: spawned.join,
                            reply,
                        }),
                    );
                    None
                }
                Err(error) => {
                    // No entry is recorded at all. There is nothing to own and
                    // nothing more to say after the two events below, so a
                    // tombstone would only be a leak waiting for a consumer
                    // that forgets to retire — the same reasoning as in
                    // `Inner::complete_open`.
                    Some((shared, reply, error))
                }
            }
        };

        if let Some((shared, reply, error)) = spawn_failure {
            // No thread, no connection, nothing to close: the session is over
            // before it started, and says so in its own two events. `Lost`
            // with the cause, exactly like a connect that failed — only an
            // abandon reports `Closed`, because only an abandon is deliberate.
            shared.mark_lost_from(&error);
            reply.answer(Err(error));
            // No thread, no connection, no transaction to lose.
            shared.emit_terminal(false);
        }
        id
    }

    /// The handle for an **open** session, for submitting work.
    ///
    /// `None` while the session is still connecting, once it has been
    /// abandoned, and after it has been retired — the three cases in which
    /// handing one out would invite a submit the registry has already decided
    /// against. A session that is open but whose connection has since been
    /// *lost* is still handed out: its requests are answered with the loss,
    /// which is what a consumer needs, and it is retired on its `Terminal`
    /// like any other.
    #[must_use]
    pub fn get(&self, id: SessionId) -> Option<Arc<DatabaseSession>> {
        match self.inner.lock().get(&id) {
            Some(Entry::Open(session)) => Some(Arc::clone(session)),
            Some(Entry::Opening(_) | Entry::Ending(_) | Entry::Ended) | None => None,
        }
    }

    /// Where a session is in its registration, if the registry still knows it.
    ///
    /// `None` also covers a session the registry has finished with on its own:
    /// an open that failed, and an abandoned open once its late connect has
    /// arrived and been closed. Those keep no entry, because there is nothing
    /// left to own and their [`crate::SessionEvent::Terminal`] has already been
    /// emitted — so "the registry forgot it" and "it never existed" are
    /// deliberately indistinguishable here. Route on events, not on this.
    #[must_use]
    pub fn state(&self, id: SessionId) -> Option<RegisteredSession> {
        self.inner.lock().get(&id).map(Entry::state)
    }

    /// How many sessions the registry still holds an entry for.
    ///
    /// A diagnostic, not a control: it exists so that a consumer which forgets
    /// to [`SessionRegistry::retire`] is *visible* rather than merely slowly
    /// leaking. In a healthy application this tracks the number of live
    /// worksheets; if it grows without bound, sessions are not being retired on
    /// their [`crate::SessionEvent::Terminal`].
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// Whether the registry holds no sessions at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }

    /// How many entries are in each [`RegisteredSession`] state.
    ///
    /// The other half of the diagnostic [`SessionRegistry::len`] starts: a
    /// count that keeps growing in `Ending` or `Ended` is a consumer that is
    /// not retiring, while one that grows in `Opening` is a driver that is not
    /// connecting.
    #[must_use]
    pub fn counts(&self) -> RegistryCounts {
        let mut counts = RegistryCounts::default();
        for entry in self.inner.lock().values() {
            match entry.state() {
                RegisteredSession::Opening => counts.opening += 1,
                RegisteredSession::Open => counts.open += 1,
                RegisteredSession::Ending => counts.ending += 1,
                RegisteredSession::Ended => counts.ended += 1,
            }
        }
        counts
    }

    /// Gives up on a pending connect, or drops an open session **without
    /// committing**. Never blocks, and is never refused.
    ///
    /// * While the session is still connecting: its open gets exactly one
    ///   [`crate::SessionEvent::OpenFailed`] with
    ///   [`reldex_db_driver_api::ErrorKind::Cancelled`] *immediately*,
    ///   followed by this session's [`crate::SessionEvent::Terminal`]. The worker
    ///   thread is detached rather than joined — a connect cannot be
    ///   interrupted — and when it finally returns, a connection that arrived
    ///   late is closed on that thread. Nothing is adopted late, so no
    ///   `Opened` can follow the `OpenFailed`, and no live database session is
    ///   left behind.
    /// * Once the session is open: the worker is asked to release everything
    ///   with the same [`Drop`]-style abandon [`DatabaseSession`] uses, which
    ///   **never commits and never rolls back explicitly** — the server's own
    ///   rollback-on-disconnect resolves whatever transaction the session held
    ///   (ADR-0002 K5). That is a transaction loss, and the session's
    ///   `Terminal` — which follows when the worker reaches the abandon; this
    ///   call does not wait for it — carries the authoritative
    ///   `transaction_possibly_lost` the caller must show the user
    ///   (`SPEC.md` §10: never hide it). The [`Abandoned::Open`] returned here
    ///   carries a **lower bound** on the same answer, for a caller that wants
    ///   to warn immediately; see that variant.
    /// * Twice, or after the session ended: [`Abandoned::Ending`], and
    ///   nothing happens. Abandoning is idempotent.
    ///
    /// **It reserves no request slot, so it can never be refused for lack of
    /// one** — which is the point, since it is the only way to stop a session
    /// that is still connecting. Neither of the events it can produce is a new
    /// request's reply: the `OpenFailed` is the answer to the open, whose slot
    /// was reserved by [`SessionRegistry::open`], and `Terminal` is not a
    /// reply at all. The published bound on what one session can have waiting
    /// in the queue is therefore unchanged by this method.
    pub fn abandon(&self, id: SessionId) -> Abandoned {
        let next = {
            let mut sessions = self.inner.lock();
            match sessions.remove(&id) {
                Some(Entry::Opening(opening)) => {
                    // A tombstone, not a removal: it is what tells the connect
                    // that is still running that nobody will adopt it.
                    sessions.insert(id, Entry::Ended);
                    Some(Next::CancelOpen(opening))
                }
                Some(Entry::Open(session)) => {
                    let handle = Arc::clone(&session);
                    sessions.insert(id, Entry::Ending(session));
                    Some(Next::AbandonOpen(handle))
                }
                Some(entry) => {
                    sessions.insert(id, entry);
                    None
                }
                None => return Abandoned::Unknown,
            }
        };

        match next {
            Some(Next::CancelOpen(opening)) => {
                cancel_open(opening);
                Abandoned::Connecting
            }
            Some(Next::AbandonOpen(session)) => {
                // A session the consumer already closed itself is *ending*,
                // not being ended by this call: saying `Open` would invite a
                // "your transaction was lost" message for a transaction the
                // close resolved. The entry still moves to `Ending` either
                // way, so `get` stops handing it out, and the abandon is still
                // issued — it is idempotent and the worker may be gone.
                let already_ended = session.session_state().is_terminal();
                let transaction_possibly_lost = session.may_be_carrying_work();
                session.abandon_now();
                if already_ended {
                    Abandoned::Ending
                } else {
                    Abandoned::Open {
                        transaction_possibly_lost,
                    }
                }
            }
            None => Abandoned::Ending,
        }
    }

    /// Releases the registry's own hold on a session, once the consumer has
    /// seen its [`crate::SessionEvent::Terminal`].
    ///
    /// `Terminal` is the only event that means a session will produce nothing
    /// further (ordering rule 3), so it is the only point at which per-session
    /// state may be retired — that is as true of the registry's map as it is
    /// of the adapter's. Returns whether there was anything to retire.
    ///
    /// Retiring a session that has **not** ended yet is allowed and does the
    /// safe thing rather than the silent one: a session still connecting is
    /// abandoned first, so its open is still answered and its `Terminal` still
    /// announced; an open one is dropped, which is [`DatabaseSession`]'s own
    /// `Drop` — a bounded wait, then detach, and never a commit (ADR-0002 K5).
    ///
    /// **Retiring an open session is therefore both blocking and lossy.** It
    /// can park the calling thread for up to [`crate::DROP_SHUTDOWN_TIMEOUT`]
    /// while the worker finishes whatever it is inside, and because that drop
    /// resolves nothing, the server rolls back any transaction the session
    /// held — reported on the session's `Terminal` as
    /// `transaction_possibly_lost`, which arrives *after* this call. Neither is
    /// acceptable on a UI thread, which is why the intended sequence is
    /// [`SessionRegistry::abandon`] (immediate, never blocks, reports the loss)
    /// and then `retire` **only on `Terminal`**, where there is nothing left to
    /// wait for.
    ///
    /// Retire every session, always, on its `Terminal`: it is the only thing
    /// that releases the registry's own entry, and
    /// [`SessionRegistry::counts`] is there to make a consumer that forgets
    /// visible. Retiring twice is harmless — the second call returns `false`,
    /// which means only "there was nothing left to release".
    ///
    /// A caller that kept its own clone of the `Arc<DatabaseSession>` keeps
    /// the session alive past this call; the registry can only let go of its
    /// own.
    pub fn retire(&self, id: SessionId) -> bool {
        let entry = {
            let mut sessions = self.inner.lock();
            sessions.remove(&id)
        };
        match entry {
            None => false,
            Some(Entry::Opening(opening)) => {
                cancel_open(opening);
                true
            }
            // Dropped here, outside the lock, because dropping a session that
            // has not ended runs `DatabaseSession::Drop` and that waits.
            Some(Entry::Open(session) | Entry::Ending(session)) => {
                drop(session);
                true
            }
            Some(Entry::Ended) => true,
        }
    }
}

/// What [`SessionRegistry::abandon`] decided under the lock.
enum Next {
    CancelOpen(Opening),
    AbandonOpen(Arc<DatabaseSession>),
}

/// Gives up on a connect that has not returned.
///
/// Called with the registry's lock **released**, because it emits. The order
/// matters and is the order ordering rule 3 asks for: the open's one reply
/// first, then the session's one `Terminal`.
fn cancel_open(opening: Opening) {
    let Opening {
        shared,
        command_tx,
        join,
        reply,
    } = opening;
    // Read by `SessionShared::terminal_error`, which is what the dropped
    // reply channel below asks for its failure — so the `OpenFailed` says
    // `Cancelled`, and says it whether the reply is dropped here or anywhere
    // else this session's open could still be dropped.
    shared.mark_open_cancelled();
    shared.mark_closed();
    // Exactly one `OpenFailed { Cancelled }`, through the same `Drop`
    // mechanism every other request's "never zero replies" comes from
    // (ADR-0002 E2). Nothing bespoke, and nothing that could answer twice.
    drop(reply);
    // `Closed`, not `Lost`: the abandon won and nothing failed. And nothing was
    // connected, so there is no transaction to have lost — whatever the late
    // connection turns out to be, it has run no statement of this consumer's.
    shared.emit_terminal(false);
    drop(command_tx);
    // **Detached, never joined.** The worker is inside a `connect` that cannot
    // be interrupted (ADR-0002 H1, spike U-15); joining it here would make
    // `abandon` wait for exactly the thing it exists to stop waiting for
    // (ADR-0003 A17). It closes the connection itself when the connect
    // returns and finds the tombstone above.
    drop(join);
}

impl Drop for SessionRegistry {
    /// Ends every session the registry still holds, promptly.
    ///
    /// Sessions still connecting are abandoned exactly as
    /// [`SessionRegistry::abandon`] does — answered, announced, detached — so
    /// nothing is joined on a parked connect and a connection that arrives
    /// afterwards is still closed on its own thread. Open sessions are told to
    /// abandon *first*, all of them, and are then waited for against **one
    /// deadline shared by the whole teardown**. None of them commits anything.
    ///
    /// The bound, stated because ADR-0003 A17 says to state it: this costs at
    /// most one [`crate::DROP_SHUTDOWN_TIMEOUT`] in total, **not** one per
    /// session, however many sessions are stuck. Issuing every abandon before
    /// waiting for any of them is only half of that; the other half is that the
    /// wait happens here, against the shared deadline, and takes each session's
    /// worker handle with it, so the `DatabaseSession::drop` that follows has
    /// nothing left to wait for. A worker still inside an uninterruptible
    /// driver call when the deadline passes is **detached**: it keeps its
    /// connection and closes it when that call finally returns.
    fn drop(&mut self) {
        let entries: Vec<Entry> = {
            let mut sessions = self.inner.lock();
            sessions.drain().map(|(_, entry)| entry).collect()
        };
        // Taken once, before the first abandon is issued, and shared by every
        // wait below: that is what makes the teardown cost one timeout rather
        // than N of them.
        let deadline = Instant::now() + DROP_SHUTDOWN_TIMEOUT;
        let mut pending: Vec<(Arc<DatabaseSession>, Option<AbandonReply>)> = Vec::new();
        for entry in entries {
            match entry {
                Entry::Opening(opening) => cancel_open(opening),
                Entry::Open(session) | Entry::Ending(session) => {
                    // Issued for all of them first, so the waits overlap.
                    let reply = session.begin_abandon();
                    pending.push((session, reply));
                }
                Entry::Ended => {}
            }
        }
        for (session, reply) in pending {
            session.finish_abandon(reply, deadline);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Abandoned, RegisteredSession, SessionRegistry};

    #[allow(clippy::extra_unused_type_parameters)]
    const fn assert_send<T: Send>() {}

    #[allow(clippy::extra_unused_type_parameters)]
    const fn assert_sync<T: Sync>() {}

    #[test]
    fn a_registry_can_be_shared_between_threads() {
        // The documented shape: one registry behind an `Arc`, with the UI
        // thread opening and abandoning while worker threads complete their
        // connects through it. Asserted here so the claim cannot rot — an
        // `Opening` entry holds an `mpsc::Sender` and a `ReplyTo`, neither of
        // which is `Sync`, and only the mutex makes this true.
        assert_send::<SessionRegistry>();
        assert_sync::<SessionRegistry>();
        assert_send::<Abandoned>();
        assert_sync::<RegisteredSession>();
    }
}
