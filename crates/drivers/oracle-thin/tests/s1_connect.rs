//! Spike S1 — connect, authenticate, ping, close (ADR-0001).
//!
//! Kill criterion: no pure-Rust path can authenticate against Oracle 19c.

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "spike results are measurements a human reads from the test output"
)]

mod common;

use std::time::{Duration, Instant};

use common::{
    DSN, PASSWORD, USER, connect, exec, exec_quietly, measurement, observation, params, setting,
    try_connect, unique,
};
use reldex_db_driver_api::{
    ConnectionParams, Credentials, Endpoint, ErrorKind, Secret, SessionState, Statement,
};

#[test]
fn easy_connect_with_a_service_name_authenticates() {
    let mut connection = connect();
    assert!(connection.ping().is_ok());
    observation(format!(
        "easy connect `{}` authenticated as `{}`",
        setting(DSN),
        setting(USER)
    ));
    connection.close().expect("close should succeed");
}

#[test]
fn a_full_tns_descriptor_authenticates_too() {
    // Derived from the Easy Connect string so the test carries no hard-coded
    // host, and proves the descriptor form the UI will offer for wallets and
    // multi-address services.
    let dsn = setting(DSN);
    let (host_port, service) = dsn.rsplit_once('/').expect("host:port/service");
    let (host, port) = host_port.rsplit_once(':').expect("host:port");
    let descriptor = format!(
        "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST={host})(PORT={port}))\
         (CONNECT_DATA=(SERVICE_NAME={service})))"
    );

    let params = ConnectionParams::new(
        Endpoint::ConnectString(descriptor.clone()),
        Credentials::UserPassword {
            username: setting(USER),
            password: Secret::new(setting(PASSWORD)),
        },
    );
    let mut connection = try_connect(&params).expect("the descriptor should connect");
    assert!(connection.ping().is_ok());
    observation("full TNS descriptor connected");
    connection.close().expect("close should succeed");
}

#[test]
fn a_wrong_password_is_an_authentication_failure_not_a_network_one() {
    let params = ConnectionParams::new(
        Endpoint::ConnectString(setting(DSN)),
        Credentials::UserPassword {
            username: setting(USER),
            // Deliberately wrong, and not a real credential.
            password: Secret::new("definitely-not-the-password-0000"),
        },
    );
    let error = match try_connect(&params) {
        Ok(_) => panic!("a wrong password must not authenticate"),
        Err(error) => error,
    };
    assert_eq!(
        error.kind(),
        ErrorKind::Authentication,
        "got {error} (native {:?})",
        error.native()
    );
    assert_eq!(error.session_state(), SessionState::Usable);
    let native = error.native().expect("the ORA code should survive");
    observation(format!("wrong password -> {:?} / {native}", error.kind()));
    assert_eq!(native.code(), 1017, "ORA-01017 expected: {native}");
    // The credential must not be echoed back in any rendering of the failure.
    assert!(
        !error.to_string().contains("definitely-not-the-password"),
        "the error text quoted the password"
    );
    assert!(!format!("{error:?}").contains("definitely-not-the-password"));
}

#[test]
fn an_unreachable_port_is_a_connection_failure() {
    let dsn = setting(DSN);
    let (host_port, service) = dsn.rsplit_once('/').expect("host:port/service");
    let (host, _) = host_port.rsplit_once(':').expect("host:port");
    // 1 is reserved and nothing listens there.
    let params = ConnectionParams::new(
        Endpoint::ConnectString(format!("{host}:1/{service}")),
        Credentials::UserPassword {
            username: setting(USER),
            password: Secret::new(setting(PASSWORD)),
        },
    );
    let started = Instant::now();
    let error = match try_connect(&params) {
        Ok(_) => panic!("nothing listens on port 1"),
        Err(error) => error,
    };
    let elapsed = started.elapsed();
    assert_eq!(
        error.kind(),
        ErrorKind::Connection,
        "got {error} (native {:?})",
        error.native()
    );
    observation(format!(
        "unreachable port -> {:?}: {} (after {elapsed:.1?})",
        error.kind(),
        error.message()
    ));
}

#[test]
fn a_closed_connection_reports_rather_than_panics() {
    let connection = connect();
    let handle = connection.cancel_handle();
    connection.close().expect("close should succeed");

    // A second connection object cannot be closed twice (the contract consumes
    // it), so the lifecycle rule is checked through the handle that outlived it:
    // it must answer, not panic and not touch the socket.
    let outcome = handle
        .request_cancel()
        .expect("a cancel handle outliving its connection must still answer");
    observation(format!("cancel handle after close -> {outcome:?}"));
}

#[test]
fn a_cursor_outliving_its_connection_reports_rather_than_panics() {
    let mut connection = connect();
    let mut outcome = connection
        .execute(&Statement::new(
            "SELECT level FROM dual CONNECT BY level <= 100",
        ))
        .expect("select should execute");
    let mut cursor = outcome.take_cursor().expect("a query returns a cursor");
    connection.close().expect("close should succeed");

    let error = match cursor.fetch_batch(std::num::NonZeroUsize::MIN) {
        Ok(_) => panic!("fetching over a closed connection must fail"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ErrorKind::DriverInternal, "got {error}");
    assert_eq!(error.session_state(), SessionState::Lost);
    observation(format!("cursor after connection close -> {error}"));
    cursor.close().expect("closing a dead cursor is still fine");
}

#[test]
fn a_handle_outliving_a_dropped_connection_reports_too() {
    // `close()` is the polite path. A `db-core` worker thread that unwinds, or
    // any owner that simply lets the connection go, is the impolite one — and
    // `oracledb`'s own `Cursor` and `Lob` hold a cloned `Arc<Mutex<Client>>`, so
    // without help they would go on using a session nothing owns. ADR-0002 D2's
    // handle lifecycle makes no distinction between the two, so neither does
    // this driver.
    let mut connection = connect();
    let mut outcome = connection
        .execute(&Statement::new(
            "SELECT level FROM dual CONNECT BY level <= 100",
        ))
        .expect("select should execute");
    let mut cursor = outcome.take_cursor().expect("a query returns a cursor");

    let table = unique("s1_drop");
    let mut lob_connection = connect();
    exec(
        lob_connection.as_mut(),
        &format!("CREATE TABLE {table} (c CLOB)"),
    );
    exec(
        lob_connection.as_mut(),
        &format!("INSERT INTO {table} VALUES (RPAD('x', 5000, 'x'))"),
    );
    exec(lob_connection.as_mut(), "COMMIT");
    let mut batch = {
        let mut outcome = lob_connection
            .execute(&Statement::new(format!("SELECT c FROM {table}")))
            .expect("select");
        let mut lob_cursor = outcome.take_cursor().expect("cursor");
        let batch = lob_cursor
            .fetch_batch(std::num::NonZeroUsize::MIN)
            .expect("fetch");
        lob_cursor.close().expect("close");
        batch
    };
    let mut locator = batch
        .column_mut(0)
        .and_then(|column| column.take_lob(0))
        .expect("a CLOB locator");

    // Dropped, not closed.
    drop(connection);
    drop(lob_connection);

    let error = match cursor.fetch_batch(std::num::NonZeroUsize::MIN) {
        Ok(_) => panic!("fetching over a dropped connection must fail"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ErrorKind::DriverInternal, "got {error}");
    assert_eq!(error.session_state(), SessionState::Lost);
    observation(format!("cursor after connection drop -> {error}"));
    cursor.close().expect("closing a dead cursor is still fine");

    let mut buffer = [0_u8; 64];
    let error = match locator.read_chunk(&mut buffer) {
        Ok(_) => panic!("reading a LOB over a dropped connection must fail"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ErrorKind::DriverInternal, "got {error}");
    assert_eq!(error.session_state(), SessionState::Lost);
    observation(format!("LOB after connection drop -> {error}"));

    let mut cleanup = connect();
    exec_quietly(cleanup.as_mut(), &format!("DROP TABLE {table} PURGE"));
    cleanup.close().expect("close");
}

#[test]
fn connection_latency_is_measured_over_ten_attempts() {
    let mut samples: Vec<Duration> = Vec::with_capacity(10);
    let mut pings: Vec<Duration> = Vec::with_capacity(10);
    for _ in 0..10 {
        let started = Instant::now();
        let mut connection = match try_connect(&params()) {
            Ok(connection) => connection,
            Err(error) => panic!("connect failed mid-measurement: {error}"),
        };
        samples.push(started.elapsed());

        let started = Instant::now();
        connection.ping().expect("ping should succeed");
        pings.push(started.elapsed());

        connection.close().expect("close should succeed");
    }
    samples.sort_unstable();
    pings.sort_unstable();

    let median = samples[samples.len() / 2];
    let ping_median = pings[pings.len() / 2];
    measurement("s1.connect_median", format!("{median:.1?}"));
    measurement("s1.connect_min", format!("{:.1?}", samples[0]));
    measurement(
        "s1.connect_max",
        format!("{:.1?}", samples[samples.len() - 1]),
    );
    measurement("s1.ping_median", format!("{ping_median:.1?}"));
    // No threshold is asserted: this is a measurement, not a performance claim.
    assert_eq!(samples.len(), 10);
}
