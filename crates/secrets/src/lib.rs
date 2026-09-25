//! Reldex credential storage: where a connection profile's password lives
//! between connects (`SPEC.md` §17, `ARCHITECTURE.md` §9 and §13 item 9).
//!
//! Decision record: `docs/decisions/0007-credential-store.md`.
//!
//! # What is here
//!
//! - [`CredentialStore`] — the one small, typed interface every platform
//!   store implements: [`get`](CredentialStore::get),
//!   [`put`](CredentialStore::put) and [`delete`](CredentialStore::delete) a
//!   [`Secret`] under a [`CredentialKey`] (the profile's UUID), plus
//!   [`kind`](CredentialStore::kind) and
//!   [`describe`](CredentialStore::describe) so a settings screen can say
//!   which backend is in use. Errors are [`CredentialError`], which never
//!   carries a value.
//! - `WindowsCredentialManager` (Windows only) — generic credentials under
//!   `Reldex/profile/<uuid>`, persisted for this user on this machine.
//! - [`NoCredentialStore`] — what every other platform gets for now: it keeps
//!   nothing, so the password is asked for at every connect.
//! - [`platform_default`] — the right one of the two for this build.
//! - [`resolve_password`] — the rule a connect flow must follow, as a
//!   function: [`PasswordSource::FromStore`], [`PasswordSource::PromptRequired`]
//!   with a [`PromptReason`], or [`PasswordSource::NotNeeded`].
//! - `MemoryCredentialStore` (`test-support` feature) — an in-process store
//!   for the core's and the UI's tests.
//!
//! # No plaintext, ever
//!
//! Owner decision 2026-09-20 (`phase-1.md` §C.3 item 7): a password is kept
//! only in the operating system's secure store; when there is none, the user
//! is prompted each time. Nothing in this crate writes a password anywhere
//! else — not to a file, not to SQLite (`reldex-workspace` has no column for
//! one, ADR-0006 P7), not to a log. [`Secret`] has no `Display` and a
//! redacting `Debug`; every error and every type here is value-free in
//! `Debug` and `Display`; and the transient buffers this crate handles are
//! wiped with `zeroize`, as is [`Secret`] itself on drop (ADR-0007 S6). The
//! wipe is hygiene, not a guarantee that no copy survives in memory — see
//! [`Secret`].
//!
//! # Threading
//!
//! Every store call may block (it is a call into the operating system's
//! security service). Make them on the workspace service thread, never on
//! the UI thread (`ARCHITECTURE.md` §6). Stores are `Send + Sync`.
//!
//! # Boundaries
//!
//! Vendor- and UI-neutral. This crate depends on `reldex-workspace` (for
//! [`CredentialKey`] and [`reldex_workspace::Profile`]), on
//! `reldex-db-driver-api` (for [`Secret`]), on `zeroize`, and — on Windows
//! only — on `windows-sys`; nothing below it depends on it
//! (`tests/dependency_rules.rs`). The composition root (`crates/ffi`, M2.11)
//! owns the store instance and hands the password to
//! `reldex_workspace::connection_params`.
//!
//! Apple Keychain, Android Keystore, iOS Keychain and Linux Secret Service
//! are later tasks; until each lands, [`platform_default`] returns
//! [`NoCredentialStore`] there.

mod none;
mod resolve;
mod store;

#[cfg(any(test, feature = "test-support"))]
mod memory;
#[cfg(windows)]
mod wincred;

pub use reldex_db_driver_api::Secret;
pub use reldex_workspace::CredentialKey;

#[cfg(any(test, feature = "test-support"))]
pub use memory::MemoryCredentialStore;
pub use none::NoCredentialStore;
pub use resolve::{PasswordSource, PromptReason, resolve_password};
pub use store::{CredentialError, CredentialStore, CredentialStoreKind};
#[cfg(windows)]
pub use wincred::{COMMENT, MAX_SECRET_BYTES, WindowsCredentialManager};

/// The credential store for this build's platform: the Windows Credential
/// Manager on Windows, [`NoCredentialStore`] everywhere else.
///
/// "Everywhere else" is an honest gap, not a choice: Apple Keychain (macOS,
/// iOS), Android Keystore and Linux Secret Service (`SPEC.md` §17) are later
/// tasks. Until each lands, that platform prompts for the password at every
/// connect — never a plaintext fallback.
///
/// The composition root calls this once and shares the result (for
/// example as an `Arc<dyn CredentialStore>` built with `Arc::from`).
#[must_use]
pub fn platform_default() -> Box<dyn CredentialStore> {
    #[cfg(windows)]
    {
        Box::new(WindowsCredentialManager::new())
    }
    #[cfg(not(windows))]
    {
        Box::new(NoCredentialStore)
    }
}

#[cfg(test)]
mod tests {
    use reldex_workspace::ProfileId;

    use super::*;

    /// The trait's contract, checked against any store that can hold a
    /// password. Uses fresh random keys and removes what it wrote.
    pub(crate) fn contract(store: &dyn CredentialStore) {
        /// Removes both keys however the check ends, so a failed assertion
        /// against a real platform store leaves nothing behind.
        struct Cleanup<'a>(&'a dyn CredentialStore, [CredentialKey; 2]);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                for key in &self.1 {
                    let _ = self.0.delete(key);
                }
            }
        }

        assert!(store.kind().can_store());
        let key = CredentialKey::for_profile(ProfileId::new_random());
        let other = CredentialKey::for_profile(ProfileId::new_random());
        let _cleanup = Cleanup(store, [key, other]);

        assert!(store.get(&key).expect("get of a new key").is_none());
        assert_eq!(store.delete(&key), Err(CredentialError::NotFound));

        for password in [
            "correct horse battery staple".to_owned(),
            "รหัสผ่านภาษาไทย-ทดสอบ-🔐".to_owned(),
            String::new(),
            "x".repeat(512),
        ] {
            store
                .put(&key, &Secret::new(password.clone()))
                .expect("put");
            let found = store.get(&key).expect("get").expect("present");
            assert_eq!(found.expose(), password, "round trip");
            assert!(store.get(&other).expect("get other").is_none(), "isolated");
        }

        store.put(&key, &Secret::new("first")).expect("put");
        store.put(&key, &Secret::new("second")).expect("overwrite");
        assert_eq!(
            store.get(&key).expect("get").expect("present").expose(),
            "second"
        );

        store.delete(&key).expect("delete");
        assert!(store.get(&key).expect("get after delete").is_none());
        assert_eq!(store.delete(&key), Err(CredentialError::NotFound));
    }

    #[test]
    fn the_platform_default_is_the_platform_store_or_none() {
        let store = platform_default();
        if cfg!(windows) {
            assert_eq!(store.kind(), CredentialStoreKind::WindowsCredentialManager);
            assert!(store.kind().can_store());
        } else {
            assert_eq!(store.kind(), CredentialStoreKind::Absent);
            assert!(!store.kind().can_store());
        }
        assert_eq!(store.describe(), store.kind().describe());
    }

    #[cfg(windows)]
    #[test]
    fn the_windows_store_honours_the_store_contract() {
        contract(&WindowsCredentialManager::new());
    }

    #[test]
    fn stores_are_send_and_sync() {
        fn check<T: Send + Sync + ?Sized>() {}
        check::<dyn CredentialStore>();
        check::<NoCredentialStore>();
        check::<MemoryCredentialStore>();
        #[cfg(windows)]
        check::<WindowsCredentialManager>();
    }
}
