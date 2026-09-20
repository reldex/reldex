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
//! * **`Ended`** — the session announced its `Terminal` and has no handle to
//!   keep: its open was abandoned, its connect failed, or its worker thread
//!   could not be spawned. The entry is a tombstone, which is what tells a
//!   connect that finishes *afterwards* that it was abandoned.
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
//! entry that owns its reply has been recorded.
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

use reldex_db_driver_api::{ConnectionParams, DatabaseDriver, DbResult};

use crate::events::{EventSink, RequestId};
use crate::ids::SessionId;
use crate::reply::{OpenedSession, ReplyTo};
use crate::session::{DatabaseSession, SessionLimits, SessionManager};
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
        /// What [`DatabaseSession::has_possibly_active_transaction`] said at
        /// the instant this session left the registry.
        ///
        /// `true` means the server rolled a transaction back as the connection
        /// went, and the user has to be told. It is the same conservative
        /// answer that method always gives — it can be `true` with nothing
        /// actually open — and it is a **snapshot**: a statement accepted
        /// concurrently with this call is not in it. The path that decides on
        /// the worker thread, after the queue has drained, is
        /// [`DatabaseSession::close`] (ADR-0002 K4); `abandon` cannot be that
        /// path, because it must not block.
        transaction_possibly_lost: bool,
    },
    /// The session was already abandoned, or has already ended. Abandoning is
    /// idempotent: the second call changes nothing and produces nothing.
    Ending,
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
                                &ready,
                                self.limits,
                            );
                            sessions.insert(id, Entry::Open(Arc::new(session)));
                            Some(OpenDecision::Opened(reply, opened))
                        }
                        Err(error) => {
                            sessions.insert(id, Entry::Ended);
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
                shared.emit_terminal();
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
                    sessions.insert(id, Entry::Ended);
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
            shared.emit_terminal();
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
    #[must_use]
    pub fn state(&self, id: SessionId) -> Option<RegisteredSession> {
        self.inner.lock().get(&id).map(Entry::state)
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
    ///   (ADR-0002 K5). That is a transaction loss, and the returned
    ///   [`Abandoned::Open`] says whether one was possibly lost so the caller
    ///   can tell the user (`SPEC.md` §10: never hide it). The session's
    ///   `Terminal` follows when the worker reaches the abandon; this call
    ///   does not wait for it.
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
                let transaction_possibly_lost = session.has_possibly_active_transaction();
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
    /// Only the first of those is guaranteed not to block, which is why
    /// [`SessionRegistry::abandon`] exists as the teardown path and this one
    /// is the clean-up after it.
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
    // `Closed`, not `Lost`: the abandon won and nothing failed.
    shared.emit_terminal();
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
    /// abandon *first*, all of them, and only then dropped, so their bounded
    /// `Drop` waits overlap instead of running one after another. None of them
    /// commits anything.
    ///
    /// The honest cost, because ADR-0003 A17 says to state it: a worker inside
    /// an uninterruptible driver call still costs its session up to
    /// [`crate::DROP_SHUTDOWN_TIMEOUT`] here before it is detached, and a
    /// detached worker keeps its connection until that call returns.
    fn drop(&mut self) {
        let entries: Vec<Entry> = {
            let mut sessions = self.inner.lock();
            sessions.drain().map(|(_, entry)| entry).collect()
        };
        let mut open: Vec<Arc<DatabaseSession>> = Vec::new();
        for entry in entries {
            match entry {
                Entry::Opening(opening) => cancel_open(opening),
                Entry::Open(session) | Entry::Ending(session) => {
                    session.abandon_now();
                    open.push(session);
                }
                Entry::Ended => {}
            }
        }
        drop(open);
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
