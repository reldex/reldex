//! "No secret is ever written to SQLite" (`phase-1.md` M2.9), proved on the
//! bytes of a real file.
//!
//! A marker password goes the way a real one will once M2.10 lands: into a
//! credential store (a stub here, keyed by the profile's credential key),
//! out again at connect time, and into `ConnectionParams` through
//! `connection_params`. Meanwhile the profile and its settings go through
//! every store write there is. The file and its WAL are then searched for
//! the marker — while the store is open, and again after it closed — in both
//! UTF-8 and UTF-16. A positive control (a marker in the profile's *name*,
//! which must be stored) proves the search would find text that is there.

mod support;

use std::collections::HashMap;
use std::path::PathBuf;

use reldex_db_driver_api::{Credentials, Secret, SessionRole};
use reldex_workspace::settings::{
    CONNECT_TIMEOUT, FETCH_ROWS, SERVER_OUTPUT_ENABLED, STATEMENT_TIME_LIMIT, TimeLimit,
};
use reldex_workspace::{
    Authentication, ConnectSettings, CredentialKey, DatabaseType, Environment, PasswordStorage,
    Profile, ProfileDetails, ProfileEndpoint, ResolveContext, Scope, ServiceTarget, Store,
    TlsOptions, Transport, WorksheetId, connection_params,
};
use support::oracle_binding::OracleThinBinding;
use support::{TempDir, bytes_of, contains, store_files};

const SECRET_MARKER: &str = "RldxSecretMarker-7f3c2a9e-must-never-be-stored";
const NAME_MARKER: &str = "RldxNameMarker-5b1d0c44";

/// Stands in for the M2.10 credential store: keyed by profile id, holds a
/// `Secret`, and is the only place the password ever lives.
#[derive(Default)]
struct StubCredentialStore {
    entries: HashMap<CredentialKey, Secret>,
}

impl StubCredentialStore {
    fn put(&mut self, key: CredentialKey, secret: Secret) {
        self.entries.insert(key, secret);
    }

    fn get(&self, key: CredentialKey) -> Option<Secret> {
        self.entries.get(&key).cloned()
    }
}

fn utf16le(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn utf16be(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_be_bytes).collect()
}

/// Every encoding SQLite could have stored the text in.
fn encodings(text: &str) -> [Vec<u8>; 3] {
    [text.as_bytes().to_vec(), utf16le(text), utf16be(text)]
}

fn store_bytes(path: &std::path::Path) -> Vec<u8> {
    store_files(path)
        .iter()
        .flat_map(|file| bytes_of(file))
        .collect()
}

#[test]
fn a_password_used_for_a_connection_never_reaches_the_sqlite_file() {
    let dir = TempDir::new("no-secrets");
    let mut credentials = StubCredentialStore::default();

    let mut store = Store::open(dir.store_path()).expect("open");
    let mut profile = Profile::create(ProfileDetails {
        name: format!("orders {NAME_MARKER}"),
        database: DatabaseType::Oracle,
        environment: Environment::Production,
        endpoint: ProfileEndpoint::HostPort {
            host: "db.example.internal".to_owned(),
            port: 2484,
            target: ServiceTarget::Sid("ORCL".to_owned()),
        },
        authentication: Authentication::Password {
            username: "app_owner".to_owned(),
            storage: PasswordStorage::CredentialStore,
        },
        role: SessionRole::Normal,
        tls: TlsOptions {
            transport: Transport::Tls,
            ca_directory: Some(PathBuf::from("/etc/reldex/ca")),
            allow_unenforced_certificate_pin: false,
        },
    })
    .expect("valid");
    credentials.put(profile.credential_key(), Secret::new(SECRET_MARKER));

    // Every write the store has: insert, update, settings at all three
    // levels, a clear, and a re-read.
    store.insert_profile(&profile).expect("insert");
    let mut edited = profile.details().clone();
    edited.environment = Environment::Staging;
    profile.update(edited).expect("valid");
    store.update_profile(&profile).expect("update");
    store
        .put_setting(Scope::Application, FETCH_ROWS, 500)
        .expect("put");
    store
        .put_setting(
            Scope::Profile(profile.id()),
            CONNECT_TIMEOUT,
            TimeLimit::NoLimit,
        )
        .expect("put");
    let worksheet = WorksheetId::new_random();
    store
        .put_setting(Scope::Worksheet(worksheet), SERVER_OUTPUT_ENABLED, true)
        .expect("put");
    store
        .put_setting(
            Scope::Worksheet(worksheet),
            STATEMENT_TIME_LIMIT,
            TimeLimit::NoLimit,
        )
        .expect("put");
    store
        .clear_setting(Scope::Application, FETCH_ROWS.id())
        .expect("clear");
    let reloaded = store.profile(profile.id()).expect("read").expect("present");
    let app = store.application_settings().expect("load").value;
    let prof = store.profile_settings(profile.id()).expect("load").value;

    // The connect path, exactly as the product will run it.
    let password = credentials.get(reloaded.credential_key());
    let settings = ConnectSettings::resolve(
        &ResolveContext::new()
            .with_application(&app)
            .with_profile(&prof),
    );
    let params =
        connection_params(&reloaded, &settings, password, &OracleThinBinding).expect("maps");
    let Credentials::UserPassword { password, .. } = params.credentials() else {
        panic!("expected user/password credentials");
    };
    assert_eq!(
        password.expose(),
        SECRET_MARKER,
        "the marker really was used for the connection"
    );
    assert!(!format!("{params:?}").contains(SECRET_MARKER));
    assert!(!format!("{reloaded:?}").contains(SECRET_MARKER));
    assert!(!format!("{store:?}").contains(SECRET_MARKER));

    // While open: the data may still be in the WAL, so search both.
    let open_bytes = store_bytes(&dir.store_path());
    for needle in encodings(SECRET_MARKER) {
        assert!(
            !contains(&open_bytes, &needle),
            "secret found in the open store"
        );
    }
    assert!(
        contains(&open_bytes, NAME_MARKER.as_bytes()),
        "positive control: the stored name must be found"
    );

    // After close: checkpointed into the main file.
    drop(store);
    let closed_bytes = store_bytes(&dir.store_path());
    for needle in encodings(SECRET_MARKER) {
        assert!(
            !contains(&closed_bytes, &needle),
            "secret found in the closed store"
        );
    }
    assert!(
        contains(&closed_bytes, NAME_MARKER.as_bytes()),
        "positive control: the stored name must be found"
    );
}
