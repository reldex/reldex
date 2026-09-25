//! Two connections on one file: the WAL reader/writer contract, a writer
//! that finds the lock held, and two opens racing to create the schema.
//!
//! No timing upper bound anywhere: a test waits for an event, never for a
//! duration. The busy timeouts that are long exist only so a broken lock
//! fails the test instead of hanging it.

mod support;

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use reldex_db_driver_api::SessionRole;
use reldex_workspace::settings::FETCH_ROWS;
use reldex_workspace::store::SCHEMA_VERSION;
use reldex_workspace::{
    Authentication, DatabaseType, Environment, PasswordStorage, Profile, ProfileDetails,
    ProfileEndpoint, Scope, Store, StoreError, StoreOptions, TlsOptions,
};
use rusqlite::Connection;
use support::TempDir;

const PATIENT: Duration = Duration::from_secs(60);

fn details(name: &str) -> ProfileDetails {
    ProfileDetails {
        name: name.to_owned(),
        database: DatabaseType::Oracle,
        environment: Environment::Test,
        treat_as_production: false,
        endpoint: ProfileEndpoint::ConnectString("db.example.internal:1521/ORDERS".to_owned()),
        authentication: Authentication::Password {
            username: "app".to_owned(),
            storage: PasswordStorage::PromptEachTime,
        },
        role: SessionRole::Normal,
        tls: TlsOptions::default(),
    }
}

fn impatient() -> StoreOptions {
    StoreOptions::default().with_busy_timeout(Duration::ZERO)
}

/// Holds the file's write lock the way another Reldex process would while
/// it is in the middle of a write.
fn hold_write_lock(path: &std::path::Path) -> Connection {
    let raw = Connection::open(path).expect("raw open");
    raw.execute_batch(
        "BEGIN IMMEDIATE; \
         INSERT INTO setting (scope, scope_id, setting_key, kind, int_value, updated_at) \
         VALUES ('application', '', 'results.fetch_rows', 'count', 999, 0);",
    )
    .expect("take the write lock");
    raw
}

#[test]
fn a_reader_is_not_blocked_by_a_writer_and_sees_only_committed_data() {
    let dir = TempDir::new("reader");
    let mut writer = Store::open(dir.store_path()).expect("open");
    let profile = Profile::create(details("committed")).expect("valid");
    writer.insert_profile(&profile).expect("insert");

    let lock = hold_write_lock(&dir.store_path());
    let reader = Store::open_with(dir.store_path(), impatient()).expect("open while locked");
    // WAL: reading needs no lock the writer holds, even with no patience.
    let seen = reader.profiles().expect("read while locked");
    assert_eq!(seen.value, vec![profile]);
    assert!(
        reader
            .application_settings()
            .expect("read")
            .value
            .is_empty(),
        "the uncommitted write is invisible"
    );
    lock.execute_batch("COMMIT").expect("commit");
    assert_eq!(
        reader
            .application_settings()
            .expect("read")
            .value
            .get(FETCH_ROWS),
        Some(999)
    );
}

#[test]
fn a_writer_that_finds_the_lock_held_gets_busy_and_writes_nothing() {
    let dir = TempDir::new("busy");
    let mut store = Store::open_with(dir.store_path(), impatient()).expect("open");
    let lock = hold_write_lock(&dir.store_path());

    assert!(matches!(
        store.put_setting(Scope::Application, FETCH_ROWS, 5),
        Err(StoreError::Busy)
    ));
    let profile = Profile::create(details("blocked")).expect("valid");
    assert!(matches!(
        store.insert_profile(&profile),
        Err(StoreError::Busy)
    ));

    lock.execute_batch("ROLLBACK").expect("release");
    drop(lock);
    // Nothing from the refused writes, nothing from the rolled-back one.
    assert!(store.profiles().expect("read").value.is_empty());
    assert!(store.application_settings().expect("read").value.is_empty());
    // And the same handle writes once the lock is free.
    store
        .put_setting(Scope::Application, FETCH_ROWS, 5)
        .expect("write after release");
    store
        .insert_profile(&profile)
        .expect("insert after release");
}

#[test]
fn a_patient_writer_waits_for_the_lock_and_then_succeeds() {
    let dir = TempDir::new("patient");
    // Opened before the lock is taken: the store moves to the waiter's thread
    // the way it moves to the workspace service thread in the product.
    let mut store = Store::open_with(
        dir.store_path(),
        StoreOptions::default().with_busy_timeout(PATIENT),
    )
    .expect("open");
    let lock = hold_write_lock(&dir.store_path());
    let started = Arc::new(Barrier::new(2));

    let signal = Arc::clone(&started);
    let waiter = thread::spawn(move || {
        signal.wait();
        store.put_setting(Scope::Application, FETCH_ROWS, 7)
    });
    started.wait();
    // Whether the waiter reached its write before or after this commit, it
    // must succeed — and it must land after the held write, not beside it.
    lock.execute_batch("COMMIT").expect("commit");
    waiter
        .join()
        .expect("waiter thread")
        .expect("the patient write succeeds");

    let store = Store::open(dir.store_path()).expect("reopen");
    assert_eq!(
        store
            .application_settings()
            .expect("read")
            .value
            .get(FETCH_ROWS),
        Some(7),
        "the later write wins"
    );
}

#[test]
fn an_exclusively_locked_file_is_busy_at_open_not_a_panic() {
    let dir = TempDir::new("exclusive");
    Store::open(dir.store_path()).expect("create");
    let raw = Connection::open(dir.store_path()).expect("raw open");
    raw.execute_batch(
        "PRAGMA locking_mode = EXCLUSIVE; BEGIN EXCLUSIVE; \
         INSERT INTO setting (scope, scope_id, setting_key, kind, int_value, updated_at) \
         VALUES ('application', '', 'results.fetch_rows', 'count', 1, 0);",
    )
    .expect("exclusive lock");
    assert!(matches!(
        Store::open_with(dir.store_path(), impatient()),
        Err(StoreError::Busy)
    ));
    raw.execute_batch("ROLLBACK").expect("release");
}

#[test]
fn two_opens_racing_on_a_new_file_both_succeed_and_migrate_once() {
    // Four, not two: more chances per run for the interleavings that matter
    // (a header read straddling another open's commit; two WAL conversions at
    // once), both of which failed an earlier version of `Store::open`.
    const RACERS: usize = 4;
    let dir = TempDir::new("race");
    let barrier = Arc::new(Barrier::new(RACERS));
    let handles: Vec<_> = (0..RACERS)
        .map(|_| {
            let path = dir.store_path();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                Store::open_with(&path, StoreOptions::default().with_busy_timeout(PATIENT))
                    .map(|store| store.migration())
            })
        })
        .collect();
    let migrations: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread").expect("open"))
        .collect();
    // Exactly one of them created the schema; every other one found it.
    let created = migrations.iter().filter(|m| m.from == 0).count();
    assert_eq!(created, 1, "{migrations:?}");
    assert!(migrations.iter().all(|m| m.to == SCHEMA_VERSION));

    let store = Store::open(dir.store_path()).expect("reopen");
    store.check_integrity().expect("intact");
}
