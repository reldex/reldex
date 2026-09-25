//! Where the password for a connect comes from: the one rule, in one pure
//! function.

use reldex_db_driver_api::Secret;
use reldex_workspace::{Authentication, PasswordStorage, Profile};

use crate::store::{CredentialError, CredentialStore};

/// Where the password for connecting to a profile comes from.
///
/// There are exactly three answers, and no fourth: the password is in the
/// credential store, the user types it, or the profile needs none. No
/// variant carries a password from anywhere else, which is how "absence of a
/// store means prompt each time, never a plaintext fallback" (owner decision
/// 2026-09-20) holds in the type: a caller has nothing to fall back *to*.
/// Deliberately **not** `#[non_exhaustive]`, so a connect flow must handle
/// each case by name and cannot hide one behind a wildcard.
///
/// `Debug` redacts: [`Secret`] prints `Secret(<redacted>)`.
#[derive(Debug)]
#[must_use]
pub enum PasswordSource {
    /// The credential store holds the password. Pass it to
    /// `reldex_workspace::connection_params` as `Some(secret)`.
    FromStore(Secret),
    /// Ask the user, and say why ([`PromptReason`]). Pass what they type as
    /// `Some(secret)`; whether to save it afterwards is the connection
    /// dialog's question, and only when the store can
    /// ([`crate::CredentialStoreKind::can_store`]).
    PromptRequired(PromptReason),
    /// The profile authenticates without a password (external
    /// authentication). Pass `None`.
    NotNeeded,
}

/// Why the user has to type the password this time.
///
/// Value-free, like [`CredentialError`]: safe to log and to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PromptReason {
    /// The profile is set to "prompt each time"; the store is not consulted,
    /// even if it still holds an old entry for the profile.
    PromptEachTime,
    /// There is no usable credential store ([`CredentialError::Unavailable`]):
    /// none on this platform yet, or not from this logon session.
    StoreUnavailable,
    /// The store holds nothing for this profile: never saved, or removed
    /// outside Reldex.
    NotStored,
    /// The store failed. The error says how (access refused, an entry Reldex
    /// did not write, a platform error code), so the prompt can say why the
    /// saved password was not used rather than silently asking again.
    StoreFailed(CredentialError),
}

/// Decides where the password for connecting to `profile` comes from.
///
/// - External authentication → [`PasswordSource::NotNeeded`]; the store is
///   not consulted.
/// - "Prompt each time" → [`PromptReason::PromptEachTime`]; the store is not
///   consulted.
/// - "Saved in the credential store" → one [`CredentialStore::get`] under
///   [`Profile::credential_key`]: the password if it is there, otherwise a
///   prompt with the reason — [`PromptReason::NotStored`],
///   [`PromptReason::StoreUnavailable`], or
///   [`PromptReason::StoreFailed`] for any other failure.
///
/// Every failure becomes a prompt, never an error, because a prompt is always
/// a correct way to connect and nothing else ever is: this function has no
/// error path for a caller to handle differently. It is pure apart from the
/// one store call, so it runs where the store may be called — the workspace
/// service thread, never the UI thread (see [`CredentialStore`]).
pub fn resolve_password(profile: &Profile, store: &dyn CredentialStore) -> PasswordSource {
    match &profile.details().authentication {
        Authentication::Password { storage, .. } => match storage {
            PasswordStorage::CredentialStore => match store.get(&profile.credential_key()) {
                Ok(Some(secret)) => PasswordSource::FromStore(secret),
                // `NotFound` from `get` is a store bending the contract
                // (`Ok(None)` is the answer); it means the same thing.
                Ok(None) | Err(CredentialError::NotFound) => {
                    PasswordSource::PromptRequired(PromptReason::NotStored)
                }
                Err(CredentialError::Unavailable) => {
                    PasswordSource::PromptRequired(PromptReason::StoreUnavailable)
                }
                Err(error) => PasswordSource::PromptRequired(PromptReason::StoreFailed(error)),
            },
            // `PromptEachTime`, and any storage mode added later: the store
            // is read only when the profile says the password is there.
            _ => PasswordSource::PromptRequired(PromptReason::PromptEachTime),
        },
        // `External`, and any authentication added later (`SPEC.md` §8's
        // Kerberos, token, wallet): no password is fetched for it. If a later
        // mechanism does need one, `connection_params` refuses the missing
        // password with `ConnectError::PasswordRequired` — it can never be
        // filled from anywhere but the store or the user.
        _ => PasswordSource::NotNeeded,
    }
}

#[cfg(test)]
mod tests {
    use reldex_workspace::{
        DatabaseType, Environment, ProfileDetails, ProfileEndpoint, ServiceTarget,
    };

    use super::*;
    use crate::memory::MemoryCredentialStore;
    use crate::none::NoCredentialStore;

    const MARKER: &str = "resolve-marker-5c1e";

    fn profile(authentication: Authentication) -> Profile {
        Profile::create(ProfileDetails::new(
            "Orders (dev)",
            DatabaseType::Oracle,
            Environment::Development,
            ProfileEndpoint::HostPort {
                host: "db.example.internal".to_owned(),
                port: 1521,
                target: ServiceTarget::ServiceName("ORDERS".to_owned()),
            },
            authentication,
        ))
        .expect("valid profile")
    }

    fn password(storage: PasswordStorage) -> Authentication {
        Authentication::Password {
            username: "app_owner".to_owned(),
            storage,
        }
    }

    /// What the store looks like when `resolve_password` asks it.
    #[derive(Debug, Clone, Copy)]
    enum StoreState {
        Holds,
        Empty,
        Absent,
        Fails(CredentialError),
    }

    /// The expected outcome, written out independently of the function.
    #[derive(Debug, PartialEq, Eq)]
    enum Expected {
        FromStore,
        Prompt(PromptReason),
        NotNeeded,
    }

    fn outcome(source: &PasswordSource) -> Expected {
        match source {
            PasswordSource::FromStore(secret) => {
                assert_eq!(secret.expose(), MARKER, "the store's own password");
                Expected::FromStore
            }
            PasswordSource::PromptRequired(reason) => Expected::Prompt(*reason),
            PasswordSource::NotNeeded => Expected::NotNeeded,
        }
    }

    /// Runs `resolve_password` for `authentication` against a store in
    /// `state`, and reports the outcome and how many times the store was read.
    fn run(authentication: Authentication, state: StoreState) -> (Expected, usize) {
        let profile = profile(authentication);
        match state {
            StoreState::Absent => (outcome(&resolve_password(&profile, &NoCredentialStore)), 0),
            _ => {
                let store = MemoryCredentialStore::new();
                // A decoy under another profile's key must never be returned.
                store
                    .put(
                        &Profile::create(profile.details().clone())
                            .expect("valid")
                            .credential_key(),
                        &Secret::new("another profile's password"),
                    )
                    .expect("put");
                match state {
                    StoreState::Holds => store
                        .put(&profile.credential_key(), &Secret::new(MARKER))
                        .expect("put"),
                    StoreState::Fails(error) => store.fail_with(Some(error)),
                    StoreState::Empty | StoreState::Absent => {}
                }
                let result = outcome(&resolve_password(&profile, &store));
                (result, store.reads())
            }
        }
    }

    #[test]
    fn truth_table() {
        let states = [
            StoreState::Holds,
            StoreState::Empty,
            StoreState::Absent,
            StoreState::Fails(CredentialError::Unavailable),
            StoreState::Fails(CredentialError::Denied),
            StoreState::Fails(CredentialError::Malformed),
            StoreState::Fails(CredentialError::NotFound),
            StoreState::Fails(CredentialError::TooLarge { max_bytes: 2560 }),
            StoreState::Fails(CredentialError::Backend { code: 1783 }),
        ];
        let mut rows = 0;
        for state in states {
            // Saved in the credential store: the store decides, read once.
            let expected = match state {
                StoreState::Holds => Expected::FromStore,
                StoreState::Empty | StoreState::Fails(CredentialError::NotFound) => {
                    Expected::Prompt(PromptReason::NotStored)
                }
                StoreState::Absent | StoreState::Fails(CredentialError::Unavailable) => {
                    Expected::Prompt(PromptReason::StoreUnavailable)
                }
                StoreState::Fails(error) => Expected::Prompt(PromptReason::StoreFailed(error)),
            };
            let reads = usize::from(!matches!(state, StoreState::Absent));
            assert_eq!(
                run(password(PasswordStorage::CredentialStore), state),
                (expected, reads),
                "saved in the store, store {state:?}"
            );

            // Prompt each time: always a prompt, the store never read — even
            // when it still holds an old password for the profile.
            assert_eq!(
                run(password(PasswordStorage::PromptEachTime), state),
                (Expected::Prompt(PromptReason::PromptEachTime), 0),
                "prompt each time, store {state:?}"
            );

            // External authentication: no password, the store never read.
            assert_eq!(
                run(Authentication::External, state),
                (Expected::NotNeeded, 0),
                "external, store {state:?}"
            );
            rows += 3;
        }
        assert_eq!(rows, 27);
    }

    #[test]
    fn debug_of_a_resolved_password_is_redacted() {
        let profile = profile(password(PasswordStorage::CredentialStore));
        let store = MemoryCredentialStore::new();
        store
            .put(&profile.credential_key(), &Secret::new(MARKER))
            .expect("put");
        let source = resolve_password(&profile, &store);
        let rendered = format!("{source:?}");
        assert_eq!(rendered, "FromStore(Secret(<redacted>))");
        assert!(!rendered.contains(MARKER));
        assert_eq!(
            format!(
                "{:?}",
                PasswordSource::PromptRequired(PromptReason::StoreFailed(
                    CredentialError::Backend { code: 5 }
                ))
            ),
            "PromptRequired(StoreFailed(Backend { code: 5 }))"
        );
    }
}
