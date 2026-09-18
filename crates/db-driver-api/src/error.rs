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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SqlPosition {
    byte_offset: Option<u32>,
    line: Option<u32>,
    column: Option<u32>,
}

impl SqlPosition {
    /// A position given as a zero-based byte offset into the statement text.
    #[must_use]
    pub const fn at_offset(byte_offset: u32) -> Self {
        Self {
            byte_offset: Some(byte_offset),
            line: None,
            column: None,
        }
    }

    /// A position given as a one-based line and column.
    #[must_use]
    pub const fn at_line_column(line: u32, column: u32) -> Self {
        Self {
            byte_offset: None,
            line: Some(line),
            column: Some(column),
        }
    }

    /// Adds a zero-based byte offset to an existing position.
    #[must_use]
    pub const fn with_offset(mut self, byte_offset: u32) -> Self {
        self.byte_offset = Some(byte_offset);
        self
    }

    /// Zero-based byte offset into the submitted statement text, if known.
    #[must_use]
    pub const fn byte_offset(self) -> Option<u32> {
        self.byte_offset
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
        self.byte_offset.is_none() && self.line.is_none() && self.column.is_none()
    }
}

impl fmt::Display for SqlPosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.line, self.column, self.byte_offset) {
            (Some(line), Some(column), _) => write!(f, "line {line}, column {column}"),
            (Some(line), None, _) => write!(f, "line {line}"),
            (None, _, Some(offset)) => write!(f, "offset {offset}"),
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
///     .with_position(SqlPosition::at_offset(14))
///     .with_session_state(SessionState::Usable);
///
/// assert_eq!(err.kind(), ErrorKind::Syntax);
/// assert_eq!(err.native().map(NativeError::code), Some(942));
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
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<Box<str>>) -> Self {
        Self {
            kind,
            session_state: SessionState::Usable,
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
    /// request (ADR-0002 D2). `session_state` defaults to
    /// [`SessionState::Usable`]; a driver using the call-timeout fallback must
    /// override it with [`SessionState::NeedsValidation`].
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
    fn cancelled_helper_reports_cancellation() {
        let err = DbError::cancelled();
        assert!(err.is_cancelled());
        assert_eq!(err.kind(), ErrorKind::Cancelled);

        let timed_out = DbError::cancelled().with_session_state(SessionState::NeedsValidation);
        assert!(timed_out.session_may_be_unusable());
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
        assert_eq!(SqlPosition::at_offset(12).to_string(), "offset 12");
        assert_eq!(
            SqlPosition::at_line_column(2, 7).to_string(),
            "line 2, column 7"
        );
        assert_eq!(
            SqlPosition::at_line_column(2, 7).with_offset(30).line(),
            Some(2)
        );
        assert!(SqlPosition::default().is_empty());
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
