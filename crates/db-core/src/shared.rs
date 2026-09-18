//! State a [`crate::DatabaseSession`] handle and its worker thread share.
//!
//! Nothing here touches a driver. The worker updates this after every command
//! it processes (`docs/decisions/0002-driver-api-and-concurrency-model.md`);
//! the handle only ever reads it, which is why every method takes `&self`
//! and a plain [`std::sync::Mutex`] is enough — this is not a hot path.

use std::sync::Mutex;

use reldex_db_driver_api::{DbError, ErrorKind, SessionState, StatementKind, TransactionState};

struct State {
    /// The session-loss ladder (`SPEC.md` §18): `Usable`, `NeedsValidation`
    /// (the worker must `ping` before the next command runs), or `Lost`
    /// (terminal; every further command fails fast).
    session_state: SessionState,
    /// Core-side conservative transaction tracking, derived from
    /// `StatementKind` (ADR-0002 D4/D6): true once a statement that may have
    /// opened a transaction ran, false again after a commit/rollback or an
    /// implicit commit (DDL).
    core_possibly_active: bool,
    /// The driver's own last-reported [`TransactionState`], which may be
    /// [`TransactionState::Unknown`].
    driver_transaction_state: TransactionState,
    /// A human-readable snapshot of the error that lost the session, for the
    /// fail-fast errors every later command gets. `DbError` is not `Clone`,
    /// so this stores its rendering rather than the value itself.
    lost_reason: Option<String>,
}

/// State shared between a [`crate::DatabaseSession`] and its worker thread.
pub(crate) struct SessionShared {
    state: Mutex<State>,
}

impl SessionShared {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(State {
                session_state: SessionState::Usable,
                core_possibly_active: false,
                // `TransactionState::default()` is `Unknown` for the same
                // reason: nobody has classified this connection yet, so the
                // safe answer is "may be open" (ADR-0002, amendment S2).
                driver_transaction_state: TransactionState::default(),
                lost_reason: None,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The session-loss state as of the last command the worker processed.
    pub(crate) fn session_state(&self) -> SessionState {
        self.lock().session_state
    }

    /// Whether the session is in the terminal `Lost` state.
    pub(crate) fn is_lost(&self) -> bool {
        self.lock().session_state == SessionState::Lost
    }

    /// Whether the worker believes it must `ping` before the next command.
    pub(crate) fn needs_validation(&self) -> bool {
        self.lock().session_state == SessionState::NeedsValidation
    }

    /// A successful `ping` clears `NeedsValidation` back to `Usable`. Never
    /// used to clear `Lost`: `SPEC.md` §18 forbids silently resurrecting a
    /// session once it is gone.
    pub(crate) fn mark_validated(&self) {
        let mut state = self.lock();
        if state.session_state == SessionState::NeedsValidation {
            state.session_state = SessionState::Usable;
        }
    }

    /// Marks the session terminally lost, capturing why for later fail-fast
    /// errors. Used when a revalidating `ping` itself fails.
    pub(crate) fn mark_lost_from(&self, error: &DbError) {
        let mut state = self.lock();
        state.session_state = SessionState::Lost;
        state.lost_reason = Some(error.to_string());
    }

    /// Applies the [`SessionState`] a [`DbError`] reported: worsens the
    /// tracked state, never improves it, and only [`SessionShared::mark_validated`]
    /// (a successful `ping`) moves it back toward `Usable`.
    pub(crate) fn note_error(&self, error: &DbError) {
        let mut state = self.lock();
        match error.session_state() {
            SessionState::Usable => {}
            SessionState::NeedsValidation => {
                if state.session_state != SessionState::Lost {
                    state.session_state = SessionState::NeedsValidation;
                }
            }
            SessionState::Lost => {
                state.session_state = SessionState::Lost;
                state.lost_reason = Some(error.to_string());
            }
        }
    }

    /// Core-side conservative update from a successfully executed
    /// statement's kind (ADR-0002 D4/D6): `Dml`/`PlSqlBlock`/`Other` may have
    /// opened a transaction; everything else leaves the flag as it was
    /// (over-prompting is acceptable, a missed one is not).
    pub(crate) fn note_statement_kind(&self, kind: StatementKind) {
        if matches!(
            kind,
            StatementKind::Dml | StatementKind::PlSqlBlock | StatementKind::Other
        ) {
            self.lock().core_possibly_active = true;
        }
    }

    /// A successful `commit`/`rollback`, or a statement that committed
    /// implicitly (DDL), resolves the transaction.
    pub(crate) fn note_commit_or_rollback(&self) {
        self.lock().core_possibly_active = false;
    }

    /// Records the driver's own last-reported transaction state.
    pub(crate) fn note_driver_transaction_state(&self, state: TransactionState) {
        self.lock().driver_transaction_state = state;
    }

    /// Whether a transaction may still be open, combining the driver's own
    /// report (which may be [`TransactionState::Unknown`]) with core-side
    /// tracking, per ADR-0002.
    pub(crate) fn has_possibly_active_transaction(&self) -> bool {
        let state = self.lock();
        state.core_possibly_active || state.driver_transaction_state.may_be_open()
    }

    /// The error every command gets once the session is lost or closed.
    ///
    /// Callers only reach this after failing to send a command to the
    /// worker's channel, which happens only once the worker thread has
    /// already exited — so "not lost" here always means "closed".
    pub(crate) fn terminal_error(&self) -> DbError {
        let state = self.lock();
        if state.session_state == SessionState::Lost {
            let reason = state.lost_reason.as_deref().unwrap_or("no further detail");
            DbError::new(
                ErrorKind::Connection,
                format!(
                    "reldex-db-core: session is lost ({reason}); open a new session to reconnect"
                ),
            )
            .with_session_state(SessionState::Lost)
        } else {
            DbError::new(ErrorKind::Connection, "reldex-db-core: session is closed")
                .with_session_state(SessionState::Lost)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_usable_with_no_possibly_active_transaction() {
        let shared = SessionShared::new();
        assert_eq!(shared.session_state(), SessionState::Usable);
        assert!(!shared.is_lost());
        // `TransactionState::default()` is `Unknown`, which `may_be_open()`.
        assert!(shared.has_possibly_active_transaction());
    }

    #[test]
    fn dml_marks_possibly_active_until_commit_or_rollback() {
        let shared = SessionShared::new();
        shared.note_driver_transaction_state(TransactionState::Inactive);
        assert!(!shared.has_possibly_active_transaction());
        shared.note_statement_kind(StatementKind::Dml);
        assert!(shared.has_possibly_active_transaction());
        shared.note_commit_or_rollback();
        assert!(!shared.has_possibly_active_transaction());
    }

    #[test]
    fn query_does_not_mark_possibly_active() {
        let shared = SessionShared::new();
        shared.note_driver_transaction_state(TransactionState::Inactive);
        shared.note_statement_kind(StatementKind::Query);
        assert!(!shared.has_possibly_active_transaction());
    }

    #[test]
    fn needs_validation_recovers_on_a_successful_ping_but_not_from_lost() {
        let shared = SessionShared::new();
        shared.note_error(&DbError::new(ErrorKind::Timeout, "slow"));
        assert!(shared.needs_validation());
        shared.mark_validated();
        assert_eq!(shared.session_state(), SessionState::Usable);

        shared.note_error(&DbError::new(ErrorKind::NetworkLost, "gone"));
        assert!(shared.is_lost());
        shared.mark_validated();
        assert!(shared.is_lost(), "a successful ping must never clear Lost");
    }

    #[test]
    fn terminal_error_reports_lost_reason_or_closed() {
        let shared = SessionShared::new();
        let closed = shared.terminal_error();
        assert!(closed.to_string().contains("closed"));

        shared.note_error(&DbError::new(ErrorKind::NetworkLost, "connection reset"));
        let lost = shared.terminal_error();
        assert_eq!(lost.session_state(), SessionState::Lost);
        assert!(lost.to_string().contains("lost"));
    }
}
