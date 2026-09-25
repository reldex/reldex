//! [`SessionResults`]: every open result store of one session, and the one
//! place that decides which of the session's events end them.
//!
//! The rules that turn events into store transitions are core rules
//! (ADR-0004 RS2), so they live here rather than in the adapter: a consumer
//! hands every event of the session to [`SessionResults::observe`] as it
//! drains it, tells it when it submits a commit, rollback or
//! rollback-to-savepoint command, and pumps the stores after each drain.

use crate::events::{CompletedOperation, RequestId, SessionEvent};
use crate::ids::{ResultId, SessionId};
use crate::session::{DatabaseSession, ExecuteOutcome};
use crate::store::policy::ResultPolicy;
use crate::store::result_store::{Fetched, ResultStore, StoreAction};

use reldex_db_driver_api::DbResult;

/// What [`SessionResults::observe`] did with an event.
#[derive(Debug)]
#[non_exhaustive]
pub enum Observed {
    /// A segment reply, taken by the store it belongs to (or dropped, when
    /// that store is gone: its result was discarded).
    Segment {
        /// The result it was for.
        result: ResultId,
        /// What the store made of it; `None` when no store holds the result.
        fetched: Option<Fetched>,
    },
    /// Any other event, handed back for the consumer to act on — after the
    /// stores took note of it when it ended a transaction or the session.
    Event(SessionEvent),
}

/// Every open result store of one session. See the module documentation.
#[derive(Debug)]
pub struct SessionResults {
    session: SessionId,
    stores: Vec<ResultStore>,
    transaction_ends_pending: usize,
}

impl SessionResults {
    /// No stores yet, for `session`.
    #[must_use]
    pub const fn new(session: SessionId) -> Self {
        Self {
            session,
            stores: Vec::new(),
            transaction_ends_pending: 0,
        }
    }

    /// The session.
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Opens a store for the result `outcome` produced, if it produced one on
    /// this session.
    ///
    /// Call it **after** [`SessionResults::observe`] has seen the `Executed`
    /// event carrying `outcome`, so a statement that ended the transaction
    /// ends the stores that existed before it and never the one it opened.
    pub fn open(
        &mut self,
        outcome: &ExecuteOutcome,
        policy: ResultPolicy,
    ) -> Option<&mut ResultStore> {
        let result = outcome.result?;
        if result.owner() != self.session {
            return None;
        }
        let mut store = ResultStore::new(result, &outcome.columns, policy);
        // A commit command already submitted will release this result's
        // cursor too; do not fetch into it until it is answered.
        for _ in 0..self.transaction_ends_pending {
            store.transaction_end_submitted();
        }
        self.stores.retain(|existing| existing.result() != result);
        self.stores.push(store);
        self.stores.last_mut()
    }

    /// One store.
    #[must_use]
    pub fn get(&self, result: ResultId) -> Option<&ResultStore> {
        self.stores.iter().find(|store| store.result() == result)
    }

    /// One store, mutably.
    pub fn get_mut(&mut self, result: ResultId) -> Option<&mut ResultStore> {
        self.stores
            .iter_mut()
            .find(|store| store.result() == result)
    }

    /// Takes a store out — once its result has been discarded and
    /// `close_result` submitted for it. Replies still in flight for it are
    /// dropped by [`SessionResults::observe`].
    pub fn remove(&mut self, result: ResultId) -> Option<ResultStore> {
        let index = self
            .stores
            .iter()
            .position(|store| store.result() == result)?;
        Some(self.stores.remove(index))
    }

    /// Every store.
    pub fn iter(&self) -> impl Iterator<Item = &ResultStore> {
        self.stores.iter()
    }

    /// How many stores are open.
    #[must_use]
    pub fn len(&self) -> usize {
        self.stores.len()
    }

    /// Whether no store is open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stores.is_empty()
    }

    /// A commit, rollback or rollback-to-savepoint **command** was just
    /// submitted on this session: every store stops submitting until its
    /// reply is observed (ADR-0004 RS2, "How the store learns it").
    ///
    /// Call it once the session has **accepted** the command, and before
    /// pumping again. A refused submit produces no reply, so announcing it
    /// would pause the stores for good; and since pumping happens on the
    /// same thread, no fetch can be submitted between the two calls.
    pub fn transaction_end_submitted(&mut self) {
        self.transaction_ends_pending += 1;
        for store in &mut self.stores {
            store.transaction_end_submitted();
        }
    }

    /// Takes one drained event of this session.
    ///
    /// * A segment reply goes to its store.
    /// * `Executed` for a statement that ended the transaction — a typed
    ///   `TransactionControl` statement or an implicit commit — ends every
    ///   store (ADR-0002 X1).
    /// * `Completed` for a commit, rollback or rollback-to-savepoint command
    ///   ends every store on success, and lets them resume on failure.
    /// * `Terminal` ends every store with the session.
    ///
    /// Events of another session are handed back untouched.
    pub fn observe(&mut self, event: SessionEvent) -> Observed {
        if event.session() != self.session {
            return Observed::Event(event);
        }
        match event {
            SessionEvent::FetchedSegment { fetch, segment, .. } => {
                let result = fetch.result();
                let fetched = self
                    .get_mut(result)
                    .map(|store| store.on_fetched(fetch, segment));
                return Observed::Segment { result, fetched };
            }
            SessionEvent::Executed {
                outcome: Ok(ref outcome),
                ..
            } => {
                for store in &mut self.stores {
                    store.statement_executed(outcome);
                }
            }
            SessionEvent::Completed {
                operation:
                    CompletedOperation::Commit
                    | CompletedOperation::Rollback
                    | CompletedOperation::RollbackToSavepoint,
                ref result,
                ..
            } => {
                self.transaction_ends_pending = self.transaction_ends_pending.saturating_sub(1);
                for store in &mut self.stores {
                    store.transaction_end_answered(result.is_ok());
                }
            }
            SessionEvent::Terminal { .. } => {
                for store in &mut self.stores {
                    store.session_ended();
                }
            }
            _ => {}
        }
        Observed::Event(event)
    }

    /// [`ResultStore::pump`] for every store, in the order they were opened.
    ///
    /// # Errors
    ///
    /// The first error `submit` returned; the stores after it are not pumped.
    pub fn pump(&mut self, mut submit: impl FnMut(StoreAction) -> DbResult<()>) -> DbResult<usize> {
        let mut submitted = 0;
        for store in &mut self.stores {
            submitted += store.pump(&mut submit)?;
        }
        Ok(submitted)
    }

    /// [`SessionResults::pump`] onto `session`'s event path.
    ///
    /// # Errors
    ///
    /// As [`SessionResults::pump`].
    pub fn submit_events(
        &mut self,
        session: &DatabaseSession,
        mut next_request: impl FnMut() -> RequestId,
    ) -> DbResult<usize> {
        let mut submitted = 0;
        for store in &mut self.stores {
            submitted += store.submit_events(session, &mut next_request)?;
        }
        Ok(submitted)
    }
}
