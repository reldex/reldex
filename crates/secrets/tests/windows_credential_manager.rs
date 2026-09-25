//! Real round trips through the Windows Credential Manager (M2.10
//! acceptance: "password round-trips through Credential Manager keyed by
//! profile UUID").
//!
//! Every test writes under fresh random profile UUIDs, so tests running in
//! parallel — and anything else in the user's credential set — never collide,
//! and each removes what it wrote however it ends ([`Cleanup`]). The
//! Credential Manager is a per-user operating-system service, not a database:
//! these run in the ordinary `cargo test` suite, on this machine and on the
//! Windows CI runner.
//!
//! `cmdkey` (in `System32` on every Windows) is used as an independent
//! witness that an entry really is in the Credential Manager under the
//! documented target name — it lists target names and never prints a
//! password.
//!
//! One test, [`an_entry_written_by_another_tool_is_malformed_and_prompts`],
//! writes through `cmdkey` itself — a second process calling `CredWriteW`
//! directly, outside `WindowsCredentialManager`'s own per-user lock
//! (ADR-0007 S4). The default test harness runs every `#[test]` in this
//! binary concurrently, so left alone, that unlocked write can land in the
//! middle of another test's locked calls and resurrect a just-deleted entry
//! or drop a write — the "residual" race the lock does not cover, because
//! it cannot cover callers outside Reldex. That is a real limitation of the
//! platform, disclosed in the ADR; it is not what the other tests are
//! proving, and self-inflicting it made
//! `concurrent_reldex_processes_lose_no_update` fail once on CI (2026-09-25,
//! run 36163250370: `assertion failed:
//! STORE.get(key).expect("get").is_none()` inside a worker process, timed
//! to land while `cmdkey` was mid-write). [`FOREIGN_WRITE_ISOLATION`] keeps
//! that one test's unlocked write from overlapping any other test's locked
//! calls.

#![cfg(windows)]

use std::collections::HashSet;
use std::process::{Command, Stdio};
use std::sync::{RwLock, RwLockReadGuard};

use reldex_secrets::{
    CredentialError, CredentialKey, CredentialStore, CredentialStoreKind, PasswordSource,
    PromptReason, Secret, WindowsCredentialManager, resolve_password,
};
use reldex_workspace::{
    Authentication, DatabaseType, Environment, PasswordStorage, Profile, ProfileDetails,
    ProfileEndpoint, ProfileId, ServiceTarget,
};

const STORE: WindowsCredentialManager = WindowsCredentialManager::new();
const MAX_SECRET_BYTES: usize = WindowsCredentialManager::MAX_SECRET_BYTES;

/// In-process isolation between this suite's ordinary tests (many readers,
/// safe to run together: `WindowsCredentialManager`'s own per-user lock
/// already serializes their real Credential Manager calls against each
/// other and against other Reldex processes) and
/// [`an_entry_written_by_another_tool_is_malformed_and_prompts`]'s `cmdkey`
/// writes (the sole writer, which must run alone). See the module doc
/// comment for why this exists.
static FOREIGN_WRITE_ISOLATION: RwLock<()> = RwLock::new(());

/// Takes the shared side of [`FOREIGN_WRITE_ISOLATION`] for the rest of the
/// caller's scope. Every test that talks to the real store, other than the
/// foreign-write test itself, takes this first.
fn isolated() -> RwLockReadGuard<'static, ()> {
    FOREIGN_WRITE_ISOLATION
        .read()
        .expect("isolation lock poisoned")
}

/// Deletes the given keys however the test ends; "not found" is fine.
struct Cleanup(Vec<CredentialKey>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for key in &self.0 {
            let _ = STORE.delete(key);
        }
    }
}

fn fresh_key() -> CredentialKey {
    CredentialKey::for_profile(ProfileId::new_random())
}

/// The target names `cmdkey` lists under Reldex's namespace.
fn listed_targets() -> HashSet<String> {
    let output = Command::new("cmdkey")
        .arg("/list:Reldex/profile/*")
        .output()
        .expect("cmdkey runs");
    assert!(output.status.success(), "cmdkey /list failed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().strip_prefix("Target: "))
        .map(|target| target.trim().to_owned())
        .collect()
}

#[test]
fn a_password_round_trips_under_the_profiles_uuid() {
    let _isolation = isolated();
    let key = fresh_key();
    let _cleanup = Cleanup(vec![key]);
    let target = WindowsCredentialManager::target_name(&key);
    assert_eq!(target, format!("Reldex/profile/{}", key.profile()));

    assert!(STORE.get(&key).expect("get before put").is_none());
    STORE
        .put(&key, &Secret::new("round-trip-marker-91d2"))
        .expect("put");
    assert!(
        listed_targets().contains(&target),
        "cmdkey sees the entry under {target}"
    );
    assert_eq!(
        STORE.get(&key).expect("get").expect("present").expose(),
        "round-trip-marker-91d2"
    );

    STORE.delete(&key).expect("delete");
    assert!(STORE.get(&key).expect("get after delete").is_none());
    assert!(
        !listed_targets().contains(&target),
        "cmdkey no longer sees {target}"
    );
}

#[test]
fn unicode_empty_and_long_passwords_round_trip_byte_exact() {
    let _isolation = isolated();
    let key = fresh_key();
    let _cleanup = Cleanup(vec![key]);
    let cases = [
        // Thai, with a combining vowel and tone mark, and a non-BMP emoji.
        "รหัสผ่าน-ที่ปลอดภัย-🔐".to_owned(),
        String::new(),
        "p".repeat(512),
        // 512 bytes of three-byte characters.
        "ก".repeat(512 / 3),
        // The largest the store accepts.
        "m".repeat(MAX_SECRET_BYTES),
    ];
    for password in cases {
        STORE
            .put(&key, &Secret::new(password.clone()))
            .expect("put");
        let found = STORE.get(&key).expect("get").expect("present");
        assert_eq!(
            found.expose().as_bytes(),
            password.as_bytes(),
            "byte-exact round trip of {} bytes",
            password.len()
        );
    }
}

#[test]
fn a_password_over_the_limit_is_refused_and_nothing_is_written() {
    let _isolation = isolated();
    // 2,560 bytes of blob less the 5-byte `RLDX` v1 prefix.
    assert_eq!(MAX_SECRET_BYTES, 2555);
    let key = fresh_key();
    let _cleanup = Cleanup(vec![key]);

    let too_long = Secret::new("z".repeat(MAX_SECRET_BYTES + 1));
    assert_eq!(
        STORE.put(&key, &too_long),
        Err(CredentialError::TooLarge {
            max_bytes: MAX_SECRET_BYTES
        })
    );
    assert!(STORE.get(&key).expect("get").is_none(), "nothing written");

    STORE.put(&key, &Secret::new("kept")).expect("put");
    assert!(STORE.put(&key, &too_long).is_err());
    assert_eq!(
        STORE.get(&key).expect("get").expect("present").expose(),
        "kept",
        "a refused overwrite leaves the old password"
    );
}

#[test]
fn put_replaces_and_other_profiles_are_untouched() {
    let _isolation = isolated();
    let (a, b) = (fresh_key(), fresh_key());
    let _cleanup = Cleanup(vec![a, b]);
    STORE.put(&a, &Secret::new("a-first")).expect("put a");
    STORE.put(&b, &Secret::new("b-only")).expect("put b");
    STORE
        .put(&a, &Secret::new("a-second"))
        .expect("overwrite a");
    assert_eq!(STORE.get(&a).expect("get").expect("a").expose(), "a-second");
    assert_eq!(STORE.get(&b).expect("get").expect("b").expose(), "b-only");

    STORE.delete(&a).expect("delete a");
    assert!(STORE.get(&a).expect("get").is_none());
    assert_eq!(STORE.get(&b).expect("get").expect("b").expose(), "b-only");
}

#[test]
fn deleting_nothing_is_not_found() {
    let _isolation = isolated();
    assert_eq!(STORE.delete(&fresh_key()), Err(CredentialError::NotFound));
}

/// A profile whose password is "saved in the credential store".
fn saved_password_profile() -> Profile {
    Profile::create(ProfileDetails::new(
        "Orders (test)",
        DatabaseType::Oracle,
        Environment::Test,
        ProfileEndpoint::HostPort {
            host: "db.example.internal".to_owned(),
            port: 1521,
            target: ServiceTarget::ServiceName("ORDERS".to_owned()),
        },
        Authentication::Password {
            username: "app_owner".to_owned(),
            storage: PasswordStorage::CredentialStore,
        },
    ))
    .expect("valid profile")
}

/// Writes a generic credential under `target` the way a user would by hand.
/// `cmdkey` stores the password as UTF-16LE.
fn cmdkey_generic(target: &str, password: &str) {
    let output = Command::new("cmdkey")
        .arg(format!("/generic:{target}"))
        .arg("/user:someone")
        .arg(format!("/pass:{password}"))
        .output()
        .expect("cmdkey runs");
    assert!(output.status.success(), "cmdkey /generic failed");
}

/// The review's must-fix: an entry Reldex did not write — here `cmdkey`'s
/// UTF-16 of "Hunter2x" (valid UTF-8 with NULs) and of Thai "กข" (bytes
/// `01 0E 02 0E`, valid UTF-8 without a NUL) — must never be handed to a
/// database as the password, where a wrong password counts towards the
/// account's failed-login limit. It is `Malformed`, and the connect flow
/// asks the user.
///
/// Each case uses a profile of its own and removes the entry with Reldex's
/// own `delete`. The test deliberately does not `put` over a `cmdkey` entry:
/// `cmdkey` saves with enterprise (roaming) persistence, and writing a
/// local-machine entry over it leaves the roaming copy behind, which
/// reappears after a later delete once the process exits (measured: 5 of 6
/// runs; ADR-0007 S4 "Residual"). Overwriting a Reldex entry is covered by
/// the other tests.
#[test]
fn an_entry_written_by_another_tool_is_malformed_and_prompts() {
    let _isolation = FOREIGN_WRITE_ISOLATION
        .write()
        .expect("isolation lock poisoned");
    for foreign in ["Hunter2x", "กข"] {
        let profile = saved_password_profile();
        let key = profile.credential_key();
        let _cleanup = Cleanup(vec![key]);
        let target = WindowsCredentialManager::target_name(&key);

        cmdkey_generic(&target, foreign);
        assert!(listed_targets().contains(&target), "cmdkey wrote {target}");
        assert_eq!(
            STORE.get(&key).map(|found| found.is_some()),
            Err(CredentialError::Malformed),
            "{foreign}"
        );
        assert!(
            matches!(
                resolve_password(&profile, &STORE),
                PasswordSource::PromptRequired(PromptReason::StoreFailed(
                    CredentialError::Malformed
                ))
            ),
            "{foreign}"
        );
        STORE
            .delete(&key)
            .expect("Reldex's delete removes a foreign entry too");
        assert!(!listed_targets().contains(&target), "{target} removed");
    }
}

#[test]
fn a_password_with_a_control_character_is_refused_and_nothing_is_written() {
    let _isolation = isolated();
    let key = fresh_key();
    let _cleanup = Cleanup(vec![key]);
    assert_eq!(
        STORE.put(&key, &Secret::new("tab\there")),
        Err(CredentialError::InvalidSecret)
    );
    assert!(STORE.get(&key).expect("get").is_none(), "nothing written");
}

#[test]
fn nothing_the_store_returns_renders_a_secret() {
    let _isolation = isolated();
    let key = fresh_key();
    let _cleanup = Cleanup(vec![key]);
    STORE
        .put(&key, &Secret::new("debug-marker-44ab"))
        .expect("put");
    let found = STORE.get(&key);
    let rendered = format!("{found:?} {STORE:?} {}", STORE.describe());
    assert_eq!(
        rendered,
        "Ok(Some(Secret(<redacted>))) WindowsCredentialManager \
         Windows Credential Manager (generic credential, this user on this machine)"
    );
    assert_eq!(STORE.kind(), CredentialStoreKind::WindowsCredentialManager);
}

#[test]
fn resolve_password_uses_the_real_store_and_prompts_once_it_is_gone() {
    let _isolation = isolated();
    let profile = saved_password_profile();
    let key = profile.credential_key();
    let _cleanup = Cleanup(vec![key]);

    assert!(matches!(
        resolve_password(&profile, &STORE),
        PasswordSource::PromptRequired(PromptReason::NotStored)
    ));
    STORE
        .put(&key, &Secret::new("resolve-marker-0e7c"))
        .expect("put");
    match resolve_password(&profile, &STORE) {
        PasswordSource::FromStore(secret) => assert_eq!(secret.expose(), "resolve-marker-0e7c"),
        other => panic!("expected the stored password, got {other:?}"),
    }
    STORE.delete(&key).expect("delete");
    assert!(matches!(
        resolve_password(&profile, &STORE),
        PasswordSource::PromptRequired(PromptReason::NotStored)
    ));
}

/// Environment variable naming the keys a worker process cycles through.
const WORKER_KEYS: &str = "RELDEX_SECRETS_TEST_WORKER_KEYS";

/// The worker half of the next test. Does nothing unless that test started
/// this process with [`WORKER_KEYS`] set: then it writes, reads back and
/// deletes each key, on two threads.
#[test]
fn worker_process_for_the_concurrency_test() {
    let Ok(keys) = std::env::var(WORKER_KEYS) else {
        return;
    };
    let keys: Vec<CredentialKey> = keys
        .split(',')
        .map(|text| CredentialKey::for_profile(ProfileId::parse(text).expect("uuid")))
        .collect();
    let (first, second) = keys.split_at(keys.len() / 2);
    std::thread::scope(|scope| {
        for half in [first, second] {
            scope.spawn(move || {
                for key in half {
                    let value = format!("worker-{}", key.profile());
                    STORE.put(key, &Secret::new(value.clone())).expect("put");
                    let found = STORE.get(key).expect("get").expect("present");
                    assert_eq!(found.expose(), value);
                    STORE.delete(key).expect("delete");
                    assert!(STORE.get(key).expect("get").is_none());
                }
            });
        }
    });
}

/// Measured before the fix (ADR-0007 S4): with local-machine persistence,
/// concurrent writers of *different* targets lose updates. A read straight
/// after a delete is clean, yet the deleted entry reappears later, while the
/// writer is still running; across processes a just-written entry can also
/// read back as absent (18 of 600 cycles left behind with two processes).
/// The store now holds a per-user named mutex around every call. Two
/// processes of two threads each, 60 keys apiece, must lose nothing: every
/// read-back succeeds in the workers and, after both have exited, no key is
/// left behind.
#[test]
fn concurrent_reldex_processes_lose_no_update() {
    let _isolation = isolated();
    let per_process = 60;
    let batches: Vec<Vec<CredentialKey>> = (0..2)
        .map(|_| (0..per_process).map(|_| fresh_key()).collect())
        .collect();
    let _cleanup = Cleanup(batches.iter().flatten().copied().collect());

    let exe = std::env::current_exe().expect("test binary path");
    let children: Vec<_> = batches
        .iter()
        .map(|batch| {
            let keys: Vec<String> = batch.iter().map(|key| key.profile().to_string()).collect();
            Command::new(&exe)
                .args([
                    "--exact",
                    "worker_process_for_the_concurrency_test",
                    "--test-threads=1",
                ])
                .env(WORKER_KEYS, keys.join(","))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn worker")
        })
        .collect();
    for child in children {
        let output = child.wait_with_output().expect("worker exits");
        assert!(output.status.success(), "a worker failed: {output:?}");
    }

    let listed = listed_targets();
    let left: Vec<String> = batches
        .iter()
        .flatten()
        .map(WindowsCredentialManager::target_name)
        .filter(|target| listed.contains(target))
        .collect();
    assert!(left.is_empty(), "entries came back after delete: {left:?}");
    for key in batches.iter().flatten() {
        assert!(STORE.get(key).expect("get").is_none());
    }
}
