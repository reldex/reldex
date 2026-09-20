//! Translation of `oracledb` failures into the contract's [`DbError`].
//!
//! Two things matter here and are tested: the native `ORA-` code and the
//! server's own text survive verbatim in [`NativeError`]
//! (`ARCHITECTURE.md` §4, invariant 9), and no credential can reach a message.
//!
//! # What this upstream version does and does not give us
//!
//! Verified against `oracledb` 26.0.0-beta.3 source on 2026-09-19:
//!
//! - The server's error **number is parsed and then discarded**:
//!   `response/error_info.rs` keeps it only long enough to build a message, and
//!   the public `ErrorKind::DbError(String)` carries just that string. So the
//!   numeric code has to be recovered by parsing `ORA-nnnnn` back out of the
//!   message. ADR-0002's "notes for driver implementers" describe a structured
//!   `DbError { code, offset }` — that is upstream `main`/beta.4, not the
//!   pinned beta.3.
//! - The server's **error position is read off the wire and thrown away**
//!   (`error_info.rs` does `resp.read_ub2()?; // error position`). A character
//!   offset for a plain SQL error is therefore unavailable at any price. What
//!   *can* be recovered is the `line n, column m` that `ORA-06550` and `PLS-`
//!   errors put in their own message text, which is the case `SPEC.md` §24.14
//!   needs.
//! - `oracledb::Error` does **not** implement [`std::error::Error`], so it
//!   cannot be attached with `DbError::with_source`. The upstream text is
//!   preserved in `NativeError` instead.

use std::fmt::Write as _;

use reldex_db_driver_api::{
    DbError, ErrorKind, NativeError, SessionState, SqlPosition, Warning, WarningKind,
};

use oracledb::ErrorKind as OraErrorKind;

/// Converts an `oracledb` failure into the contract error.
pub(crate) fn map(error: &oracledb::Error) -> DbError {
    match error.kind() {
        OraErrorKind::DbError(_) => from_server_message(&error.to_string()),

        // Transport and lifecycle -------------------------------------------
        OraErrorKind::CallTimeoutExceeded => DbError::new(
            ErrorKind::Timeout,
            "the call timeout armed for this statement expired",
        ),
        OraErrorKind::DeadConnection => DbError::new(
            ErrorKind::NetworkLost,
            "the database or network closed the connection",
        ),
        OraErrorKind::UnableToRecover => DbError::new(
            ErrorKind::NetworkLost,
            "the connection could not be recovered and was closed",
        ),
        // Upstream's own text for this kind is the fixed string "stream
        // operation failed", but its `Display` appends the cause — and for a
        // TCPS session the cause is the `rustls` failure: "invalid peer
        // certificate: UnknownIssuer", "NotValidForName", "invalid peer
        // certificate: Expired". An earlier version of this arm replaced the
        // whole thing with a sentence of its own and threw that away, so every
        // TLS failure read as "the network stream failed" and an untrusted CA
        // could not be told from a host-name mismatch or from a dead socket.
        // Spike S8 is what found it. The cause carries no credential — it is an
        // I/O or certificate error — and `s8_tcps.rs` asserts as much.
        OraErrorKind::StreamOperation => DbError::new(ErrorKind::NetworkLost, error.to_string()),
        OraErrorKind::NotConnected => DbError::connection_closed("connection"),

        // Listener / name resolution ----------------------------------------
        OraErrorKind::InvalidServiceName(..)
        | OraErrorKind::InvalidSid(..)
        | OraErrorKind::ListenerRefusedConnection(..)
        | OraErrorKind::UnexpectedRefuse(_)
        | OraErrorKind::InvalidRedirect(_) => {
            DbError::new(ErrorKind::Connection, error.to_string())
        }

        // Configuration ------------------------------------------------------
        OraErrorKind::InvalidConnectString(..)
        | OraErrorKind::NoConnectString
        | OraErrorKind::NoCredentials
        | OraErrorKind::NoConfigDir
        | OraErrorKind::InvalidNetworkName(_)
        | OraErrorKind::InvalidDescriptorNode(..)
        | OraErrorKind::IfileCycleDetected(..)
        | OraErrorKind::TnsAliasNotFound(..)
        | OraErrorKind::TlsOperation
        | OraErrorKind::PemFileOperation
        | OraErrorKind::WalletUnreadable(_)
        | OraErrorKind::WalletPrivateKeyInvalid(_)
        | OraErrorKind::WalletPasswordMissingOrInvalid(..)
        | OraErrorKind::InvalidBindName(_)
        | OraErrorKind::MissingBindValue(_)
        | OraErrorKind::WrongNumPositionalBinds(..)
        | OraErrorKind::NameHasEmbeddedQuotes => {
            DbError::new(ErrorKind::Configuration, error.to_string())
        }

        // Value conversion ---------------------------------------------------
        OraErrorKind::ColumnTruncated(..)
        | OraErrorKind::UnsupportedConversion(..)
        | OraErrorKind::InvalidOracleNumber(_)
        | OraErrorKind::InvalidEncodedString
        | OraErrorKind::InvalidEncodedVector
        | OraErrorKind::InvalidOsonEncodedBytes
        | OraErrorKind::OutOfRange(_)
        | OraErrorKind::ValueWasNull
        | OraErrorKind::DifferentTypes(..)
        | OraErrorKind::IntegerTooLarge(..)
        | OraErrorKind::InvalidColumnIndex(_)
        | OraErrorKind::InvalidColumnName(_) => {
            DbError::new(ErrorKind::DataConversion, error.to_string())
        }

        // Things the driver cannot do ---------------------------------------
        OraErrorKind::UnsupportedDbType(_)
        | OraErrorKind::NotImplemented(_)
        | OraErrorKind::ServerVersionNotSupported
        | OraErrorKind::UnsupportedArrowType(_)
        | OraErrorKind::UnsupportedOsonNodeType(_)
        | OraErrorKind::UnsupportedOsonVersion(_)
        | OraErrorKind::UnsupportedVectorFormat(_)
        | OraErrorKind::UnsupportedVectorVersion(_)
        | OraErrorKind::UnsupportedDeepDataSecurityFeature
        | OraErrorKind::EndUserSecurityContextRequiresTcps => {
            DbError::new(ErrorKind::Unsupported, error.to_string())
        }

        OraErrorKind::ParseError(..) => DbError::new(ErrorKind::Syntax, error.to_string()),

        // `no data found` is control flow for `query_row`, which this wrapper
        // does not use; if it ever escapes it is not a session problem.
        OraErrorKind::NoDataFound => DbError::new(ErrorKind::Other, error.to_string()),

        _ => DbError::new(ErrorKind::DriverInternal, error.to_string()),
    }
}

/// Converts a failure raised while *opening* a connection.
///
/// At connect time there is no session to lose, so a refused port, an
/// unreachable host or a truncated handshake is [`ErrorKind::Connection`] —
/// retryable, and not something that invalidates an open transaction.
/// `oracledb` reports those through the same transport kinds it uses for a
/// mid-session drop ([`OraErrorKind::StreamOperation`] and friends), so the
/// phase has to supply the distinction the upstream error does not carry.
/// Measured in spike S1: connecting to a closed port reports
/// `StreamOperation`, which [`map`] alone would call a lost session.
///
/// A socket-level timeout needs the same correction for a different reason.
/// `oracledb` turns **every** `TimedOut`/`WouldBlock` `io::Error` into
/// `CallTimeoutExceeded` (`src/error.rs:131`, cause discarded), so the TCP
/// handshake giving up after the operating system's own patience — 22.0 s
/// against an unroutable address, spike S10 — arrived here indistinguishable
/// from a fired call deadline. Reporting that as [`ErrorKind::Timeout`] told
/// the user a deadline they never set had expired, and attached a session state
/// to a session that was never created.
pub(crate) fn map_connect(error: &oracledb::Error) -> DbError {
    let mapped = map(error);
    if mapped.kind() == ErrorKind::Timeout {
        return DbError::new(
            ErrorKind::Connection,
            "the database could not be reached: the connection attempt timed out in the \
             network layer. The wait was the operating system's own, not a deadline Reldex \
             asked for — this driver's connect limit reports itself in as many words when \
             it is what ended the attempt, and the Oracle crate has no connect timeout of \
             any kind (upstream gap U-15)",
        );
    }
    if mapped.kind() != ErrorKind::NetworkLost {
        return mapped;
    }
    let mut rebuilt = DbError::new(
        ErrorKind::Connection,
        format!("the database could not be reached: {}", mapped.message()),
    )
    .with_retryable(mapped.is_retryable());
    if let Some(native) = mapped.native() {
        rebuilt = rebuilt.with_native(NativeError::new(native.code(), native.message()));
    }
    // The session state is **not** carried over, and that is the point of this
    // function: `map` would have said `Lost`, and there is no session to lose
    // while one is being opened. `ErrorKind::Connection`'s own default —
    // `NeedsValidation` — is the honest answer, and
    // `a_connect_that_times_out_in_the_socket_is_not_reported_as_a_call_timeout`
    // asserts it. A position is likewise meaningless for a connect failure.
    rebuilt
}

/// Replaces upstream's bind-count complaint with the reason a caller who
/// supplied **no** binds can still trigger it.
///
/// `oracledb`'s SQL parser scans the whole statement text for `:name` and turns
/// every hit into a bind placeholder, DDL included
/// (`src/statement/sql_parser.rs`: `determine_statement_type` sets `is_ddl` and
/// the scan carries on regardless). So `CREATE TRIGGER … :NEW.col := …` — which
/// every Oracle IDE has to be able to run, and which SQL\*Plus accepts — comes
/// back as "1 positional bind values are required but 0 were provided": a
/// message that blames the caller for a placeholder they did not write.
///
/// The failure cannot be prevented from here; upstream offers no way to turn
/// the scan off. What it can do is say what happened and what works instead.
/// Spike S12 found it on `CREATE TRIGGER`.
///
/// **Since the automatic rewrite landed, a trigger no longer reaches this**:
/// [`crate::rewrite`] applies the workaround itself by default, so this message
/// is what a caller sees when they have switched that off with
/// [`crate::EXT_REWRITE_TRIGGER_DDL`] — which is exactly the owner's decision,
/// "the existing explanatory refusal is returned". It still covers any *other*
/// statement whose text upstream reads a placeholder into, which is why it is
/// not narrowed to triggers.
pub(crate) fn explain_parsed_placeholders(error: DbError) -> DbError {
    if error.kind() != ErrorKind::Configuration
        || !error
            .message()
            .contains("positional bind values are required but 0 were provided")
    {
        return error;
    }
    DbError::new(
        ErrorKind::Unsupported,
        format!(
            "this statement declares no bind values, but the Oracle crate's SQL parser \
             treated a `:name` in its text as a bind placeholder and then required a \
             value for it. `:NEW` and `:OLD` in a trigger body are the usual cause: the \
             parser applies the same scan to DDL as to DML and offers no way to turn it \
             off. Submitting the same text inside a PL/SQL block works, because a quoted \
             string is skipped: BEGIN EXECUTE IMMEDIATE q'[<the DDL>]'; END;. Upstream \
             reported: {}",
            error.message()
        ),
    )
    .with_session_state(SessionState::Usable)
}

/// Oracle's "the object was created, and it does not compile".
///
/// A plain `CREATE` that compiles with errors **succeeds** and leaves the
/// diagnosis in `USER_ERRORS`; the driver reports it through
/// `Connection::last_warning`. The same statement inside `EXECUTE IMMEDIATE`
/// raises this as a PL/SQL exception instead, because that is how dynamic SQL
/// reports it.
const ORA_SUCCESS_WITH_COMPILATION_ERROR: i32 = 24344;

/// Whether this failure is Oracle saying "created, but with compilation
/// errors".
pub(crate) fn is_compiled_with_errors(error: &DbError) -> bool {
    error
        .native()
        .is_some_and(|native| native.code() == ORA_SUCCESS_WITH_COMPILATION_ERROR)
}

/// Turns that exception back into what the user would have seen had the
/// statement not been rewritten: a success carrying a
/// [`WarningKind::CompiledWithErrors`] warning.
///
/// This is not softening a failure. The object **is** created — that is exactly
/// what ORA-24344 means — so reporting an error would tell the user nothing
/// happened when something did, and would hide the object their next statement
/// is about to find. `SPEC.md` §24.14 needs the compilation errors to reach the
/// editor, and a warning is the channel the contract has for them
/// (ADR-0002 D6); the driver's rewrite must not be the reason they arrive as
/// something else. The native code and the server's own text are preserved, so
/// nothing about the server's answer is lost.
pub(crate) fn compiled_with_errors(error: &DbError) -> Warning {
    let mut warning = Warning::new(
        WarningKind::CompiledWithErrors,
        format!(
            "the object was created but did not compile cleanly: {}. Query USER_ERRORS (or \
             ALL_ERRORS) for the details",
            error.message()
        ),
    );
    if let Some(native) = error.native() {
        warning = warning.with_native(NativeError::new(native.code(), native.message()));
    }
    warning
}

/// Adds the sentence a certificate **name** failure needs when the descriptor
/// carried Oracle's own server-certificate parameters.
///
/// The two are easy to confuse and the confusion wastes real time: a descriptor
/// that sets `SSL_SERVER_CERT_DN` or `SSL_SERVER_DN_MATCH` looks like it
/// configures which name is checked, and it does not — neither parameter
/// reaches this client's TLS layer at all (upstream gap U-14). The name that
/// fails is the descriptor's `HOST`, matched against `subjectAltName`, so
/// "the DN is right, why is this failing" has an answer only if somebody says
/// so. Called only when [`crate::descriptor`] found one of the parameters, so an
/// ordinary TCPS deployment never sees it.
///
/// # How the failure is recognized
///
/// Not by a transcribed string. `oracledb` keeps the `rustls` failure as a
/// private `cause` and exposes it only through `Display` (its `ErrorKind` for
/// the whole family is the payload-free `StreamOperation`), so the text is all
/// there is. What can be avoided is *guessing* at the text: the markers are
/// rendered by the `rustls` that is actually linked, from the two variants that
/// mean "this certificate does not cover that name", so a change to its wording
/// changes the markers with it rather than silently stopping the match. A
/// unit test asserts the markers still derive to something usable.
pub(crate) fn explain_name_verification(error: DbError) -> DbError {
    if !names_a_certificate_name_failure(error.message()) {
        return error;
    }
    // Every field is carried across deliberately: `DbError` has no setter for
    // its message, so the only way to add a sentence is to rebuild, and a
    // rebuild that copies three fields out of five quietly resets the other
    // two. `source` is the one that cannot be carried — `Error::source` lends
    // it, it cannot be taken back out — and it is always `None` here because
    // neither `map` nor `map_connect` ever sets one (`oracledb::Error` does not
    // implement `std::error::Error`; see the module documentation). The
    // assertion says so out loud rather than leaving it to be rediscovered.
    debug_assert!(
        std::error::Error::source(&error).is_none(),
        "this rebuild would drop a source; give `DbError` a way to take one first"
    );
    let mut rebuilt = DbError::new(
        error.kind(),
        format!(
            "{}. The name this driver verifies is the one in the descriptor's HOST, matched \
             against the certificate's subjectAltName — not its distinguished name. \
             SSL_SERVER_CERT_DN and SSL_SERVER_DN_MATCH have no influence on it: the Oracle \
             crate underneath parses them, sends them to the server and never applies them \
             (upstream gap U-14). A certificate that identifies the server only by DN, or a \
             HOST its subjectAltName does not list, cannot be accepted by any descriptor \
             setting",
            error.message()
        ),
    )
    .with_session_state(error.session_state())
    .with_retryable(error.is_retryable());
    if let Some(native) = error.native() {
        rebuilt = rebuilt.with_native(NativeError::new(native.code(), native.message()));
    }
    if let Some(position) = error.position() {
        rebuilt = rebuilt.with_position(*position);
    }
    rebuilt
}

fn names_a_certificate_name_failure(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    name_mismatch_markers()
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// What `rustls` itself says when a certificate does not cover the name asked
/// for, taken from `rustls` rather than copied out of it.
fn name_mismatch_markers() -> Vec<String> {
    /// A name no certificate carries, used to find where `rustls` renders the
    /// expected name inside its own message so the invariant wording around it
    /// can be kept and the variable part dropped.
    const SENTINEL: &str = "reldex-marker.invalid";

    // The context-free variant renders as its own `Debug` name.
    let mut markers = vec![
        rustls::CertificateError::NotValidForName
            .to_string()
            .to_ascii_lowercase(),
    ];
    // The one with context renders a sentence, and is what a real handshake
    // produces: `certificate not valid for name "…"; certificate is only valid
    // for DnsName("…")`.
    if let Ok(expected) = rustls::pki_types::ServerName::try_from(SENTINEL) {
        let rendered = rustls::CertificateError::NotValidForNameContext {
            expected,
            presented: Vec::new(),
        }
        .to_string()
        .to_ascii_lowercase();
        if let Some((prefix, _)) = rendered.split_once(SENTINEL) {
            let prefix = prefix.trim_end_matches(['"', '\'', ' ']);
            if !prefix.is_empty() {
                markers.push(prefix.to_owned());
            }
        }
    }
    markers
}

/// Converts a failure raised by `Lob::read` while streaming a large object.
///
/// `oracledb` collapses its structured error into
/// `io::Error::other(error.to_string())` (`src/lob.rs`'s `io_error`), so by the
/// time the wrapper sees it the only thing left is the message text. Calling
/// every one of those a [`ErrorKind::DataConversion`] — which
/// [`SessionState::initial_for`] reads as "the session is fine" — would tell
/// `db-core` that a network drop half way through a 100 MB CLOB left the
/// transaction intact, and `SPEC.md` §18 exists precisely so that cannot happen.
///
/// So the text is classified the way every other failure is: by its `ORA-` code
/// where it has one, by upstream's own fixed wording for the transport kinds
/// where it does not, and otherwise as [`ErrorKind::DriverInternal`], whose
/// default session state is [`SessionState::NeedsValidation`]. `DataConversion`
/// is left for the one case that really is one: a decode the wrapper recognizes.
pub(crate) fn map_lob_read(error: &std::io::Error) -> DbError {
    let text = error.to_string();
    if first_native_code(&text).is_some() {
        return from_server_message(&text);
    }
    // Upstream's `Display` for the transport and lifecycle kinds is a fixed
    // string per kind, listed in `src/error.rs`. Matching it is fragile across
    // versions, which is exactly why the `oracledb` dependency is pinned exactly
    // and why the fall-through below is conservative rather than cheerful.
    let lost = |message: &str| {
        DbError::new(ErrorKind::NetworkLost, message.to_owned())
            .with_session_state(SessionState::Lost)
    };
    if text.contains("the database or network closed the connection")
        || text.contains("stream operation failed")
        || text.contains("unable to recover from error")
    {
        return lost(&format!(
            "the connection failed while reading a large object: {text}"
        ));
    }
    if text.contains("not connected to database") {
        return DbError::connection_closed("LOB stream");
    }
    if text.contains("the configured call timeout was exceeded") {
        return DbError::new(
            ErrorKind::Timeout,
            "the call timeout armed for this connection expired while reading a large object",
        );
    }
    if text.contains("invalid encoded string") || text.contains("invalid utf-16") {
        return DbError::new(
            ErrorKind::DataConversion,
            format!("a large object contained text this driver could not decode: {text}"),
        );
    }
    DbError::internal(format!("reading a large object failed: {text}"))
}

/// Builds a contract error from a server error message such as
/// `ORA-00942: table or view does not exist`.
fn from_server_message(message: &str) -> DbError {
    let code = first_native_code(message);
    let kind = code.map_or(ErrorKind::Other, classify_code);
    let mut error = DbError::new(kind, first_line(message).to_owned());
    if let Some(code) = code {
        error = error.with_native(NativeError::new(code, message));
    }
    if let Some(position) = plsql_position(message) {
        error = error.with_position(position);
    }
    if matches!(kind, ErrorKind::NetworkLost) {
        error = error.with_session_state(SessionState::Lost);
    }
    error
}

/// The first line of a multi-line server message, for the short summary.
fn first_line(message: &str) -> &str {
    message.lines().next().unwrap_or(message).trim_end()
}

/// Extracts the first `ORA-`, `PLS-` or `TNS-` code in a server message.
///
/// Oracle stacks several codes in one message (`ORA-06550` wrapping a
/// `PLS-00201`, for instance); the first one is the one that classifies the
/// failure, and the whole text is preserved in [`NativeError`] regardless.
fn first_native_code(message: &str) -> Option<i32> {
    for prefix in ["ORA-", "PLS-", "TNS-"] {
        if let Some(code) = find_code(message, prefix) {
            return Some(code);
        }
    }
    None
}

fn find_code(message: &str, prefix: &str) -> Option<i32> {
    let start = message.find(prefix)? + prefix.len();
    let digits: String = message[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Recovers `line n, column m` from a PL/SQL compilation error message.
///
/// `ORA-06550` reports the position in its own text, which is the only place
/// this upstream version leaves one (see the module documentation). The format
/// is fixed — `ORA-06550: line 3, column 5:` — and only that format is read: an
/// earlier version searched the whole message for the word "line ", which any
/// server message or application `RAISE_APPLICATION_ERROR` text may contain
/// ("ORA-20001: line 7 of the input file is malformed"), and reported whatever
/// number followed as a position in the *statement*. A position pointing at the
/// wrong token is worse than none, because `SPEC.md` §24.14 has the editor
/// highlight it.
fn plsql_position(message: &str) -> Option<SqlPosition> {
    message.lines().find_map(|line| {
        let rest = line
            .trim_start()
            .strip_prefix("ORA-06550:")
            .or_else(|| line.trim_start().strip_prefix("ORA-06553:"))?
            .trim_start()
            .strip_prefix("line ")?;
        let number = take_number(rest)?;
        let column = take_number(rest.strip_prefix(&format!("{number}, column "))?)?;
        Some(SqlPosition::at_line_column(number, column))
    })
}

fn take_number(text: &str) -> Option<u32> {
    let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Maps a native `ORA-` code to a vendor-neutral category.
///
/// Anything unmapped stays [`ErrorKind::Other`] rather than being forced into a
/// wrong category (ADR-0002 D3); the native code is always available.
fn classify_code(code: i32) -> ErrorKind {
    match code {
        // Authentication and account state.
        1017 | 1004 | 1005 | 1045 | 9911 | 28000 | 28001 | 28002 | 28003 | 28008 | 28009
        | 28040 => ErrorKind::Authentication,

        // The user asked the server to stop.
        1013 => ErrorKind::Cancelled,

        // The session is gone.
        28 | 1012 | 1089 | 2396 | 3113 | 3114 | 3135 | 12571 | 12537 | 12547 | 12152 => {
            ErrorKind::NetworkLost
        }

        // Privileges. Kept separate from Syntax so permission-dependent
        // metadata and monitoring failures stay distinguishable
        // (`ARCHITECTURE.md` §4).
        1031 | 1749 | 1919 | 1924 | 1950 => ErrorKind::Permission,

        // Constraints.
        1 | 1400 | 1407 | 2270..=2279 | 2290..=2299 => ErrorKind::Constraint,

        // Transaction state.
        60 | 1002 | 1086 | 1555 | 1591 | 2091 | 2092 | 8177 | 8176 => ErrorKind::Transaction,

        // Server-side and client-side resource limits.
        18 | 20 | 1000 | 1536 | 1631 | 1632 | 1652 | 1653 | 1654 | 1688 | 4030 | 4031 => {
            ErrorKind::Resource
        }

        // Value conversion.
        932 | 1438 | 1722 | 1830 | 1839 | 1841 | 1847 | 1858 | 1861 | 1866 | 6502 | 12899 => {
            ErrorKind::DataConversion
        }

        // Parse and compile failures, including the PL/SQL wrapper codes.
        900..=931 | 933..=999 | 1747 | 1756 | 1789 | 1790 | 6550 | 6553 | 24344 => {
            ErrorKind::Syntax
        }

        // Everything in the TNS/Net range that is not a mid-session loss.
        12000..=12999 => ErrorKind::Connection,

        _ => ErrorKind::Other,
    }
}

/// Renders a value into a reusable buffer, so a column costs one allocation
/// rather than one per row (ADR-0002, "notes for driver implementers").
pub(crate) fn render_into<'a>(buffer: &'a mut String, value: &impl std::fmt::Display) -> &'a str {
    buffer.clear();
    // Writing into a `String` cannot fail; the result is discarded rather than
    // unwrapped so this stays free of `unwrap` in non-test code.
    let _ = write!(buffer, "{value}");
    buffer.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(message: &str) -> DbError {
        from_server_message(message)
    }

    #[test]
    fn the_native_code_and_text_survive_verbatim() {
        let error = server("ORA-00942: table or view does not exist");
        assert_eq!(error.kind(), ErrorKind::Syntax);
        assert_eq!(error.native().map(NativeError::code), Some(942));
        assert_eq!(
            error.native().map(NativeError::message),
            Some("ORA-00942: table or view does not exist")
        );
        assert_eq!(error.session_state(), SessionState::Usable);
    }

    #[test]
    fn authentication_failures_are_classified_and_do_not_disturb_the_session() {
        for message in [
            "ORA-01017: invalid username/password; logon denied",
            "ORA-28000: the account is locked",
            "ORA-28001: the password has expired",
        ] {
            let error = server(message);
            assert_eq!(error.kind(), ErrorKind::Authentication, "{message}");
            assert_eq!(error.session_state(), SessionState::Usable, "{message}");
        }
    }

    #[test]
    fn a_lost_session_is_reported_as_lost() {
        for message in [
            "ORA-03113: end-of-file on communication channel",
            "ORA-03114: not connected to ORACLE",
            "ORA-03135: connection lost contact",
            "ORA-00028: your session has been killed",
        ] {
            let error = server(message);
            assert_eq!(error.kind(), ErrorKind::NetworkLost, "{message}");
            assert_eq!(error.session_state(), SessionState::Lost, "{message}");
            assert!(error.session_may_be_unusable(), "{message}");
        }
    }

    #[test]
    fn network_and_listener_errors_are_connection_errors() {
        for (message, code) in [
            (
                "ORA-12514: TNS:listener does not currently know of service",
                12514,
            ),
            ("ORA-12541: TNS:no listener", 12541),
            (
                "ORA-12154: TNS:could not resolve the connect identifier",
                12154,
            ),
        ] {
            let error = server(message);
            assert_eq!(error.kind(), ErrorKind::Connection, "{message}");
            assert_eq!(error.native().map(NativeError::code), Some(code));
            // Connection failures leave the session in doubt, never "fine".
            assert_eq!(error.session_state(), SessionState::NeedsValidation);
        }
    }

    #[test]
    fn a_cancelled_statement_is_cancelled_not_an_unknown_failure() {
        let error = server("ORA-01013: user requested cancel of current operation");
        assert_eq!(error.kind(), ErrorKind::Cancelled);
        assert!(error.is_cancelled());
        assert_eq!(error.session_state(), SessionState::NeedsValidation);
    }

    #[test]
    fn constraints_permissions_and_syntax_are_separate_categories() {
        assert_eq!(
            server("ORA-00001: unique constraint (RELDEX_TEST.PK) violated").kind(),
            ErrorKind::Constraint
        );
        assert_eq!(
            server("ORA-02291: integrity constraint violated - parent key not found").kind(),
            ErrorKind::Constraint
        );
        assert_eq!(
            server("ORA-01400: cannot insert NULL into (\"T\".\"A\")").kind(),
            ErrorKind::Constraint
        );
        assert_eq!(
            server("ORA-01031: insufficient privileges").kind(),
            ErrorKind::Permission
        );
        assert_eq!(
            server("ORA-00904: \"NOSUCH\": invalid identifier").kind(),
            ErrorKind::Syntax
        );
        assert_eq!(
            server("ORA-00900: invalid SQL statement").kind(),
            ErrorKind::Syntax
        );
        assert_eq!(
            server("ORA-01722: invalid number").kind(),
            ErrorKind::DataConversion
        );
        assert_eq!(
            server("ORA-01652: unable to extend temp segment").kind(),
            ErrorKind::Resource
        );
        assert_eq!(
            server("ORA-00060: deadlock detected while waiting for resource").kind(),
            ErrorKind::Transaction
        );
        assert_eq!(
            server("ORA-01086: savepoint 'X' never established").kind(),
            ErrorKind::Transaction
        );
    }

    #[test]
    fn a_plsql_compile_error_keeps_its_line_and_column() {
        // `SPEC.md` §24.14: compile errors must reach the user, positioned.
        let message = "ORA-06550: line 3, column 5:\n\
                       PLS-00201: identifier 'NOSUCH' must be declared\n\
                       ORA-06550: line 3, column 5:\n\
                       PL/SQL: Statement ignored";
        let error = server(message);
        assert_eq!(error.kind(), ErrorKind::Syntax);
        assert_eq!(error.native().map(NativeError::code), Some(6550));
        let position = error.position().copied().expect("position recovered");
        assert_eq!(position.line(), Some(3));
        assert_eq!(position.column(), Some(5));
        // The whole stack, not just the first line, is preserved natively.
        assert!(
            error
                .native()
                .map(NativeError::message)
                .unwrap_or_default()
                .contains("PLS-00201")
        );
        // The short message is only the first line.
        assert_eq!(error.message(), "ORA-06550: line 3, column 5:");
    }

    #[test]
    fn a_position_is_only_read_from_the_format_that_carries_one() {
        // Every one of these contains the word "line " followed by a number,
        // and none of them is reporting a position in the submitted statement.
        // Highlighting a token on the strength of them would point the editor
        // at something unrelated (`SPEC.md` §24.14).
        for message in [
            "ORA-20001: line 7 of the import file is malformed",
            "ORA-29283: invalid file operation: line 3, column 9 unreadable",
            "ORA-00942: table or view does not exist",
            "ORA-06512: at line 4",
            "something about a line 9, column 2 that is not an Oracle position",
        ] {
            assert!(
                server(message).position().is_none(),
                "{message} must not produce a statement position"
            );
        }

        // The ORA-06550 family does carry one, and it is still recovered even
        // when the compile error is not the first line of the stack.
        let wrapped = "ORA-06512: at \"RELDEX_TEST.P\", line 12\n\
                       ORA-06550: line 2, column 3:\n\
                       PLS-00103: Encountered the symbol \"END\"";
        let position = server(wrapped).position().copied().expect("recovered");
        assert_eq!((position.line(), position.column()), (Some(2), Some(3)));
    }

    #[test]
    fn an_object_that_is_not_there_is_a_compile_failure_not_a_lost_session() {
        // ORA-00942 is deliberately `Syntax`. The contract defines that kind as
        // "the statement could not be parsed or compiled (**syntax or
        // semantic**)", and an object that does not exist — or that the session
        // cannot see, which Oracle reports identically — is exactly a semantic
        // compile failure. There is no object-not-found kind to move it to, and
        // the native ORA-00942 is preserved for anything that wants to tell the
        // two apart.
        let error = server("ORA-00942: table or view does not exist");
        assert_eq!(error.kind(), ErrorKind::Syntax);
        assert_eq!(error.native().map(NativeError::code), Some(942));
        assert_eq!(error.session_state(), SessionState::Usable);
    }

    #[test]
    fn a_lob_read_that_lost_the_connection_does_not_claim_the_session_is_fine() {
        // `SPEC.md` §18: a lost transaction is never hidden. `Lob::read` throws
        // the structured error away, so the text is all there is — but calling
        // every one of them `DataConversion` told `db-core` the session was
        // `Usable` after the socket had gone.
        for text in [
            "the database or network closed the connection",
            "stream operation failed",
            "unable to recover from error: connection has been closed",
        ] {
            let error = map_lob_read(&std::io::Error::other(text));
            assert_eq!(error.kind(), ErrorKind::NetworkLost, "{text}");
            assert_eq!(error.session_state(), SessionState::Lost, "{text}");
        }

        // A server error keeps its code and its own classification.
        let error = map_lob_read(&std::io::Error::other(
            "ORA-03113: end-of-file on communication channel",
        ));
        assert_eq!(error.kind(), ErrorKind::NetworkLost);
        assert_eq!(error.native().map(NativeError::code), Some(3113));

        let error = map_lob_read(&std::io::Error::other("not connected to database"));
        assert!(error.session_may_be_unusable());

        let error = map_lob_read(&std::io::Error::other(
            "the configured call timeout was exceeded",
        ));
        assert_eq!(error.kind(), ErrorKind::Timeout);

        // Only a decode failure is a conversion failure.
        let error = map_lob_read(&std::io::Error::other(
            "invalid encoded string: invalid utf-16: lone surrogate",
        ));
        assert_eq!(error.kind(), ErrorKind::DataConversion);
        assert_eq!(error.session_state(), SessionState::Usable);

        // Anything unrecognized is a driver problem, and the session is in
        // doubt rather than fine.
        let error = map_lob_read(&std::io::Error::other("something nobody has seen"));
        assert_eq!(error.kind(), ErrorKind::DriverInternal);
        assert_eq!(error.session_state(), SessionState::NeedsValidation);
    }

    #[test]
    fn a_bare_pls_error_is_still_classified() {
        let error = server("PLS-00103: Encountered the symbol \"END\"");
        assert_eq!(error.native().map(NativeError::code), Some(103));
    }

    #[test]
    fn an_unmapped_code_is_other_rather_than_a_wrong_guess() {
        let error = server("ORA-65535: something nobody mapped");
        assert_eq!(error.kind(), ErrorKind::Other);
        assert_eq!(error.native().map(NativeError::code), Some(65535));
    }

    #[test]
    fn a_message_with_no_code_at_all_still_produces_an_error() {
        let error = server("something went wrong");
        assert_eq!(error.kind(), ErrorKind::Other);
        assert!(error.native().is_none());
        assert!(error.position().is_none());
    }

    #[test]
    fn thai_text_in_a_server_message_survives_and_does_not_break_parsing() {
        // `SPEC.md` §14 makes non-ASCII an ordinary case, including in errors.
        let error = server("ORA-00001: unique constraint (RELDEX_TEST.ข้อมูล_PK) violated");
        assert_eq!(error.kind(), ErrorKind::Constraint);
        assert!(
            error
                .native()
                .map(NativeError::message)
                .unwrap_or_default()
                .contains("ข้อมูล")
        );
    }

    #[test]
    fn a_connect_that_times_out_in_the_socket_is_not_reported_as_a_call_timeout() {
        // `oracledb`'s `impl From<std::io::Error>` turns **any** `TimedOut` or
        // `WouldBlock` I/O error into `ErrorKind::CallTimeoutExceeded` and
        // discards the cause (`src/error.rs:131`). During `connect` there is no
        // statement and no armed deadline, so passing that through told the
        // user "the call timeout armed for this statement expired" about a
        // connection that was never established, and left the session
        // `NeedsValidation` — a session state for a session that does not
        // exist. Spike S10 found it against 192.0.2.1 (22.0 s, RFC 5737
        // TEST-NET-1).
        let upstream = oracledb::Error::from(std::io::Error::from(std::io::ErrorKind::TimedOut));
        let mapped = map_connect(&upstream);
        assert_eq!(mapped.kind(), ErrorKind::Connection);
        assert_eq!(mapped.session_state(), SessionState::NeedsValidation);
        assert!(
            mapped.message().contains("could not be reached"),
            "the message should describe a connect failure, not a fired deadline: {}",
            mapped.message()
        );

        // The same upstream kind on an established connection still means what
        // it says: a deadline the caller armed really did fire.
        let on_a_session = map(&oracledb::Error::from(std::io::Error::from(
            std::io::ErrorKind::TimedOut,
        )));
        assert_eq!(on_a_session.kind(), ErrorKind::Timeout);
    }

    #[test]
    fn a_name_failure_says_which_name_was_checked() {
        // Driven through the **real** composition rather than a hand-built
        // error: `connect` applies `map_connect` first, so by the time the
        // explanation is added the kind is already `Connection` and the text
        // already carries the "could not be reached" prefix. Asserting against
        // a `NetworkLost` error built here would have pinned a shape production
        // never produces. The cause text is the one spike S8 observed.
        let cause = "invalid peer certificate: certificate not valid for name \"127.0.0.1\"; \
                     certificate is only valid for DnsName(\"localhost\") or \
                     DnsName(\"reldex-oracle19c\")";
        let mapped = map_connect(&oracledb::Error::from(std::io::Error::other(cause)));
        assert_eq!(
            mapped.kind(),
            ErrorKind::Connection,
            "the connect-phase correction runs first"
        );
        let explained = explain_name_verification(mapped);
        assert_eq!(explained.kind(), ErrorKind::Connection);
        assert_eq!(explained.session_state(), SessionState::NeedsValidation);
        assert!(
            explained
                .message()
                .starts_with("the database could not be reached: stream operation failed: "),
            "the whole composition must survive: {explained}"
        );
        assert!(explained.message().contains(cause), "{explained}");
        assert!(
            explained.message().contains("subjectAltName"),
            "{explained}"
        );
        assert!(
            explained.message().contains("SSL_SERVER_CERT_DN"),
            "{explained}"
        );

        // The context-free variant, which is what `rustls` renders when it has
        // no names to report, through the same path.
        let bare = map_connect(&oracledb::Error::from(std::io::Error::other(
            "invalid peer certificate: NotValidForName",
        )));
        assert!(
            explain_name_verification(bare)
                .message()
                .contains("subjectAltName")
        );
    }

    #[test]
    fn a_failure_that_is_not_about_the_name_is_left_exactly_as_it_was() {
        // Nothing may be appended to an untrusted issuer, an expired
        // certificate or a dead socket: the sentence would point at the wrong
        // thing, which is worse than no sentence (the same rule the error
        // position follows).
        for message in [
            "stream operation failed: invalid peer certificate: UnknownIssuer",
            "stream operation failed: invalid peer certificate: Expired",
            "stream operation failed: connection reset by peer",
            "ORA-01017: invalid username/password; logon denied",
        ] {
            let error = DbError::new(ErrorKind::NetworkLost, message);
            assert_eq!(
                explain_name_verification(error).message(),
                message,
                "{message}"
            );
        }
    }

    #[test]
    fn the_name_failure_markers_are_derived_from_the_rustls_that_is_linked() {
        // This is the test that makes the recognition non-fragile. The markers
        // are rendered by `rustls` itself, so if its wording changes they change
        // with it — and if a future version stops producing something usable,
        // this fails loudly instead of the match quietly never firing again.
        let markers = name_mismatch_markers();
        assert_eq!(markers.len(), 2, "{markers:?}");
        for marker in &markers {
            assert!(!marker.is_empty(), "{markers:?}");
            assert!(
                marker.chars().all(|c| !c.is_ascii_uppercase()),
                "markers are matched against lowercased text: {markers:?}"
            );
        }
        // And each one really does identify the failure it was derived from.
        let rendered = rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForName)
            .to_string();
        assert!(names_a_certificate_name_failure(&rendered), "{rendered}");
    }

    #[test]
    fn render_into_reuses_one_buffer() {
        let mut buffer = String::new();
        assert_eq!(render_into(&mut buffer, &42_i32), "42");
        assert_eq!(render_into(&mut buffer, &"ข้อมูล"), "ข้อมูล");
        assert_eq!(buffer, "ข้อมูล", "the buffer is cleared, not appended to");
    }
}
