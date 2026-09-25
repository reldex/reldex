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

/// The environment a database belongs to (`SPEC.md` §17). Production gets a
/// persistent visual indicator (M3.4).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    /// A user-named environment. Not treated as production, whatever it is
    /// called: the indicator must not depend on guessing from a label.
    Custom(String),
}

impl Environment {
    /// Whether the production indicator applies.
    #[must_use]
    pub const fn is_production(&self) -> bool {
        matches!(self, Self::Production)
    }
}

/// How a host/port endpoint names the database on that listener.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ServiceTarget {
    /// A service name — what the vendor-neutral `Endpoint::HostPort` carries.
    ServiceName(String),
    /// A system identifier. There is no vendor-neutral way to say it, so the
    /// driver binding builds the endpoint ([`crate::DriverBinding::sid_endpoint`]).
    Sid(String),
}

/// Where the database is (`SPEC.md` §17: "host/port; service/SID/descriptor
/// where applicable").
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

/// Whether the operating system's credential store holds the password.
///
/// The only thing a profile knows about its password.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

impl ProfileDetails {
    /// Checks every field.
    ///
    /// Deliberately shallow: whether a host resolves or a descriptor parses is
    /// the driver's to say, at connect time, with its own error. This only
    /// refuses what could never be a profile.
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
        match &self.endpoint {
            ProfileEndpoint::HostPort { host, port, target } => {
                check_text(ProfileField::Host, host)?;
                if *port == 0 {
                    return Err(ProfileError::PortZero);
                }
                match target {
                    ServiceTarget::ServiceName(name) => {
                        check_text(ProfileField::ServiceName, name)?;
                    }
                    ServiceTarget::Sid(sid) => check_text(ProfileField::Sid, sid)?,
                }
            }
            ProfileEndpoint::ConnectString(text) => {
                check_text(ProfileField::ConnectString, text)?;
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
    fn only_production_gets_the_indicator() {
        assert!(Environment::Production.is_production());
        for other in [
            Environment::Development,
            Environment::Test,
            Environment::Uat,
            Environment::Staging,
            Environment::Custom("Production".to_owned()),
        ] {
            assert!(!other.is_production(), "{other:?}");
        }
    }
}
