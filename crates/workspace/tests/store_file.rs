//! The store against real files: WAL mode, reopening an existing file,
//! migration and refusal, corruption and permission failures. No database
//! server, no network.

mod support;

use reldex_db_driver_api::SessionRole;
use reldex_workspace::settings::{FETCH_ROWS, STATEMENT_TIME_LIMIT, TimeLimit};
use reldex_workspace::store::{APPLICATION_ID, Migrated, SCHEMA_VERSION};
use reldex_workspace::{
    Authentication, DatabaseType, Environment, HistoryEntry, HistoryOutcome, HistoryPage,
    PasswordStorage, Profile, ProfileDetails, ProfileEndpoint, ProfileId, Scope, ServiceTarget,
    Store, StoreError, TlsOptions, UnixTimeMs,
};
use rusqlite::Connection;
use support::{TempDir, bytes_of};

fn details(name: &str) -> ProfileDetails {
    ProfileDetails {
        name: name.to_owned(),
        database: DatabaseType::Oracle,
        environment: Environment::Production,
        treat_as_production: true,
        endpoint: ProfileEndpoint::HostPort {
            host: "db.example.internal".to_owned(),
            port: 1521,
            target: ServiceTarget::ServiceName("ORDERS".to_owned()),
        },
        authentication: Authentication::Password {
            username: "app".to_owned(),
            storage: PasswordStorage::CredentialStore,
        },
        role: SessionRole::Normal,
        tls: TlsOptions::default(),
    }
}

/// A v1 store with one profile and two settings, closed.
fn populated(dir: &TempDir) -> Profile {
    let mut store = Store::open(dir.store_path()).expect("open");
    let profile = Profile::create(details("orders")).expect("valid");
    store.insert_profile(&profile).expect("insert");
    store
        .put_setting(Scope::Application, FETCH_ROWS, 250)
        .expect("put");
    store
        .put_setting(
            Scope::Profile(profile.id()),
            STATEMENT_TIME_LIMIT,
            TimeLimit::NoLimit,
        )
        .expect("put");
    profile
}

#[test]
fn a_new_file_is_created_in_wal_mode_at_the_current_version() {
    let dir = TempDir::new("new");
    let store = Store::open(dir.store_path()).expect("open");
    assert!(store.is_wal());
    assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
    assert_eq!(
        store.migration(),
        Migrated {
            from: 0,
            to: SCHEMA_VERSION
        }
    );
    assert_eq!(store.path(), Some(dir.store_path().as_path()));
    drop(store);

    let raw = Connection::open(dir.store_path()).expect("raw open");
    let application_id: i32 = raw
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .expect("pragma");
    let journal: String = raw
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .expect("pragma");
    assert_eq!(application_id, APPLICATION_ID);
    assert_eq!(journal.to_ascii_lowercase(), "wal");
}

#[test]
fn a_v1_file_reopens_unchanged_with_its_data() {
    let dir = TempDir::new("reopen");
    let profile = populated(&dir);
    let before = bytes_of(&dir.store_path());
    assert!(!before.is_empty());

    let store = Store::open(dir.store_path()).expect("reopen");
    assert_eq!(
        store.migration(),
        Migrated {
            from: SCHEMA_VERSION,
            to: SCHEMA_VERSION
        }
    );
    assert_eq!(
        store.profile(profile.id()).expect("read"),
        Some(profile.clone())
    );
    assert_eq!(
        store
            .application_settings()
            .expect("load")
            .value
            .get(FETCH_ROWS),
        Some(250)
    );
    assert_eq!(
        store
            .profile_settings(profile.id())
            .expect("load")
            .value
            .get(STATEMENT_TIME_LIMIT),
        Some(TimeLimit::NoLimit)
    );
    store.check_integrity().expect("intact");
    drop(store);

    // Opening and reading wrote nothing: not the schema, not the header.
    assert_eq!(bytes_of(&dir.store_path()), before);
}

#[test]
fn a_genuine_v1_file_migrates_forward_keeping_its_data_and_gains_the_new_tables() {
    // A real schema-1 file: built with today's `Store` (which always writes
    // the *current* schema) and then reduced to exactly what schema 1 ever
    // had — the tables M4.10/M6.2 added, dropped, and the header's own
    // version pragma set back to 1 — rather than hand-written DDL, so the
    // profile/setting rows are guaranteed the shape the real schema-1 code
    // produced, not an approximation of it.
    let dir = TempDir::new("v1-real");
    let profile = populated(&dir);
    {
        let raw = Connection::open(dir.store_path()).expect("raw open");
        raw.execute_batch(
            "DROP TABLE layout; DROP TABLE worksheet_setting; DROP TABLE history; \
             DROP TABLE worksheet;",
        )
        .expect("drop the v2/v3 tables");
        raw.pragma_update(None, "user_version", 1u32)
            .expect("pragma");
    }

    let mut store = Store::open(dir.store_path()).expect("open the reduced v1 file");
    assert_eq!(
        store.migration(),
        Migrated {
            from: 1,
            to: SCHEMA_VERSION
        }
    );
    assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
    // The v1 data survived the migration untouched.
    assert_eq!(
        store.profile(profile.id()).expect("read"),
        Some(profile.clone())
    );
    assert_eq!(
        store
            .application_settings()
            .expect("load")
            .value
            .get(FETCH_ROWS),
        Some(250)
    );
    // And the migrated file now supports M4.10/M6.2, not just schema 1's own
    // tables.
    store
        .record_history(HistoryEntry {
            profile: profile.id(),
            executed_at: UnixTimeMs::now(),
            statement: "select 1 from dual".to_owned(),
            outcome: HistoryOutcome::Succeeded,
            elapsed_ms: 3,
            row_count: Some(1),
        })
        .expect("history works on a migrated file");
    assert_eq!(
        store
            .history(profile.id(), HistoryPage::first(10))
            .expect("history")
            .len(),
        1
    );
}

#[test]
fn a_file_from_a_newer_reldex_is_refused_and_left_untouched() {
    let dir = TempDir::new("newer");
    populated(&dir);
    {
        let raw = Connection::open(dir.store_path()).expect("raw open");
        raw.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .expect("pragma");
    }
    let before = bytes_of(&dir.store_path());

    match Store::open(dir.store_path()) {
        Err(StoreError::NewerSchema { found, supported }) => {
            assert_eq!(found, SCHEMA_VERSION + 1);
            assert_eq!(supported, SCHEMA_VERSION);
        }
        other => panic!("expected NewerSchema, got {other:?}"),
    }
    assert_eq!(bytes_of(&dir.store_path()), before, "never downgraded");
}

#[test]
fn a_foreign_sqlite_file_is_refused_and_left_untouched() {
    let dir = TempDir::new("foreign");
    {
        let raw = Connection::open(dir.store_path()).expect("raw open");
        raw.execute_batch(
            "CREATE TABLE their_data (x INTEGER); INSERT INTO their_data VALUES (1);",
        )
        .expect("create");
    }
    let before = bytes_of(&dir.store_path());
    assert!(matches!(
        Store::open(dir.store_path()),
        Err(StoreError::NotAReldexStore)
    ));
    assert_eq!(bytes_of(&dir.store_path()), before);
}

#[test]
fn a_file_that_is_not_a_database_is_a_typed_error_and_left_untouched() {
    let dir = TempDir::new("garbage");
    let garbage: Vec<u8> = b"this is not an SQLite database, just text. "
        .iter()
        .copied()
        .cycle()
        .take(8192)
        .collect();
    std::fs::write(dir.store_path(), &garbage).expect("write");
    assert!(matches!(
        Store::open(dir.store_path()),
        Err(StoreError::Corrupt { .. })
    ));
    assert_eq!(bytes_of(&dir.store_path()), garbage);
}

#[test]
fn damaged_pages_are_a_typed_error_not_a_panic() {
    let dir = TempDir::new("damaged");
    let profile = populated(&dir);
    let mut bytes = bytes_of(&dir.store_path());
    // Keep page 1 (the header and the schema) and overwrite every page after
    // it, so the tables' own pages are what is damaged.
    let page_size = usize::from(u16::from_be_bytes([bytes[16], bytes[17]]));
    assert!(bytes.len() > page_size, "the store has more than one page");
    for byte in &mut bytes[page_size..] {
        *byte = 0xFF;
    }
    std::fs::write(dir.store_path(), &bytes).expect("write");

    let outcome = Store::open(dir.store_path()).and_then(|store| {
        let integrity = store.check_integrity();
        let profiles = store.profiles().map(|_| ());
        let one = store.profile(profile.id()).map(|_| ());
        let settings = store.application_settings().map(|_| ());
        assert!(
            matches!(integrity, Err(StoreError::Corrupt { .. })),
            "{integrity:?}"
        );
        profiles.and(one).and(settings)
    });
    assert!(
        matches!(outcome, Err(StoreError::Corrupt { .. })),
        "{outcome:?}"
    );
}

#[test]
fn a_directory_that_does_not_exist_cannot_be_opened() {
    let dir = TempDir::new("missing");
    let path = dir
        .path()
        .join("no")
        .join("such")
        .join("dir")
        .join("reldex.sqlite3");
    assert!(matches!(
        Store::open(&path),
        Err(StoreError::CannotOpen { path: reported, .. }) if reported == path
    ));
}

#[test]
fn an_empty_file_is_a_new_store() {
    // A zero-length file is what SQLite itself leaves after creating one and
    // being interrupted before the first write.
    let dir = TempDir::new("empty");
    std::fs::write(dir.store_path(), b"").expect("write");
    let store = Store::open(dir.store_path()).expect("open");
    assert_eq!(store.migration().from, 0);
    assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
}

#[test]
fn an_identified_file_with_tables_but_no_version_is_refused_and_left_untouched() {
    // Reldex's identity, schema version 0, and a table: a header Reldex never
    // writes. Refused before the switch to WAL, which would itself write.
    let dir = TempDir::new("incomplete");
    {
        let raw = Connection::open(dir.store_path()).expect("raw open");
        raw.pragma_update(None, "application_id", APPLICATION_ID)
            .expect("pragma");
        raw.execute_batch("CREATE TABLE profile (id TEXT); INSERT INTO profile VALUES ('x');")
            .expect("create");
    }
    let before = bytes_of(&dir.store_path());
    assert!(matches!(
        Store::open(dir.store_path()),
        Err(StoreError::IncompleteHeader)
    ));
    assert_eq!(bytes_of(&dir.store_path()), before, "left byte-for-byte");
    for suffix in ["-wal", "-shm"] {
        let mut side = dir.store_path().into_os_string();
        side.push(suffix);
        assert!(
            !std::path::Path::new(&side).exists(),
            "no {suffix} file: WAL was never switched on"
        );
    }
}

#[test]
fn tables_that_differ_from_their_version_are_a_schema_mismatch() {
    // Identity and version say "current schema", the tables say otherwise: a
    // file altered outside Reldex. Opening only reads the header; the first
    // statement that touches a missing table or column fails with a typed
    // error. Deliberately missing every table added since schema 1
    // (`history`, `worksheet`, `worksheet_setting`, `layout`), so every kind
    // of store method — not only the profile/setting ones schema 1 always
    // had — meets the same mismatch, whether the underlying SQLite error
    // names a missing column or (for a whole missing table) a bare
    // `SQLITE_ERROR`.
    let dir = TempDir::new("mismatch");
    {
        let raw = Connection::open(dir.store_path()).expect("raw open");
        raw.execute_batch(
            "CREATE TABLE profile (id TEXT); \
             CREATE TABLE setting (scope TEXT, scope_id TEXT, setting_key TEXT);",
        )
        .expect("create");
        raw.pragma_update(None, "application_id", APPLICATION_ID)
            .expect("pragma");
        raw.pragma_update(None, "user_version", SCHEMA_VERSION)
            .expect("pragma");
    }
    let store = Store::open(dir.store_path()).expect("the header is current");
    match store.profiles() {
        Err(StoreError::SchemaMismatch { detail }) => {
            assert!(detail.contains("no such column"), "{detail}");
        }
        other => panic!("expected SchemaMismatch, got {other:?}"),
    }
    assert!(matches!(
        store.application_settings(),
        Err(StoreError::SchemaMismatch { .. })
    ));
    match store.history(ProfileId::new_random(), HistoryPage::first(1)) {
        Err(StoreError::SchemaMismatch { detail }) => {
            assert!(detail.contains("no such table"), "{detail}");
        }
        other => panic!("expected SchemaMismatch, got {other:?}"),
    }
    assert!(matches!(
        store.load_worksheets(),
        Err(StoreError::SchemaMismatch { .. })
    ));
    assert!(matches!(
        store.load_layout(),
        Err(StoreError::SchemaMismatch { .. })
    ));
}

#[test]
fn a_read_only_file_is_a_typed_error_and_nothing_is_written() {
    let dir = TempDir::new("read-only");
    let profile = populated(&dir);
    let set_read_only = |read_only: bool| {
        let mut permissions = std::fs::metadata(dir.store_path())
            .expect("metadata")
            .permissions();
        permissions.set_readonly(read_only);
        std::fs::set_permissions(dir.store_path(), permissions).expect("permissions");
    };
    set_read_only(true);
    let before = bytes_of(&dir.store_path());
    let outcome = Store::open(dir.store_path()).and_then(|mut store| {
        // Reading a read-only store works; the first write is refused.
        assert_eq!(
            store.profile(profile.id()).expect("read"),
            Some(profile.clone())
        );
        store.put_setting(Scope::Application, FETCH_ROWS, 99)
    });
    let after = bytes_of(&dir.store_path());
    set_read_only(false);
    assert!(matches!(outcome, Err(StoreError::ReadOnly)), "{outcome:?}");
    assert_eq!(after, before, "nothing was written");
}
