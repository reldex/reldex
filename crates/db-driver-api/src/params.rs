//! Vendor-neutral connection parameters and credential handling (ADR-0002 D7).
//!
//! Everything the core needs to understand is a typed field. Everything only a
//! particular driver understands goes into [`Extensions`], which the core passes
//! through without interpreting it.
//!
//! Secrets live in [`Secret`], which redacts itself in `Debug` and implements no
//! `Display`, so a `Debug` of a whole [`ConnectionParams`] is safe to log
//! (`AGENTS.md`, "Code quality"). [`Secret`] also wipes its buffer on drop
//! (with `zeroize`), but that is a hygiene measure, not a guarantee that no
//! copy survives in memory — see its documentation.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use zeroize::Zeroize;

/// A credential value that never renders itself.
///
/// What this type actually promises:
///
/// - It has no `Display`, and its `Debug` prints `Secret(<redacted>)`, so a
///   password cannot reach a log through an ordinary `{:?}` of any struct that
///   contains one. That is the property `AGENTS.md` requires, and it is tested.
/// - [`Secret::expose`] is the single, greppable place the plaintext is read.
/// - [`Drop`] wipes the buffer — its whole capacity, not just its length —
///   with `zeroize`, whose volatile writes the compiler may not elide (a plain
///   `fill(0)` just before a free is a dead store it may remove). Every
///   [`Clone`] is a separate buffer wiped the same way. ADR-0007 S6.
///
/// What it does **not** promise: that the plaintext is gone from memory. A
/// `String` passed to [`Secret::new`] becomes the secret's buffer without a
/// copy, but a `&str` is copied and its source is the caller's; a `String`
/// that reallocated while it was being built left its old buffer behind; and
/// the allocator, the OS page cache and any swap file are outside this crate's
/// reach. Treating the wipe as a security control would be exactly the kind of
/// overclaim `SPEC.md` §2 rules out.
pub struct Secret {
    bytes: Vec<u8>,
}

impl Secret {
    /// Wraps a secret value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            bytes: value.into().into_bytes(),
        }
    }

    /// Borrows the secret for the one place that must see it: the driver's
    /// authentication call.
    #[must_use]
    pub fn expose(&self) -> &str {
        std::str::from_utf8(&self.bytes).unwrap_or_default()
    }

    /// Whether the secret is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl Drop for Secret {
    /// Wipes the buffer. See the type documentation: this is hygiene, not a
    /// guarantee that no copy survives, and nothing in Reldex may rely on it.
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

impl Clone for Secret {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
        }
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// A driver-specific parameter value.
///
/// The core stores and forwards these; it never interprets them.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ExtensionValue {
    /// Free text (a wallet path, a cipher list, a vendor option name).
    Text(String),
    /// A secret (a wallet password); redacted in `Debug`.
    Secret(Secret),
    /// A whole number.
    Integer(i64),
    /// An on/off switch.
    Flag(bool),
}

/// A keyed bag of driver-specific parameters, opaque to the core.
///
/// Keys are ordered so that `Debug` output and any future serialization are
/// deterministic.
#[derive(Debug, Clone, Default)]
pub struct Extensions {
    entries: BTreeMap<Box<str>, ExtensionValue>,
}

impl Extensions {
    /// An empty bag.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets a key, replacing any previous value.
    pub fn set(&mut self, key: impl Into<Box<str>>, value: ExtensionValue) -> &mut Self {
        self.entries.insert(key.into(), value);
        self
    }

    /// Looks up a key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&ExtensionValue> {
        self.entries.get(key)
    }

    /// Whether the bag is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates keys and values in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &ExtensionValue)> {
        self.entries.iter().map(|(key, value)| (&**key, value))
    }
}

/// Where the database is and how it is named.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Endpoint {
    /// A host, a port, and a service or database name the driver resolves.
    HostPort {
        /// Host name or address.
        host: String,
        /// TCP port.
        port: u16,
        /// Service name, database name or SID, as the driver interprets it.
        service: String,
    },
    /// A complete vendor connect string or descriptor, opaque to the core.
    ConnectString(String),
}

/// How the session authenticates.
///
/// `#[non_exhaustive]`: `SPEC.md` §8 lists external authentication mechanisms
/// (Kerberos, token, wallet/SEPS) that no driver supports yet, and adding one
/// must not be a breaking change.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Credentials {
    /// A user name and password.
    UserPassword {
        /// Database user name.
        username: String,
        /// Password.
        password: Secret,
    },
    /// Authentication handled outside Reldex (operating system, wallet, or
    /// another external mechanism the driver supports).
    External,
}

/// The administrative role a session connects with (`SPEC.md` §8, "privileged
/// connections where supported").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SessionRole {
    /// An ordinary session.
    #[default]
    Normal,
    /// The highest administrative role the server offers.
    SysDba,
    /// The restricted operator role.
    SysOper,
}

/// Whether the transport is encrypted.
///
/// Certificate, wallet and cipher detail stays in [`Extensions`] until an ADR
/// settles a vendor-neutral shape for it. `#[non_exhaustive]` because native
/// network encryption and mutual TLS are plausible additional modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TlsMode {
    /// Plain TCP.
    Disabled,
    /// TLS is required; the driver must fail rather than fall back.
    Required,
}

/// Everything needed to open one connection.
///
/// `Debug` is safe to log: the password is a [`Secret`].
///
/// ```
/// use reldex_db_driver_api::{ConnectionParams, Credentials, Endpoint, Secret, TlsMode};
///
/// let params = ConnectionParams::new(
///     Endpoint::HostPort {
///         host: "db.example.internal".to_owned(),
///         port: 1521,
///         service: "ORCLPDB1".to_owned(),
///     },
///     Credentials::UserPassword {
///         username: "reldex".to_owned(),
///         password: Secret::new("hunter2"),
///     },
/// )
/// .with_tls(TlsMode::Required);
///
/// assert!(!format!("{params:?}").contains("hunter2"));
/// ```
#[derive(Debug, Clone)]
pub struct ConnectionParams {
    endpoint: Endpoint,
    credentials: Credentials,
    role: SessionRole,
    tls: TlsMode,
    connect_timeout: Option<Duration>,
    extensions: Extensions,
}

impl ConnectionParams {
    /// Builds parameters with an ordinary role, no TLS and no timeout.
    ///
    /// Plain TCP is the default because it is what an unconfigured listener
    /// offers; the profile layer is responsible for requiring TLS where policy
    /// says so.
    #[must_use]
    pub fn new(endpoint: Endpoint, credentials: Credentials) -> Self {
        Self {
            endpoint,
            credentials,
            role: SessionRole::Normal,
            tls: TlsMode::Disabled,
            connect_timeout: None,
            extensions: Extensions::new(),
        }
    }

    /// Sets the administrative role.
    #[must_use]
    pub fn with_role(mut self, role: SessionRole) -> Self {
        self.role = role;
        self
    }

    /// Sets the transport encryption requirement.
    #[must_use]
    pub fn with_tls(mut self, tls: TlsMode) -> Self {
        self.tls = tls;
        self
    }

    /// Sets how long the driver may spend establishing the connection.
    #[must_use]
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    /// Replaces the driver-specific parameter bag.
    #[must_use]
    pub fn with_extensions(mut self, extensions: Extensions) -> Self {
        self.extensions = extensions;
        self
    }

    /// Where the database is.
    #[must_use]
    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// How the session authenticates.
    #[must_use]
    pub const fn credentials(&self) -> &Credentials {
        &self.credentials
    }

    /// The administrative role.
    #[must_use]
    pub const fn role(&self) -> SessionRole {
        self.role
    }

    /// The transport encryption requirement.
    #[must_use]
    pub const fn tls(&self) -> TlsMode {
        self.tls
    }

    /// The connect timeout, if one was set.
    #[must_use]
    pub const fn connect_timeout(&self) -> Option<Duration> {
        self.connect_timeout
    }

    /// The driver-specific parameter bag.
    #[must_use]
    pub const fn extensions(&self) -> &Extensions {
        &self.extensions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ConnectionParams {
        let mut extensions = Extensions::new();
        extensions
            .set(
                "wallet_path",
                ExtensionValue::Text("/opt/wallet".to_owned()),
            )
            .set(
                "wallet_password",
                ExtensionValue::Secret(Secret::new("walletpw")),
            )
            .set("sdu", ExtensionValue::Integer(8192))
            .set("prefer_ipv6", ExtensionValue::Flag(true));

        ConnectionParams::new(
            Endpoint::HostPort {
                host: "db.example.internal".to_owned(),
                port: 1521,
                service: "ORCLPDB1".to_owned(),
            },
            Credentials::UserPassword {
                username: "reldex".to_owned(),
                password: Secret::new("hunter2"),
            },
        )
        .with_tls(TlsMode::Required)
        .with_role(SessionRole::SysDba)
        .with_connect_timeout(Duration::from_secs(5))
        .with_extensions(extensions)
    }

    #[test]
    fn secret_redacts_in_debug_and_has_no_display() {
        let secret = Secret::new("hunter2");
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        assert_eq!(secret.expose(), "hunter2");
        assert!(!secret.is_empty());
        assert!(Secret::new("").is_empty());
    }

    #[test]
    fn debug_of_params_leaks_no_secret() {
        let rendered = format!("{:?}", sample());
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(!rendered.contains("walletpw"), "{rendered}");
        assert!(rendered.contains("Secret(<redacted>)"), "{rendered}");
        // Non-secret detail is still there, which is the point of logging it.
        assert!(rendered.contains("db.example.internal"), "{rendered}");
        assert!(rendered.contains("SysDba"), "{rendered}");
    }

    #[test]
    fn cloning_params_keeps_the_secret_usable_and_redacted() {
        let params = sample();
        let clone = params.clone();
        let Credentials::UserPassword { password, .. } = clone.credentials() else {
            panic!("expected user/password credentials");
        };
        assert_eq!(password.expose(), "hunter2");
        assert!(!format!("{clone:?}").contains("hunter2"));
    }

    #[test]
    fn accessors_report_what_was_set() {
        let params = sample();
        assert_eq!(params.tls(), TlsMode::Required);
        assert_eq!(params.role(), SessionRole::SysDba);
        assert_eq!(params.connect_timeout(), Some(Duration::from_secs(5)));
        match params.endpoint() {
            Endpoint::HostPort {
                host,
                port,
                service,
            } => {
                assert_eq!(host, "db.example.internal");
                assert_eq!(*port, 1521);
                assert_eq!(service, "ORCLPDB1");
            }
            Endpoint::ConnectString(_) => panic!("expected host/port endpoint"),
        }
    }

    #[test]
    fn extensions_are_ordered_and_opaque() {
        let params = sample();
        let keys: Vec<&str> = params.extensions().iter().map(|(key, _)| key).collect();
        assert_eq!(
            keys,
            ["prefer_ipv6", "sdu", "wallet_password", "wallet_path"]
        );
        assert!(matches!(
            params.extensions().get("sdu"),
            Some(ExtensionValue::Integer(8192))
        ));
        assert!(params.extensions().get("missing").is_none());
        assert!(Extensions::new().is_empty());
    }

    #[test]
    fn default_role_is_ordinary() {
        assert_eq!(SessionRole::default(), SessionRole::Normal);
        let params = ConnectionParams::new(
            Endpoint::ConnectString("(DESCRIPTION=...)".to_owned()),
            Credentials::External,
        );
        assert_eq!(params.role(), SessionRole::Normal);
        assert_eq!(params.tls(), TlsMode::Disabled);
        assert!(params.extensions().is_empty());
    }
}
