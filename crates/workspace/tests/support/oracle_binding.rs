//! The reference [`DriverBinding`] for the Oracle thin driver.
//!
//! Test code here, production code in M2.11: the binding names a concrete
//! driver's extension keys, and only the composition root may depend on a
//! concrete driver (`ARCHITECTURE.md` §2; ADR-0006 "Driver binding"). It is
//! written against the driver's own `EXT_*` constants — not copies of them —
//! so these tests fail if the driver renames a key, and M2.11 can lift it
//! into `crates/ffi` unchanged.

use reldex_db_driver_api::{DbError, Endpoint, ErrorKind, ExtensionValue, Extensions};
use reldex_driver_oracle_thin::{
    EXT_ALLOW_UNENFORCED_SERVER_CERT_DN, EXT_CONNECT_TIMEOUT_UNBOUNDED, EXT_REWRITE_TRIGGER_DDL,
    EXT_WALLET_DIR,
};
use reldex_workspace::{DatabaseType, DriverBinding, DriverOptions, Transport};

/// Binds profiles to `reldex_driver_oracle_thin::OracleThinDriver`.
pub struct OracleThinBinding;

/// The same rule the driver applies to an Easy Connect host or service
/// name: letters, digits, `.`, `-`, `_` (and `:` inside `[...]` for IPv6).
/// A SID is interpolated into a descriptor, so anything that could close a
/// parenthesis or start a new keyword must be refused, not escaped.
fn plain_name(value: &str, what: &str) -> Result<(), DbError> {
    let (text, bracketed) = match value.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        Some(inner) => (inner, true),
        None => (value, false),
    };
    let ok = !text.is_empty()
        && text.len() <= 255
        && text.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') || (bracketed && c == ':')
        });
    if ok {
        Ok(())
    } else {
        Err(DbError::new(
            ErrorKind::Configuration,
            format!(
                "the {what} must be a plain name to build a SID descriptor; use a \
                 connect-string endpoint for anything else"
            ),
        ))
    }
}

impl DriverBinding for OracleThinBinding {
    fn database_type(&self) -> DatabaseType {
        DatabaseType::Oracle
    }

    fn sid_endpoint(
        &self,
        host: &str,
        port: u16,
        sid: &str,
        transport: Transport,
    ) -> Result<Endpoint, DbError> {
        plain_name(host, "host")?;
        plain_name(sid, "SID")?;
        // Easy Connect cannot name a SID, so a SID is always a descriptor —
        // and the descriptor must say TCPS itself, because the driver refuses
        // a TLS-required connect string that does not.
        let protocol = match transport {
            Transport::Plain => "TCP",
            Transport::Tls => "TCPS",
        };
        Ok(Endpoint::ConnectString(format!(
            "(DESCRIPTION=(ADDRESS=(PROTOCOL={protocol})(HOST={host})(PORT={port}))\
             (CONNECT_DATA=(SID={sid})))"
        )))
    }

    fn extensions(&self, options: &DriverOptions<'_>) -> Result<Extensions, DbError> {
        let mut extensions = Extensions::new();
        // Always explicit, so the value in force is visible in a debug
        // rendering of the parameters rather than implied by an absence.
        extensions.set(
            EXT_REWRITE_TRIGGER_DDL,
            ExtensionValue::Flag(options.rewrite_trigger_ddl),
        );
        if options.connect_without_limit {
            extensions.set(EXT_CONNECT_TIMEOUT_UNBOUNDED, ExtensionValue::Flag(true));
        }
        if options.allow_unenforced_certificate_pin {
            extensions.set(
                EXT_ALLOW_UNENFORCED_SERVER_CERT_DN,
                ExtensionValue::Flag(true),
            );
        }
        if let Some(directory) = options.ca_directory {
            let text = directory.to_str().ok_or_else(|| {
                DbError::new(
                    ErrorKind::Configuration,
                    "the CA directory is not a valid Unicode path",
                )
            })?;
            extensions.set(EXT_WALLET_DIR, ExtensionValue::Text(text.to_owned()));
        }
        Ok(extensions)
    }
}
