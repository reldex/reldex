//! Connect strings this driver builds from plain parts: an Easy Connect string
//! for a host, a port and a service name, and a connect descriptor for a SID.
//!
//! Both interpolate caller-supplied text into Oracle Net syntax, so both refuse
//! anything that is not a plain name first. A host of
//! `(DESCRIPTION=(ADDRESS=(HOST=elsewhere)…)` — or a SID of
//! `ORCL)(SERVICE_NAME=OTHER` — would otherwise turn an interpolated string
//! into a descriptor pointing somewhere else entirely, and an imported or
//! synced connection profile could silently redirect the session. A caller
//! that really wants a descriptor says so through `Endpoint::ConnectString`.

use reldex_db_driver_api::{DbError, DbResult, ErrorKind};

/// What the validated parts are about to become, for the refusal's wording.
#[derive(Clone, Copy)]
enum Building {
    EasyConnect,
    SidDescriptor,
}

/// The characters a host, service name or SID may contain here.
///
/// Host names, IPv4 literals, Oracle service names and SIDs are all drawn from
/// letters, digits, `.`, `-` and `_`; an IPv6 literal additionally needs `:`
/// inside square brackets. Everything else — parentheses, `=`, `/`, whitespace,
/// control characters — is refused, because those are what a descriptor is made
/// of. Returns the name without IPv6 brackets.
fn plain_name<'a>(value: &'a str, what: &str, building: Building) -> DbResult<&'a str> {
    let refuse = |reason: &str| {
        let explanation = match building {
            Building::EasyConnect => {
                "This driver builds an Easy Connect string from the host, port and service \
                 name, so they must be plain names; supply a TNS descriptor through a \
                 connect-string endpoint"
            }
            Building::SidDescriptor => {
                "This driver builds a connect descriptor from the host, port and SID, so they \
                 must be plain names; supply a full TNS descriptor through a connect-string \
                 endpoint"
            }
        };
        Err(DbError::new(
            ErrorKind::Configuration,
            format!(
                "the {what} in this connection's endpoint {reason}. {explanation} instead of \
                 hiding one in the {what}"
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
    Ok(text)
}

/// The Easy Connect string for a host, port and service name.
///
/// Easy Connect defaults to plain TCP, so TLS has to say `tcps://` explicitly —
/// and the port has to be named, because upstream's parser defaults an
/// unqualified port to 1521 whatever the protocol is.
pub(crate) fn easy_connect(host: &str, port: u16, service: &str, tls: bool) -> DbResult<String> {
    plain_name(host, "host", Building::EasyConnect)?;
    plain_name(service, "service name", Building::EasyConnect)?;
    Ok(if tls {
        format!("tcps://{host}:{port}/{service}")
    } else {
        format!("{host}:{port}/{service}")
    })
}

/// The connect descriptor that names a database by **SID**, for
/// `Endpoint::ConnectString`.
///
/// Easy Connect cannot name a SID (the `host:port:SID` shorthand is not Easy
/// Connect, and the Phase 0 image refuses it with `ORA-12545` —
/// `tools/oracle-test-db/README.md`), so a SID is always a full descriptor:
///
/// ```text
/// (DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=db)(PORT=1521))(CONNECT_DATA=(SID=ORCL)))
/// ```
///
/// With `tls` the protocol is `TCPS`: a connection that requires TLS refuses a
/// connect string that does not ask for TCPS itself, and this is where it asks.
/// An IPv6 host is written without its square brackets, which are Easy Connect
/// syntax, not descriptor syntax (not yet exercised against a live listener).
///
/// ```
/// let descriptor = reldex_driver_oracle_thin::sid_endpoint("db", 1521, "ORCL", false)?;
/// assert_eq!(
///     descriptor,
///     "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=db)(PORT=1521))(CONNECT_DATA=(SID=ORCL)))"
/// );
/// # Ok::<(), reldex_db_driver_api::DbError>(())
/// ```
///
/// # Errors
///
/// A `Configuration` [`DbError`] when the host or SID is not a plain name.
pub fn sid_endpoint(host: &str, port: u16, sid: &str, tls: bool) -> DbResult<String> {
    let host = plain_name(host, "host", Building::SidDescriptor)?;
    let sid = plain_name(sid, "SID", Building::SidDescriptor)?;
    let protocol = if tls { "TCPS" } else { "TCP" };
    Ok(format!(
        "(DESCRIPTION=(ADDRESS=(PROTOCOL={protocol})(HOST={host})(PORT={port}))\
         (CONNECT_DATA=(SID={sid})))"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easy_connect_output_is_byte_identical_to_the_inline_format_it_replaced() {
        for (host, port, service, tls) in [
            ("127.0.0.1", 1521, "RELDEX", false),
            ("db.example.internal", 2484, "ORDERS_1", true),
            ("[::1]", 1521, "svc", false),
            ("[2001:db8::1]", 2484, "svc", true),
        ] {
            let expected = if tls {
                format!("tcps://{host}:{port}/{service}")
            } else {
                format!("{host}:{port}/{service}")
            };
            assert_eq!(
                easy_connect(host, port, service, tls).expect("plain names"),
                expected
            );
        }
    }

    #[test]
    fn the_easy_connect_refusal_wording_is_unchanged() {
        let error = easy_connect("(DESCRIPTION=(x))", 1521, "s", false).expect_err("refused");
        assert_eq!(error.kind(), ErrorKind::Configuration);
        assert_eq!(
            error.message(),
            "the host in this connection's endpoint contains a character that is not allowed \
             in a plain name. This driver builds an Easy Connect string from the host, port and \
             service name, so they must be plain names; supply a TNS descriptor through a \
             connect-string endpoint instead of hiding one in the host"
        );
    }

    #[test]
    fn a_sid_becomes_a_descriptor_whose_protocol_follows_tls() {
        assert_eq!(
            sid_endpoint("10.0.0.5", 2484, "ORCL", true).expect("plain names"),
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=10.0.0.5)(PORT=2484))\
             (CONNECT_DATA=(SID=ORCL)))"
        );
        assert_eq!(
            sid_endpoint("[::1]", 1521, "ORCL", false).expect("plain names"),
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=::1)(PORT=1521))\
             (CONNECT_DATA=(SID=ORCL)))"
        );
    }

    #[test]
    fn a_sid_or_host_that_could_rewrite_the_descriptor_is_refused() {
        for (host, sid) in [
            ("db", "ORCL)(SERVICE_NAME=OTHER"),
            ("db)(HOST=elsewhere", "ORCL"),
            ("db", "OR CL"),
            ("db", ""),
            ("", "ORCL"),
            ("::1", "ORCL"),
        ] {
            let error = sid_endpoint(host, 1521, sid, false).expect_err("refused");
            assert_eq!(error.kind(), ErrorKind::Configuration, "{host} / {sid}");
            assert!(error.message().contains("SID"), "{}", error.message());
        }
    }
}
