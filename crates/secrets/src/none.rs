//! The "absent" backend: what a platform without a credential store gets.

use reldex_db_driver_api::Secret;
use reldex_workspace::CredentialKey;

use crate::store::{CredentialError, CredentialStore, CredentialStoreKind};

/// The credential store of a platform that has none (yet): it keeps nothing
/// and refuses to.
///
/// Every call fails with [`CredentialError::Unavailable`], so
/// [`crate::resolve_password`] answers "ask the user" for every profile and a
/// connection dialog does not offer to save the password
/// ([`CredentialStoreKind::can_store`] is `false`). There is deliberately no
/// other behaviour to choose: "no store" means **prompt each time**, never a
/// plaintext fallback (owner decision 2026-09-20, `phase-1.md` §C.3 item 7).
///
/// [`crate::platform_default`] returns this on every platform but Windows
/// until its own backend lands: Apple Keychain (macOS, iOS), Android Keystore
/// and Linux Secret Service are later tasks (`SPEC.md` §17).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct NoCredentialStore;

impl CredentialStore for NoCredentialStore {
    fn kind(&self) -> CredentialStoreKind {
        CredentialStoreKind::Absent
    }

    fn get(&self, _key: &CredentialKey) -> Result<Option<Secret>, CredentialError> {
        Err(CredentialError::Unavailable)
    }

    fn put(&self, _key: &CredentialKey, _secret: &Secret) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable)
    }

    fn delete(&self, _key: &CredentialKey) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use reldex_workspace::ProfileId;

    use super::*;

    #[test]
    fn it_keeps_nothing_and_says_so() {
        let store = NoCredentialStore;
        let key = CredentialKey::for_profile(ProfileId::new_random());
        assert_eq!(store.kind(), CredentialStoreKind::Absent);
        assert!(!store.kind().can_store());
        assert_eq!(
            store.put(&key, &Secret::new("never kept")),
            Err(CredentialError::Unavailable)
        );
        assert_eq!(
            store.get(&key).map(|found| found.is_some()),
            Err(CredentialError::Unavailable)
        );
        assert_eq!(store.delete(&key), Err(CredentialError::Unavailable));
        assert_eq!(store.describe(), CredentialStoreKind::Absent.describe());
    }
}
