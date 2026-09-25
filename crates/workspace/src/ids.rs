//! Stable identifiers for things that outlive a process: connection profiles,
//! worksheets, and the credential-store key derived from a profile.
//!
//! All three are random (version 4) UUIDs. A row number would be reused after
//! a delete, and a credential keyed by a reused number would silently attach
//! one profile's password to another; a random UUID is never reused and is not
//! guessable from anything the store holds.
//!
//! The `uuid` crate is an implementation detail: the public surface is
//! [`ProfileId::from_bytes`] / [`ProfileId::as_bytes`] (16 raw bytes, which is
//! what the C ABI will carry — M2.11) and the canonical hyphenated text form
//! through [`std::fmt::Display`] and [`ProfileId::parse`].

use std::fmt;

use uuid::Uuid;

/// Why a byte string or text could not be read as an identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IdError {
    /// The text is not a hyphenated UUID.
    Malformed,
    /// The all-zero ("nil") UUID, which no Reldex identifier ever is.
    Nil,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => f.write_str("not a hyphenated UUID"),
            Self::Nil => f.write_str("the nil UUID is not a valid identifier"),
        }
    }
}

impl std::error::Error for IdError {}

fn parse_uuid(text: &str) -> Result<Uuid, IdError> {
    // Only the canonical 36-character hyphenated form: it is the only form this
    // crate ever writes, so accepting braces, URNs or bare hex would only widen
    // what a hand-edited or foreign file could smuggle in.
    if text.len() != 36 {
        return Err(IdError::Malformed);
    }
    let uuid = Uuid::try_parse(text).map_err(|_| IdError::Malformed)?;
    if uuid.is_nil() {
        return Err(IdError::Nil);
    }
    Ok(uuid)
}

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(Uuid);

        impl $name {
            /// A new, random (version 4) identifier.
            #[must_use]
            pub fn new_random() -> Self {
                Self(Uuid::new_v4())
            }

            /// Reads the canonical hyphenated form, as [`std::fmt::Display`]
            /// writes it. Case-insensitive; the nil UUID is refused.
            ///
            /// # Errors
            ///
            /// [`IdError`] when the text is not a hyphenated UUID or is nil.
            pub fn parse(text: &str) -> Result<Self, IdError> {
                parse_uuid(text).map(Self)
            }

            /// Reads 16 raw bytes (big-endian UUID layout). The nil UUID is
            /// refused.
            ///
            /// # Errors
            ///
            /// [`IdError::Nil`] for sixteen zero bytes.
            pub fn from_bytes(bytes: [u8; 16]) -> Result<Self, IdError> {
                let uuid = Uuid::from_bytes(bytes);
                if uuid.is_nil() {
                    return Err(IdError::Nil);
                }
                Ok(Self(uuid))
            }

            /// The 16 raw bytes (big-endian UUID layout).
            #[must_use]
            pub fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }
        }

        impl fmt::Display for $name {
            /// The canonical lowercase hyphenated form, 36 characters.
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0.hyphenated(), f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0.hyphenated())
            }
        }
    };
}

uuid_id!(
    /// Identifies one connection profile, for its whole life and across
    /// machines (`SPEC.md` §17).
    ///
    /// It is also the credential-store key for the profile's password — see
    /// [`CredentialKey`].
    ProfileId
);

uuid_id!(
    /// Identifies one worksheet, stable across restarts so that a restored
    /// workspace (M6.2) finds its own worksheet-level settings again.
    ///
    /// Distinct from `reldex_db_core::SessionId`: a worksheet outlives any one
    /// session it owns, and a reconnect is a new session for the same
    /// worksheet (`ARCHITECTURE.md` §5).
    WorksheetId
);

/// The key under which the operating system's credential store holds a
/// profile's password: the profile's own id, and nothing else.
///
/// This is the seam M2.10 builds on (`phase-1.md` M2.10: "keyed by profile
/// UUID"). The store in this crate never sees a password: a profile records
/// only whether the credential store holds one
/// ([`crate::PasswordStorage`]); the password itself goes from the credential
/// store straight into [`crate::connection_params`] as a
/// [`reldex_db_driver_api::Secret`]. How the key is spelled inside a platform
/// store (a target-name prefix, a service name) is M2.10's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CredentialKey(ProfileId);

impl CredentialKey {
    /// The key for one profile.
    #[must_use]
    pub const fn for_profile(profile: ProfileId) -> Self {
        Self(profile)
    }

    /// The profile this key belongs to.
    #[must_use]
    pub const fn profile(self) -> ProfileId {
        self.0
    }
}

impl fmt::Display for CredentialKey {
    /// The profile id's canonical text form.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_random_id_round_trips_through_text_and_bytes() {
        let id = ProfileId::new_random();
        let text = id.to_string();
        assert_eq!(text.len(), 36);
        assert_eq!(ProfileId::parse(&text), Ok(id));
        assert_eq!(ProfileId::parse(&text.to_uppercase()), Ok(id));
        assert_eq!(ProfileId::from_bytes(*id.as_bytes()), Ok(id));
    }

    #[test]
    fn random_ids_are_version_4_and_distinct() {
        let a = WorksheetId::new_random();
        let b = WorksheetId::new_random();
        assert_ne!(a, b);
        // Version nibble: the 13th hex digit of the canonical form.
        assert_eq!(a.to_string().as_bytes()[14], b'4');
    }

    #[test]
    fn nil_and_malformed_text_are_refused() {
        assert_eq!(
            ProfileId::parse("00000000-0000-0000-0000-000000000000"),
            Err(IdError::Nil)
        );
        assert_eq!(ProfileId::from_bytes([0; 16]), Err(IdError::Nil));
        for bad in [
            "",
            "not-a-uuid",
            "{6f1c1f7e-3a4b-4c5d-8e9f-0a1b2c3d4e5f}",
            "6f1c1f7e3a4b4c5d8e9f0a1b2c3d4e5f",
            "urn:uuid:6f1c1f7e-3a4b-4c5d-8e9f-0a1b2c3d4e5f",
        ] {
            assert_eq!(ProfileId::parse(bad), Err(IdError::Malformed), "{bad}");
        }
    }

    #[test]
    fn the_credential_key_is_the_profile_id() {
        let id = ProfileId::new_random();
        let key = CredentialKey::for_profile(id);
        assert_eq!(key.profile(), id);
        assert_eq!(key.to_string(), id.to_string());
    }

    #[test]
    fn debug_names_the_type() {
        let id = ProfileId::parse("6f1c1f7e-3a4b-4c5d-8e9f-0a1b2c3d4e5f").expect("valid");
        assert_eq!(
            format!("{id:?}"),
            "ProfileId(6f1c1f7e-3a4b-4c5d-8e9f-0a1b2c3d4e5f)"
        );
    }
}
