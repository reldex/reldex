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

use reldex_db_driver_api::{DbError, ErrorKind, NativeError, SessionState, SqlPosition};

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
pub(crate) fn map_connect(error: &oracledb::Error) -> DbError {
    let mapped = map(error);
    if mapped.kind() != ErrorKind::NetworkLost {
        return mapped;
    }
    let mut rebuilt = DbError::new(
        ErrorKind::Connection,
        format!("the database could not be reached: {}", mapped.message()),
    );
    if let Some(native) = mapped.native() {
        rebuilt = rebuilt.with_native(NativeError::new(native.code(), native.message()));
    }
    rebuilt
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
    fn render_into_reuses_one_buffer() {
        let mut buffer = String::new();
        assert_eq!(render_into(&mut buffer, &42_i32), "42");
        assert_eq!(render_into(&mut buffer, &"ข้อมูล"), "ข้อมูล");
        assert_eq!(buffer, "ข้อมูล", "the buffer is cleared, not appended to");
    }
}
