//! [`MockDriver`] and [`MockConnection`]: the driver-contract implementation
//! that runs a [`Scenario`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use reldex_db_driver_api::{
    CancelHandle, CancelKind, CancelOutcome, Capabilities, ConnectionId, DatabaseConnection,
    DatabaseDriver, DbError, DbResult, ErrorKind, ExecutionOutcome, OutValues, SavepointName,
    SessionState, Statement, StatementKind, TransactionState, Value,
};

use crate::cursor::MockCursor;
use crate::scenario::{
    Action, BlockSpec, ParkOutcome, QueryPlan, QuerySource, Scenario, ScriptValue,
};

/// The uncommitted table-store overlay for one connection.
///
/// Ops are recorded in order so a savepoint can remember "how many ops
/// existed when I was created" and `ROLLBACK TO SAVEPOINT` can truncate back
/// to it. Nothing here touches the [`Scenario`]'s shared, committed tables
/// until `commit` (see [`Scenario::apply_commit`]).
#[derive(Default)]
struct TxOverlay {
    ops: Vec<(String, Vec<ScriptValue>)>,
    savepoints: Vec<(String, usize)>,
}

impl TxOverlay {
    fn insert(&mut self, table: String, row: Vec<ScriptValue>) {
        self.ops.push((table, row));
    }

    fn visible_rows(&self, table: &str) -> Vec<Vec<ScriptValue>> {
        self.ops
            .iter()
            .filter(|(name, _)| name == table)
            .map(|(_, row)| row.clone())
            .collect()
    }

    fn clear(&mut self) {
        self.ops.clear();
        self.savepoints.clear();
    }

    fn savepoint(&mut self, name: &str) {
        self.savepoints.push((name.to_owned(), self.ops.len()));
    }

    fn rollback_to_savepoint(&mut self, name: &str) -> DbResult<()> {
        let position = self
            .savepoints
            .iter()
            .rposition(|(existing, _)| existing == name)
            .ok_or_else(|| {
                DbError::new(
                    ErrorKind::Transaction,
                    format!("reldex-driver-mock: savepoint `{name}` does not exist"),
                )
            })?;
        let (_, len) = self.savepoints[position];
        self.ops.truncate(len);
        self.savepoints.truncate(position + 1);
        Ok(())
    }
}

/// A `Send + Sync` cancellation handle for one [`MockConnection`].
struct MockCancelHandle {
    kind: CancelKind,
    active_gate: Arc<Mutex<Option<Arc<crate::scenario::BlockGate>>>>,
    armed_deadline: Arc<Mutex<Option<Instant>>>,
}

impl CancelHandle for MockCancelHandle {
    fn request_cancel(&self) -> DbResult<CancelOutcome> {
        match self.kind {
            CancelKind::Unsupported => Err(DbError::new(
                ErrorKind::Unsupported,
                "reldex-driver-mock: this connection does not support cancellation",
            )),
            CancelKind::Native => {
                let gate = self
                    .active_gate
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if let Some(gate) = gate {
                    gate.request_cancel();
                }
                // Idempotent no-op when nothing is running, per the contract.
                Ok(CancelOutcome::Requested)
            }
            CancelKind::PreArmedDeadline => {
                let deadline_remaining = self
                    .armed_deadline
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .map(|deadline| deadline.saturating_duration_since(Instant::now()));
                Ok(CancelOutcome::NotInterruptible { deadline_remaining })
            }
            // `CancelKind` is `#[non_exhaustive]`; a future variant this mock
            // does not yet know about is treated the same as `Unsupported`.
            _ => Err(DbError::new(
                ErrorKind::Unsupported,
                "reldex-driver-mock: unknown cancellation kind",
            )),
        }
    }

    fn kind(&self) -> CancelKind {
        self.kind
    }
}

/// The test-support driver entry point: connects only to a [`Scenario`].
pub struct MockDriver {
    scenario: Arc<Scenario>,
}

impl MockDriver {
    /// Builds a driver over `scenario`. Every connection it opens shares that
    /// scenario's committed table store and scripted responses.
    #[must_use]
    pub fn new(scenario: Arc<Scenario>) -> Self {
        Self { scenario }
    }
}

impl DatabaseDriver for MockDriver {
    fn name(&self) -> &str {
        "reldex-driver-mock"
    }

    fn capabilities(&self) -> Capabilities {
        self.scenario.capabilities()
    }

    fn connect(
        &self,
        _params: &reldex_db_driver_api::ConnectionParams,
    ) -> DbResult<Box<dyn DatabaseConnection>> {
        self.scenario.connect_behavior()?;
        let id = ConnectionId::allocate();
        self.scenario.record_thread(id);
        Ok(Box::new(MockConnection::new(
            id,
            Arc::clone(&self.scenario),
        )))
    }
}

/// The test-support connection: interprets one [`Scenario`]'s scripted
/// responses.
pub struct MockConnection {
    id: ConnectionId,
    scenario: Arc<Scenario>,
    capabilities: Capabilities,
    overlay: TxOverlay,
    transaction_state: TransactionState,
    closed: Arc<AtomicBool>,
    active_gate: Arc<Mutex<Option<Arc<crate::scenario::BlockGate>>>>,
    armed_deadline: Arc<Mutex<Option<Instant>>>,
    cancel_handle: Arc<MockCancelHandle>,
}

impl MockConnection {
    fn new(id: ConnectionId, scenario: Arc<Scenario>) -> Self {
        let capabilities = scenario.capabilities();
        let active_gate = Arc::new(Mutex::new(None));
        let armed_deadline = Arc::new(Mutex::new(None));
        let cancel_handle = Arc::new(MockCancelHandle {
            kind: capabilities.cancel(),
            active_gate: Arc::clone(&active_gate),
            armed_deadline: Arc::clone(&armed_deadline),
        });
        // A precise driver genuinely knows a fresh connection has no
        // transaction yet. An imprecise one (`exact_transaction_state ==
        // false`) must not assert that on its own: per ADR-0002 D4 it may
        // only report `Inactive` immediately after its own `commit`/
        // `rollback`, so it starts at the same conservative `Unknown` every
        // other untouched moment reports.
        let transaction_state = if capabilities.exact_transaction_state() {
            TransactionState::Inactive
        } else {
            TransactionState::Unknown
        };
        Self {
            id,
            scenario,
            capabilities,
            overlay: TxOverlay::default(),
            transaction_state,
            closed: Arc::new(AtomicBool::new(false)),
            active_gate,
            armed_deadline,
            cancel_handle,
        }
    }

    /// This connection's shared "is the underlying connection closed" flag,
    /// handed to every cursor and LOB stream it produces so they can report
    /// [`DbError::connection_closed`] instead of touching a connection that
    /// no longer exists (ADR-0002 D2, handle lifecycle).
    pub(crate) fn closed_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.closed)
    }

    fn record(&self) {
        self.scenario.record_thread(self.id);
    }

    fn resolve_query_source(&self, source: &QuerySource) -> QueryPlan {
        match source {
            QuerySource::Fixed(plan) => plan.clone(),
            QuerySource::Table { table, columns } => {
                let mut rows = self.scenario.committed_rows(table);
                rows.extend(self.overlay.visible_rows(table));
                QueryPlan::new(columns.clone(), rows)
            }
        }
    }

    /// Updates the driver-reported transaction state after a non-blocking
    /// statement of `kind` ran successfully. See the module documentation on
    /// [`Capabilities::exact_transaction_state`] for the two modes this
    /// mock offers.
    fn note_statement_kind(&mut self, kind: StatementKind) {
        if self.capabilities.exact_transaction_state() {
            if matches!(kind, StatementKind::Dml | StatementKind::PlSqlBlock) {
                self.transaction_state = TransactionState::Active;
            }
        } else if matches!(
            kind,
            StatementKind::Dml | StatementKind::PlSqlBlock | StatementKind::Other
        ) {
            self.transaction_state = TransactionState::Unknown;
        }
    }

    fn run_block(&mut self, spec: &BlockSpec, statement: &Statement) -> DbResult<ExecutionOutcome> {
        let deadline = statement.deadline().map(|d| Instant::now() + d);
        if self.capabilities.cancel() == CancelKind::PreArmedDeadline {
            *self
                .armed_deadline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = deadline;
        }
        *self
            .active_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&spec.gate));

        let outcome = spec.gate.park(deadline);

        *self
            .active_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .armed_deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;

        match outcome {
            ParkOutcome::Released => {
                self.note_statement_kind(spec.release_statement_kind);
                let mut result =
                    ExecutionOutcome::new().with_statement_kind(spec.release_statement_kind);
                if let Some(rows_affected) = spec.release_rows_affected {
                    result = result.with_rows_affected(rows_affected);
                }
                Ok(result)
            }
            ParkOutcome::Cancelled => {
                let mut error = DbError::cancelled();
                if let Some(session_state) = spec.cancelled_session_state {
                    error = error.with_session_state(session_state);
                }
                Err(error)
            }
            ParkOutcome::TimedOut => Err(DbError::new(
                ErrorKind::Timeout,
                "reldex-driver-mock: statement deadline elapsed",
            )
            .with_session_state(SessionState::NeedsValidation)),
        }
    }

    fn run_action(&mut self, action: &Action, statement: &Statement) -> DbResult<ExecutionOutcome> {
        match action {
            Action::Query(source) => {
                let plan = self.resolve_query_source(source);
                self.note_statement_kind(StatementKind::Query);
                let cursor = MockCursor::new(
                    self.id,
                    plan,
                    Arc::clone(&self.scenario),
                    self.closed_flag(),
                );
                Ok(ExecutionOutcome::new()
                    .with_cursor(Box::new(cursor))
                    .with_statement_kind(StatementKind::Query))
            }
            Action::Dml {
                rows_affected,
                insert,
            } => {
                if let Some((table, row)) = insert {
                    self.overlay.insert(table.clone(), row.clone());
                }
                self.note_statement_kind(StatementKind::Dml);
                Ok(ExecutionOutcome::new()
                    .with_rows_affected(*rows_affected)
                    .with_statement_kind(StatementKind::Dml))
            }
            Action::Ddl => {
                // A real server commits before and after DDL: flush the
                // pending overlay rather than discarding it (ADR-0002 D4/S9).
                self.scenario.apply_commit(&self.overlay.ops);
                self.overlay.clear();
                self.transaction_state = TransactionState::Inactive;
                Ok(ExecutionOutcome::new().with_statement_kind(StatementKind::Ddl))
            }
            Action::Execute {
                statement_kind,
                rows_affected,
            } => {
                self.note_statement_kind(*statement_kind);
                let mut outcome = ExecutionOutcome::new().with_statement_kind(*statement_kind);
                if let Some(rows_affected) = rows_affected {
                    outcome = outcome.with_rows_affected(*rows_affected);
                }
                Ok(outcome)
            }
            Action::RefCursorOut { name, source } => {
                let plan = self.resolve_query_source(source);
                self.note_statement_kind(StatementKind::PlSqlBlock);
                let cursor = MockCursor::new(
                    self.id,
                    plan,
                    Arc::clone(&self.scenario),
                    self.closed_flag(),
                );
                Ok(ExecutionOutcome::new()
                    .with_statement_kind(StatementKind::PlSqlBlock)
                    .with_out_values(OutValues::Named(vec![(
                        name.as_str().into(),
                        Value::Cursor(Box::new(cursor)),
                    )])))
            }
            Action::Fail(error) => Err(error.build()),
            Action::Block(spec) => self.run_block(spec, statement),
        }
    }
}

impl DatabaseConnection for MockConnection {
    fn id(&self) -> ConnectionId {
        self.id
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        Arc::clone(&self.cancel_handle) as Arc<dyn CancelHandle>
    }

    fn execute(&mut self, statement: &Statement) -> DbResult<ExecutionOutcome> {
        self.record();
        if self.closed.load(Ordering::SeqCst) {
            return Err(DbError::connection_closed("connection"));
        }
        let Some(action) = self.scenario.find_action(statement.sql()) else {
            return Err(DbError::new(
                ErrorKind::Other,
                format!(
                    "reldex-driver-mock: no scripted response for statement `{}`",
                    statement.sql()
                ),
            ));
        };
        self.run_action(&action, statement)
    }

    fn commit(&mut self) -> DbResult<()> {
        self.record();
        self.scenario.apply_commit(&self.overlay.ops);
        self.overlay.clear();
        self.transaction_state = TransactionState::Inactive;
        Ok(())
    }

    fn rollback(&mut self) -> DbResult<()> {
        self.record();
        self.overlay.clear();
        self.transaction_state = TransactionState::Inactive;
        Ok(())
    }

    fn savepoint(&mut self, name: &SavepointName) -> DbResult<()> {
        self.record();
        if !self.capabilities.savepoints() {
            return Err(DbError::unsupported("savepoints"));
        }
        self.overlay.savepoint(name.as_str());
        Ok(())
    }

    fn rollback_to_savepoint(&mut self, name: &SavepointName) -> DbResult<()> {
        self.record();
        if !self.capabilities.savepoints() {
            return Err(DbError::unsupported("savepoints"));
        }
        self.overlay.rollback_to_savepoint(name.as_str())
    }

    fn transaction_state(&self) -> TransactionState {
        self.transaction_state
    }

    fn ping(&mut self) -> DbResult<()> {
        self.record();
        self.scenario.ping_behavior()
    }

    fn close(self: Box<Self>) -> DbResult<()> {
        self.record();
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}
