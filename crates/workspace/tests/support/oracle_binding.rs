//! The reference [`DriverBinding`] for the Oracle thin driver.
//!
//! Test code here, production code in M2.11: the binding names a concrete
//! driver, and only the composition root may depend on a concrete driver
//! (`ARCHITECTURE.md` §2; ADR-0006 "Driver binding"). It holds no vendor
//! syntax of its own — it maps options onto the driver's own `EXT_*`
//! constants (not copies of them, so these tests fail if the driver renames
//! a key) and calls the driver's own SID builder — which is exactly the
//! shape M2.11 lifts into `crates/ffi`.

use reldex_db_driver_api::{DbError, Endpoint, ErrorKind, ExtensionValue, Extensions};
use reldex_driver_oracle_thin::{
    EXT_ALLOW_UNENFORCED_SERVER_CERT_DN, EXT_CONNECT_TIMEOUT_UNBOUNDED, EXT_REWRITE_TRIGGER_DDL,
    EXT_WALLET_DIR, sid_endpoint,
};
use reldex_workspace::{DatabaseType, DriverBinding, DriverOptions, Transport};

/// Binds profiles to `reldex_driver_oracle_thin::OracleThinDriver`.
pub struct OracleThinBinding;

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
        // The driver builds the descriptor and refuses anything that is not
        // a plain name; TLS decides whether it asks for TCPS.
        sid_endpoint(host, port, sid, transport == Transport::Tls).map(Endpoint::ConnectString)
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
