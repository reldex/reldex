//! The driver and connection implementations.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use reldex_db_driver_api::{
    Bind, Binds, CancelHandle, CancelKind, CancelOutcome, Capabilities, ConnectionId, Credentials,
    DatabaseConnection, DatabaseDriver, DbError, DbResult, Endpoint, ErrorKind, ExecutionOutcome,
    ExtensionValue, LobKind, LobLocator, NamedBind, OutBindSpec, OutValues, SavepointName,
    SessionRole, SqlType, Statement, StatementKind, TlsMode, TransactionState, Value, Warning,
    WarningKind,
};

use oracledb::{
    AUTH_MODE_SYSDBA, AUTH_MODE_SYSOPER, Config, Connection, ExecResult, Lob, OracleNumber,
    OracleTimestamp, Row, ToDbValue,
};

use crate::binds::{OwnedBind, out_bind_type, to_owned_bind};
use crate::classify::{Classification, classify};
use crate::cursor::OracleCursor;
use crate::lob::OracleLobStream;
use crate::value::to_timestamp;

/// A flag shared with every handle derived from a connection, so a cursor or
/// LOB stream that outlives its connection **reports** instead of touching a
/// socket that is gone (ADR-0002 D2, handle lifecycle).
#[derive(Clone, Default)]
pub(crate) struct Closed(Arc<AtomicBool>);

impl Closed {
    pub(crate) fn is_closed(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn mark_closed(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// The capabilities this driver actually delivers.
///
/// Every value here is a claim Phase 0 can defend; see the crate documentation
/// and `docs/exec-plans/active/phase-0-spike-results.md` for the evidence.
fn capabilities() -> Capabilities {
    Capabilities::none()
        // Verified from source and measured in spike S4: `set_call_timeout`
        // takes the same lock `execute` holds for the whole round trip, so a
        // running statement cannot be interrupted on request. A deadline armed
        // before the call does work.
        .with_cancel(CancelKind::PreArmedDeadline)
        .with_savepoints(true)
        .with_named_binds(true)
        .with_out_binds(true)
        .with_ref_cursor(true)
        .with_lob_streaming(true)
        // Deliberately NOT advertised: `oracledb` implements TCPS, but the
        // Phase 0 test database has no TLS listener, so spike S8 has not run
        // and there is no Reldex evidence. `connect` refuses
        // `TlsMode::Required` for the same reason.
        .with_tls(false)
        // `Client::transaction_in_progress` exists upstream but is not exposed,
        // so this driver tracks the transaction conservatively.
        .with_exact_transaction_state(false)
        // True, with a documented limit: this upstream version discards the
        // server's SQL error offset, so a position is only available for PL/SQL
        // compilation errors, which is the case `SPEC.md` §24.14 needs.
        .with_error_position(true)
}

/// The thin Oracle Database driver.
///
/// One instance may be shared and used to open any number of connections.
#[derive(Debug, Default, Clone, Copy)]
pub struct OracleThinDriver;

impl OracleThinDriver {
    /// Creates the driver.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl DatabaseDriver for OracleThinDriver {
    fn name(&self) -> &str {
        "oracle-thin"
    }

    fn capabilities(&self) -> Capabilities {
        capabilities()
    }

    fn connect(
        &self,
        params: &reldex_db_driver_api::ConnectionParams,
    ) -> DbResult<Box<dyn DatabaseConnection>> {
        let config = build_config(params)?;
        let connection =
            oracledb::connect(config).map_err(|error| crate::error::map_connect(&error))?;
        Ok(Box::new(OracleConnection::new(connection)))
    }
}

/// Builds the upstream configuration from vendor-neutral parameters.
fn build_config(params: &reldex_db_driver_api::ConnectionParams) -> DbResult<Config> {
    if params.tls() == TlsMode::Required {
        return Err(DbError::new(
            ErrorKind::Unsupported,
            "encrypted transport is not validated by this driver yet \
             (ADR-0001 spike S8); refusing rather than claiming TLS works",
        ));
    }

    let connect_string = match params.endpoint() {
        Endpoint::HostPort {
            host,
            port,
            service,
        } => format!("{host}:{port}/{service}"),
        Endpoint::ConnectString(value) => value.clone(),
        _ => {
            return Err(DbError::new(
                ErrorKind::Configuration,
                "this driver does not understand this endpoint shape",
            ));
        }
    };

    let mut config = Config::default()
        .set_connect_string(&connect_string)
        .map_err(|error| crate::error::map(&error))?;

    match params.credentials() {
        Credentials::UserPassword { username, password } => {
            config = config.set_credentials(username, password.expose());
        }
        _ => {
            return Err(DbError::new(
                ErrorKind::Unsupported,
                "this driver supports user-name and password authentication only; \
                 external, wallet and token authentication are upstream gaps",
            ));
        }
    }

    config = match params.role() {
        SessionRole::Normal => config,
        SessionRole::SysDba => config.set_auth_mode(AUTH_MODE_SYSDBA),
        SessionRole::SysOper => config.set_auth_mode(AUTH_MODE_SYSOPER),
    };

    if let Some(ExtensionValue::Integer(size)) = params.extensions().get(EXT_STATEMENT_CACHE_SIZE) {
        config = config.set_stmtcachesize(usize::try_from(*size).unwrap_or(0));
    }

    Ok(config)
}

/// Extension key: the size of the driver's statement cache.
pub const EXT_STATEMENT_CACHE_SIZE: &str = "oracle.statement_cache_size";

/// The cancel handle for a connection.
///
/// This driver is [`CancelKind::PreArmedDeadline`], so `request_cancel` sends
/// nothing and returns immediately. It reports how long is left on the deadline
/// the caller armed through [`Statement::with_deadline`] so the UI can say what
/// will actually happen rather than show a cancel that is not happening.
struct OracleCancelHandle {
    baseline: Instant,
    /// Milliseconds after `baseline` at which the armed deadline expires, or
    /// zero when no deadline is armed. Atomic rather than locked because
    /// `request_cancel` must never block (ADR-0002 D2 rule 1) — including on a
    /// mutex the worker thread happens to hold.
    deadline_at_millis: AtomicU64,
}

impl OracleCancelHandle {
    fn new() -> Self {
        Self {
            baseline: Instant::now(),
            deadline_at_millis: AtomicU64::new(0),
        }
    }

    fn arm(&self, deadline: Option<Duration>) {
        let value = deadline.map_or(0, |deadline| {
            let at = self.baseline.elapsed() + deadline;
            u64::try_from(at.as_millis()).unwrap_or(u64::MAX).max(1)
        });
        self.deadline_at_millis.store(value, Ordering::Release);
    }

    fn remaining(&self) -> Option<Duration> {
        let at = self.deadline_at_millis.load(Ordering::Acquire);
        if at == 0 {
            return None;
        }
        let elapsed = u64::try_from(self.baseline.elapsed().as_millis()).unwrap_or(u64::MAX);
        Some(Duration::from_millis(at.saturating_sub(elapsed)))
    }
}

impl CancelHandle for OracleCancelHandle {
    fn request_cancel(&self) -> DbResult<CancelOutcome> {
        Ok(CancelOutcome::NotInterruptible {
            deadline_remaining: self.remaining(),
        })
    }

    fn kind(&self) -> CancelKind {
        CancelKind::PreArmedDeadline
    }
}

/// One Oracle Database session.
pub(crate) struct OracleConnection {
    id: ConnectionId,
    inner: Connection,
    cancel: Arc<OracleCancelHandle>,
    closed: Closed,
    transaction: TransactionState,
}

impl OracleConnection {
    fn new(inner: Connection) -> Self {
        Self {
            id: ConnectionId::allocate(),
            inner,
            cancel: Arc::new(OracleCancelHandle::new()),
            closed: Closed::default(),
            // A freshly opened session has no transaction. This is the only
            // place besides a successful commit or rollback where the driver
            // claims to know.
            transaction: TransactionState::Inactive,
        }
    }

    fn guard(&self) -> DbResult<()> {
        if self.closed.is_closed() {
            return Err(DbError::connection_closed("connection"));
        }
        Ok(())
    }

    /// Arms (or clears) the per-call deadline before the call starts.
    ///
    /// `oracledb` implements this as the socket read timeout, so it bounds the
    /// time the client waits for *each* server response rather than the total
    /// wall-clock time of a statement. When it fires, the driver sends an
    /// interrupt marker and resets the protocol, so the session survives and the
    /// statement is stopped server-side — which is exactly what makes
    /// [`CancelKind::PreArmedDeadline`] meaningful rather than cosmetic.
    ///
    /// The value stays in force until the next `execute`, so batches fetched
    /// from the resulting cursor are bounded the same way. It cannot be changed
    /// once a call is running: that is the whole of ADR-0001 C1.
    fn apply_deadline(&mut self, deadline: Option<Duration>) -> DbResult<()> {
        self.cancel.arm(deadline);
        self.inner
            .set_call_timeout(deadline)
            .map_err(|error| crate::error::map(&error))
    }

    /// Collects the server's warning for the statement just executed.
    fn warnings(&self) -> Vec<Warning> {
        match self.inner.last_warning() {
            Ok(Some(message)) => {
                let kind = if message.to_ascii_lowercase().contains("compilation error") {
                    WarningKind::CompiledWithErrors
                } else {
                    // `oracledb` exposes warnings as a bare `String` with no
                    // code and no structure, so anything it does not phrase as a
                    // compilation failure is reported rather than guessed at.
                    WarningKind::Informational
                };
                vec![Warning::new(kind, message)]
            }
            _ => Vec::new(),
        }
    }

    /// Updates the conservative transaction tracking after a statement ran.
    fn record_statement(&mut self, kind: StatementKind) {
        self.transaction = match kind {
            // DDL commits on the server before and after, whatever the client
            // asked for. Nothing is open afterwards, and the contract reports
            // the commit through `ExecutionOutcome::committed_implicitly`.
            StatementKind::Ddl => TransactionState::Inactive,
            // `ALTER SESSION` and friends change session state, not the
            // transaction.
            StatementKind::SessionControl => self.transaction,
            // Everything else may have opened or resolved a transaction. A
            // plain `SELECT` is included deliberately: `SELECT … FOR UPDATE`
            // opens one, and over-prompting is acceptable where a silent commit
            // is not (`SPEC.md` §10).
            _ => TransactionState::Unknown,
        };
    }

    fn execute_query(
        &mut self,
        statement: &Statement,
        owned: &[OwnedBind],
        names: Option<&[&str]>,
    ) -> DbResult<ExecutionOutcome> {
        let refs: Vec<&dyn ToDbValue> = owned.iter().map(OwnedBind::as_dyn).collect();
        let mut prepared = self
            .inner
            .statement(statement.sql())
            .map_err(|error| crate::error::map(&error))?;
        // Without this, `oracledb` materializes CLOB and BLOB values into the
        // row, which breaks `SPEC.md` §12's bounded-memory rule silently.
        prepared.fetch_lobs();
        if let Some(rows) = statement.fetch_rows() {
            let rows = u32::try_from(rows.get()).unwrap_or(u32::MAX);
            prepared.fetch_array_size(rows);
            prepared.prefetch_rows(rows);
        }
        let cursor = match names {
            Some(names) => {
                let pairs: Vec<(&str, &dyn ToDbValue)> = names.iter().copied().zip(refs).collect();
                prepared.query_named(&pairs)
            }
            None => prepared.query(&refs),
        }
        .map_err(|error| crate::error::map(&error))?;

        let cursor = OracleCursor::new(cursor, self.id, self.closed.clone())?;
        Ok(ExecutionOutcome::new()
            .with_statement_kind(StatementKind::Query)
            .with_cursor(Box::new(cursor))
            .with_warnings(self.warnings()))
    }

    fn execute_non_query(
        &mut self,
        statement: &Statement,
        kind: StatementKind,
        owned: &[OwnedBind],
        names: Option<&[&str]>,
    ) -> DbResult<ExecutionOutcome> {
        let refs: Vec<&dyn ToDbValue> = owned.iter().map(OwnedBind::as_dyn).collect();
        let mut prepared = self
            .inner
            .statement(statement.sql())
            .map_err(|error| crate::error::map(&error))?;
        prepared.fetch_lobs();
        let mut result = match names {
            Some(names) => {
                let pairs: Vec<(&str, &dyn ToDbValue)> = names.iter().copied().zip(refs).collect();
                prepared.execute_named(&pairs)
            }
            None => prepared.execute(&refs),
        }
        .map_err(|error| crate::error::map(&error))?;

        let rows_affected = result.rows_affected();
        let out_values = self.collect_out_values(statement, &mut result)?;
        Ok(ExecutionOutcome::new()
            .with_statement_kind(kind)
            .with_rows_affected(rows_affected)
            .with_out_values(out_values)
            .with_warnings(self.warnings()))
    }

    /// Reads the values the server wrote back through OUT and IN OUT binds.
    ///
    /// `oracledb` returns them in the order the placeholders appear in the SQL
    /// text, not in the order the caller declared them, so named binds are
    /// re-aligned through the statement's own bind-name list.
    fn collect_out_values(
        &mut self,
        statement: &Statement,
        result: &mut ExecResult,
    ) -> DbResult<OutValues> {
        if !statement.binds().has_outputs() {
            return Ok(OutValues::None);
        }
        let mut row = result.out_bind_data();

        match statement.binds() {
            Binds::Positional(binds) => {
                let mut values = Vec::with_capacity(binds.len());
                let mut slot = 0_usize;
                for bind in binds {
                    if let Some(spec) = bind.out_spec() {
                        values.push(Some(self.read_out_value(&mut row, slot, spec)?));
                        slot += 1;
                    } else {
                        values.push(None);
                    }
                }
                Ok(OutValues::Positional(values))
            }
            Binds::Named(binds) => {
                let order = self.bind_name_order(statement)?;
                let mut values = Vec::new();
                let mut slot = 0_usize;
                for name in &order {
                    let Some(declared) = find_named(binds, name) else {
                        continue;
                    };
                    if let Some(spec) = declared.bind().out_spec() {
                        let value = self.read_out_value(&mut row, slot, spec)?;
                        values.push((declared.name().into(), value));
                        slot += 1;
                    }
                }
                Ok(OutValues::Named(values))
            }
            _ => Ok(OutValues::None),
        }
    }

    /// The bind placeholder names in the order they appear in the statement.
    fn bind_name_order(&self, statement: &Statement) -> DbResult<Vec<String>> {
        let prepared = self
            .inner
            .statement(statement.sql())
            .map_err(|error| crate::error::map(&error))?;
        prepared
            .bind_names()
            .map_err(|error| crate::error::map(&error))
    }

    fn read_out_value(&self, row: &mut Row, slot: usize, spec: OutBindSpec) -> DbResult<Value> {
        // Validates the type once more so an unsupported OUT type fails the same
        // way whether it is caught on the way in or on the way out.
        out_bind_type(spec)?;
        let value = match spec.sql_type() {
            SqlType::Number => take_out::<OracleNumber>(row, slot)?
                .map(|value| {
                    reldex_db_driver_api::Number::parse(&value.to_string())
                        .map(Value::Number)
                        .map_err(DbError::from)
                })
                .transpose()?,
            SqlType::BinaryFloat => take_out::<f32>(row, slot)?.map(Value::Float),
            SqlType::BinaryDouble => take_out::<f64>(row, slot)?.map(Value::Double),
            SqlType::Boolean => take_out::<bool>(row, slot)?.map(Value::Boolean),
            SqlType::Text { .. } => take_out::<String>(row, slot)?.map(Value::Text),
            SqlType::Json => take_out::<String>(row, slot)?.map(Value::Json),
            SqlType::Raw => take_out::<Vec<u8>>(row, slot)?.map(Value::Bytes),
            SqlType::Date | SqlType::Timestamp => take_out::<OracleTimestamp>(row, slot)?
                .map(|value| to_timestamp(&value, false).map(Value::Timestamp))
                .transpose()?,
            SqlType::TimestampWithTimeZone => take_out::<OracleTimestamp>(row, slot)?
                .map(|value| to_timestamp(&value, true).map(Value::Timestamp))
                .transpose()?,
            SqlType::CharacterLob { national } => {
                let kind = if national {
                    LobKind::NationalCharacter
                } else {
                    LobKind::Character
                };
                take_out::<Lob>(row, slot)?.map(|lob| {
                    Value::Lob(LobLocator::new(Box::new(OracleLobStream::new(
                        lob,
                        kind,
                        self.id,
                        self.closed.clone(),
                    ))))
                })
            }
            SqlType::BinaryLob => take_out::<Lob>(row, slot)?.map(|lob| {
                Value::Lob(LobLocator::new(Box::new(OracleLobStream::new(
                    lob,
                    LobKind::Binary,
                    self.id,
                    self.closed.clone(),
                ))))
            }),
            SqlType::Cursor => take_out::<oracledb::Cursor>(row, slot)?
                .map(|cursor| {
                    OracleCursor::new(cursor, self.id, self.closed.clone())
                        .map(|cursor| Value::Cursor(Box::new(cursor)))
                })
                .transpose()?,
            other => {
                return Err(DbError::new(
                    ErrorKind::Unsupported,
                    format!("an output bind of type {other} is not supported"),
                ));
            }
        };
        Ok(value.unwrap_or(Value::Null))
    }

    /// Runs a statement this driver builds itself (savepoints).
    fn execute_internal(&mut self, sql: &str) -> DbResult<()> {
        self.inner
            .execute(sql, &[])
            .map(|_| ())
            .map_err(|error| crate::error::map(&error))
    }
}

fn take_out<'a, T>(row: &'a mut Row, slot: usize) -> DbResult<Option<T>>
where
    Option<T>: oracledb::FromDbValue<'a>,
{
    row.take::<Option<T>>(slot)
        .map_err(|error| crate::error::map(&error))
}

fn find_named<'a>(binds: &'a [NamedBind], name: &str) -> Option<&'a NamedBind> {
    binds
        .iter()
        .find(|candidate| candidate.name().eq_ignore_ascii_case(name))
}

impl DatabaseConnection for OracleConnection {
    fn id(&self) -> ConnectionId {
        self.id
    }

    fn capabilities(&self) -> Capabilities {
        capabilities()
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        self.cancel.clone()
    }

    fn execute(&mut self, statement: &Statement) -> DbResult<ExecutionOutcome> {
        self.guard()?;
        let Classification {
            kind, returns_rows, ..
        } = classify(statement.sql());

        let (owned, names) = prepare_binds(statement)?;
        let name_refs: Option<Vec<&str>> = names
            .as_ref()
            .map(|names| names.iter().map(String::as_str).collect());

        self.apply_deadline(statement.deadline())?;

        let outcome = if returns_rows {
            self.execute_query(statement, &owned, name_refs.as_deref())
        } else {
            self.execute_non_query(statement, kind, &owned, name_refs.as_deref())
        };

        if outcome.is_ok() {
            self.record_statement(kind);
        } else {
            // A failed statement may still have opened a transaction before it
            // failed, so the safe answer is "do not know".
            self.transaction = TransactionState::Unknown;
        }
        outcome
    }

    fn commit(&mut self) -> DbResult<()> {
        self.guard()?;
        self.inner
            .commit()
            .map_err(|error| crate::error::map(&error))?;
        self.transaction = TransactionState::Inactive;
        Ok(())
    }

    fn rollback(&mut self) -> DbResult<()> {
        self.guard()?;
        self.inner
            .rollback()
            .map_err(|error| crate::error::map(&error))?;
        self.transaction = TransactionState::Inactive;
        Ok(())
    }

    fn savepoint(&mut self, name: &SavepointName) -> DbResult<()> {
        self.guard()?;
        // The name is a validated plain identifier, so this interpolation
        // cannot inject anything (ADR-0002 D4).
        self.execute_internal(&format!("SAVEPOINT {name}"))?;
        self.transaction = TransactionState::Unknown;
        Ok(())
    }

    fn rollback_to_savepoint(&mut self, name: &SavepointName) -> DbResult<()> {
        self.guard()?;
        self.execute_internal(&format!("ROLLBACK TO SAVEPOINT {name}"))?;
        // Rolling back to a savepoint leaves the transaction open.
        self.transaction = TransactionState::Unknown;
        Ok(())
    }

    fn transaction_state(&self) -> TransactionState {
        self.transaction
    }

    fn ping(&mut self) -> DbResult<()> {
        self.guard()?;
        self.inner.ping().map_err(|error| crate::error::map(&error))
    }

    fn close(mut self: Box<Self>) -> DbResult<()> {
        if self.closed.is_closed() {
            return Ok(());
        }
        self.closed.mark_closed();
        // `oracledb` rolls an open transaction back as part of closing and
        // never commits, which is what `SPEC.md` §10 requires.
        self.inner
            .close()
            .map_err(|error| crate::error::map(&error))
    }
}

/// Converts the contract's binds into the upstream shape.
type PreparedBinds = (Vec<OwnedBind>, Option<Vec<String>>);

fn prepare_binds(statement: &Statement) -> DbResult<PreparedBinds> {
    match statement.binds() {
        Binds::None => Ok((Vec::new(), None)),
        Binds::Positional(binds) => {
            let owned = binds.iter().map(to_owned_bind).collect::<DbResult<_>>()?;
            Ok((owned, None))
        }
        Binds::Named(binds) => {
            let mut owned = Vec::with_capacity(binds.len());
            let mut names = Vec::with_capacity(binds.len());
            for bind in binds {
                owned.push(to_owned_bind(bind.bind())?);
                names.push(bind.name().to_owned());
            }
            Ok((owned, Some(names)))
        }
        _ => Err(DbError::new(
            ErrorKind::Unsupported,
            "this driver does not understand this bind scheme",
        )),
    }
}

/// Keeps the unused-import checker honest about the bind helper types.
const _: fn(&Bind) -> DbResult<OwnedBind> = to_owned_bind;

#[cfg(test)]
mod tests {
    use super::*;
    use reldex_db_driver_api::{ConnectionParams, Endpoint, Secret};

    fn params(endpoint: Endpoint) -> ConnectionParams {
        ConnectionParams::new(
            endpoint,
            Credentials::UserPassword {
                username: "reldex_test".to_owned(),
                password: Secret::new("not-a-real-password"),
            },
        )
    }

    #[test]
    fn the_driver_reports_only_what_it_can_deliver() {
        let capabilities = OracleThinDriver::new().capabilities();
        assert_eq!(capabilities.cancel(), CancelKind::PreArmedDeadline);
        assert!(
            !capabilities.can_interrupt_running_call(),
            "a pre-armed deadline cannot stop a running statement, and \
             `SPEC.md` §24.8 must not be claimed"
        );
        assert!(capabilities.savepoints());
        assert!(capabilities.named_binds());
        assert!(capabilities.out_binds());
        assert!(capabilities.ref_cursor());
        assert!(capabilities.lob_streaming());
        assert!(
            !capabilities.tls(),
            "TCPS is unproven in Phase 0 (spike S8 not run)"
        );
        assert!(
            !capabilities.exact_transaction_state(),
            "the upstream transaction flag is not exposed"
        );
        assert_eq!(OracleThinDriver::new().name(), "oracle-thin");
    }

    #[test]
    fn a_host_port_endpoint_becomes_an_easy_connect_string() {
        // The Phase 0 database only answers to a service name; the SID
        // shorthand does not work on it (tools/oracle-test-db/README.md).
        let config = build_config(&params(Endpoint::HostPort {
            host: "127.0.0.1".to_owned(),
            port: 1521,
            service: "RELDEX".to_owned(),
        }))
        .expect("valid parameters");
        let descriptor = config.get_connect_descriptor();
        assert!(descriptor.contains("127.0.0.1"), "{descriptor}");
        assert!(descriptor.contains("1521"), "{descriptor}");
        assert!(descriptor.contains("RELDEX"), "{descriptor}");
    }

    #[test]
    fn a_full_descriptor_is_passed_through() {
        let descriptor = "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=127.0.0.1)(PORT=1521))\
                          (CONNECT_DATA=(SERVICE_NAME=RELDEX)))";
        let config = build_config(&params(Endpoint::ConnectString(descriptor.to_owned())))
            .expect("valid descriptor");
        assert!(config.get_connect_descriptor().contains("RELDEX"));
    }

    /// `Config` is not `Debug`, so `expect_err` cannot be used on it.
    fn refusal(parameters: &ConnectionParams) -> DbError {
        match build_config(parameters) {
            Ok(_) => panic!("these parameters should have been refused"),
            Err(error) => error,
        }
    }

    #[test]
    fn requiring_tls_is_refused_rather_than_silently_downgraded() {
        let error = refusal(
            &params(Endpoint::HostPort {
                host: "127.0.0.1".to_owned(),
                port: 1521,
                service: "RELDEX".to_owned(),
            })
            .with_tls(TlsMode::Required),
        );
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(error.message().contains("S8"), "{error}");
    }

    #[test]
    fn external_authentication_is_reported_as_unsupported() {
        let error = refusal(&ConnectionParams::new(
            Endpoint::ConnectString("127.0.0.1:1521/RELDEX".to_owned()),
            Credentials::External,
        ));
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn the_password_never_reaches_a_message_or_a_debug_rendering() {
        let parameters = params(Endpoint::HostPort {
            host: "127.0.0.1".to_owned(),
            port: 1521,
            service: "RELDEX".to_owned(),
        });
        assert!(!format!("{parameters:?}").contains("not-a-real-password"));

        // Whatever the driver renders from a connect string — the parsed
        // descriptor or the parse failure — the credential is not in it.
        let rendered = match build_config(&params(Endpoint::ConnectString("((((".to_owned()))) {
            Ok(config) => config.get_connect_descriptor(),
            Err(error) => error.to_string(),
        };
        assert!(!rendered.contains("not-a-real-password"), "{rendered}");
    }

    #[test]
    fn an_unarmed_cancel_handle_says_the_statement_cannot_be_stopped() {
        let handle = OracleCancelHandle::new();
        let outcome = handle.request_cancel().expect("never fails");
        assert!(
            !outcome.is_requested(),
            "reporting `Requested` here would be a lie the UI would render as a \
             cancel in progress"
        );
        assert_eq!(
            outcome,
            CancelOutcome::NotInterruptible {
                deadline_remaining: None
            }
        );
        assert_eq!(handle.kind(), CancelKind::PreArmedDeadline);
    }

    #[test]
    fn an_armed_deadline_is_reported_so_the_ui_can_say_when_it_will_stop() {
        let handle = OracleCancelHandle::new();
        handle.arm(Some(Duration::from_secs(30)));
        let CancelOutcome::NotInterruptible { deadline_remaining } =
            handle.request_cancel().expect("never fails")
        else {
            panic!("a pre-armed-deadline driver never reports Requested");
        };
        let remaining = deadline_remaining.expect("a deadline is armed");
        assert!(
            remaining <= Duration::from_secs(30) && remaining > Duration::from_secs(25),
            "{remaining:?}"
        );

        handle.arm(None);
        assert_eq!(handle.remaining(), None);
    }

    #[test]
    fn repeated_cancels_are_a_successful_no_op() {
        let handle = OracleCancelHandle::new();
        for _ in 0..3 {
            assert!(handle.request_cancel().is_ok());
        }
    }

    #[test]
    fn bind_preparation_keeps_declaration_order_and_names() {
        let statement = Statement::new("BEGIN p(:a, :b); END;").with_named_binds(vec![
            NamedBind::new("a", Bind::input(1_i64)),
            NamedBind::new("b", Bind::output(SqlType::Number)),
        ]);
        let (owned, names) = prepare_binds(&statement).expect("valid binds");
        assert_eq!(owned.len(), 2);
        assert_eq!(
            names.as_deref(),
            Some(["a".to_owned(), "b".to_owned()].as_slice())
        );

        let positional = Statement::new("BEGIN p(:1, :2); END;")
            .with_positional_binds(vec![Bind::input("x"), Bind::output(SqlType::VARCHAR)]);
        let (owned, names) = prepare_binds(&positional).expect("valid binds");
        assert_eq!(owned.len(), 2);
        assert!(names.is_none());
    }

    #[test]
    fn named_binds_are_found_case_insensitively() {
        let binds = vec![NamedBind::new("Employee_Id", Bind::input(1_i64))];
        assert!(find_named(&binds, "EMPLOYEE_ID").is_some());
        assert!(find_named(&binds, "employee_id").is_some());
        assert!(find_named(&binds, "other").is_none());
    }
}
