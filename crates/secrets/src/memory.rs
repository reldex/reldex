//! An in-process credential store for tests (`test-support` feature).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use reldex_db_driver_api::Secret;
use reldex_workspace::CredentialKey;

use crate::store::{CredentialError, CredentialStore, CredentialStoreKind};

/// A [`CredentialStore`] that keeps passwords in a map for the life of the
/// process — for the core's and the UI's tests, which must exercise every
/// [`crate::resolve_password`] outcome without touching the machine's real
/// credential store. Only in `cfg(test)` and the `test-support` feature; a
/// product build never contains it.
///
/// It follows the trait's contract exactly — `get` of an absent key is
/// `Ok(None)`, `delete` of one is [`CredentialError::NotFound`], `put`
/// replaces — and adds two test hooks: [`MemoryCredentialStore::fail_with`]
/// makes every call fail with a chosen error, and
/// [`MemoryCredentialStore::reads`] counts `get` calls, so a test can prove a
/// store was *not* consulted. `Debug` prints only how many entries it holds.
#[derive(Default)]
pub struct MemoryCredentialStore {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    entries: HashMap<CredentialKey, Secret>,
    failure: Option<CredentialError>,
    reads: usize,
}

impl MemoryCredentialStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// While `Some`, every call — `get`, `put` and `delete` — fails with this
    /// error and changes nothing; `None` restores normal behaviour.
    pub fn fail_with(&self, failure: Option<CredentialError>) {
        self.lock().failure = failure;
    }

    /// How many entries it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Whether it holds no entry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many times [`CredentialStore::get`] has been called, failed calls
    /// included.
    #[must_use]
    pub fn reads(&self) -> usize {
        self.lock().reads
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A panicking test thread must not turn every later call into a
        // second panic; the map itself is never left half-updated.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for MemoryCredentialStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryCredentialStore")
            .field("entries", &self.len())
            .finish_non_exhaustive()
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn kind(&self) -> CredentialStoreKind {
        CredentialStoreKind::Memory
    }

    fn get(&self, key: &CredentialKey) -> Result<Option<Secret>, CredentialError> {
        let mut state = self.lock();
        state.reads += 1;
        if let Some(failure) = state.failure {
            return Err(failure);
        }
        Ok(state.entries.get(key).cloned())
    }

    fn put(&self, key: &CredentialKey, secret: &Secret) -> Result<(), CredentialError> {
        let mut state = self.lock();
        if let Some(failure) = state.failure {
            return Err(failure);
        }
        state.entries.insert(*key, secret.clone());
        Ok(())
    }

    fn delete(&self, key: &CredentialKey) -> Result<(), CredentialError> {
        let mut state = self.lock();
        if let Some(failure) = state.failure {
            return Err(failure);
        }
        state
            .entries
            .remove(key)
            .map(drop)
            .ok_or(CredentialError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use reldex_workspace::ProfileId;

    use super::*;
    use crate::tests::contract;

    #[test]
    fn it_honours_the_store_contract() {
        contract(&MemoryCredentialStore::new());
    }

    #[test]
    fn a_failure_is_reported_by_every_call_and_changes_nothing() {
        let store = MemoryCredentialStore::new();
        let key = CredentialKey::for_profile(ProfileId::new_random());
        store.put(&key, &Secret::new("kept")).expect("put");

        store.fail_with(Some(CredentialError::Denied));
        assert_eq!(
            store.get(&key).map(|found| found.is_some()),
            Err(CredentialError::Denied)
        );
        assert_eq!(
            store.put(&key, &Secret::new("replaced")),
            Err(CredentialError::Denied)
        );
        assert_eq!(store.delete(&key), Err(CredentialError::Denied));

        store.fail_with(None);
        assert_eq!(
            store.get(&key).expect("get").expect("still there").expose(),
            "kept"
        );
        assert_eq!(store.reads(), 2);
    }

    #[test]
    fn debug_shows_a_count_and_never_a_secret() {
        let store = MemoryCredentialStore::new();
        store
            .put(
                &CredentialKey::for_profile(ProfileId::new_random()),
                &Secret::new("marker-7f3a"),
            )
            .expect("put");
        let rendered = format!("{store:?}");
        assert_eq!(rendered, "MemoryCredentialStore { entries: 1, .. }");
        assert!(!rendered.contains("marker-7f3a"));
    }
}
