//! The driver and connection implementations.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use reldex_db_driver_api::{
    Bind, Binds, CancelHandle, CancelKind, CancelOutcome, Capabilities, ConnectionId, Credentials,
    DatabaseConnection, DatabaseDriver, DbError, DbResult, Endpoint, ErrorKind, ExecutionOutcome,
    ExtensionValue, LobKind, LobLocator, NamedBind, OutBindSpec, OutValues, SavepointName,
    SessionRole, SqlType, Statement, StatementKind, TlsMode, TransactionState, Value, Warning,
    WarningKind,
};

use oracledb::{
    AUTH_MODE_SYSDBA, AUTH_MODE_SYSOPER, Config, Connection, ErrorKind as OraErrorKind, ExecResult,
    Lob, OracleNumber, OracleTimestamp, Row, ToDbValue,
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

/// How many result sets derived from one connection are still open.
///
/// `oracledb`'s call timeout is a property of the **connection**'s socket, not
/// of a statement, so a second `execute` re-arms (or clears) the limit that also
/// bounds every open cursor's fetches. The count makes that visible instead of
/// silent: [`OracleCancelHandle::remaining`] declines to name a stop time while
/// work spans more than one round trip, and [`OracleConnection::execute`] warns
/// when a new statement changes a limit an open result set is relying on.
#[derive(Clone, Default)]
pub(crate) struct OpenResultSets(Arc<AtomicUsize>);

impl OpenResultSets {
    fn count(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }

    pub(crate) fn opened(&self) -> ResultSetGuard {
        self.0.fetch_add(1, Ordering::AcqRel);
        ResultSetGuard(self.clone())
    }
}

/// Decrements the open-result-set count when a cursor goes away, however it
/// goes away — `close`, `drop`, or a failed `OracleCursor::new`.
pub(crate) struct ResultSetGuard(OpenResultSets);

impl Drop for ResultSetGuard {
    fn drop(&mut self) {
        self.0.0.fetch_sub(1, Ordering::AcqRel);
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
        // Spike S8: proven against a TCPS listener on the Phase 0 container.
        // TLS 1.2 with `TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384`, certificate and
        // host name verified against a **private** CA supplied through
        // [`EXT_WALLET_DIR`]. What this `true` does *not* claim is mutual TLS:
        // see [`EXT_WALLET_DIR`] and the crate documentation.
        .with_tls(true)
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
        let allow_timestamp_with_time_zone = matches!(
            params.extensions().get(EXT_ALLOW_TIMESTAMP_WITH_TIME_ZONE),
            Some(ExtensionValue::Flag(true))
        );
        let connection =
            oracledb::connect(config).map_err(|error| crate::error::map_connect(&error))?;
        Ok(Box::new(OracleConnection::new(
            connection,
            allow_timestamp_with_time_zone,
        )))
    }
}

/// Installs a process-wide `rustls` crypto provider, but only if nothing has
/// installed one yet.
///
/// `rustls` resolves its default provider from the compiled-in features, which
/// works while exactly one is enabled — today, `aws-lc-rs`. The moment a second
/// one can be (a `ring` fallback for a platform without a C toolchain, or a
/// dependency that turns one on transitively) that resolution becomes ambiguous
/// and the **first TLS handshake panics** with "no process-level
/// `CryptoProvider` available": a run-time failure from a build-time change,
/// which is the worst shape a failure can have.
///
/// The guard is what makes this acceptable in a library. An application that
/// installed its own provider — FIPS, a hardware backend, `ring` — keeps it,
/// because `install_default` is only reached when `get_default()` is `None`, and
/// its own result is discarded so a race between two threads arriving here at
/// once is a no-op rather than a panic.
///
/// Callers who want the choice made explicitly should call this from their
/// start-up before opening any connection; it is idempotent.
pub fn install_default_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

/// Builds the upstream configuration from vendor-neutral parameters.
fn build_config(params: &reldex_db_driver_api::ConnectionParams) -> DbResult<Config> {
    let tls = params.tls() == TlsMode::Required;
    if tls {
        install_default_crypto_provider();
    }

    let connect_string = match params.endpoint() {
        Endpoint::HostPort {
            host,
            port,
            service,
        } => {
            // `Endpoint::HostPort` promises an Easy Connect target, and
            // `Config::set_connect_string` accepts a full TNS descriptor in the
            // same slot: a host of `(DESCRIPTION=(ADDRESS=(HOST=elsewhere)…)`
            // would turn an interpolated "host:port/service" into a descriptor
            // pointing somewhere else entirely, so an imported or synced
            // connection profile could silently redirect the session. Validate
            // the two free-text parts; a caller that really wants a descriptor
            // says so through `Endpoint::ConnectString`.
            validate_easy_connect(host, "host")?;
            validate_easy_connect(service, "service name")?;
            // Easy Connect defaults to plain TCP, so `TlsMode::Required` has to
            // say `tcps://` explicitly — and the port has to be named, because
            // upstream's parser defaults an unqualified port to 1521 whatever
            // the protocol is.
            if tls {
                format!("tcps://{host}:{port}/{service}")
            } else {
                format!("{host}:{port}/{service}")
            }
        }
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

    // `TlsMode::Required` promises the driver fails rather than falls back, and
    // a descriptor is free text: `Endpoint::ConnectString` can carry
    // `(PROTOCOL=TCP)` while the profile says TLS is mandatory. Upstream
    // negotiates TLS from the **address**, not from anything this wrapper sets,
    // so the only way to keep that promise is to look at the address upstream
    // parsed and refuse when it is not TCPS.
    if tls
        && !config
            .get_connect_descriptor()
            .to_ascii_lowercase()
            .contains("(protocol=tcps)")
    {
        return Err(DbError::new(
            ErrorKind::Configuration,
            "this connection requires encrypted transport, but its endpoint does not \
             ask for TCPS. A connect-string endpoint must name (PROTOCOL=TCPS) — and \
             the TLS port, usually 2484 — itself; this driver will not rewrite a \
             descriptor, and it will not open a plaintext session for a profile that \
             requires TLS",
        ));
    }

    // The wallet directory is honoured whatever the mode says. A descriptor that
    // asks for TCPS gets TLS from upstream regardless of `TlsMode`, and a
    // connection that reaches a private CA only under `Required` would fail in a
    // way that has nothing to do with what the caller changed.
    match params.extensions().get(EXT_WALLET_DIR) {
        Some(ExtensionValue::Text(directory)) => {
            config = config.set_wallet_location(directory.clone());
        }
        Some(_) => {
            return Err(DbError::new(
                ErrorKind::Configuration,
                format!("\"{EXT_WALLET_DIR}\" must be a text value: the path of a directory"),
            ));
        }
        None => {}
    }
    match params.extensions().get(EXT_WALLET_PASSWORD) {
        Some(ExtensionValue::Secret(password)) => {
            config = config.set_wallet_password(password.expose());
        }
        Some(_) => {
            return Err(DbError::new(
                ErrorKind::Configuration,
                format!(
                    "\"{EXT_WALLET_PASSWORD}\" must be a secret value, so that it cannot \
                     reach a log through an ordinary debug rendering"
                ),
            ));
        }
        None => {}
    }

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

    // **Zero by default**, which is a deliberate departure from `oracledb`'s own
    // default of 20 slots. See [`EXT_STATEMENT_CACHE_SIZE`]: a cached statement
    // carries its server-side cursor id into the next execute, and that is what
    // turns the U-3 containment off. A caller may set it back, and is told what
    // that costs.
    let cache_size = match params.extensions().get(EXT_STATEMENT_CACHE_SIZE) {
        Some(ExtensionValue::Integer(size)) => usize::try_from(*size).unwrap_or(0),
        _ => 0,
    };
    config = config.set_stmtcachesize(cache_size);

    Ok(config)
}

/// The characters an Easy Connect host or service name may contain.
///
/// Host names, IPv4 literals and Oracle service names are all drawn from
/// letters, digits, `.`, `-` and `_`; an IPv6 literal additionally needs `:`
/// inside square brackets. Everything else — parentheses, `=`, `/`, whitespace,
/// control characters — is refused, because those are what a descriptor is made
/// of.
fn validate_easy_connect(value: &str, what: &str) -> DbResult<()> {
    let refuse = |reason: &str| {
        Err(DbError::new(
            ErrorKind::Configuration,
            format!(
                "the {what} in this connection's endpoint {reason}. This driver builds an \
                 Easy Connect string from the host, port and service name, so they must be \
                 plain names; supply a TNS descriptor through a connect-string endpoint \
                 instead of hiding one in the {what}"
            ),
        ))
    };
    let (text, bracketed) = match value.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        Some(inner) => (inner, true),
        None => (value, false),
    };
    if text.is_empty() {
        return refuse("is empty");
    }
    if text.len() > 255 {
        return refuse("is longer than 255 characters");
    }
    let allowed = |c: char| {
        c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' || (bracketed && c == ':')
    };
    if !text.chars().all(allowed) {
        return refuse("contains a character that is not allowed in a plain name");
    }
    Ok(())
}

/// Extension key: the size of the upstream statement cache. **Defaults to 0 on
/// this driver**, where `oracledb`'s own default is 20.
///
/// The cache is off because leaving it on defeats the U-3 containment, which is
/// the difference between a refused column and a dead process.
///
/// `messages/execute.rs`'s `write_full_execute` chooses how many rows the
/// *execute* round trip brings back like this:
///
/// ```text
/// if !statement.has_cursor() || statement.requires_define() {
///     num_iters = options.prefetch_rows();      // the wrapper sets this to 0
/// } else {
///     num_iters = options.fetch_array_size();   // default 100
/// }
/// if num_iters > 0 && !statement.no_prefetch() { options |= TTC_EXEC_OPTION_FETCH }
/// ```
///
/// So `prefetch_rows(0)` only describes instead of fetching while the statement
/// has **no** cursor. A cached statement keeps the `cursor_id` the server sent
/// last time (`StatementCache::return_statement` stores it, `get_statement`
/// hands it back), and `serialize` takes the `write_full_execute` path again as
/// soon as `binds_changed()` — which `BindInfo::check_and_set_metadata` sets for
/// something as ordinary as a longer string in the same placeholder. The second
/// execution of `SELECT tz FROM t WHERE k = :1` would then fetch and decode 100
/// rows inside the execute, before `OracleCursor::new` could refuse the column,
/// and a region-encoded `TIMESTAMP WITH TIME ZONE` among them aborts the
/// process.
///
/// The wrapper therefore does both: it calls `Statement::exclude_from_cache` on
/// every statement it prepares, so no cursor id is ever carried into a later
/// execute, and it leaves the cache itself empty so nothing else can. Setting
/// this extension to a non-zero value sizes a cache the driver does not use;
/// it is kept so the knob does not disappear before the upstream fix lands.
pub const EXT_STATEMENT_CACHE_SIZE: &str = "oracle.statement_cache_size";

/// Extension key: allow `TIMESTAMP WITH TIME ZONE` values to be decoded,
/// accepting that one carrying a **named region** will abort the process.
///
/// Off by default, and the default is the whole point. `oracledb`
/// 26.0.0-beta.3 decodes the region-encoded form of that type through a bare
/// `todo!()` (`src/ora_type/timestamp.rs:238`, upstream defect U-3), and
/// because that panic unwinds while the client mutex is held it becomes a
/// process abort rather than a statement failure (U-4) — so no wrapper can
/// contain it, `catch_unwind` included.
///
/// The two encodings cannot be told apart before the value is decoded: the
/// region flag is a bit in the value's own wire bytes, which `oracledb` reads
/// inside the round trip, and the column's describe metadata says only
/// `TIMESTAMP WITH TIME ZONE`. So the choice is per **column**, not per value,
/// and the safe side of it is to refuse the column. A plain-offset value would
/// decode correctly (spike S2 proves it does), which is what this switch is for
/// — but turning it on means any query that happens to touch a region-encoded
/// value kills the application, which is not a trade a database tool should
/// make for the user by default.
pub const EXT_ALLOW_TIMESTAMP_WITH_TIME_ZONE: &str = "oracle.allow_timestamp_with_time_zone";

/// Extension key: a directory containing **`ewallet.pem`**, whose certificates
/// are added to the roots this connection will trust for TCPS.
///
/// This is how a **private CA** is trusted, and it is the only way. `oracledb`
/// 26.0.0-beta.3 builds its `rustls` client configuration from
/// `webpki_roots::TLS_SERVER_ROOTS` — the public web PKI and nothing else — and
/// then, if a wallet location is set, reads `<directory>/ewallet.pem` and:
///
/// - **adds every certificate in it to the root store** when the file contains
///   no private key. That is the enterprise case: put the internal CA (or the
///   self-signed server certificate) in the file and the connection verifies
///   against it *in addition to* the public roots;
/// - treats the file as a **client** certificate instead when it does contain a
///   private key, in which case none of its certificates are trusted as roots.
///   So one file cannot do both jobs: a wallet exported for mutual TLS trusts
///   no private issuer, and a wallet that trusts a private issuer presents no
///   client certificate.
///
/// The file name is fixed — `ewallet.pem`, in the directory named here — and
/// upstream reads nothing else: no `cwallet.sso`, no `ewallet.p12`, no
/// `tnsnames.ora`-style `MY_WALLET_DIRECTORY`, and no `SSL_CERT_FILE`-style
/// environment variable. An Oracle wallet produced by `orapki` therefore has to
/// be converted before this driver can use it (`orapki wallet pkcs12_to_pem`, or
/// simply the issuer's PEM, which is public).
///
/// # What is verified, and what cannot be turned off
///
/// The server certificate is checked by `rustls`'s default verifier: chain,
/// validity, `serverAuth` extended key usage, and the **name** — matched against
/// `subjectAltName` only. A common name of `localhost` is invisible to it, and
/// so is Oracle's `SSL_SERVER_DN_MATCH` / `SSL_SERVER_CERT_DN`: upstream parses
/// both out of a descriptor and sends them to the server in the connect data,
/// but its TLS layer never reads them. The name that is verified is whatever the
/// descriptor's `HOST` says, so `HOST=127.0.0.1` requires an IP address in the
/// SAN and `HOST=db.example.internal` requires that DNS name.
///
/// There is no way to disable verification, and this driver would not offer one
/// if there were.
pub const EXT_WALLET_DIR: &str = "oracle.wallet_dir";

/// Extension key: the password for an **encrypted private key** inside
/// [`EXT_WALLET_DIR`]'s `ewallet.pem`.
///
/// It is only consulted for the client-certificate case — the private key is
/// decrypted with it — so a wallet that merely carries a CA to trust needs no
/// password. Must be an [`ExtensionValue::Secret`], not `Text`, so a debug
/// rendering of the parameters cannot print it.
pub const EXT_WALLET_PASSWORD: &str = "oracle.wallet_password";

/// The cancel handle for a connection.
///
/// This driver is [`CancelKind::PreArmedDeadline`], so `request_cancel` sends
/// nothing and returns immediately.
///
/// # What the reported time actually means
///
/// `oracledb` implements [`Statement::with_deadline`] as the **socket read
/// timeout**, so it bounds each round trip rather than the whole operation. It
/// is therefore an end time only when the remaining work is one round trip — a
/// DML statement, a PL/SQL block, the execute of a query. A result set fetched
/// in several batches gets the full limit again on every `fetch_batch`, so the
/// operation as a whole has no deadline at all, and a number that says
/// otherwise would be a lie the UI renders as a countdown.
///
/// [`CancelOutcome::NotInterruptible`] documents `None` as "the driver cannot
/// measure the remainder", and that is the honest answer in exactly two cases,
/// both of which this handle reports:
///
/// - a result set derived from this connection is still open, so the remaining
///   work spans an unknown number of round trips;
/// - the armed instant has already passed, so the next read is bounded but the
///   call is not about to end.
struct OracleCancelHandle {
    baseline: Instant,
    /// Milliseconds after `baseline` at which the armed deadline expires, or
    /// zero when no deadline is armed. Atomic rather than locked because
    /// `request_cancel` must never block (ADR-0002 D2 rule 1) — including on a
    /// mutex the worker thread happens to hold.
    deadline_at_millis: AtomicU64,
    /// Shared with the connection, for the same reason: no lock.
    open_result_sets: OpenResultSets,
}

impl OracleCancelHandle {
    fn new(open_result_sets: OpenResultSets) -> Self {
        Self {
            baseline: Instant::now(),
            deadline_at_millis: AtomicU64::new(0),
            open_result_sets,
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
        if at == 0 || self.open_result_sets.count() > 0 {
            return None;
        }
        let elapsed = u64::try_from(self.baseline.elapsed().as_millis()).unwrap_or(u64::MAX);
        if elapsed >= at {
            // Expired. The socket timeout still bounds each read, so the call
            // may well continue; promising "0 left" would be worse than saying
            // nothing.
            return None;
        }
        Some(Duration::from_millis(at - elapsed))
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
    /// See [`EXT_ALLOW_TIMESTAMP_WITH_TIME_ZONE`].
    allow_timestamp_with_time_zone: bool,
    open_result_sets: OpenResultSets,
    /// The deadline currently armed on the connection's socket, so a change can
    /// be reported rather than made silently.
    armed_deadline: Option<Duration>,
}

impl OracleConnection {
    fn new(inner: Connection, allow_timestamp_with_time_zone: bool) -> Self {
        let open_result_sets = OpenResultSets::default();
        Self {
            id: ConnectionId::allocate(),
            inner,
            cancel: Arc::new(OracleCancelHandle::new(open_result_sets.clone())),
            closed: Closed::default(),
            // A freshly opened session has no transaction. This is the only
            // place besides a successful commit or rollback where the driver
            // claims to know.
            transaction: TransactionState::Inactive,
            allow_timestamp_with_time_zone,
            open_result_sets,
            armed_deadline: None,
        }
    }

    /// Refuses an output bind this upstream version cannot decode without
    /// risking the process.
    ///
    /// The OUT-bind path has no describe to inspect — the values come back
    /// decoded in the execute response — so the only moment a
    /// `TIMESTAMP WITH TIME ZONE` output can be refused is before the statement
    /// runs, from the type the caller declared. See
    /// [`EXT_ALLOW_TIMESTAMP_WITH_TIME_ZONE`].
    fn check_out_bind_types(&self, statement: &Statement) -> DbResult<()> {
        if self.allow_timestamp_with_time_zone {
            return Ok(());
        }
        let refused = |spec: Option<OutBindSpec>| {
            spec.is_some_and(|spec| spec.sql_type() == SqlType::TimestampWithTimeZone)
        };
        let found = match statement.binds() {
            Binds::Positional(binds) => binds.iter().any(|bind| refused(bind.out_spec())),
            Binds::Named(binds) => binds.iter().any(|bind| refused(bind.bind().out_spec())),
            _ => false,
        };
        if found {
            return Err(DbError::new(
                ErrorKind::Unsupported,
                format!(
                    "an output bind of type TIMESTAMP WITH TIME ZONE is refused on \
                     oracledb 26.0.0-beta.3: a value whose zone is a named region \
                     reaches an unimplemented branch in the upstream decoder and takes \
                     the whole process down with it, and the two forms cannot be told \
                     apart before the value is decoded. Declare the bind as VARCHAR2 \
                     and format it in PL/SQL, or set the connection extension \
                     \"{EXT_ALLOW_TIMESTAMP_WITH_TIME_ZONE}\" to accept the risk"
                ),
            ));
        }
        Ok(())
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
    /// from the resulting cursor are bounded the same way — and so a later
    /// statement that arms a different limit, or none, silently re-bounds every
    /// result set already open on this connection. That is upstream's shape, not
    /// a choice this wrapper can make differently; what it can do is say so,
    /// which is [`deadline_change_warning`]. It cannot be changed once a call is
    /// running: that is the whole of ADR-0001 C1.
    fn apply_deadline(&mut self, deadline: Option<Duration>) -> DbResult<()> {
        self.cancel.arm(deadline);
        self.inner
            .set_call_timeout(deadline)
            .map_err(|error| crate::error::map(&error))?;
        self.armed_deadline = deadline;
        Ok(())
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
        // Never cached: a cached statement carries its server-side cursor id
        // into the next execute, and `write_full_execute` then fetches
        // `fetch_array_size` rows during the execute however small
        // `prefetch_rows` is. See [`EXT_STATEMENT_CACHE_SIZE`] — this is half of
        // the U-3 containment, and the half that survives a caller re-enabling
        // the cache.
        prepared.exclude_from_cache();
        // Without this, `oracledb` materializes CLOB and BLOB values into the
        // row, which breaks `SPEC.md` §12's bounded-memory rule silently.
        prepared.fetch_lobs();
        // **Describe before fetching.** With prefetch on, `oracledb` asks the
        // server for rows in the same round trip as the execute and decodes
        // them there — so by the time this driver can look at a single column
        // type, every prefetched value has already been through the decoder.
        // That is fatal rather than merely awkward: a `TIMESTAMP WITH TIME
        // ZONE` carrying a named region hits a `todo!()` inside that decode and
        // the panic becomes a process abort (U-3 with U-4), which no wrapper
        // can contain. Asking for zero prefetched rows turns the execute into a
        // describe, and `OracleCursor::new` then refuses the column before
        // anything is decoded.
        //
        // The cost is one extra round trip on a *small* result — measured on
        // the Phase 0 container as a 1.3 ms median for a one-row query against
        // a 634 µs median `ping`, i.e. exactly one bare round trip
        // (`describing_before_fetching_costs_about_one_extra_round_trip`). A
        // large result pays nothing: `fetch_array_size` still sizes every
        // fetch, so the same number of batches crosses the wire either way.
        //
        // It does move *where* a query blocks. A `SELECT`'s row source only
        // runs when rows are asked for, so `execute` now returns as soon as the
        // server has described the select list and the work — and any armed
        // deadline — lands on the first `fetch_batch`. For a UI that is an
        // improvement (the grid's columns are known immediately); for spike S4
        // it means a long-running query has to be driven to its first batch to
        // be observed running at all.
        prepared.prefetch_rows(0);
        if let Some(rows) = statement.fetch_rows() {
            let rows = u32::try_from(rows.get()).unwrap_or(u32::MAX);
            prepared.fetch_array_size(rows);
        }
        let cursor = match names {
            Some(names) => {
                let pairs: Vec<(&str, &dyn ToDbValue)> = names.iter().copied().zip(refs).collect();
                prepared.query_named(&pairs)
            }
            None => prepared.query(&refs),
        }
        .map_err(|error| crate::error::map(&error))?;

        let cursor = OracleCursor::new(
            cursor,
            self.id,
            self.closed.clone(),
            self.allow_timestamp_with_time_zone,
            self.open_result_sets.opened(),
        )?;
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
        prepared.exclude_from_cache();
        prepared.fetch_lobs();
        // The non-query path needs this as much as the query path does. If
        // `oracledb`'s own parser disagrees with the classifier about what is a
        // query — the guard in `execute` makes that a refusal rather than a
        // silent loss, but belt and braces — the execute must still not carry
        // rows back, because they would be decoded before any check could run
        // (U-3). Nothing else on this path fetches, so it costs nothing.
        prepared.prefetch_rows(0);
        let mut result = match names {
            Some(names) => {
                let pairs: Vec<(&str, &dyn ToDbValue)> = names.iter().copied().zip(refs).collect();
                prepared.execute_named(&pairs)
            }
            None => prepared.execute(&refs),
        }
        .map_err(|error| crate::error::map(&error))?;

        let rows_affected = result.rows_affected();
        let out_values = self.collect_out_values(statement, kind, &mut result)?;
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
    ///
    /// The slot numbering is the delicate part. The wrapper counts a slot for
    /// every bind the **caller** declared as an output, while the row it indexes
    /// into was built from what the **server's** describe said. Those two agree
    /// for an honest declaration and disagree the moment a parameter the caller
    /// called `In` turns out to be `IN OUT` in the PL/SQL signature — and with
    /// compatible types the disagreement is silent: every later slot shifts by
    /// one and the caller is handed a neighbour's value. [`upstream_out_slots`]
    /// asks the row how many slots the server actually sent, so that becomes a
    /// loud failure instead.
    fn collect_out_values(
        &mut self,
        statement: &Statement,
        kind: StatementKind,
        result: &mut ExecResult,
    ) -> DbResult<OutValues> {
        if !statement.binds().has_outputs() {
            return Ok(OutValues::None);
        }
        let declared = declared_output_count(statement.binds());
        let Some(mut row) = self.output_row(declared, kind, result)? else {
            // A DML `RETURNING … INTO` that matched no row: the outputs exist
            // and are all NULL, which is what the server means by sending none.
            return Ok(all_null_outputs(statement.binds()));
        };

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

    /// The row the declared output binds are read from, or `None` when a DML
    /// `RETURNING` matched no rows.
    ///
    /// PL/SQL out binds and DML `RETURNING … INTO` come back through different
    /// accessors: `ExecResult::out_bind_data` is populated only when
    /// `statement.is_plsql()`, and `returned_data` only when
    /// `is_dml_returning()`. Reading the wrong one is what made `RETURNING`
    /// fail with "invalid column index 0" — a message about nothing the caller
    /// did.
    fn output_row(
        &self,
        declared: usize,
        kind: StatementKind,
        result: &mut ExecResult,
    ) -> DbResult<Option<Row>> {
        let mut returned = result.returned_data();
        if returned.len() > 1 {
            return Err(DbError::new(
                ErrorKind::Unsupported,
                format!(
                    "this statement returned {} rows through RETURNING … INTO, and this \
                     driver supports single-row RETURNING only: the contract's output \
                     binds carry one value each, not an array. Use a cursor or a PL/SQL \
                     block with a collection instead",
                    returned.len()
                ),
            ));
        }
        if let Some(row) = returned.pop() {
            check_out_slots(declared, &row)?;
            return Ok(Some(row));
        }

        let row = result.out_bind_data();
        let slots = upstream_out_slots(&row);
        if slots == 0 && kind == StatementKind::Dml {
            return Ok(None);
        }
        check_out_slots(declared, &row)?;
        Ok(Some(row))
    }

    /// The bind placeholder names in the order they appear in the statement.
    fn bind_name_order(&self, statement: &Statement) -> DbResult<Vec<String>> {
        let mut prepared = self
            .inner
            .statement(statement.sql())
            .map_err(|error| crate::error::map(&error))?;
        prepared.exclude_from_cache();
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
                    // A nested cursor never prefetches, so this describe-time
                    // check runs before any of its values are decoded, exactly
                    // as it does for a top-level cursor.
                    OracleCursor::new(
                        cursor,
                        self.id,
                        self.closed.clone(),
                        self.allow_timestamp_with_time_zone,
                        self.open_result_sets.opened(),
                    )
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

/// How many binds the caller declared as outputs.
fn declared_output_count(binds: &Binds) -> usize {
    match binds {
        Binds::Positional(binds) => binds.iter().filter(|b| b.out_spec().is_some()).count(),
        Binds::Named(binds) => binds
            .iter()
            .filter(|b| b.bind().out_spec().is_some())
            .count(),
        _ => 0,
    }
}

/// A container of the right shape with every output slot NULL.
fn all_null_outputs(binds: &Binds) -> OutValues {
    match binds {
        Binds::Positional(binds) => OutValues::Positional(
            binds
                .iter()
                .map(|bind| bind.out_spec().map(|_| Value::Null))
                .collect(),
        ),
        Binds::Named(binds) => OutValues::Named(
            binds
                .iter()
                .filter(|bind| bind.bind().out_spec().is_some())
                .map(|bind| (bind.name().into(), Value::Null))
                .collect(),
        ),
        _ => OutValues::None,
    }
}

/// How many output slots the **server's** response actually carries.
///
/// `oracledb` exposes no column count on `Row`, but `DbRow::get` bounds-checks
/// against the values it holds and reports `InvalidColumnIndex` past the end, so
/// the count can be probed. `get` borrows and never consumes, so this runs
/// before anything is taken. A conversion failure means the slot exists and
/// simply is not a string, which is the common case and counts.
fn upstream_out_slots(row: &Row) -> usize {
    /// More output parameters than any real call has, so a change in upstream's
    /// error reporting cannot turn this into an endless loop.
    const LIMIT: usize = 1024;
    let mut slots = 0_usize;
    while slots < LIMIT {
        if let Err(error) = row.get::<Option<&str>>(slots)
            && matches!(error.kind(), OraErrorKind::InvalidColumnIndex(_))
        {
            break;
        }
        slots += 1;
    }
    slots
}

/// Refuses to read output binds whose numbering the server does not share.
fn check_out_slots(declared: usize, row: &Row) -> DbResult<()> {
    let slots = upstream_out_slots(row);
    if slots == declared {
        return Ok(());
    }
    Err(DbError::new(
        ErrorKind::DataConversion,
        format!(
            "the server reports {slots} output parameter(s) for this call but {declared} \
             were declared. The most likely cause is a parameter declared as an input \
             that the PL/SQL signature makes IN OUT, which shifts every later output by \
             one slot; this driver refuses to read them rather than return a neighbouring \
             parameter's value. Declare every IN OUT parameter as IN OUT"
        ),
    ))
}

/// The warning a statement carries when arming its own deadline changed the
/// limit that also bounds result sets already open on the same connection.
///
/// Upstream's call timeout lives on the connection's socket, not on the
/// statement, so this is unavoidable; reporting it is not. Returns `None` when
/// nothing is open or nothing changed.
fn deadline_change_warning(
    open_result_sets: usize,
    armed: Option<Duration>,
    requested: Option<Duration>,
) -> Option<Warning> {
    if open_result_sets == 0 || armed == requested {
        return None;
    }
    let change = match requested {
        Some(deadline) => format!("changed the connection's time limit to {deadline:?}"),
        None => "cleared the connection's time limit".to_owned(),
    };
    Some(Warning::new(
        WarningKind::Informational,
        format!(
            "this statement {change}. The underlying Oracle crate keeps that limit on the \
             connection's socket rather than on a statement, and bounds each round trip \
             rather than a whole operation, so the {open_result_sets} result set(s) \
             already open on this connection now fetch under the new limit"
        ),
    ))
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
        self.check_out_bind_types(statement)?;
        let Classification {
            kind, returns_rows, ..
        } = classify(statement.sql());

        // Defence in depth. `oracledb`'s `execute` does not reject a query — it
        // runs it and silently discards the rows — so this classification is the
        // only thing between a valid `SELECT` and a lost result set.
        // `classify::upstream_would_return_rows` is a separate, literal
        // transcription of the upstream parser; if the two ever disagree the
        // statement is refused instead of run.
        if !returns_rows && crate::classify::upstream_would_return_rows(statement.sql()) {
            return Err(DbError::internal(
                "this statement returns rows, but the driver classified it as one that \
                 does not. The underlying Oracle crate would execute it and discard the \
                 result set without reporting anything, so it is refused instead. This is \
                 a driver bug: please report the statement text",
            ));
        }

        let (owned, names) = prepare_binds(statement)?;
        let name_refs: Option<Vec<&str>> = names
            .as_ref()
            .map(|names| names.iter().map(String::as_str).collect());

        let deadline_warning = deadline_change_warning(
            self.open_result_sets.count(),
            self.armed_deadline,
            statement.deadline(),
        );
        self.apply_deadline(statement.deadline())?;

        let outcome = if returns_rows {
            self.execute_query(statement, &owned, name_refs.as_deref())
        } else {
            self.execute_non_query(statement, kind, &owned, name_refs.as_deref())
        };

        let outcome = match (outcome, deadline_warning) {
            (Ok(outcome), Some(warning)) => {
                let mut warnings = outcome.warnings().to_vec();
                warnings.push(warning);
                Ok(outcome.with_warnings(warnings))
            }
            (outcome, _) => outcome,
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

/// A connection that is **dropped** rather than closed must retire its derived
/// handles just the same.
///
/// `oracledb`'s own `Cursor` and `Lob` hold a cloned `Arc<Mutex<Client>>`, so
/// they keep working after the `Connection` value is gone — the session outlives
/// the handle that opened it, and a cursor would go on fetching from a session
/// nothing owns. ADR-0002 D2's handle lifecycle says a derived handle reports
/// once its connection is finished with, and "finished with" includes a
/// `db-core` worker thread unwinding.
impl Drop for OracleConnection {
    fn drop(&mut self) {
        self.closed.mark_closed();
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
    use reldex_db_driver_api::{ConnectionParams, Endpoint, Extensions, Secret};

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
            capabilities.tls(),
            "TCPS is proven by spike S8 against the Phase 0 container's TLS listener"
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
    fn requiring_tls_turns_a_host_port_endpoint_into_a_tcps_address() {
        let config = build_config(
            &params(Endpoint::HostPort {
                host: "localhost".to_owned(),
                port: 2484,
                service: "RELDEX".to_owned(),
            })
            .with_tls(TlsMode::Required),
        )
        .expect("valid parameters");
        let descriptor = config.get_connect_descriptor().to_ascii_lowercase();
        assert!(descriptor.contains("(protocol=tcps)"), "{descriptor}");
        assert!(descriptor.contains("2484"), "{descriptor}");
    }

    #[test]
    fn requiring_tls_over_a_plaintext_descriptor_is_refused_not_downgraded() {
        // The dangerous direction: a profile that says TLS is mandatory, and a
        // descriptor that quietly says TCP. Upstream negotiates from the
        // address, so without this check the session would be plaintext and
        // nothing would say so.
        let error = refusal(
            &params(Endpoint::ConnectString(
                "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=localhost)(PORT=1521))\
                 (CONNECT_DATA=(SERVICE_NAME=RELDEX)))"
                    .to_owned(),
            ))
            .with_tls(TlsMode::Required),
        );
        assert_eq!(error.kind(), ErrorKind::Configuration);
        assert!(error.message().contains("TCPS"), "{error}");

        // The same descriptor with TCPS is accepted.
        assert!(
            build_config(
                &params(Endpoint::ConnectString(
                    "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=localhost)(PORT=2484))\
                     (CONNECT_DATA=(SERVICE_NAME=RELDEX)))"
                        .to_owned(),
                ))
                .with_tls(TlsMode::Required)
            )
            .is_ok()
        );
    }

    #[test]
    fn a_wallet_directory_is_accepted_and_its_password_must_be_a_secret() {
        let mut extensions = Extensions::new();
        extensions
            .set(
                EXT_WALLET_DIR,
                ExtensionValue::Text("/etc/reldex".to_owned()),
            )
            .set(
                EXT_WALLET_PASSWORD,
                ExtensionValue::Secret(Secret::new("not-a-real-password")),
            );
        let parameters = params(Endpoint::ConnectString(
            "tcps://localhost:2484/RELDEX".to_owned(),
        ))
        .with_tls(TlsMode::Required)
        .with_extensions(extensions);
        assert!(build_config(&parameters).is_ok());
        assert!(!format!("{parameters:?}").contains("not-a-real-password"));

        // A password passed as plain text is refused rather than accepted: the
        // contract has a redacting type for exactly this, and taking `Text`
        // would put the value in every `{:?}` of the parameters.
        let mut wrong = Extensions::new();
        wrong.set(
            EXT_WALLET_PASSWORD,
            ExtensionValue::Text("not-a-real-password".to_owned()),
        );
        let error = refusal(
            &params(Endpoint::ConnectString("localhost:1521/RELDEX".to_owned()))
                .with_extensions(wrong),
        );
        assert_eq!(error.kind(), ErrorKind::Configuration);

        let mut wrong_dir = Extensions::new();
        wrong_dir.set(EXT_WALLET_DIR, ExtensionValue::Flag(true));
        let error = refusal(
            &params(Endpoint::ConnectString("localhost:1521/RELDEX".to_owned()))
                .with_extensions(wrong_dir),
        );
        assert_eq!(error.kind(), ErrorKind::Configuration);
    }

    #[test]
    fn installing_the_crypto_provider_is_idempotent() {
        // Called twice on purpose: the second call must not panic, because
        // `connect` reaches it once per connection and two threads can arrive
        // together.
        install_default_crypto_provider();
        install_default_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
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
        let handle = OracleCancelHandle::new(OpenResultSets::default());
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
        let handle = OracleCancelHandle::new(OpenResultSets::default());
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
    fn no_stop_time_is_promised_for_work_that_spans_several_round_trips() {
        // The deadline is upstream's **socket read timeout**: every
        // `fetch_batch` gets the whole of it again, so a result set read in ten
        // batches has no end time at all. `CancelOutcome::NotInterruptible`
        // documents `None` as "the driver cannot measure the remainder", and
        // that is the only honest answer while a cursor is open — a countdown
        // the fetch will not honour is worse than no number.
        let open = OpenResultSets::default();
        let handle = OracleCancelHandle::new(open.clone());
        handle.arm(Some(Duration::from_secs(30)));
        assert!(handle.remaining().is_some(), "one round trip left: a time");

        let guard = open.opened();
        assert_eq!(
            handle.remaining(),
            None,
            "a result set is open, so the remaining work is unbounded"
        );
        drop(guard);
        assert!(handle.remaining().is_some(), "and the cursor is gone again");

        // An expired deadline is not "zero left" either: the socket timeout
        // still bounds each read, so the call may run on, and a UI that saw
        // `Some(0)` would say it was about to stop.
        handle.arm(Some(Duration::from_millis(1)));
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(handle.remaining(), None);
    }

    #[test]
    fn changing_a_deadline_under_an_open_result_set_is_reported() {
        // One socket, one timeout: a second statement re-arms the limit that
        // also bounds fetches from a cursor the caller still holds. The driver
        // cannot give them separate limits, so it says what it did.
        assert!(
            deadline_change_warning(0, Some(Duration::from_secs(5)), None).is_none(),
            "nothing is open, so nothing was disturbed"
        );
        assert!(
            deadline_change_warning(
                2,
                Some(Duration::from_secs(5)),
                Some(Duration::from_secs(5))
            )
            .is_none(),
            "the same deadline is not a change"
        );
        let warning = deadline_change_warning(2, Some(Duration::from_secs(5)), None)
            .expect("clearing the deadline disarms two open result sets");
        assert_eq!(warning.kind(), WarningKind::Informational);
        assert!(warning.message().contains("cleared"), "{warning:?}");
        assert!(warning.message().contains('2'), "{warning:?}");

        let warning = deadline_change_warning(1, None, Some(Duration::from_secs(5)))
            .expect("arming one is a change too");
        assert!(warning.message().contains("changed"), "{warning:?}");
    }

    #[test]
    fn repeated_cancels_are_a_successful_no_op() {
        let handle = OracleCancelHandle::new(OpenResultSets::default());
        for _ in 0..3 {
            assert!(handle.request_cancel().is_ok());
        }
    }

    #[test]
    fn a_host_or_service_that_could_carry_a_descriptor_is_refused() {
        // `Config::set_connect_string` accepts a full TNS descriptor, so an
        // unvalidated host turns "host:port/service" into one — and an imported
        // connection profile could then point the session anywhere.
        for (host, service) in [
            ("(DESCRIPTION=(ADDRESS=(HOST=elsewhere)))", "RELDEX"),
            ("127.0.0.1)(x=", "RELDEX"),
            ("127.0.0.1 ", "RELDEX"),
            ("", "RELDEX"),
            ("127.0.0.1", "RELDEX)(SERVICE_NAME=other"),
            ("127.0.0.1", ""),
            ("127.0.0.1", "REL DEX"),
            ("127.0.0.1", "RELDEX\nX"),
        ] {
            let error = refusal(&params(Endpoint::HostPort {
                host: host.to_owned(),
                port: 1521,
                service: service.to_owned(),
            }));
            assert_eq!(error.kind(), ErrorKind::Configuration, "{host}/{service}");
        }

        // The shapes a real deployment uses still work.
        for (host, service) in [
            ("127.0.0.1", "RELDEX"),
            ("db-01.example.com", "orclpdb1.sub.vcn.oraclevcn.com"),
            ("[::1]", "RELDEX"),
            ("[2001:db8::1]", "FREEPDB1"),
            ("my_host", "RELDEX_TEST"),
        ] {
            assert!(
                build_config(&params(Endpoint::HostPort {
                    host: host.to_owned(),
                    port: 1521,
                    service: service.to_owned(),
                }))
                .is_ok(),
                "{host}/{service} is an ordinary Easy Connect target"
            );
        }

        // A descriptor is still perfectly reachable — through the endpoint that
        // says it is one.
        assert!(
            build_config(&params(Endpoint::ConnectString(
                "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=127.0.0.1)(PORT=1521))\
                 (CONNECT_DATA=(SERVICE_NAME=RELDEX)))"
                    .to_owned()
            )))
            .is_ok()
        );
    }

    #[test]
    fn the_statement_cache_is_off_unless_the_caller_turns_it_on() {
        // Not cosmetic: a cached statement keeps its server-side cursor id, and
        // `write_full_execute` then fetches `fetch_array_size` rows during the
        // *execute* however small `prefetch_rows` is — which is the U-3
        // containment gone. `Config` exposes no getter for the size, so this
        // asserts the decision at the point it is made.
        let extensions = params(Endpoint::ConnectString("127.0.0.1:1521/RELDEX".to_owned()));
        assert!(
            extensions
                .extensions()
                .get(EXT_STATEMENT_CACHE_SIZE)
                .is_none(),
            "the default path must not depend on the caller setting anything"
        );
        assert!(build_config(&extensions).is_ok());

        let mut requested = Extensions::new();
        requested.set(EXT_STATEMENT_CACHE_SIZE, ExtensionValue::Integer(20));
        assert!(
            build_config(
                &params(Endpoint::ConnectString("127.0.0.1:1521/RELDEX".to_owned()))
                    .with_extensions(requested)
            )
            .is_ok(),
            "the knob is still honoured, and documented as sizing an unused cache"
        );
    }

    #[test]
    fn a_statement_that_returns_rows_is_never_sent_down_the_discarding_path() {
        // The guard `execute` applies. It cannot be reached through a real
        // connection without a database, so the invariant it rests on is
        // asserted directly: for every statement, this driver's own routing and
        // `oracledb`'s parser agree about what returns rows.
        for sql in [
            "SELECT 1 FROM dual",
            "(SELECT 1 FROM dual)",
            "  ((select a from t))",
            "WITH x AS (SELECT 1 c FROM dual) SELECT c FROM x",
            "INSERT INTO t (a) SELECT a FROM s",
            "BEGIN NULL; END;",
            "CREATE TABLE t (a NUMBER)",
            "EXPLAIN PLAN FOR SELECT 1 FROM dual",
        ] {
            let classification = classify(sql);
            assert_eq!(
                classification.returns_rows,
                crate::classify::upstream_would_return_rows(sql),
                "{sql}: one of these two decides where the statement goes, and the \
                 other decides whether its rows survive"
            );
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
