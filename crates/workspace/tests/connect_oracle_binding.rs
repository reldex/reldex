//! Profile + resolved settings → `ConnectionParams`, through the reference
//! binding for the Oracle thin driver, checked against the keys and values
//! that driver actually reads.
//!
//! Where the driver can be asked without opening a socket — it refuses a
//! configuration before connecting — the parameters are handed to the real
//! `OracleThinDriver::connect`, which proves the whole chain for the refusal
//! paths. Nothing here reaches a network.

mod support;

use std::path::PathBuf;
use std::time::Duration;

use reldex_db_driver_api::{
    DatabaseDriver, Endpoint, ErrorKind, ExtensionValue, Secret, SessionRole, TlsMode,
};
use reldex_driver_oracle_thin::{
    DEFAULT_CONNECT_TIMEOUT, EXT_ALLOW_UNENFORCED_SERVER_CERT_DN, EXT_CONNECT_TIMEOUT_UNBOUNDED,
    EXT_REWRITE_TRIGGER_DDL, EXT_WALLET_DIR, MAX_CONNECT_TIMEOUT, OracleThinDriver, sid_endpoint,
};
use reldex_workspace::settings::{
    CONNECT_TIMEOUT, ProfileScope, REWRITE_TRIGGER_DDL, SettingsLayer, TimeLimit,
};
use reldex_workspace::{
    Authentication, ConnectError, ConnectSettings, DatabaseType, Environment, PasswordStorage,
    Profile, ProfileDetails, ProfileEndpoint, ResolveContext, ServiceTarget, TlsOptions, Transport,
    connection_params,
};
use support::oracle_binding::OracleThinBinding;

fn details(endpoint: ProfileEndpoint, transport: Transport) -> ProfileDetails {
    ProfileDetails {
        name: "orders".to_owned(),
        database: DatabaseType::Oracle,
        environment: Environment::Development,
        treat_as_production: false,
        endpoint,
        authentication: Authentication::Password {
            username: "app".to_owned(),
            storage: PasswordStorage::CredentialStore,
        },
        role: SessionRole::Normal,
        tls: TlsOptions {
            transport,
            ..TlsOptions::default()
        },
    }
}

fn params(
    details: ProfileDetails,
    settings: &ConnectSettings,
) -> Result<reldex_db_driver_api::ConnectionParams, ConnectError> {
    let profile = Profile::create(details).expect("valid");
    connection_params(
        &profile,
        settings,
        Some(Secret::new("not-a-real-password")),
        &OracleThinBinding,
    )
}

fn defaults() -> ConnectSettings {
    ConnectSettings::resolve(&ResolveContext::new())
}

fn flag(params: &reldex_db_driver_api::ConnectionParams, key: &str) -> Option<bool> {
    match params.extensions().get(key) {
        Some(ExtensionValue::Flag(value)) => Some(*value),
        _ => None,
    }
}

#[test]
fn the_registry_agrees_with_the_drivers_own_connect_timeout_constants() {
    assert_eq!(
        CONNECT_TIMEOUT.default_value().as_duration(),
        Some(DEFAULT_CONNECT_TIMEOUT)
    );
    let max = CONNECT_TIMEOUT
        .descriptor()
        .bounds()
        .map(|bounds| Duration::from_secs(u64::from(bounds.max)));
    assert_eq!(max, Some(MAX_CONNECT_TIMEOUT));
}

#[test]
fn defaults_map_to_an_explicit_15_seconds_and_the_rewrite_on() {
    let params = params(
        details(
            ProfileEndpoint::HostPort {
                host: "db.example.internal".to_owned(),
                port: 1521,
                target: ServiceTarget::ServiceName("ORDERS".to_owned()),
            },
            Transport::Plain,
        ),
        &defaults(),
    )
    .expect("maps");
    assert_eq!(params.connect_timeout(), Some(Duration::from_secs(15)));
    assert_eq!(flag(&params, EXT_REWRITE_TRIGGER_DDL), Some(true));
    assert_eq!(flag(&params, EXT_CONNECT_TIMEOUT_UNBOUNDED), None);
    assert_eq!(flag(&params, EXT_ALLOW_UNENFORCED_SERVER_CERT_DN), None);
    assert!(params.extensions().get(EXT_WALLET_DIR).is_none());
    assert!(matches!(
        params.endpoint(),
        Endpoint::HostPort { host, port: 1521, service }
            if host == "db.example.internal" && service == "ORDERS"
    ));
    assert_eq!(params.tls(), TlsMode::Disabled);
}

#[test]
fn no_connect_limit_and_rewrite_off_use_the_drivers_own_switches() {
    let mut profile_layer = SettingsLayer::<ProfileScope>::new();
    profile_layer
        .set(CONNECT_TIMEOUT, TimeLimit::NoLimit)
        .expect("allowed");
    profile_layer
        .set(REWRITE_TRIGGER_DDL, false)
        .expect("allowed");
    let settings = ConnectSettings::resolve(&ResolveContext::new().with_profile(&profile_layer));
    let params = params(
        details(
            ProfileEndpoint::ConnectString("db:1521/ORDERS".to_owned()),
            Transport::Plain,
        ),
        &settings,
    )
    .expect("maps");
    assert_eq!(params.connect_timeout(), None);
    assert_eq!(flag(&params, EXT_CONNECT_TIMEOUT_UNBOUNDED), Some(true));
    assert_eq!(flag(&params, EXT_REWRITE_TRIGGER_DDL), Some(false));
}

/// The descriptor is the driver's own `sid_endpoint`, called with TLS
/// exactly when the profile requires it (the descriptor's content is that
/// function's to test, in the driver).
#[test]
fn a_sid_becomes_the_drivers_descriptor_with_tls_following_the_transport() {
    for (transport, tls, mode) in [
        (Transport::Plain, false, TlsMode::Disabled),
        (Transport::Tls, true, TlsMode::Required),
    ] {
        let params = params(
            details(
                ProfileEndpoint::HostPort {
                    host: "10.0.0.5".to_owned(),
                    port: 2484,
                    target: ServiceTarget::Sid("ORCL".to_owned()),
                },
                transport,
            ),
            &defaults(),
        )
        .expect("maps");
        let Endpoint::ConnectString(descriptor) = params.endpoint() else {
            panic!("a SID is always a connect string");
        };
        assert_eq!(
            descriptor,
            &sid_endpoint("10.0.0.5", 2484, "ORCL", tls).expect("plain names")
        );
        assert_eq!(params.tls(), mode);
    }
}

#[test]
fn a_sid_or_host_that_could_rewrite_the_descriptor_is_refused() {
    for (host, sid) in [
        ("db", "ORCL)(SERVICE_NAME=OTHER"),
        ("db)(HOST=elsewhere", "ORCL"),
        ("db", "OR CL"),
    ] {
        let result = params(
            details(
                ProfileEndpoint::HostPort {
                    host: host.to_owned(),
                    port: 1521,
                    target: ServiceTarget::Sid(sid.to_owned()),
                },
                Transport::Plain,
            ),
            &defaults(),
        );
        assert!(
            matches!(&result, Err(ConnectError::Binding(error))
                if error.kind() == ErrorKind::Configuration),
            "{host} / {sid}: {result:?}"
        );
    }
}

#[test]
fn tls_options_become_the_drivers_wallet_and_pin_switches() {
    let mut tls = details(
        ProfileEndpoint::HostPort {
            host: "db.example.internal".to_owned(),
            port: 2484,
            target: ServiceTarget::ServiceName("ORDERS".to_owned()),
        },
        Transport::Tls,
    );
    let ca = PathBuf::from("/etc/reldex/ca");
    tls.tls.ca_directory = Some(ca.clone());
    tls.tls.allow_unenforced_certificate_pin = true;
    let params = params(tls, &defaults()).expect("maps");
    assert_eq!(params.tls(), TlsMode::Required);
    assert!(matches!(
        params.extensions().get(EXT_WALLET_DIR),
        Some(ExtensionValue::Text(text)) if std::path::Path::new(text) == ca
    ));
    assert_eq!(
        flag(&params, EXT_ALLOW_UNENFORCED_SERVER_CERT_DN),
        Some(true)
    );
}

/// The C-6 guard, end to end: a profile whose descriptor pins the server
/// certificate's DN, without the opt-out, is refused by the real driver
/// before any socket is opened.
#[test]
fn the_driver_refuses_an_unenforceable_pin_without_the_opt_out() {
    let descriptor = "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=127.0.0.1)(PORT=9))\
                      (CONNECT_DATA=(SERVICE_NAME=S))\
                      (SECURITY=(SSL_SERVER_CERT_DN=\"CN=db,O=Example\")))";
    let params = params(
        details(
            ProfileEndpoint::ConnectString(descriptor.to_owned()),
            Transport::Tls,
        ),
        &defaults(),
    )
    .expect("maps");
    match OracleThinDriver::new().connect(&params) {
        Err(error) => {
            assert_eq!(error.kind(), ErrorKind::Configuration, "{error}");
            assert!(error.to_string().contains("SSL_SERVER_CERT_DN"), "{error}");
        }
        Ok(_) => panic!("the pin must be refused before connecting"),
    }
}

/// `Transport::Tls` really becomes `TlsMode::Required`: the real driver
/// refuses a TLS profile whose descriptor asks for plain TCP, before any
/// socket is opened.
#[test]
fn the_driver_refuses_a_tls_profile_with_a_plaintext_descriptor() {
    let descriptor = "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=127.0.0.1)(PORT=9))\
                      (CONNECT_DATA=(SERVICE_NAME=S)))";
    let params = params(
        details(
            ProfileEndpoint::ConnectString(descriptor.to_owned()),
            Transport::Tls,
        ),
        &defaults(),
    )
    .expect("maps");
    match OracleThinDriver::new().connect(&params) {
        Err(error) => assert_eq!(error.kind(), ErrorKind::Configuration, "{error}"),
        Ok(_) => panic!("a plaintext descriptor must be refused for a TLS profile"),
    }
}
