//! Normalized, vendor-neutral error model (ADR-0002 D3).
//!
//! Drivers translate their native failures into [`DbError`] while preserving
//! the native database error code and message verbatim
//! (`docs/architecture/ARCHITECTURE.md` §4, invariant 9).
//!
//! A `DbError` never carries connection parameters, passwords, tokens or keys:
//! there is no field that could hold one, and [`crate::params::Secret`] redacts
//! itself in `Debug` (`AGENTS.md`, "Code quality").

use std::error::Error;
use std::fmt;

/// Result of any fallible driver-contract operation.
pub type DbResult<T> = Result<T, DbError>;

/// Stable, vendor-neutral classification of a database failure.
///
/// The set is `#[non_exhaustive]`: new categories may be added without a major
/// contract break, so callers must include a wildcard arm. A driver that cannot
/// map a native code to a category must use [`ErrorKind::Other`] and rely on
/// [`DbError::native`] rather than guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Connection parameters are invalid or incomplete; nothing was attempted.
    Configuration,
    /// A connection could not be established (host unreachable, listener down).
    Connection,
    /// The server rejected the supplied credentials or authentication method.
    Authentication,
    /// An established connection was lost mid-operation.
    NetworkLost,
    /// An operation exceeded a configured time limit.
    Timeout,
    /// The operation was cancelled through a [`crate::session::CancelHandle`].
    Cancelled,
    /// The statement could not be parsed or compiled (syntax or semantic).
    Syntax,
    /// A database constraint was violated (unique, check, foreign key, NOT NULL).
    Constraint,
    /// The session lacks privileges for the requested object or operation.
    ///
    /// Kept distinct so permission-dependent metadata and monitoring failures
    /// are separable from genuine driver or connection failures.
    Permission,
    /// The operation failed because of transaction state (deadlock,
    /// serialization failure, rollback required, savepoint not found).
    Transaction,
    /// A server-side or client-side resource limit was reached (quota,
    /// tablespace, session limit, out of memory).
    Resource,
    /// A value could not be represented in the vendor-neutral value model
    /// without loss, or a supplied value was not valid for its declared type.
    DataConversion,
    /// The driver does not implement the requested capability.
    Unsupported,
    /// The driver reached a state it considers impossible; a Reldex or driver bug.
    DriverInternal,
    /// A database-reported error the driver could not classify.
    Other,
}

impl ErrorKind {
    /// Short, stable, lowercase identifier for this category.
    ///
    /// Suitable for logs and for building a stable UI message key. It is part
    /// of the contract: do not change an existing string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Configuration => "configuration",
            Self::Connection => "connection",
            Self::Authentication => "authentication",
            Self::NetworkLost => "network-lost",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Syntax => "syntax",
            Self::Constraint => "constraint",
            Self::Permission => "permission",
            Self::Transaction => "transaction",
            Self::Resource => "resource",
            Self::DataConversion => "data-conversion",
            Self::Unsupported => "unsupported",
            Self::DriverInternal => "driver-internal",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the driver believes about the session after a failure.
///
/// The driver reports this; the core never infers it. `SPEC.md` §18 forbids
/// silently replacing a lost transactional session, so this value drives whether
/// the core revalidates, or surfaces the loss to the user.
///
/// Deliberately **not** `#[non_exhaustive]`: it is a closed three-state ladder
/// (fine / check it / gone) and `db-core` must handle every rung explicitly.
/// Exhaustive matching is the point — a new state would be a semantic change
/// that every call site has to revisit anyway (ADR-0002, amendment S1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SessionState {
    /// The session is known to be usable; the failure was confined to the call.
    #[default]
    Usable,
    /// The session may be usable; the core must
    /// [`ping`](crate::session::DatabaseConnection::ping) before reusing it.
    NeedsValidation,
    /// The session is gone. Any transaction it held is lost and must be
    /// surfaced, never silently re-established.
    Lost,
}

impl SessionState {
    /// The state a failure of `kind` implies before the driver says otherwise.
    ///
    /// [`DbError::new`] applies this, so an error built by a driver that forgets
    /// to call [`DbError::with_session_state`] still errs on the safe side:
    ///
    /// - [`ErrorKind::NetworkLost`] — the session is [`SessionState::Lost`].
    /// - [`ErrorKind::Connection`], [`ErrorKind::Timeout`],
    ///   [`ErrorKind::Cancelled`] and [`ErrorKind::DriverInternal`] — the
    ///   connection may be mid-protocol, so [`SessionState::NeedsValidation`].
    /// - everything else — [`SessionState::Usable`]: the failure was a
    ///   server-side rejection of one statement, which does not disturb the
    ///   session.
    ///
    /// A driver that knows better overrides it with
    /// [`DbError::with_session_state`]; that is the only way to *narrow* the
    /// default, and doing so is a deliberate claim.
    #[must_use]
    pub const fn initial_for(kind: ErrorKind) -> Self {
        match kind {
            ErrorKind::NetworkLost => Self::Lost,
            ErrorKind::Connection
            | ErrorKind::Timeout
            | ErrorKind::Cancelled
            | ErrorKind::DriverInternal => Self::NeedsValidation,
            _ => Self::Usable,
        }
    }
}

/// A native database error, preserved verbatim for diagnostics.
///
/// For Oracle Database, `code` is the numeric part of an `ORA-` code (`ORA-00942`
/// is `942`) and `message` is the server's own text.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NativeError {
    code: i32,
    message: Box<str>,
}

impl NativeError {
    /// Creates a native error from a vendor code and the vendor's own message.
    #[must_use]
    pub fn new(code: i32, message: impl Into<Box<str>>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The vendor's numeric error code.
    #[must_use]
    pub const fn code(&self) -> i32 {
        self.code
    }

    /// The vendor's own error text, unmodified.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for NativeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// Where in the submitted statement text an error occurred.
///
/// Every field is optional because drivers report different things: a SQL parse
/// error usually yields an offset, a PL/SQL compilation error a line and column.
/// This is what later maps compile errors to editor positions (`TASKS.md` P2).
///
/// # The offset is counted in characters, not bytes
///
/// Servers report parse-error offsets in **characters** of the statement text,
/// so this type stores a character offset and says so in the name. Getting this
/// wrong is not theoretical: `SPEC.md` §14 requires Thai data and Thai
/// identifiers to work, and every non-ASCII character makes the byte offset and
/// the character offset diverge.
///
/// A driver reports what the server gave it. The editor, which indexes bytes,
/// converts once with [`SqlPosition::byte_offset_in`] against the exact text
/// that was submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SqlPosition {
    char_offset: Option<u32>,
    line: Option<u32>,
    column: Option<u32>,
}

impl SqlPosition {
    /// A position given as a zero-based **character** offset into the statement
    /// text: the number of Unicode scalar values (Rust `char`s) that precede it.
    #[must_use]
    pub const fn at_char_offset(char_offset: u32) -> Self {
        Self {
            char_offset: Some(char_offset),
            line: None,
            column: None,
        }
    }

    /// A position given as a one-based line and column.
    ///
    /// The column is counted in characters, for the same reason as
    /// [`SqlPosition::at_char_offset`].
    #[must_use]
    pub const fn at_line_column(line: u32, column: u32) -> Self {
        Self {
            char_offset: None,
            line: Some(line),
            column: Some(column),
        }
    }

    /// Zero-based character offset into the submitted statement text, if known.
    #[must_use]
    pub const fn char_offset(self) -> Option<u32> {
        self.char_offset
    }

    /// Resolves the character offset to a byte offset within `sql`.
    ///
    /// `sql` must be the exact text that was submitted. Returns `None` when no
    /// character offset was reported, or when the offset is past the end of
    /// `sql` — which means the driver and the text disagree, and the caller must
    /// not guess.
    ///
    /// ```
    /// use reldex_db_driver_api::SqlPosition;
    ///
    /// let sql = "SELECT 'ข้อมูล' FROM nosuchtable";
    /// let byte = SqlPosition::at_char_offset(16)
    ///     .byte_offset_in(sql)
    ///     .expect("offset is inside the statement");
    ///
    /// assert_eq!(&sql[byte..byte + 4], "FROM");
    /// assert_ne!(byte, 16, "the two units diverge as soon as the text is not ASCII");
    /// ```
    #[must_use]
    pub fn byte_offset_in(self, sql: &str) -> Option<usize> {
        let target = self.char_offset? as usize;
        sql.char_indices()
            .nth(target)
            .map(|(byte_offset, _)| byte_offset)
            .or_else(|| (sql.chars().count() == target).then_some(sql.len()))
    }

    /// One-based line number, if known.
    #[must_use]
    pub const fn line(self) -> Option<u32> {
        self.line
    }

    /// One-based column number, if known.
    #[must_use]
    pub const fn column(self) -> Option<u32> {
        self.column
    }

    /// Whether the position carries any information at all.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.char_offset.is_none() && self.line.is_none() && self.column.is_none()
    }
}

impl fmt::Display for SqlPosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.line, self.column, self.char_offset) {
            (Some(line), Some(column), _) => write!(f, "line {line}, column {column}"),
            (Some(line), None, _) => write!(f, "line {line}"),
            (None, _, Some(offset)) => write!(f, "character offset {offset}"),
            (None, _, None) => f.write_str("unknown position"),
        }
    }
}

/// The normalized error every driver-contract operation returns.
///
/// Construction is a builder chain starting at [`DbError::new`]:
///
/// ```
/// use reldex_db_driver_api::{DbError, ErrorKind, NativeError, SessionState, SqlPosition};
///
/// let err = DbError::new(ErrorKind::Syntax, "table or view does not exist")
///     .with_native(NativeError::new(942, "ORA-00942: table or view does not exist"))
///     .with_position(SqlPosition::at_char_offset(14));
///
/// assert_eq!(err.kind(), ErrorKind::Syntax);
/// assert_eq!(err.native().map(NativeError::code), Some(942));
/// // A syntax error does not disturb the session, so the derived default says so.
/// assert_eq!(err.session_state(), SessionState::Usable);
///
/// // A lost network does, without the driver having to remember to say it.
/// let lost = DbError::new(ErrorKind::NetworkLost, "connection reset");
/// assert_eq!(lost.session_state(), SessionState::Lost);
/// ```
#[derive(Debug)]
pub struct DbError {
    kind: ErrorKind,
    session_state: SessionState,
    retryable: bool,
    message: Box<str>,
    // Boxed so that `DbError` stays small enough for `Result<T, DbError>` to be
    // cheap to move; errors are not a hot path, so the extra allocation only
    // happens when the detail is actually present.
    native: Option<Box<NativeError>>,
    position: Option<Box<SqlPosition>>,
    source: Option<Box<dyn Error + Send + Sync + 'static>>,
}

impl DbError {
    /// Creates an error with a vendor-neutral, credential-free message.
    ///
    /// `session_state` starts at [`SessionState::initial_for`] the kind, not at
    /// [`SessionState::Usable`]: a driver that forgets to classify the session
    /// then under-reports nothing, and `SPEC.md` §18's "never silently replace a
    /// lost transactional session" does not depend on every driver remembering
    /// one builder call. Override with [`DbError::with_session_state`].
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<Box<str>>) -> Self {
        Self {
            kind,
            session_state: SessionState::initial_for(kind),
            retryable: false,
            message: message.into(),
            native: None,
            position: None,
            source: None,
        }
    }

    /// Shorthand for an [`ErrorKind::Cancelled`] error.
    ///
    /// This is what a blocked call returns after a successful cancellation
    /// request (ADR-0002 D2). `session_state` therefore starts at
    /// [`SessionState::NeedsValidation`], because a cancelled call may have left
    /// the connection mid-exchange. A driver that can prove the session is clean
    /// narrows it with [`DbError::with_session_state`].
    #[must_use]
    pub fn cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, "operation cancelled")
    }

    /// Shorthand for an [`ErrorKind::Unsupported`] error naming the capability.
    #[must_use]
    pub fn unsupported(capability: &str) -> Self {
        Self::new(
            ErrorKind::Unsupported,
            format!("driver does not support {capability}"),
        )
    }

    /// Shorthand for an [`ErrorKind::DriverInternal`] error.
    #[must_use]
    pub fn internal(message: impl Into<Box<str>>) -> Self {
        Self::new(ErrorKind::DriverInternal, message)
    }

    /// The error every connection-derived handle returns once its connection has
    /// been closed.
    ///
    /// `handle` names the kind of object for diagnostics ("cursor", "LOB
    /// stream"). Using a [`crate::Cursor`] or [`crate::LobStream`] after
    /// [`DatabaseConnection::close`](crate::DatabaseConnection::close) is a
    /// `db-core` bug, but the driver must report it rather than panic or block
    /// on a connection that no longer exists (ADR-0002 D2, handle lifecycle).
    /// The session is [`SessionState::Lost`] because it is, by construction,
    /// gone.
    #[must_use]
    pub fn connection_closed(handle: &str) -> Self {
        Self::new(
            ErrorKind::DriverInternal,
            format!("{handle} was used after its connection was closed"),
        )
        .with_session_state(SessionState::Lost)
    }

    /// Attaches the preserved native database error.
    #[must_use]
    pub fn with_native(mut self, native: NativeError) -> Self {
        self.native = Some(Box::new(native));
        self
    }

    /// Attaches the position in the statement text the error refers to.
    #[must_use]
    pub fn with_position(mut self, position: SqlPosition) -> Self {
        self.position = Some(Box::new(position));
        self
    }

    /// Records what the driver believes about the session after this failure.
    #[must_use]
    pub fn with_session_state(mut self, session_state: SessionState) -> Self {
        self.session_state = session_state;
        self
    }

    /// Marks the failure as transient.
    ///
    /// This is a driver observation, not a policy: `db-core` decides whether to
    /// retry, and must never retry inside an open transaction.
    #[must_use]
    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    /// Chains the underlying error, reachable through [`Error::source`].
    #[must_use]
    pub fn with_source(mut self, source: impl Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// The vendor-neutral category.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The vendor-neutral message. Never contains credentials.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The preserved native database error, if the failure came from the server.
    #[must_use]
    pub fn native(&self) -> Option<&NativeError> {
        self.native.as_deref()
    }

    /// The position in the statement text, if the driver reported one.
    #[must_use]
    pub fn position(&self) -> Option<&SqlPosition> {
        self.position.as_deref()
    }

    /// What the driver believes about the session after this failure.
    #[must_use]
    pub const fn session_state(&self) -> SessionState {
        self.session_state
    }

    /// Whether the driver considers the failure transient.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        self.retryable
    }

    /// Whether this error reports a cancellation.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.kind == ErrorKind::Cancelled
    }

    /// Whether the session must be revalidated or is gone.
    #[must_use]
    pub fn session_may_be_unusable(&self) -> bool {
        self.session_state != SessionState::Usable
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)?;
        if let Some(native) = &self.native {
            write!(f, " [{native}]")?;
        }
        if let Some(position) = &self.position {
            if !position.is_empty() {
                write!(f, " at {position}")?;
            }
        }
        Ok(())
    }
}

impl Error for DbError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source.as_ref() as &(dyn Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_preserves_native_code_and_position() {
        let err = DbError::new(ErrorKind::Syntax, "table or view does not exist")
            .with_native(NativeError::new(
                942,
                "ORA-00942: table or view does not exist",
            ))
            .with_position(SqlPosition::at_line_column(3, 5));

        let rendered = err.to_string();
        assert!(rendered.contains("942"), "{rendered}");
        assert!(rendered.contains("ORA-00942"), "{rendered}");
        assert!(rendered.contains("line 3, column 5"), "{rendered}");
        assert!(rendered.starts_with("syntax: "), "{rendered}");
    }

    #[test]
    fn defaults_are_conservative_but_not_alarmist() {
        let err = DbError::new(ErrorKind::Other, "boom");
        assert_eq!(err.session_state(), SessionState::Usable);
        assert!(!err.is_retryable());
        assert!(!err.session_may_be_unusable());
        assert!(err.native().is_none());
        assert!(err.position().is_none());
    }

    #[test]
    fn session_state_is_derived_from_the_kind_not_assumed_usable() {
        // Every kind, so a new variant cannot quietly inherit "Usable".
        let expected = [
            (ErrorKind::Configuration, SessionState::Usable),
            (ErrorKind::Connection, SessionState::NeedsValidation),
            (ErrorKind::Authentication, SessionState::Usable),
            (ErrorKind::NetworkLost, SessionState::Lost),
            (ErrorKind::Timeout, SessionState::NeedsValidation),
            (ErrorKind::Cancelled, SessionState::NeedsValidation),
            (ErrorKind::Syntax, SessionState::Usable),
            (ErrorKind::Constraint, SessionState::Usable),
            (ErrorKind::Permission, SessionState::Usable),
            (ErrorKind::Transaction, SessionState::Usable),
            (ErrorKind::Resource, SessionState::Usable),
            (ErrorKind::DataConversion, SessionState::Usable),
            (ErrorKind::Unsupported, SessionState::Usable),
            (ErrorKind::DriverInternal, SessionState::NeedsValidation),
            (ErrorKind::Other, SessionState::Usable),
        ];

        for (kind, state) in expected {
            assert_eq!(SessionState::initial_for(kind), state, "{kind}");
            assert_eq!(
                DbError::new(kind, "message").session_state(),
                state,
                "DbError::new did not apply the derived state for {kind}"
            );
        }
    }

    #[test]
    fn a_driver_may_still_override_the_derived_session_state() {
        let narrowed = DbError::new(
            ErrorKind::Timeout,
            "call timed out before any bytes were sent",
        )
        .with_session_state(SessionState::Usable);
        assert_eq!(narrowed.session_state(), SessionState::Usable);
        assert!(!narrowed.session_may_be_unusable());

        let widened = DbError::new(ErrorKind::Syntax, "socket died mid-parse")
            .with_session_state(SessionState::Lost);
        assert_eq!(widened.session_state(), SessionState::Lost);
    }

    #[test]
    fn cancelled_helper_reports_cancellation_and_a_suspect_session() {
        let err = DbError::cancelled();
        assert!(err.is_cancelled());
        assert_eq!(err.kind(), ErrorKind::Cancelled);
        // A cancel may leave the connection mid-exchange, so this is the default
        // rather than something each driver has to remember.
        assert_eq!(err.session_state(), SessionState::NeedsValidation);
        assert!(err.session_may_be_unusable());
    }

    #[test]
    fn a_handle_used_after_its_connection_closed_reports_a_lost_session() {
        let err = DbError::connection_closed("cursor");
        assert_eq!(err.kind(), ErrorKind::DriverInternal);
        assert_eq!(err.session_state(), SessionState::Lost);
        assert!(err.to_string().contains("cursor"), "{err}");
    }

    #[test]
    fn source_chain_is_reachable() {
        #[derive(Debug)]
        struct Inner;
        impl fmt::Display for Inner {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("inner cause")
            }
        }
        impl Error for Inner {}

        let err = DbError::new(ErrorKind::NetworkLost, "connection reset").with_source(Inner);
        let source = err.source().expect("source should be set");
        assert_eq!(source.to_string(), "inner cause");
    }

    #[test]
    fn error_kind_strings_are_stable_and_unique() {
        let kinds = [
            ErrorKind::Configuration,
            ErrorKind::Connection,
            ErrorKind::Authentication,
            ErrorKind::NetworkLost,
            ErrorKind::Timeout,
            ErrorKind::Cancelled,
            ErrorKind::Syntax,
            ErrorKind::Constraint,
            ErrorKind::Permission,
            ErrorKind::Transaction,
            ErrorKind::Resource,
            ErrorKind::DataConversion,
            ErrorKind::Unsupported,
            ErrorKind::DriverInternal,
            ErrorKind::Other,
        ];
        let mut seen = Vec::new();
        for kind in kinds {
            let text = kind.as_str();
            assert!(!text.is_empty());
            assert!(!seen.contains(&text), "duplicate kind string {text}");
            seen.push(text);
        }
    }

    #[test]
    fn sql_position_renders_the_most_useful_form() {
        assert_eq!(
            SqlPosition::at_char_offset(12).to_string(),
            "character offset 12"
        );
        assert_eq!(
            SqlPosition::at_line_column(2, 7).to_string(),
            "line 2, column 7"
        );
        assert_eq!(SqlPosition::at_char_offset(12).char_offset(), Some(12));
        assert_eq!(SqlPosition::at_line_column(2, 7).char_offset(), None);
        assert!(SqlPosition::default().is_empty());
        assert_eq!(SqlPosition::default().to_string(), "unknown position");
    }

    #[test]
    fn character_offsets_resolve_correctly_through_non_ascii_sql() {
        // `SPEC.md` §14: Thai text is a supported, ordinary case. A server
        // reports the offset in characters; the editor indexes bytes.
        let sql = "SELECT 'ข้อมูลพนักงาน' FROM nosuchtable";
        let byte_of_from = sql.find("FROM").expect("FROM is present");
        let chars_before_from = sql[..byte_of_from].chars().count();
        assert_ne!(
            chars_before_from, byte_of_from,
            "the test text must actually distinguish the two units"
        );

        let position =
            SqlPosition::at_char_offset(u32::try_from(chars_before_from).expect("fits in u32"));
        assert_eq!(position.byte_offset_in(sql), Some(byte_of_from));

        // Offset zero and offset "one past the end" both resolve; anything
        // beyond that means the driver and the text disagree.
        assert_eq!(SqlPosition::at_char_offset(0).byte_offset_in(sql), Some(0));
        let char_count = u32::try_from(sql.chars().count()).expect("fits in u32");
        assert_eq!(
            SqlPosition::at_char_offset(char_count).byte_offset_in(sql),
            Some(sql.len())
        );
        assert_eq!(
            SqlPosition::at_char_offset(char_count + 1).byte_offset_in(sql),
            None
        );
        assert_eq!(SqlPosition::at_line_column(1, 1).byte_offset_in(sql), None);
    }

    #[test]
    fn db_error_stays_small_enough_for_cheap_results() {
        // `clippy::result_large_err` flags error types above 128 bytes; keep the
        // headroom explicit so a future field addition is a deliberate choice.
        assert!(
            size_of::<DbError>() <= 128,
            "DbError grew to {} bytes",
            size_of::<DbError>()
        );
    }
}
