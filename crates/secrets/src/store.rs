//! The [`CredentialStore`] contract, the kinds of store, and the one error
//! type every store reports.

use std::fmt;

use reldex_db_driver_api::Secret;
use reldex_workspace::CredentialKey;

/// Where a profile's password is kept between connects: the operating
/// system's secure store, behind one small interface (`SPEC.md` §17,
/// `ARCHITECTURE.md` §9, ADR-0007).
///
/// Keyed by [`CredentialKey`] — the profile's UUID and nothing else — and
/// holding a [`Secret`], which never renders itself in `Debug` and has no
/// `Display`.
///
/// # Blocking and threads
///
/// Every method may block: a platform store is a call into another process
/// (Windows Credential Manager goes through the local security authority),
/// and a future one may show the user a system prompt (macOS Keychain). Call
/// it from the workspace service thread — **never the UI thread**
/// (`ARCHITECTURE.md` §6) — and never from a session's worker thread. A store
/// is `Send + Sync`, so one instance can be shared (`Arc<dyn
/// CredentialStore>`); the platform stores serialise concurrent calls
/// themselves.
///
/// # No fallback
///
/// A store never falls back to keeping a password anywhere else. When there
/// is no usable store, [`CredentialStore::put`] fails with
/// [`CredentialError::Unavailable`] and the password is asked for at every
/// connect (owner decision 2026-09-20, `phase-1.md` §C.3 item 7) — which is
/// what [`crate::resolve_password`] turns every failure into.
pub trait CredentialStore: Send + Sync {
    /// Which backend this is — what a settings screen names and whether it
    /// can save a password at all ([`CredentialStoreKind::can_store`]).
    fn kind(&self) -> CredentialStoreKind;

    /// A short English description of the backend for diagnostics and logs.
    /// Not user-interface text: a UI names the backend from [`Self::kind`]
    /// in its own language. Never contains a key or a secret.
    fn describe(&self) -> &'static str {
        self.kind().describe()
    }

    /// The password stored under `key`, or `Ok(None)` when the store holds
    /// none (never saved, or removed outside Reldex).
    ///
    /// # Errors
    ///
    /// [`CredentialError`]: the store is unavailable or refused access, the
    /// stored entry is not a password Reldex wrote, or the platform failed.
    fn get(&self, key: &CredentialKey) -> Result<Option<Secret>, CredentialError>;

    /// Stores `secret` under `key`, replacing whatever was there.
    ///
    /// # Errors
    ///
    /// [`CredentialError`]: the store is unavailable or refused access; the
    /// secret is larger than this backend accepts
    /// ([`CredentialError::TooLarge`]; the limit is per backend — for
    /// Windows it is `WindowsCredentialManager::MAX_SECRET_BYTES`) or
    /// contains a control character ([`CredentialError::InvalidSecret`]),
    /// and nothing is written; or the platform failed.
    fn put(&self, key: &CredentialKey, secret: &Secret) -> Result<(), CredentialError>;

    /// Removes the password stored under `key`.
    ///
    /// # Errors
    ///
    /// [`CredentialError::NotFound`] when nothing was stored under `key` — a
    /// caller that only wants it gone (deleting a profile, switching it to
    /// "prompt each time") treats that as success — or any other
    /// [`CredentialError`].
    fn delete(&self, key: &CredentialKey) -> Result<(), CredentialError>;
}

/// Which credential store backs a [`CredentialStore`].
///
/// `#[non_exhaustive]`: Apple Keychain, Android Keystore and Linux Secret
/// Service (`SPEC.md` §17) are later tasks, each a new variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CredentialStoreKind {
    /// Windows Credential Manager: generic credentials, persisted for this
    /// user on this machine (`WindowsCredentialManager`, Windows builds only).
    WindowsCredentialManager,
    /// No credential store: this platform has no backend yet, or none is
    /// usable. Passwords are asked for at every connect
    /// ([`crate::NoCredentialStore`]).
    Absent,
    /// An in-process map that forgets everything when the process exits.
    /// Reported only by the `test-support` feature's `MemoryCredentialStore`;
    /// a product build never contains it.
    Memory,
}

impl CredentialStoreKind {
    /// Whether this store can save a password at all. A connection dialog
    /// offers "save password" only when it can; when it cannot, the profile
    /// is saved as "prompt each time".
    #[must_use]
    pub const fn can_store(self) -> bool {
        match self {
            Self::WindowsCredentialManager | Self::Memory => true,
            Self::Absent => false,
        }
    }

    /// A short English description for diagnostics (see
    /// [`CredentialStore::describe`]).
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::WindowsCredentialManager => {
                "Windows Credential Manager (generic credential, this user on this machine)"
            }
            Self::Absent => "no credential store: the password is asked for at every connect",
            Self::Memory => "in-memory credential store (tests only; forgotten at exit)",
        }
    }
}

/// Why a credential store call failed.
///
/// Value-free by construction: no variant holds a key, a target name or a
/// secret — only the class of failure and, for [`CredentialError::Backend`]
/// and [`CredentialError::Locked`], the platform's numeric error code — so
/// `Debug` and `Display` are safe to log and to show. The caller already
/// knows which profile it asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CredentialError {
    /// There is no usable credential store: none on this platform yet
    /// ([`crate::NoCredentialStore`]), or the platform's store cannot be used
    /// from this logon session (Windows `ERROR_NO_SUCH_LOGON_SESSION`, e.g. a
    /// network logon). The password must be asked for.
    Unavailable,
    /// Nothing is stored under the key ([`CredentialStore::delete`] only;
    /// [`CredentialStore::get`] reports absence as `Ok(None)`).
    NotFound,
    /// The store refused access (Windows `ERROR_ACCESS_DENIED` from the
    /// credential call itself).
    Denied,
    /// The secret is larger than this backend accepts; nothing was written.
    TooLarge {
        /// The largest secret the backend accepts, in UTF-8 bytes.
        max_bytes: usize,
    },
    /// The secret contains a control character (U+0000–U+001F), which no
    /// Reldex store keeps (ADR-0007 S8); nothing was written. The password
    /// can still be typed at every connect.
    InvalidSecret,
    /// The stored entry is not a password Reldex wrote: it lacks the
    /// versioned Reldex prefix, carries a version this build does not read,
    /// or its payload is not UTF-8 text free of control characters — for
    /// example an entry created by hand with `cmdkey` under Reldex's name.
    /// Never sent to a database; the entry is left as it is and the user is
    /// asked instead (ADR-0007 S8).
    Malformed,
    /// The lock every Reldex process takes around a store call could not be
    /// taken: another process has held it past the wait (`258`,
    /// `WAIT_TIMEOUT`), or an object of that name refuses access (`5`,
    /// `ERROR_ACCESS_DENIED` — for example one created by another program
    /// with a restrictive security descriptor). Nothing was read or written.
    Locked {
        /// The platform's error code.
        code: i64,
    },
    /// Any other platform failure, with the platform's own error code (a
    /// Win32 error code on Windows), preserved for diagnostics.
    Backend {
        /// The platform's error code.
        code: i64,
    },
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("no credential store is available"),
            Self::NotFound => {
                f.write_str("the credential store holds no password for this profile")
            }
            Self::Denied => f.write_str("the credential store refused access"),
            Self::TooLarge { max_bytes } => write!(
                f,
                "the password is longer than the credential store accepts ({max_bytes} bytes)"
            ),
            Self::InvalidSecret => f.write_str(
                "the password contains a control character, which the credential store does not keep",
            ),
            Self::Malformed => f.write_str(
                "the credential store's entry for this profile is not a password Reldex saved",
            ),
            Self::Locked { code } => write!(
                f,
                "the credential store is locked by another process (platform error {code})"
            ),
            Self::Backend { code } => {
                write!(f, "the credential store failed (platform error {code})")
            }
        }
    }
}

impl std::error::Error for CredentialError {}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_ERRORS: [CredentialError; 8] = [
        CredentialError::Unavailable,
        CredentialError::NotFound,
        CredentialError::Denied,
        CredentialError::TooLarge { max_bytes: 2555 },
        CredentialError::InvalidSecret,
        CredentialError::Malformed,
        CredentialError::Locked { code: 258 },
        CredentialError::Backend { code: 1783 },
    ];

    #[test]
    fn every_error_renders_without_a_value() {
        // The renderings are asserted exactly, so nothing a caller could
        // mistake for a secret, a key or a target name can be in them.
        let rendered: Vec<(String, String)> = ALL_ERRORS
            .iter()
            .map(|error| (format!("{error:?}"), error.to_string()))
            .collect();
        assert_eq!(
            rendered,
            [
                ("Unavailable", "no credential store is available"),
                (
                    "NotFound",
                    "the credential store holds no password for this profile"
                ),
                ("Denied", "the credential store refused access"),
                (
                    "TooLarge { max_bytes: 2555 }",
                    "the password is longer than the credential store accepts (2555 bytes)"
                ),
                (
                    "InvalidSecret",
                    "the password contains a control character, which the credential store does not keep"
                ),
                (
                    "Malformed",
                    "the credential store's entry for this profile is not a password Reldex saved"
                ),
                (
                    "Locked { code: 258 }",
                    "the credential store is locked by another process (platform error 258)"
                ),
                (
                    "Backend { code: 1783 }",
                    "the credential store failed (platform error 1783)"
                ),
            ]
            .map(|(debug, display)| (debug.to_owned(), display.to_owned()))
        );
    }

    #[test]
    fn kinds_say_whether_they_can_store_and_describe_themselves() {
        assert!(CredentialStoreKind::WindowsCredentialManager.can_store());
        assert!(CredentialStoreKind::Memory.can_store());
        assert!(!CredentialStoreKind::Absent.can_store());
        for kind in [
            CredentialStoreKind::WindowsCredentialManager,
            CredentialStoreKind::Absent,
            CredentialStoreKind::Memory,
        ] {
            assert!(!kind.describe().is_empty());
        }
    }
}
