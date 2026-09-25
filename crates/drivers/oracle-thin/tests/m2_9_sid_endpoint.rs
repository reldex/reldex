//! M2.9 — `sid_endpoint` against the live Oracle 19c test database: the
//! descriptor it builds reaches the listener, which resolves the SID.
//!
//! ```text
//! sh tools/oracle-test-db/run-it.sh m2_9_sid_endpoint
//! ```
//!
//! Behind the `oracle-it` feature like every live test here, and each test
//! also skips itself, saying so, when the connection settings are absent.
//! The SID is `RELDEX_TEST_ORACLE_SID` when set, else the DSN's service name —
//! the same name on this image (`tools/oracle-test-db/README.md`: SID
//! `RELDEX`).
//!
//! A wrong-password attempt counts against the account's
//! `FAILED_LOGIN_ATTEMPTS`, so the test that makes one follows it with a
//! successful login through the same descriptor, which resets the count: the
//! test can be looped without locking the test user.

#![cfg(feature = "oracle-it")]

mod common;

use common::{DSN, PASSWORD, USER, dsn_address, dsn_service, observation, optional, setting};
use reldex_db_driver_api::{
    ConnectionParams, Credentials, DatabaseDriver, Endpoint, ErrorKind, NativeError, Secret,
};
use reldex_driver_oracle_thin::{OracleThinDriver, sid_endpoint};

/// Host, port and SID, or `None` (and a note) when the environment is absent.
fn target() -> Option<(String, u16, String)> {
    if optional(DSN).is_none() || optional(USER).is_none() || optional(PASSWORD).is_none() {
        observation("skipped: the live database settings are not in the environment");
        return None;
    }
    let address = dsn_address();
    let (host, port) = address
        .rsplit_once(':')
        .unwrap_or((address.as_str(), "1521"));
    let port = port.parse().expect("the DSN's port is a number");
    let sid = optional("RELDEX_TEST_ORACLE_SID").unwrap_or_else(dsn_service);
    Some((host.to_owned(), port, sid))
}

fn at_sid(host: &str, port: u16, sid: &str, password: Secret) -> ConnectionParams {
    ConnectionParams::new(
        Endpoint::ConnectString(sid_endpoint(host, port, sid, false).expect("plain names")),
        Credentials::UserPassword {
            username: setting(USER),
            password,
        },
    )
}

#[test]
fn the_listener_resolves_the_sid_and_the_server_checks_the_password() {
    let Some((host, port, sid)) = target() else {
        return;
    };
    let driver = OracleThinDriver::new();

    // Wrong password: the listener must have resolved the SID to an instance
    // for the server to reject the credentials at all.
    let wrong = at_sid(
        &host,
        port,
        &sid,
        Secret::new("m2-9-deliberately-wrong-password"),
    );
    match driver.connect(&wrong) {
        Err(error) => {
            assert_eq!(error.kind(), ErrorKind::Authentication, "{error}");
            assert_eq!(error.native().map(NativeError::code), Some(1017), "{error}");
        }
        Ok(_) => panic!("a wrong password must not open a session"),
    }

    // Right password through the same descriptor: a session opens, and the
    // failed-login count the attempt above added is reset.
    let right = at_sid(&host, port, &sid, Secret::new(setting(PASSWORD)));
    let connection = driver
        .connect(&right)
        .expect("a session through the SID descriptor");
    connection.close().expect("close");
}

#[test]
fn an_unknown_sid_is_a_connection_error() {
    let Some((host, port, _)) = target() else {
        return;
    };
    let bogus = at_sid(&host, port, "NOSUCHSID_M29", Secret::new(setting(PASSWORD)));
    match OracleThinDriver::new().connect(&bogus) {
        Err(error) => {
            assert_eq!(error.kind(), ErrorKind::Connection, "{error}");
            observation(format!(
                "unknown SID refused with native code {:?}",
                error.native().map(NativeError::code)
            ));
        }
        Ok(_) => panic!("an unknown SID must not open a session"),
    }
}
