//! Connection profiles (`SPEC.md` §17).
//!
//! A profile is what the user saved about a database: where it is, how to
//! authenticate, which environment it is, and its transport security. Its
//! session and display options are not fields here — they are the profile's
//! **settings overrides**, stored at [`crate::Level::Profile`] and resolved
//! like any other setting, so "inherited from application" works for them
//! too.
//!
//! # No secret, by construction
//!
//! No type in this module can hold a password. [`Authentication::Password`]
//! records the user name and **whether** the operating system's credential
//! store holds the password ([`PasswordStorage`]); the password itself lives
//! only there, under [`crate::CredentialKey`] — the profile's id — and reaches
//! a connection through [`crate::connection_params`] as a
//! [`reldex_db_driver_api::Secret`] without ever passing through this crate's
//! store. The owner's rule is "no plaintext fallback, ever" (`phase-1.md`
//! §C.3 item 7): with no credential store, the user is prompted each time.
//!
//! The endpoint's free-text fields are the one place a user could still paste
//! a password — a connect string copied from another tool with the logon in
//! front of it. Validation refuses credential-looking text there
//! ([`CredentialPattern`]) before a profile exists, so neither the store nor
//! the connect mapping ever sees it; and `Debug` of a connect string prints
//! only its length.

use std::fmt;
use std::path::PathBuf;

use reldex_db_driver_api::SessionRole;

use crate::ids::{CredentialKey, ProfileId};
use crate::time::UnixTimeMs;

/// The longest text any profile field may hold, in bytes.
///
/// Generous — a multi-address connect descriptor can run to a few kilobytes —
/// and there only so that a paste accident cannot put megabytes into a row.
pub const MAX_FIELD_BYTES: usize = 16 * 1024;

/// The longest profile name, in characters.
pub const MAX_NAME_CHARS: usize = 200;

/// Which kind of database a profile connects to, and so which driver opens
/// it.
///
/// `#[non_exhaustive]`: Oracle Database is the only driver today (`SPEC.md`
/// §7), and the product must be able to add others without a breaking
/// change. Naming the vendor here is the technical-compatibility use
/// `AGENTS.md` allows; nothing vendor-specific *happens* in this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DatabaseType {
    /// Oracle Database, through the thin driver.
    Oracle,
}

/// The environment a database belongs to (`SPEC.md` §17).
///
/// Whether the production indicator shows (M3.4) is the profile's own
/// [`ProfileDetails::treat_as_production`] flag, not this enum: it is always
/// set for [`Environment::Production`], never for the other named
/// environments, and the user's choice for [`Environment::Custom`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Environment {
    /// Development.
    Development,
    /// Test.
    Test,
    /// User acceptance testing.
    Uat,
    /// Staging.
    Staging,
    /// Production.
    Production,
    /// A user-named environment. Whether it is treated as production is the
    /// user's explicit choice, never a guess from the label.
    Custom(String),
}

impl Environment {
    /// The [`ProfileDetails::treat_as_production`] value a new profile in this
    /// environment starts with: `true` for production only.
    #[must_use]
    pub const fn production_by_default(&self) -> bool {
        matches!(self, Self::Production)
    }

    /// Whether the user may choose [`ProfileDetails::treat_as_production`]
    /// here: only for a custom environment. Production is always treated as
    /// production, and the other named environments never are.
    #[must_use]
    pub const fn production_flag_is_settable(&self) -> bool {
        matches!(self, Self::Custom(_))
    }
}

/// How a host/port endpoint names the database on that listener.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ServiceTarget {
    /// A service name — what the vendor-neutral `Endpoint::HostPort` carries.
    ServiceName(String),
    /// A system identifier. There is no vendor-neutral way to say it, so the
    /// driver binding builds the endpoint ([`crate::DriverBinding::sid_endpoint`]).
    Sid(String),
}

/// Where the database is (`SPEC.md` §17: "host/port; service/SID/descriptor
/// where applicable").
///
/// `Debug` prints a connect string's length, never its text: it is the one
/// endpoint field whose content this crate cannot vouch for.
#[derive(Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProfileEndpoint {
    /// A host, a port, and the service or SID on that listener.
    HostPort {
        /// Host name or address.
        host: String,
        /// TCP port.
        port: u16,
        /// Service name or SID.
        target: ServiceTarget,
    },
    /// A complete connect descriptor or connect string, passed to the driver
    /// verbatim (`Endpoint::ConnectString`). The driver, not this crate,
    /// decides what it may contain.
    ConnectString(String),
}

impl fmt::Debug for ProfileEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostPort { host, port, target } => f
                .debug_struct("HostPort")
                .field("host", host)
                .field("port", port)
                .field("target", target)
                .finish(),
            Self::ConnectString(text) => {
                write!(f, "ConnectString(<redacted, {} bytes>)", text.len())
            }
        }
    }
}

/// Whether the operating system's credential store holds the password.
///
/// The only thing a profile knows about its password.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PasswordStorage {
    /// The credential store holds it, under [`Profile::credential_key`].
    CredentialStore,
    /// Nothing holds it: the user is asked for it at every connect. This is
    /// also what a machine with no usable credential store gets — never a
    /// plaintext fallback.
    PromptEachTime,
}

/// How the session authenticates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Authentication {
    /// A database user name and a password. The password is never here.
    Password {
        /// Database user name. Not a secret.
        username: String,
        /// Where the password is.
        storage: PasswordStorage,
    },
    /// Authentication by the operating system or another external mechanism
    /// (`Credentials::External`). No driver supports it yet (`SPEC.md` §8);
    /// it is modelled so a profile can say so and the driver can refuse it
    /// plainly.
    External,
}

/// Whether the transport is encrypted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Transport {
    /// Plain TCP.
    Plain,
    /// TLS is required: the driver fails rather than falls back
    /// (`TlsMode::Required`).
    Tls,
}

/// Transport security for one profile (`SPEC.md` §8, spike S8, C-6).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TlsOptions {
    /// Plain or TLS.
    pub transport: Transport,
    /// A directory holding the certificate authority (PEM) to trust in
    /// addition to the public roots — how a private CA is trusted. The layout
    /// inside it is the driver's: the Oracle thin driver reads exactly
    /// `ewallet.pem` there. Verification itself is always on and cannot be
    /// turned off.
    pub ca_directory: Option<PathBuf>,
    /// Open a session even though the connect descriptor asks for a
    /// certificate pin (Oracle's `SSL_SERVER_CERT_DN`) that the driver cannot
    /// enforce — the C-6 guard's opt-out. Off by default: without it the
    /// connect is refused, because the session would be weaker than the
    /// profile configured.
    pub allow_unenforced_certificate_pin: bool,
}

impl Default for TlsOptions {
    fn default() -> Self {
        Self {
            transport: Transport::Plain,
            ca_directory: None,
            allow_unenforced_certificate_pin: false,
        }
    }
}

/// Everything about a profile the user edits.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProfileDetails {
    /// Display name.
    pub name: String,
    /// Which driver opens it.
    pub database: DatabaseType,
    /// Which environment it is.
    pub environment: Environment,
    /// Whether the production indicator shows for this profile (M3.4 reads
    /// this, not [`ProfileDetails::environment`]). Must be `true` for
    /// [`Environment::Production`] and `false` for the other named
    /// environments; the user's choice for [`Environment::Custom`]. See
    /// [`Environment::production_by_default`].
    pub treat_as_production: bool,
    /// Where the database is.
    pub endpoint: ProfileEndpoint,
    /// How the session authenticates.
    pub authentication: Authentication,
    /// Administrative role (`SPEC.md` §8: privileged connections where
    /// supported).
    pub role: SessionRole,
    /// Transport security.
    pub tls: TlsOptions,
}

impl ProfileDetails {
    /// Details with the defaults a new profile starts with: an ordinary role,
    /// plain TCP, and the environment's production default.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        database: DatabaseType,
        environment: Environment,
        endpoint: ProfileEndpoint,
        authentication: Authentication,
    ) -> Self {
        let treat_as_production = environment.production_by_default();
        Self {
            name: name.into(),
            database,
            environment,
            treat_as_production,
            endpoint,
            authentication,
            role: SessionRole::Normal,
            tls: TlsOptions::default(),
        }
    }
}

/// A class of credential-looking text refused in an endpoint field.
///
/// Names the pattern, never the text: the error that carries it is safe to
/// log and to show, and a UI must show it without echoing the field (M3.2).
/// Matching is case-insensitive and ignores all whitespace, so `PASSWORD = x`
/// and `( password=` are caught as well. The rule is deliberately
/// vendor-neutral — a guard against credential-looking text, not a parser of
/// any vendor's syntax — and errs towards refusing: an endpoint that trips it
/// can always be written without the pattern.
///
/// | Pattern | Matches |
/// | --- | --- |
/// | [`WalletPassword`](Self::WalletPassword) | `wallet_password=` anywhere |
/// | [`QueryParameter`](Self::QueryParameter) | `?password=` or `&password=` |
/// | [`PasswordKeyword`](Self::PasswordKeyword) | `password=` anywhere else, e.g. `(PASSWORD=…)` in a descriptor |
/// | [`UserPasswordPrefix`](Self::UserPasswordPrefix) | a `user/password@` prefix: `^[^/@()]+/[^@]+@` |
///
/// Checked in the host, service name, SID and connect string. A driver
/// binding cannot add patterns yet: validation runs before any binding is
/// chosen. A hook on [`crate::DriverBinding`] can be added when a driver
/// needs one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CredentialPattern {
    /// `wallet_password=`.
    WalletPassword,
    /// `?password=` or `&password=`: a password as a query parameter.
    QueryParameter,
    /// `password=` elsewhere, e.g. `(PASSWORD=…)` inside a descriptor.
    PasswordKeyword,
    /// A `user/password@` prefix, the logon form some tools accept in front
    /// of a connect identifier.
    UserPasswordPrefix,
}

impl CredentialPattern {
    /// The first pattern `text` matches, if any. See the type's table.
    #[must_use]
    pub fn find(text: &str) -> Option<Self> {
        let squeezed: String = text
            .chars()
            .filter(|c| !c.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect();
        if squeezed.contains("wallet_password=") {
            return Some(Self::WalletPassword);
        }
        if squeezed.contains("?password=") || squeezed.contains("&password=") {
            return Some(Self::QueryParameter);
        }
        if squeezed.contains("password=") {
            return Some(Self::PasswordKeyword);
        }
        // `^[^/@()]+/[^@]+@`: a non-empty prefix free of `/`, `@` and
        // parentheses, a slash, at least one character that is not `@`, then
        // an `@`.
        let (prefix, rest) = squeezed.split_once('/')?;
        let prefix_ok = !prefix.is_empty() && !prefix.contains(['@', '(', ')']);
        match rest.find('@') {
            Some(at) if prefix_ok && at > 0 => Some(Self::UserPasswordPrefix),
            _ => None,
        }
    }
}

/// Why profile details were refused.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProfileError {
    /// A required text field is empty (or only whitespace, for the name).
    Empty {
        /// Which field.
        field: ProfileField,
    },
    /// A text field is longer than [`MAX_FIELD_BYTES`] (or the name longer
    /// than [`MAX_NAME_CHARS`] characters).
    TooLong {
        /// Which field.
        field: ProfileField,
    },
    /// A text field contains a NUL or another control character.
    ControlCharacter {
        /// Which field.
        field: ProfileField,
    },
    /// Port 0 is not a listener.
    PortZero,
    /// The CA directory path is not valid Unicode, so it cannot be stored
    /// faithfully.
    PathNotUnicode,
    /// An endpoint field contains credential-looking text. Value-free: it
    /// names the field and the pattern, never the text, so a UI can show it
    /// without echoing the field.
    CredentialInEndpoint {
        /// Which field.
        field: ProfileField,
        /// Which pattern it matched.
        pattern: CredentialPattern,
    },
    /// [`ProfileDetails::treat_as_production`] contradicts the environment:
    /// production must show the indicator, and the other named environments
    /// must not.
    ProductionFlagMismatch,
}

/// A profile field, for error reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProfileField {
    /// [`ProfileDetails::name`].
    Name,
    /// The custom environment's label.
    EnvironmentLabel,
    /// The endpoint's host.
    Host,
    /// The endpoint's service name.
    ServiceName,
    /// The endpoint's SID.
    Sid,
    /// The connect string.
    ConnectString,
    /// The user name.
    Username,
    /// The CA directory.
    CaDirectory,
}

impl fmt::Display for ProfileField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Name => "name",
            Self::EnvironmentLabel => "environment label",
            Self::Host => "host",
            Self::ServiceName => "service name",
            Self::Sid => "SID",
            Self::ConnectString => "connect string",
            Self::Username => "user name",
            Self::CaDirectory => "CA directory",
        })
    }
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty { field } => write!(f, "the profile's {field} is empty"),
            Self::TooLong { field } => write!(f, "the profile's {field} is too long"),
            Self::ControlCharacter { field } => {
                write!(f, "the profile's {field} contains a control character")
            }
            Self::PortZero => f.write_str("the profile's port is 0"),
            Self::PathNotUnicode => {
                f.write_str("the profile's CA directory is not a valid Unicode path")
            }
            Self::CredentialInEndpoint { field, pattern } => write!(
                f,
                "the profile's {field} looks like it contains a credential ({pattern:?}); \
                 passwords belong in the operating system's credential store, never in a \
                 profile"
            ),
            Self::ProductionFlagMismatch => f.write_str(
                "the production indicator must be on for a production profile and off for \
                 the other named environments",
            ),
        }
    }
}

impl std::error::Error for ProfileError {}

fn check_text(field: ProfileField, text: &str) -> Result<(), ProfileError> {
    if text.trim().is_empty() {
        return Err(ProfileError::Empty { field });
    }
    if text.len() > MAX_FIELD_BYTES {
        return Err(ProfileError::TooLong { field });
    }
    // A connect descriptor may legitimately span lines; nothing else may hold
    // a control character, and nothing at all may hold a NUL.
    let allows_line_breaks = field == ProfileField::ConnectString;
    if text
        .chars()
        .any(|c| c.is_control() && !(allows_line_breaks && matches!(c, '\n' | '\r' | '\t')))
    {
        return Err(ProfileError::ControlCharacter { field });
    }
    Ok(())
}

/// [`check_text`] for an endpoint field, plus the credential guard.
fn check_endpoint_text(field: ProfileField, text: &str) -> Result<(), ProfileError> {
    check_text(field, text)?;
    match CredentialPattern::find(text) {
        Some(pattern) => Err(ProfileError::CredentialInEndpoint { field, pattern }),
        None => Ok(()),
    }
}

impl ProfileDetails {
    /// Checks every field.
    ///
    /// Deliberately shallow: whether a host resolves or a descriptor parses is
    /// the driver's to say, at connect time, with its own error. This only
    /// refuses what could never be a profile — and credential-looking text in
    /// an endpoint field ([`CredentialPattern`]).
    ///
    /// # Errors
    ///
    /// The first [`ProfileError`] found.
    pub fn validate(&self) -> Result<(), ProfileError> {
        check_text(ProfileField::Name, &self.name)?;
        if self.name.chars().count() > MAX_NAME_CHARS {
            return Err(ProfileError::TooLong {
                field: ProfileField::Name,
            });
        }
        if let Environment::Custom(label) = &self.environment {
            check_text(ProfileField::EnvironmentLabel, label)?;
        }
        if !self.environment.production_flag_is_settable()
            && self.treat_as_production != self.environment.production_by_default()
        {
            return Err(ProfileError::ProductionFlagMismatch);
        }
        match &self.endpoint {
            ProfileEndpoint::HostPort { host, port, target } => {
                check_endpoint_text(ProfileField::Host, host)?;
                if *port == 0 {
                    return Err(ProfileError::PortZero);
                }
                match target {
                    ServiceTarget::ServiceName(name) => {
                        check_endpoint_text(ProfileField::ServiceName, name)?;
                    }
                    ServiceTarget::Sid(sid) => check_endpoint_text(ProfileField::Sid, sid)?,
                }
            }
            ProfileEndpoint::ConnectString(text) => {
                check_endpoint_text(ProfileField::ConnectString, text)?;
            }
        }
        match &self.authentication {
            Authentication::Password { username, .. } => {
                check_text(ProfileField::Username, username)?;
            }
            Authentication::External => {}
        }
        if let Some(directory) = &self.tls.ca_directory {
            let Some(text) = directory.to_str() else {
                return Err(ProfileError::PathNotUnicode);
            };
            check_text(ProfileField::CaDirectory, text)?;
        }
        Ok(())
    }
}

/// A saved connection profile: its details plus an identity and provenance.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Profile {
    id: ProfileId,
    details: ProfileDetails,
    created_at: UnixTimeMs,
    modified_at: UnixTimeMs,
}

impl Profile {
    /// A new profile with a fresh random id, created and modified now.
    ///
    /// # Errors
    ///
    /// [`ProfileError`] if the details do not validate.
    pub fn create(details: ProfileDetails) -> Result<Self, ProfileError> {
        details.validate()?;
        let now = UnixTimeMs::now();
        Ok(Self {
            id: ProfileId::new_random(),
            details,
            created_at: now,
            modified_at: now,
        })
    }

    /// Reassembles a stored profile. Validates, so a row edited outside Reldex
    /// cannot produce a profile `create` would have refused.
    pub(crate) fn from_stored(
        id: ProfileId,
        details: ProfileDetails,
        created_at: UnixTimeMs,
        modified_at: UnixTimeMs,
    ) -> Result<Self, ProfileError> {
        details.validate()?;
        Ok(Self {
            id,
            details,
            created_at,
            modified_at,
        })
    }

    /// Replaces the details and stamps the modification time.
    ///
    /// # Errors
    ///
    /// [`ProfileError`] if the new details do not validate; the profile is
    /// then unchanged.
    pub fn update(&mut self, details: ProfileDetails) -> Result<(), ProfileError> {
        details.validate()?;
        self.details = details;
        self.modified_at = UnixTimeMs::now();
        Ok(())
    }

    /// The profile's id.
    #[must_use]
    pub const fn id(&self) -> ProfileId {
        self.id
    }

    /// The key its password is stored under in the credential store (M2.10):
    /// its id.
    #[must_use]
    pub const fn credential_key(&self) -> CredentialKey {
        CredentialKey::for_profile(self.id)
    }

    /// The editable details.
    #[must_use]
    pub const fn details(&self) -> &ProfileDetails {
        &self.details
    }

    /// Whether the production indicator shows for this profile (M3.4).
    #[must_use]
    pub const fn treat_as_production(&self) -> bool {
        self.details.treat_as_production
    }

    /// When it was created.
    #[must_use]
    pub const fn created_at(&self) -> UnixTimeMs {
        self.created_at
    }

    /// When its details last changed.
    #[must_use]
    pub const fn modified_at(&self) -> UnixTimeMs {
        self.modified_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample() -> ProfileDetails {
        ProfileDetails {
            name: "Orders (dev)".to_owned(),
            database: DatabaseType::Oracle,
            environment: Environment::Development,
            treat_as_production: false,
            endpoint: ProfileEndpoint::HostPort {
                host: "db.example.internal".to_owned(),
                port: 1521,
                target: ServiceTarget::ServiceName("ORDERS".to_owned()),
            },
            authentication: Authentication::Password {
                username: "app_owner".to_owned(),
                storage: PasswordStorage::CredentialStore,
            },
            role: SessionRole::Normal,
            tls: TlsOptions::default(),
        }
    }

    #[test]
    fn create_assigns_an_id_and_timestamps() {
        let profile = Profile::create(sample()).expect("valid");
        assert_eq!(profile.created_at(), profile.modified_at());
        assert_eq!(profile.credential_key().profile(), profile.id());
        assert_ne!(Profile::create(sample()).expect("valid").id(), profile.id());
    }

    #[test]
    fn update_validates_first_and_stamps_the_modification() {
        let mut profile = Profile::create(sample()).expect("valid");
        let before = profile.clone();
        let mut bad = sample();
        bad.name = "   ".to_owned();
        assert_eq!(
            profile.update(bad),
            Err(ProfileError::Empty {
                field: ProfileField::Name
            })
        );
        assert_eq!(profile, before);
        let mut renamed = sample();
        renamed.name = "Orders".to_owned();
        profile.update(renamed).expect("valid");
        assert_eq!(profile.details().name, "Orders");
        assert!(profile.modified_at() >= before.modified_at());
        assert_eq!(profile.created_at(), before.created_at());
    }

    #[test]
    fn validation_refuses_what_could_never_be_a_profile() {
        type Edit = Box<dyn Fn(&mut ProfileDetails)>;
        let cases: Vec<(Edit, ProfileError)> = vec![
            (
                Box::new(|d| d.environment = Environment::Custom(String::new())),
                ProfileError::Empty {
                    field: ProfileField::EnvironmentLabel,
                },
            ),
            (
                Box::new(|d| {
                    d.endpoint = ProfileEndpoint::HostPort {
                        host: "h".to_owned(),
                        port: 0,
                        target: ServiceTarget::Sid("ORCL".to_owned()),
                    };
                }),
                ProfileError::PortZero,
            ),
            (
                Box::new(|d| {
                    d.endpoint = ProfileEndpoint::HostPort {
                        host: "h".to_owned(),
                        port: 1521,
                        target: ServiceTarget::Sid(" ".to_owned()),
                    };
                }),
                ProfileError::Empty {
                    field: ProfileField::Sid,
                },
            ),
            (
                Box::new(|d| d.endpoint = ProfileEndpoint::ConnectString("a\0b".to_owned())),
                ProfileError::ControlCharacter {
                    field: ProfileField::ConnectString,
                },
            ),
            (
                Box::new(|d| {
                    d.endpoint = ProfileEndpoint::ConnectString("x".repeat(MAX_FIELD_BYTES + 1));
                }),
                ProfileError::TooLong {
                    field: ProfileField::ConnectString,
                },
            ),
            (
                Box::new(|d| {
                    d.authentication = Authentication::Password {
                        username: String::new(),
                        storage: PasswordStorage::PromptEachTime,
                    };
                }),
                ProfileError::Empty {
                    field: ProfileField::Username,
                },
            ),
            (
                Box::new(|d| d.name = "n".repeat(MAX_NAME_CHARS + 1)),
                ProfileError::TooLong {
                    field: ProfileField::Name,
                },
            ),
            (
                Box::new(|d| d.name = "tab\there".to_owned()),
                ProfileError::ControlCharacter {
                    field: ProfileField::Name,
                },
            ),
        ];
        for (edit, expected) in cases {
            let mut details = sample();
            edit(&mut details);
            assert_eq!(details.validate(), Err(expected.clone()), "{expected}");
        }
    }

    #[test]
    fn a_multi_line_descriptor_and_thai_text_are_accepted() {
        let mut details = sample();
        details.name = "ฐานข้อมูลคำสั่งซื้อ".to_owned();
        details.endpoint = ProfileEndpoint::ConnectString(
            "(DESCRIPTION=\n  (ADDRESS=(PROTOCOL=TCP)(HOST=h)(PORT=1521))\n  \
             (CONNECT_DATA=(SERVICE_NAME=s)))"
                .to_owned(),
        );
        details.authentication = Authentication::External;
        assert_eq!(details.validate(), Ok(()));
    }

    #[test]
    fn the_production_flag_follows_the_environment_and_is_free_only_for_custom() {
        for (environment, default) in [
            (Environment::Development, false),
            (Environment::Test, false),
            (Environment::Uat, false),
            (Environment::Staging, false),
            (Environment::Production, true),
            (Environment::Custom("Production".to_owned()), false),
        ] {
            assert_eq!(
                environment.production_by_default(),
                default,
                "{environment:?}"
            );
            let details = ProfileDetails::new(
                "p",
                DatabaseType::Oracle,
                environment.clone(),
                sample().endpoint,
                Authentication::External,
            );
            assert_eq!(details.treat_as_production, default);
            assert_eq!(details.validate(), Ok(()));
            let flipped = ProfileDetails {
                treat_as_production: !default,
                ..details
            };
            let expected = if environment.production_flag_is_settable() {
                Ok(())
            } else {
                Err(ProfileError::ProductionFlagMismatch)
            };
            assert_eq!(flipped.validate(), expected, "{environment:?}");
        }
        let mut custom = sample();
        custom.environment = Environment::Custom("DR site".to_owned());
        custom.treat_as_production = true;
        assert!(
            Profile::create(custom)
                .expect("valid")
                .treat_as_production()
        );
    }

    #[test]
    fn each_credential_pattern_is_refused_without_echoing_the_text() {
        let secret = "Hunter2Marker";
        for (text, pattern) in [
            (
                format!("(DESCRIPTION=(ADDRESS=(HOST=h)(PORT=1))(PASSWORD={secret}))"),
                CredentialPattern::PasswordKeyword,
            ),
            (
                format!("( password = {secret} )(CONNECT_DATA=(SERVICE_NAME=s))"),
                CredentialPattern::PasswordKeyword,
            ),
            (
                format!("(SECURITY=(WALLET_PASSWORD={secret}))"),
                CredentialPattern::WalletPassword,
            ),
            (
                format!("tcps://h:2484/s?password={secret}"),
                CredentialPattern::QueryParameter,
            ),
            (
                format!("tcps://h:2484/s?ssl=true&PASSWORD={secret}"),
                CredentialPattern::QueryParameter,
            ),
            (
                format!("scott/{secret}@db.example.internal:1521/ORDERS"),
                CredentialPattern::UserPasswordPrefix,
            ),
            (
                format!("scott / {secret} @ //db:1521/ORDERS"),
                CredentialPattern::UserPasswordPrefix,
            ),
        ] {
            let mut details = sample();
            details.endpoint = ProfileEndpoint::ConnectString(text.clone());
            let error = details.validate().expect_err(&text);
            assert_eq!(
                error,
                ProfileError::CredentialInEndpoint {
                    field: ProfileField::ConnectString,
                    pattern
                }
            );
            assert!(!error.to_string().contains(secret), "{error}");
            assert!(!format!("{error:?}").contains(secret), "{error:?}");
            assert!(Profile::create(details).is_err());
        }
        // The plain-name fields are guarded too.
        let mut host = sample();
        host.endpoint = ProfileEndpoint::HostPort {
            host: format!("scott/{secret}@db"),
            port: 1521,
            target: ServiceTarget::Sid("ORCL".to_owned()),
        };
        assert_eq!(
            host.validate(),
            Err(ProfileError::CredentialInEndpoint {
                field: ProfileField::Host,
                pattern: CredentialPattern::UserPasswordPrefix
            })
        );
        let mut service = sample();
        service.endpoint = ProfileEndpoint::HostPort {
            host: "db".to_owned(),
            port: 1521,
            target: ServiceTarget::ServiceName(format!("ORDERS?password={secret}")),
        };
        assert_eq!(
            service.validate(),
            Err(ProfileError::CredentialInEndpoint {
                field: ProfileField::ServiceName,
                pattern: CredentialPattern::QueryParameter
            })
        );
    }

    #[test]
    fn near_misses_are_not_credentials() {
        for text in [
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=h)(PORT=1521))\
             (CONNECT_DATA=(SERVICE_NAME=PASSWORDS)))",
            "(DESCRIPTION=(ADDRESS=(HOST=h)(PORT=1))(CONNECT_DATA=(SERVICE_NAME=PASSWORD)))",
            "pw.example:1521/ORDERS",
            "pw.example/ORDERS",
            "tcps://db.example.internal:2484/ORDERS",
            "//db:1521/ORDERS",
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=h)(PORT=2484))\
             (SECURITY=(SSL_SERVER_CERT_DN=\"EMAIL=dba@example.com,CN=db\")))",
        ] {
            assert_eq!(CredentialPattern::find(text), None, "{text}");
            let mut details = sample();
            details.endpoint = ProfileEndpoint::ConnectString(text.to_owned());
            assert_eq!(details.validate(), Ok(()), "{text}");
        }
        let mut service = sample();
        service.endpoint = ProfileEndpoint::HostPort {
            host: "pw.example".to_owned(),
            port: 1521,
            target: ServiceTarget::ServiceName("PASSWORDS".to_owned()),
        };
        assert_eq!(service.validate(), Ok(()));
    }

    #[test]
    fn debug_of_a_connect_string_prints_only_its_length() {
        let text = "(DESCRIPTION=(ADDRESS=(HOST=secret-host.internal)(PORT=1521)))";
        let endpoint = ProfileEndpoint::ConnectString(text.to_owned());
        assert_eq!(
            format!("{endpoint:?}"),
            format!("ConnectString(<redacted, {} bytes>)", text.len())
        );
        let mut details = sample();
        details.endpoint = endpoint;
        let profile = Profile::create(details).expect("valid");
        assert!(!format!("{profile:?}").contains("secret-host"));
        assert!(format!("{:?}", sample().endpoint).contains("db.example.internal"));
    }
}
