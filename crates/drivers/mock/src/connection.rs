//! [`MockDriver`] and [`MockConnection`]: the driver-contract implementation
//! that runs a [`Scenario`].

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use reldex_db_driver_api::{
    CancelHandle, CancelKind, CancelOutcome, Capabilities, ConnectionId, DatabaseConnection,
    DatabaseDriver, DbError, DbResult, ErrorKind, ExecutionOutcome, LobLocator, OutValues,
    SavepointName, SessionState, Statement, StatementKind, TransactionState, Value, Warning,
};

use crate::cursor::{MockCursor, MockLobStream};
use crate::generated::GeneratedCursor;
use crate::scenario::{
    Action, BlockSpec, ParkOutcome, QueryPlan, QuerySource, Scenario, ScriptValue, TransactionEpoch,
};
use crate::server_output::{self, ConnectionOutput};

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
    /// A cancel requested while nothing was running, kept for the next
    /// statement. Only ever set when the scenario asks for it; see
    /// [`Scenario::set_late_cancel_lands_on_next_statement`].
    latched: Arc<AtomicBool>,
    scenario: Arc<Scenario>,
}

impl CancelHandle for MockCancelHandle {
    fn request_cancel(&self) -> DbResult<CancelOutcome> {
        // A scripted failure wins over the class: discovering the session is
        // gone is something a real cancel path can do whatever it advertises.
        self.scenario.cancel_behavior()?;
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
                match gate {
                    Some(gate) => gate.request_cancel(),
                    None if self.scenario.latches_late_cancel() => {
                        self.latched.store(true, Ordering::SeqCst);
                    }
                    // Idempotent no-op when nothing is running, per the
                    // contract.
                    None => {}
                }
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
        if let Err(error) = self.scenario.connect_behavior() {
            self.scenario.record_connect_failed();
            return Err(error);
        }
        let id = ConnectionId::allocate();
        let connection = Box::new(MockConnection::new(id, Arc::clone(&self.scenario)));
        self.scenario.record_thread(id);
        // Last, and after the connection exists: a test waiting on
        // `Counts::connects_finished` must never see the connect counted
        // before the connection it produced.
        self.scenario.record_connection_opened();
        Ok(connection)
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
    /// Bumped whenever a transaction ends, so handles opened inside it can be
    /// invalidated the way a real server's are.
    transaction_epoch: Arc<AtomicU64>,
    active_gate: Arc<Mutex<Option<Arc<crate::scenario::BlockGate>>>>,
    armed_deadline: Arc<Mutex<Option<Instant>>>,
    latched_cancel: Arc<AtomicBool>,
    cancel_handle: Arc<MockCancelHandle>,
    /// Findings this connection reports once, through
    /// [`DatabaseConnection::take_connect_warnings`]; see
    /// [`Scenario::set_connect_warnings`].
    connect_warnings: Vec<Warning>,
    /// This connection's server-side output buffer; see [`crate::server_output`].
    output: ConnectionOutput,
}

impl MockConnection {
    fn new(id: ConnectionId, scenario: Arc<Scenario>) -> Self {
        let capabilities = scenario.capabilities();
        let active_gate = Arc::new(Mutex::new(None));
        let armed_deadline = Arc::new(Mutex::new(None));
        let latched_cancel = Arc::new(AtomicBool::new(false));
        let cancel_handle = Arc::new(MockCancelHandle {
            kind: capabilities.cancel(),
            active_gate: Arc::clone(&active_gate),
            armed_deadline: Arc::clone(&armed_deadline),
            latched: Arc::clone(&latched_cancel),
            scenario: Arc::clone(&scenario),
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
        let connect_warnings = scenario.connect_warnings();
        Self {
            id,
            scenario,
            capabilities,
            overlay: TxOverlay::default(),
            transaction_state,
            closed: Arc::new(AtomicBool::new(false)),
            transaction_epoch: Arc::new(AtomicU64::new(0)),
            active_gate,
            armed_deadline,
            latched_cancel,
            cancel_handle,
            connect_warnings,
            output: ConnectionOutput::default(),
        }
    }

    /// This connection's shared "is the underlying connection closed" flag,
    /// handed to every cursor and LOB stream it produces so they can report
    /// [`DbError::connection_closed`] instead of touching a connection that
    /// no longer exists (ADR-0002 D2, handle lifecycle).
    pub(crate) fn closed_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.closed)
    }

    fn epoch(&self) -> TransactionEpoch {
        TransactionEpoch::new(
            &self.transaction_epoch,
            self.scenario.invalidates_handles_on_transaction_end(),
        )
    }

    fn end_transaction(&mut self) {
        self.transaction_epoch.fetch_add(1, Ordering::SeqCst);
        self.transaction_state = TransactionState::Inactive;
    }

    fn record(&self) {
        self.scenario.record_thread(self.id);
    }

    fn new_cursor(&self, plan: QueryPlan) -> MockCursor {
        self.scenario.record_cursor_opened();
        MockCursor::new(
            self.id,
            plan,
            Arc::clone(&self.scenario),
            self.closed_flag(),
            self.epoch(),
        )
    }

    fn new_generated_cursor(&self, spec: crate::GeneratedQuerySpec) -> GeneratedCursor {
        self.scenario.record_cursor_opened();
        GeneratedCursor::new(
            self.id,
            spec,
            Arc::clone(&self.scenario),
            self.closed_flag(),
            self.epoch(),
        )
    }

    fn new_lob(&self, kind: reldex_db_driver_api::LobKind, bytes: Vec<u8>) -> LobLocator {
        LobLocator::new(Box::new(MockLobStream::new(
            self.id,
            kind,
            bytes,
            Arc::clone(&self.scenario),
            self.closed_flag(),
            self.epoch(),
        )))
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
    /// statement ran successfully.
    ///
    /// `opens_transaction` is the scripted answer to the question no
    /// [`StatementKind`] can settle: a `SELECT … FOR UPDATE` is a `Query` and
    /// still opens a transaction.
    fn note_statement(&mut self, kind: StatementKind, opens_transaction: bool) {
        let opens =
            opens_transaction || matches!(kind, StatementKind::Dml | StatementKind::PlSqlBlock);
        if self.capabilities.exact_transaction_state() {
            if opens {
                self.transaction_state = TransactionState::Active;
            }
        } else if opens || matches!(kind, StatementKind::Other) {
            self.transaction_state = TransactionState::Unknown;
        }
    }

    fn run_block(&mut self, spec: &BlockSpec, statement: &Statement) -> DbResult<ExecutionOutcome> {
        // A cancel that arrived while nothing was running, latched by a driver
        // that has no way to tell which statement it was meant for.
        if self.latched_cancel.swap(false, Ordering::SeqCst) {
            return Err(Self::cancelled_error(spec));
        }

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

        let outcome = spec.gate.park(deadline, spec.observe_cancel);

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
                self.note_statement(spec.release_statement_kind, false);
                let mut result =
                    ExecutionOutcome::new().with_statement_kind(spec.release_statement_kind);
                if let Some(rows_affected) = spec.release_rows_affected {
                    result = result.with_rows_affected(rows_affected);
                }
                Ok(result)
            }
            ParkOutcome::Cancelled => Err(Self::cancelled_error(spec)),
            ParkOutcome::TimedOut => Err(spec.timeout_error.as_ref().map_or_else(
                || {
                    DbError::new(
                        ErrorKind::Timeout,
                        "reldex-driver-mock: statement deadline elapsed",
                    )
                    .with_session_state(SessionState::NeedsValidation)
                },
                |scripted| scripted.build(),
            )),
        }
    }

    fn cancelled_error(spec: &BlockSpec) -> DbError {
        let mut error = DbError::cancelled();
        if let Some(session_state) = spec.cancelled_session_state {
            error = error.with_session_state(session_state);
        }
        error
    }

    fn run_action(&mut self, action: &Action, statement: &Statement) -> DbResult<ExecutionOutcome> {
        match action {
            Action::Query {
                source,
                opens_transaction,
            } => {
                let plan = self.resolve_query_source(source);
                self.note_statement(StatementKind::Query, *opens_transaction);
                let cursor = self.new_cursor(plan);
                Ok(ExecutionOutcome::new()
                    .with_cursor(Box::new(cursor))
                    .with_statement_kind(StatementKind::Query))
            }
            Action::GeneratedQuery(spec) => {
                self.note_statement(StatementKind::Query, false);
                let cursor = self.new_generated_cursor(spec.clone());
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
                self.note_statement(StatementKind::Dml, false);
                Ok(ExecutionOutcome::new()
                    .with_rows_affected(*rows_affected)
                    .with_statement_kind(StatementKind::Dml))
            }
            Action::Ddl => {
                // A real server commits before and after DDL: flush the
                // pending overlay rather than discarding it (ADR-0002 D4/S9).
                self.scenario.apply_commit(&self.overlay.ops);
                self.overlay.clear();
                self.end_transaction();
                Ok(ExecutionOutcome::new().with_statement_kind(StatementKind::Ddl))
            }
            Action::Execute {
                statement_kind,
                rows_affected,
                opens_transaction,
            } => {
                self.note_statement(*statement_kind, *opens_transaction);
                let mut outcome = ExecutionOutcome::new().with_statement_kind(*statement_kind);
                if let Some(rows_affected) = rows_affected {
                    outcome = outcome.with_rows_affected(*rows_affected);
                }
                Ok(outcome)
            }
            Action::RefCursorOut { name, source } => {
                let plan = self.resolve_query_source(source);
                self.note_statement(StatementKind::PlSqlBlock, false);
                let cursor = self.new_cursor(plan);
                Ok(ExecutionOutcome::new()
                    .with_statement_kind(StatementKind::PlSqlBlock)
                    .with_out_values(OutValues::Named(vec![(
                        name.as_str().into(),
                        Value::Cursor(Box::new(cursor)),
                    )])))
            }
            Action::LobOut { name, kind, bytes } => {
                self.note_statement(StatementKind::PlSqlBlock, false);
                let locator = self.new_lob(*kind, bytes.clone());
                Ok(ExecutionOutcome::new()
                    .with_statement_kind(StatementKind::PlSqlBlock)
                    .with_out_values(OutValues::Named(vec![(
                        name.as_str().into(),
                        Value::Lob(locator),
                    )])))
            }
            Action::Fail(error) => Err(error.build()),
            Action::Block(spec) => self.run_block(spec, statement),
            Action::Panic(message) => panic!("{message}"),
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

    fn take_connect_warnings(&mut self) -> Vec<Warning> {
        // Taken, not cloned: the contract reports each finding once, and a mock
        // that handed the same warning out twice would let a bug in the core's
        // "collect exactly once" go unnoticed.
        std::mem::take(&mut self.connect_warnings)
    }

    fn execute(&mut self, statement: &Statement) -> DbResult<ExecutionOutcome> {
        self.record();
        if self.closed.load(Ordering::SeqCst) {
            return Err(DbError::connection_closed("connection"));
        }
        // Printed as the statement starts, so a statement scripted to fail
        // still leaves its lines behind.
        self.output
            .statement_started(&self.scenario, statement.sql());
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
        // A failed commit leaves the transaction exactly where it was: the
        // overlay is not flushed and not discarded.
        self.scenario.commit_behavior()?;
        self.scenario.apply_commit(&self.overlay.ops);
        self.overlay.clear();
        self.end_transaction();
        Ok(())
    }

    fn rollback(&mut self) -> DbResult<()> {
        self.record();
        self.scenario.rollback_behavior()?;
        self.overlay.clear();
        self.end_transaction();
        Ok(())
    }

    fn savepoint(&mut self, name: &SavepointName) -> DbResult<()> {
        self.record();
        if !self.capabilities.savepoints() {
            return Err(DbError::unsupported("savepoints"));
        }
        self.scenario.savepoint_behavior()?;
        self.overlay.savepoint(name.as_str());
        Ok(())
    }

    fn rollback_to_savepoint(&mut self, name: &SavepointName) -> DbResult<()> {
        self.record();
        if !self.capabilities.savepoints() {
            return Err(DbError::unsupported("savepoints"));
        }
        self.scenario.savepoint_behavior()?;
        self.overlay.rollback_to_savepoint(name.as_str())?;
        // The transaction stays open, but handles opened before the savepoint
        // can still be invalidated; that is what a real rollback does.
        self.transaction_epoch.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn transaction_state(&self) -> TransactionState {
        self.transaction_state
    }

    fn ping(&mut self) -> DbResult<()> {
        self.record();
        self.scenario.ping_behavior()
    }

    fn set_server_output(
        &mut self,
        setting: reldex_db_driver_api::ServerOutputSetting,
    ) -> DbResult<reldex_db_driver_api::ServerOutputSetting> {
        self.record();
        if !self.capabilities.server_output() {
            return Err(server_output::unsupported());
        }
        if self.closed.load(Ordering::SeqCst) {
            return Err(DbError::connection_closed("connection"));
        }
        self.output.set(&self.scenario, setting)
    }

    fn take_server_output(
        &mut self,
        max_lines: std::num::NonZeroUsize,
        max_bytes: std::num::NonZeroUsize,
    ) -> DbResult<reldex_db_driver_api::ServerOutputChunk> {
        self.record();
        if !self.capabilities.server_output() {
            return Err(server_output::unsupported());
        }
        if self.closed.load(Ordering::SeqCst) {
            return Err(DbError::connection_closed("connection"));
        }
        self.output.take(&self.scenario, max_lines, max_bytes)
    }

    fn close(self: Box<Self>) -> DbResult<()> {
        self.record();
        self.scenario.record_connection_closed();
        self.closed.store(true, Ordering::SeqCst);
        self.scenario.close_behavior()
    }
}
