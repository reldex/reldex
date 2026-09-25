//! In-memory tests of the store's behaviour. File-level behaviour — WAL,
//! migration of an existing file, corruption, locking, two handles — is in
//! `crates/workspace/tests/`.

use std::num::NonZeroU32;
use std::path::PathBuf;

use reldex_db_driver_api::SessionRole;
use rusqlite::params;

use super::*;
use crate::profile::{
    Authentication, CredentialPattern, DatabaseType, Environment, PasswordStorage, ProfileDetails,
    ProfileEndpoint, ProfileError, ProfileField, ServiceTarget, TlsOptions, Transport,
};
use crate::settings::{
    ByteLimit, CONNECT_TIMEOUT, EntryLimit, FETCH_ROWS, FETCHES_IN_FLIGHT, ResolveContext,
    SERVER_OUTPUT_BUFFER, SERVER_OUTPUT_ENABLED, STATEMENT_TIME_LIMIT, TimeLimit,
};
use crate::worksheet::{Worksheet, WorksheetState};

fn store() -> Store {
    Store::open_in_memory().expect("in-memory store")
}

fn details(name: &str) -> ProfileDetails {
    ProfileDetails {
        name: name.to_owned(),
        database: DatabaseType::Oracle,
        environment: Environment::Staging,
        treat_as_production: false,
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

/// One profile of every shape the model has, so a round trip covers every
/// column and every enumeration word.
fn worksheet_state() -> WorksheetState {
    WorksheetState {
        title: "scratch".to_owned(),
        text: "select 1;".to_owned(),
        caret: 0,
        scroll: 0,
    }
}

fn every_shape() -> Vec<ProfileDetails> {
    let mut out = Vec::new();
    for (index, environment) in [
        Environment::Development,
        Environment::Test,
        Environment::Uat,
        Environment::Staging,
        Environment::Production,
        Environment::Custom("DR site".to_owned()),
    ]
    .into_iter()
    .enumerate()
    {
        let mut d = details(&format!("env {index}"));
        d.treat_as_production = environment.production_by_default();
        d.environment = environment;
        out.push(d);
    }
    let mut custom_production = details("custom production");
    custom_production.environment = Environment::Custom("Live (EU)".to_owned());
    custom_production.treat_as_production = true;
    out.push(custom_production);
    let mut sid = details("sid");
    sid.endpoint = ProfileEndpoint::HostPort {
        host: "10.0.0.5".to_owned(),
        port: 1522,
        target: ServiceTarget::Sid("ORCL".to_owned()),
    };
    sid.role = SessionRole::SysDba;
    out.push(sid);
    let mut descriptor = details("descriptor");
    descriptor.endpoint = ProfileEndpoint::ConnectString(
        "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=h)(PORT=2484))\n\
         (CONNECT_DATA=(SERVICE_NAME=s)))"
            .to_owned(),
    );
    descriptor.tls = TlsOptions {
        transport: Transport::Tls,
        ca_directory: Some(PathBuf::from("/etc/reldex/ca")),
        allow_unenforced_certificate_pin: true,
    };
    descriptor.role = SessionRole::SysOper;
    out.push(descriptor);
    let mut external = details("external");
    external.authentication = Authentication::External;
    out.push(external);
    let mut prompt = details("prompt ทดสอบ");
    prompt.authentication = Authentication::Password {
        username: "ผู้ใช้".to_owned(),
        storage: PasswordStorage::PromptEachTime,
    };
    out.push(prompt);
    out
}

#[test]
fn a_new_store_is_at_the_current_schema_version() {
    let store = store();
    assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
    assert_eq!(
        store.migration(),
        Migrated {
            from: 0,
            to: SCHEMA_VERSION
        }
    );
    store.check_integrity().expect("intact");
    assert!(store.path().is_none());
}

#[test]
fn every_profile_shape_round_trips() {
    let mut store = store();
    let profiles: Vec<Profile> = every_shape()
        .into_iter()
        .map(|d| Profile::create(d).expect("valid"))
        .collect();
    for profile in &profiles {
        store.insert_profile(profile).expect("insert");
    }
    for profile in &profiles {
        assert_eq!(
            store.profile(profile.id()).expect("read").as_ref(),
            Some(profile)
        );
    }
    let loaded = store.profiles().expect("list");
    assert!(loaded.rejected.is_empty(), "{:?}", loaded.rejected);
    assert_eq!(loaded.value.len(), profiles.len());
    let mut names: Vec<&str> = loaded
        .value
        .iter()
        .map(|p| p.details().name.as_str())
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "listed by name");
    names.dedup();
    assert_eq!(names.len(), profiles.len());
}

#[test]
fn insert_refuses_a_duplicate_and_update_refuses_a_stranger() {
    let mut store = store();
    let profile = Profile::create(details("a")).expect("valid");
    store.insert_profile(&profile).expect("insert");
    assert!(matches!(
        store.insert_profile(&profile),
        Err(StoreError::ProfileExists(id)) if id == profile.id()
    ));
    let stranger = Profile::create(details("b")).expect("valid");
    assert!(matches!(
        store.update_profile(&stranger),
        Err(StoreError::ProfileNotFound(id)) if id == stranger.id()
    ));
}

#[test]
fn update_keeps_the_creation_time_and_writes_the_new_details() {
    let mut store = store();
    let mut profile = Profile::create(details("before")).expect("valid");
    store.insert_profile(&profile).expect("insert");
    let mut changed = details("after");
    changed.environment = Environment::Production;
    changed.treat_as_production = true;
    profile.update(changed).expect("valid");
    store.update_profile(&profile).expect("update");
    let stored = store.profile(profile.id()).expect("read").expect("present");
    assert_eq!(stored, profile);
    assert!(stored.treat_as_production());
}

#[test]
fn deleting_a_profile_deletes_its_overrides_and_nothing_else() {
    let mut store = store();
    let keep = Profile::create(details("keep")).expect("valid");
    let gone = Profile::create(details("gone")).expect("valid");
    store.insert_profile(&keep).expect("insert");
    store.insert_profile(&gone).expect("insert");
    for profile in [&keep, &gone] {
        store
            .put_setting(Scope::Profile(profile.id()), FETCH_ROWS, 42)
            .expect("put");
    }
    store
        .put_setting(Scope::Application, FETCH_ROWS, 7)
        .expect("put");

    assert!(store.delete_profile(gone.id()).expect("delete"));
    assert!(!store.delete_profile(gone.id()).expect("delete again"));
    assert_eq!(store.profile(gone.id()).expect("read"), None);
    assert!(
        store
            .profile_settings(gone.id())
            .expect("load")
            .value
            .is_empty()
    );
    assert_eq!(
        store
            .profile_settings(keep.id())
            .expect("load")
            .value
            .get(FETCH_ROWS),
        Some(42)
    );
    assert_eq!(
        store
            .application_settings()
            .expect("load")
            .value
            .get(FETCH_ROWS),
        Some(7)
    );
}

#[test]
fn every_setting_kind_round_trips_at_every_level_it_allows() {
    let mut store = store();
    let profile = Profile::create(details("p")).expect("valid");
    store.insert_profile(&profile).expect("insert");
    let worksheet = WorksheetId::new_random();
    store
        .save_worksheet(&Worksheet::new(worksheet, None, worksheet_state(), 0).expect("valid"))
        .expect("save worksheet");
    let seconds = |n| TimeLimit::Seconds(NonZeroU32::new(n).expect("non-zero"));

    let values = [
        (
            SettingId::ConnectTimeout,
            SettingValue::TimeLimit(TimeLimit::NoLimit),
        ),
        (SettingId::RewriteTriggerDdl, SettingValue::Bool(false)),
        (
            SettingId::StatementTimeLimit,
            SettingValue::TimeLimit(seconds(86_400)),
        ),
        (SettingId::FetchRows, SettingValue::Count(100_000)),
        (SettingId::FetchesInFlight, SettingValue::Count(8)),
        (SettingId::ServerOutputEnabled, SettingValue::Bool(true)),
        (
            SettingId::ServerOutputBuffer,
            SettingValue::ByteLimit(ByteLimit::Unlimited),
        ),
        (
            SettingId::HistoryMaxEntriesPerProfile,
            SettingValue::EntryLimit(EntryLimit::Unlimited),
        ),
    ];
    assert_eq!(values.len(), SettingId::ALL.len(), "one value per setting");
    let scopes = [
        Scope::Application,
        Scope::Profile(profile.id()),
        Scope::Worksheet(worksheet),
    ];
    for (id, value) in values {
        for scope in scopes {
            let allowed = id.descriptor().levels().contains(scope.level());
            let result = store.put_setting_value(scope, id, value);
            assert_eq!(result.is_ok(), allowed, "{id} at {scope:?}: {result:?}");
        }
    }
    let app = store.application_settings().expect("load");
    let prof = store.profile_settings(profile.id()).expect("load");
    let sheet = store.worksheet_settings(worksheet).expect("load");
    assert!(app.rejected.is_empty() && prof.rejected.is_empty() && sheet.rejected.is_empty());
    for (id, value) in values {
        let levels = id.descriptor().levels();
        assert_eq!(
            app.value.get_value(id),
            levels.contains(Level::Application).then_some(value)
        );
        assert_eq!(
            prof.value.get_value(id),
            levels.contains(Level::Profile).then_some(value)
        );
        assert_eq!(
            sheet.value.get_value(id),
            levels.contains(Level::Worksheet).then_some(value)
        );
    }

    // And the loaded layers resolve as the rule says.
    let context = ResolveContext::new()
        .with_application(&app.value)
        .with_profile(&prof.value)
        .with_worksheet(&sheet.value);
    assert_eq!(context.resolve(CONNECT_TIMEOUT).source, Level::Profile);
    assert_eq!(
        context.resolve(STATEMENT_TIME_LIMIT).source,
        Level::Worksheet
    );
    assert_eq!(
        context.resolve(FETCHES_IN_FLIGHT).source,
        Level::Application
    );
    assert!(context.resolve(SERVER_OUTPUT_ENABLED).value);
    assert_eq!(
        context.resolve(SERVER_OUTPUT_BUFFER).value,
        ByteLimit::Unlimited
    );
}

#[test]
fn a_refused_write_writes_nothing() {
    let mut store = store();
    assert!(matches!(
        store.put_setting(Scope::Application, FETCH_ROWS, 0),
        Err(StoreError::InvalidSetting(SettingError::OutOfBounds { .. }))
    ));
    assert!(matches!(
        store.put_setting(
            Scope::Worksheet(WorksheetId::new_random()),
            CONNECT_TIMEOUT,
            TimeLimit::NoLimit
        ),
        Err(StoreError::InvalidSetting(
            SettingError::LevelNotAllowed { .. }
        ))
    ));
    assert!(matches!(
        store.put_setting(Scope::Profile(ProfileId::new_random()), FETCH_ROWS, 10),
        Err(StoreError::ProfileNotFound(_))
    ));
    let count: i64 = store
        .connection
        .query_row("SELECT count(*) FROM setting", [], |row| row.get(0))
        .expect("count");
    assert_eq!(count, 0);
}

#[test]
fn put_replaces_and_clear_restores_inheritance() {
    let mut store = store();
    store
        .put_setting(Scope::Application, FETCH_ROWS, 10)
        .expect("put");
    store
        .put_setting(Scope::Application, FETCH_ROWS, 20)
        .expect("replace");
    assert_eq!(
        store
            .application_settings()
            .expect("load")
            .value
            .get(FETCH_ROWS),
        Some(20)
    );
    assert!(
        store
            .clear_setting(Scope::Application, SettingId::FetchRows)
            .expect("clear")
    );
    assert!(
        !store
            .clear_setting(Scope::Application, SettingId::FetchRows)
            .expect("clear again")
    );
    assert!(store.application_settings().expect("load").value.is_empty());

    let worksheet = WorksheetId::new_random();
    store
        .save_worksheet(&Worksheet::new(worksheet, None, worksheet_state(), 0).expect("valid"))
        .expect("save worksheet");
    store
        .put_setting(Scope::Worksheet(worksheet), FETCH_ROWS, 5)
        .expect("put");
    store
        .put_setting(Scope::Worksheet(worksheet), SERVER_OUTPUT_ENABLED, true)
        .expect("put");
    assert_eq!(store.clear_worksheet_settings(worksheet).expect("clear"), 2);
    assert!(
        store
            .worksheet_settings(worksheet)
            .expect("load")
            .value
            .is_empty()
    );
}

#[test]
fn unusable_setting_rows_are_reported_kept_and_the_level_below_used() {
    let mut store = store();
    store
        .put_setting(Scope::Application, FETCH_ROWS, 300)
        .expect("put");
    let insert = |key: &str, kind: &str, int: Option<i64>| {
        store
            .connection
            .execute(
                "INSERT INTO setting (scope, scope_id, setting_key, kind, int_value, updated_at) \
                 VALUES ('application', '', ?1, ?2, ?3, 0)",
                params![key, kind, int],
            )
            .expect("raw insert");
    };
    // A newer Reldex's setting, a kind nobody knows, a malformed value, a
    // value out of today's bounds, and a value of the wrong kind.
    insert("future.setting", "bool", Some(1));
    insert("execution.statement_time_limit", "colour", Some(3));
    insert("server_output.enabled", "bool", Some(7));
    insert("results.fetches_in_flight", "count", Some(99));
    insert("connection.connect_timeout", "count", Some(3));

    let loaded = store.application_settings().expect("load");
    assert_eq!(loaded.value.get(FETCH_ROWS), Some(300));
    assert_eq!(loaded.value.len(), 1, "{:?}", loaded.value);
    let reasons: Vec<(&str, &RejectReason)> = loaded
        .rejected
        .iter()
        .map(|row| (row.key.as_str(), &row.reason))
        .collect();
    assert_eq!(reasons.len(), 5, "{reasons:?}");
    assert!(reasons.contains(&("future.setting", &RejectReason::UnknownSetting)));
    assert!(reasons.contains(&(
        "execution.statement_time_limit",
        &RejectReason::UnknownKind("colour".to_owned())
    )));
    assert!(reasons.contains(&("server_output.enabled", &RejectReason::Malformed)));
    assert!(
        reasons
            .iter()
            .any(|(key, reason)| *key == "results.fetches_in_flight"
                && matches!(
                    reason,
                    RejectReason::Refused(SettingError::OutOfBounds { .. })
                ))
    );
    assert!(
        reasons
            .iter()
            .any(|(key, reason)| *key == "connection.connect_timeout"
                && matches!(
                    reason,
                    RejectReason::Refused(SettingError::KindMismatch { .. })
                ))
    );

    // Resolution uses the level below for every rejected row.
    let context = ResolveContext::new().with_application(&loaded.value);
    assert_eq!(context.resolve(STATEMENT_TIME_LIMIT).source, Level::BuiltIn);
    assert_eq!(context.resolve(CONNECT_TIMEOUT).source, Level::BuiltIn);

    // Nothing was deleted: a newer build still finds its row.
    let kept: i64 = store
        .connection
        .query_row(
            "SELECT count(*) FROM setting WHERE setting_key = 'future.setting'",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(kept, 1);
}

#[test]
fn an_undecodable_profile_row_is_reported_without_hiding_the_others() {
    let mut store = store();
    let good = Profile::create(details("good")).expect("valid");
    store.insert_profile(&good).expect("insert");
    let bad = Profile::create(details("bad")).expect("valid");
    store.insert_profile(&bad).expect("insert");
    store
        .connection
        .execute(
            "UPDATE profile SET environment = 'mars' WHERE id = ?1",
            params![bad.id().to_string()],
        )
        .expect("raw update");
    let loaded = store.profiles().expect("list");
    assert_eq!(loaded.value, vec![good]);
    assert_eq!(loaded.rejected.len(), 1);
    assert_eq!(loaded.rejected[0].key, bad.id().to_string());
    assert!(matches!(
        &loaded.rejected[0].reason,
        RejectReason::Undecodable(detail) if detail.contains("environment")
    ));
    assert!(matches!(
        store.profile(bad.id()),
        Err(StoreError::InvalidRow { .. })
    ));
}

#[test]
fn a_stored_row_is_validated_exactly_like_create() {
    // A row edited outside Reldex must not produce a profile `create` would
    // have refused: decoding runs the same validation.
    let profile = Profile::create(details("x")).expect("valid");
    let row = codec::encode_profile(&profile).expect("encodes");
    assert_eq!(codec::decode_profile(row.clone()), Ok(profile));
    let blank_name = ProfileRow {
        name: " ".to_owned(),
        ..row.clone()
    };
    assert!(matches!(
        codec::decode_profile(blank_name),
        Err(detail) if detail.contains("name")
    ));
    let bad_port = ProfileRow {
        port: Some(70_000),
        ..row.clone()
    };
    assert!(matches!(
        codec::decode_profile(bad_port),
        Err(detail) if detail.contains("port")
    ));
    let missing_flag = ProfileRow {
        password_in_credential_store: None,
        ..row.clone()
    };
    assert!(codec::decode_profile(missing_flag).is_err());
    // A production indicator that contradicts the environment is refused
    // like it is at `create`.
    let production_without_indicator = ProfileRow {
        environment: "production".to_owned(),
        treat_as_production: 0,
        ..row.clone()
    };
    assert!(matches!(
        codec::decode_profile(production_without_indicator),
        Err(detail) if detail.contains("production indicator")
    ));
    let not_a_flag = ProfileRow {
        treat_as_production: 2,
        ..row
    };
    assert!(matches!(
        codec::decode_profile(not_a_flag),
        Err(detail) if detail.contains("treat_as_production")
    ));
}

#[test]
fn a_stored_connect_string_holding_a_credential_is_rejected_without_echoing_it() {
    // A row edited outside Reldex, with a password pasted into its connect
    // string: rejected on load like any invalid row, and neither the report
    // nor the row's Debug repeats what it held.
    let marker = "RldxPastedPassword-3e1f";
    let mut store = store();
    let profile = Profile::create(details("pasted")).expect("valid");
    store.insert_profile(&profile).expect("insert");
    store
        .connection
        .execute(
            "UPDATE profile SET endpoint_kind = 'connect_string', host = NULL, port = NULL, \
             service_name = NULL, connect_string = ?2 WHERE id = ?1",
            params![
                profile.id().to_string(),
                format!("scott/{marker}@db:1521/ORDERS")
            ],
        )
        .expect("raw update");
    let loaded = store.profiles().expect("list");
    assert!(loaded.value.is_empty());
    assert_eq!(loaded.rejected.len(), 1);
    let report = format!("{:?}", loaded.rejected[0]);
    assert!(report.contains("UserPasswordPrefix"), "{report}");
    assert!(!report.contains(marker), "{report}");
    let error = store.profile(profile.id()).expect_err("rejected");
    assert!(!error.to_string().contains(marker), "{error}");
    assert!(!format!("{error:?}").contains(marker), "{error:?}");
    let row = store
        .connection
        .query_row(
            &format!("SELECT {PROFILE_COLUMNS} FROM profile"),
            [],
            ProfileRow::from_sql,
        )
        .expect("row");
    assert!(!format!("{row:?}").contains(marker));
}

#[test]
fn a_credential_in_an_endpoint_never_becomes_a_profile() {
    let mut pasted = details("pasted");
    pasted.endpoint =
        ProfileEndpoint::ConnectString("(DESCRIPTION=(PASSWORD=x)(ADDRESS=(HOST=h)))".to_owned());
    let expected = ProfileError::CredentialInEndpoint {
        field: ProfileField::ConnectString,
        pattern: CredentialPattern::PasswordKeyword,
    };
    assert_eq!(Profile::create(pasted.clone()), Err(expected.clone()));
    let mut store = store();
    let mut profile = Profile::create(details("clean")).expect("valid");
    store.insert_profile(&profile).expect("insert");
    assert_eq!(profile.update(pasted), Err(expected));
    store
        .update_profile(&profile)
        .expect("unchanged profile saves");
    assert_eq!(
        store.profile(profile.id()).expect("read"),
        Some(profile.clone())
    );
}

#[test]
fn an_uppercase_id_written_outside_reldex_still_names_its_profile() {
    let mut store = store();
    let profile = Profile::create(details("shouting")).expect("valid");
    store.insert_profile(&profile).expect("insert");
    store
        .put_setting(Scope::Profile(profile.id()), FETCH_ROWS, 42)
        .expect("put");
    store
        .connection
        .execute_batch(
            "UPDATE profile SET id = upper(id); \
             UPDATE setting SET scope_id = upper(scope_id) WHERE scope = 'profile';",
        )
        .expect("raw update");
    let stored_id: String = store
        .connection
        .query_row("SELECT id FROM profile", [], |row| row.get(0))
        .expect("id");
    assert_eq!(stored_id, profile.id().to_string().to_ascii_uppercase());

    // Read, listed, and its overrides found, under the lowercase id.
    assert_eq!(
        store.profile(profile.id()).expect("read"),
        Some(profile.clone())
    );
    assert_eq!(store.profiles().expect("list").value, vec![profile.clone()]);
    assert_eq!(
        store
            .profile_settings(profile.id())
            .expect("layer")
            .value
            .get(FETCH_ROWS),
        Some(42)
    );
    // Updated and overridden in place, not duplicated.
    let mut renamed = profile.clone();
    renamed.update(details("quieter")).expect("valid");
    store
        .update_profile(&renamed)
        .expect("update finds the row");
    store
        .put_setting(Scope::Profile(profile.id()), FETCH_ROWS, 43)
        .expect("put finds the profile");
    let settings: i64 = store
        .connection
        .query_row("SELECT count(*) FROM setting", [], |row| row.get(0))
        .expect("count");
    assert_eq!(settings, 1);
    // Deleted together with its overrides.
    assert!(store.delete_profile(profile.id()).expect("delete"));
    let left: i64 = store
        .connection
        .query_row(
            "SELECT (SELECT count(*) FROM profile) + (SELECT count(*) FROM setting)",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(left, 0);
}

#[cfg(unix)]
#[test]
fn a_directory_and_file_the_store_creates_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let root = std::env::temp_dir().join(format!(
        "reldex-workspace-perm-{}-{}",
        std::process::id(),
        UnixTimeMs::now().as_millis()
    ));
    let directory = root.join("nested").join(super::APP_ID);
    let store = Store::open_in_directory(&directory, StoreOptions::default()).expect("open");
    let mode = |path: &std::path::Path| {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(&directory), 0o700);
    assert_eq!(mode(&root.join("nested")), 0o700);
    let file = directory.join(super::STORE_FILE_NAME);
    assert_eq!(mode(&file), 0o600);
    let mut wal = file.as_os_str().to_owned();
    wal.push("-wal");
    assert_eq!(mode(std::path::Path::new(&wal)), 0o600);
    drop(store);
    // A second open of the existing file changes nothing.
    drop(Store::open_in_directory(&directory, StoreOptions::default()).expect("reopen"));
    assert_eq!(mode(&file), 0o600);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn open_in_directory_creates_what_is_missing_and_reuses_what_exists() {
    let root = std::env::temp_dir().join(format!(
        "reldex-workspace-dir-{}-{}",
        std::process::id(),
        UnixTimeMs::now().as_millis()
    ));
    let directory = root.join("a").join("b");
    let mut store = Store::open_in_directory(&directory, StoreOptions::default()).expect("open");
    store
        .put_setting(Scope::Application, FETCH_ROWS, 7)
        .expect("put");
    drop(store);
    let store = Store::open_in_directory(&directory, StoreOptions::default()).expect("reopen");
    assert_eq!(
        store
            .application_settings()
            .expect("layer")
            .value
            .get(FETCH_ROWS),
        Some(7)
    );
    assert_eq!(
        store.path(),
        Some(directory.join(super::STORE_FILE_NAME).as_path())
    );
    drop(store);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_schema_has_no_column_that_could_hold_a_secret() {
    let store = store();
    let mut statement = store
        .connection
        .prepare(
            "SELECT m.name, p.name FROM sqlite_schema AS m, pragma_table_info(m.name) AS p \
             WHERE m.type = 'table'",
        )
        .expect("prepare");
    let columns: Vec<(String, String)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert!(columns.len() > 20, "{columns:?}");
    for (table, column) in &columns {
        let lower = column.to_ascii_lowercase();
        let allowed = lower == "password_in_credential_store";
        for word in [
            "password",
            "passwd",
            "pwd",
            "secret",
            "token",
            "private_key",
            "credential",
        ] {
            assert!(
                allowed || !lower.contains(word),
                "{table}.{column} looks like it could hold a secret"
            );
        }
    }
}

#[test]
fn profile_error_converts_into_a_store_error() {
    let error: StoreError = ProfileError::PortZero.into();
    assert!(matches!(
        error,
        StoreError::InvalidProfile(ProfileError::PortZero)
    ));
    assert!(!error.to_string().is_empty());
}
