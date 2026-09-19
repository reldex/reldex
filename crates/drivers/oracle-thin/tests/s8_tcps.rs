//! Spike S8 — TCPS (TLS) against a TLS-enabled listener (ADR-0001).
//!
//! Kill criterion: **"TCPS cannot be established without Instant Client."**
//!
//! The listener these tests need is not part of the stock image. Add it with
//! `tools/oracle-test-db/startup/10_enable_tcps.sh` (it runs by itself on every
//! container start once `compose.yaml` mounts it) and run the suite through
//! `tools/oracle-test-db/run-it.ps1` / `.sh`, which export
//! `RELDEX_TEST_ORACLE_TCPS_DSN`, `RELDEX_TEST_ORACLE_TCPS_CA_DIR` and
//! `RELDEX_TEST_ORACLE_TCPS_WRONG_CA_DIR`. Every test here **skips and says so**
//! when those are missing, so a checkout with a plain TCP container still runs
//! the rest of the suite.
//!
//! What is deliberately *not* here: a test that turns certificate verification
//! off. `oracledb` 26.0.0-beta.3 offers no such switch, and the spike would be
//! worthless if it did and this used it.

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "spike results are measurements a human reads from the test output"
)]

mod common;

use std::time::{Duration, Instant};

use common::{
    PASSWORD, TCPS_CA_DIR, TCPS_DSN, TCPS_WRONG_CA_DIR, USER, exec, exec_quietly, measurement,
    observation, optional, params, params_at, query, scalar, setting, tcps_params,
    tcps_params_bare, tcps_params_with_ca, try_connect, unique,
};
use reldex_db_driver_api::{
    ConnectionParams, Credentials, DatabaseDriver, DbError, Endpoint, ErrorKind, Secret,
    SessionState, Statement, TlsMode, TransactionState, WarningKind,
};
use reldex_driver_oracle_thin::OracleThinDriver;

/// Prints the reason and returns `None` when the TLS listener is not configured.
fn tcps() -> Option<ConnectionParams> {
    match tcps_params() {
        Some(params) => Some(params),
        None => {
            observation(format!(
                "SKIPPED: {TCPS_DSN} or {TCPS_CA_DIR} is not set; \
                 run tools/oracle-test-db/startup/10_enable_tcps.sh and use run-it.ps1/.sh"
            ));
            None
        }
    }
}

/// The failure a connection attempt produced, or a panic naming what happened.
fn refusal(params: &ConnectionParams, what: &str) -> DbError {
    match try_connect(params) {
        Ok(_) => panic!("{what} must not produce a usable session"),
        Err(error) => error,
    }
}

#[test]
fn a_tcps_session_is_established_with_verification_on_and_the_test_ca_trusted() {
    let Some(params) = tcps() else { return };
    let mut connection =
        try_connect(&params).expect("the TCPS listener should accept this session");
    assert!(connection.ping().is_ok());

    // The claim that matters: the server itself says the transport is TLS.
    // Anything short of asking the server would be the client believing its own
    // configuration.
    let protocol = scalar(
        connection.as_mut(),
        "SELECT sys_context('USERENV', 'NETWORK_PROTOCOL') FROM dual",
    );
    assert_eq!(
        protocol.to_ascii_lowercase(),
        "tcps",
        "the session reports {protocol}, so it is not encrypted"
    );
    observation(format!(
        "TCPS session established; USERENV.NETWORK_PROTOCOL = {protocol}"
    ));

    // And an ordinary query still behaves.
    let batch = query(
        connection.as_mut(),
        "SELECT level FROM dual CONNECT BY level <= 5",
    );
    assert_eq!(batch.row_count(), 5);
    connection.close().expect("close should succeed");
}

#[test]
fn the_driver_advertises_tls_now_that_it_is_proven() {
    // The capability and the spike are the same claim; if the listener is not
    // configured this still asserts that the driver has not quietly gone back
    // on it.
    assert!(OracleThinDriver::new().capabilities().tls());
}

#[test]
fn a_transaction_round_trip_survives_the_encrypted_transport() {
    let Some(params) = tcps() else { return };
    let mut connection = try_connect(&params).expect("TCPS session");
    let table = unique("s8_tx");
    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER PRIMARY KEY, note VARCHAR2(100))"),
    );

    // Auto-commit is off, so a second session must not see the row until the
    // commit — the same invariant S3 proves over plain TCP, re-checked here
    // because a different transport is a different code path in upstream's
    // message layer.
    exec(
        connection.as_mut(),
        &format!("INSERT INTO {table} VALUES (1, 'ข้อมูล over TLS')"),
    );
    assert_eq!(connection.transaction_state(), TransactionState::Unknown);

    let mut observer = try_connect(&params).expect("a second TCPS session");
    assert_eq!(
        scalar(observer.as_mut(), &format!("SELECT COUNT(*) FROM {table}")),
        "0",
        "an uncommitted row was visible to another session"
    );

    connection.commit().expect("commit should succeed");
    assert_eq!(connection.transaction_state(), TransactionState::Inactive);
    assert_eq!(
        scalar(observer.as_mut(), &format!("SELECT COUNT(*) FROM {table}")),
        "1"
    );

    // Rollback over the same transport.
    exec(connection.as_mut(), &format!("DELETE FROM {table}"));
    connection.rollback().expect("rollback should succeed");
    assert_eq!(
        scalar(
            connection.as_mut(),
            &format!("SELECT COUNT(*) FROM {table}")
        ),
        "1",
        "the rollback did not restore the row"
    );

    // Non-ASCII survives the encrypted transport byte for byte (`SPEC.md` §14).
    assert_eq!(
        scalar(connection.as_mut(), &format!("SELECT note FROM {table}")),
        "ข้อมูล over TLS"
    );
    observation("commit, rollback and Thai text all behave over TCPS");

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
    observer.close().expect("close");
}

#[test]
fn a_certificate_from_an_untrusted_issuer_is_refused_without_leaking_the_credential() {
    let Some(_) = tcps() else { return };
    let Some(wrong) = optional(TCPS_WRONG_CA_DIR) else {
        observation(format!("SKIPPED: {TCPS_WRONG_CA_DIR} is not set"));
        return;
    };
    let error = refusal(
        &tcps_params_with_ca(&wrong),
        "a server certificate signed by a CA the client does not trust",
    );
    assert!(
        matches!(
            error.kind(),
            ErrorKind::Connection | ErrorKind::Configuration | ErrorKind::NetworkLost
        ),
        "an untrusted issuer should read as a transport failure, got {:?}: {error}",
        error.kind()
    );
    assert_ne!(
        error.kind(),
        ErrorKind::Authentication,
        "a TLS trust failure must not look like a bad password"
    );
    assert!(
        !error.to_string().contains(&setting(PASSWORD)),
        "the failure quoted the password"
    );
    assert!(!format!("{error:?}").contains(&setting(PASSWORD)));
    observation(format!(
        "untrusted CA -> {:?}: {}",
        error.kind(),
        error.message()
    ));
}

#[test]
fn a_private_ca_is_not_trusted_without_the_wallet() {
    let Some(_) = tcps() else { return };
    // No wallet at all: `oracledb` trusts `webpki-roots` — the public web PKI —
    // and nothing else, so a private issuer must fail. This is the test that
    // makes the wallet mechanism load-bearing rather than decorative: if it ever
    // starts passing, the client has stopped verifying something.
    let error = refusal(
        &tcps_params_bare(),
        "a private CA with only the public web PKI trusted",
    );
    assert_ne!(error.kind(), ErrorKind::Authentication, "{error}");
    observation(format!(
        "no wallet -> {:?}: {}",
        error.kind(),
        error.message()
    ));
}

#[test]
fn a_host_name_the_certificate_does_not_cover_is_refused() {
    let Some(_) = tcps() else { return };
    let Some(ca) = optional(TCPS_CA_DIR) else {
        return;
    };
    let dsn = setting(TCPS_DSN);

    // The listener's certificate carries `DNS:localhost` and the container name
    // and deliberately **no IP address**, so reaching exactly the same socket by
    // its numeric address has to fail the name check. `oracledb` verifies
    // whatever the descriptor's HOST says: there is no `ssl_server_dn_match=off`
    // to fall back on, which is the finding this test records.
    let numeric = dsn.replace("localhost", "127.0.0.1");
    if numeric == dsn {
        observation(format!(
            "SKIPPED: {TCPS_DSN} does not name `localhost`, so there is no mismatch to make"
        ));
        return;
    }

    let mut params = tcps_params_with_ca(&ca);
    params = ConnectionParams::new(
        Endpoint::ConnectString(numeric.clone()),
        Credentials::UserPassword {
            username: setting(USER),
            password: Secret::new(setting(PASSWORD)),
        },
    )
    .with_tls(TlsMode::Required)
    .with_extensions(params.extensions().clone());

    let error = refusal(
        &params,
        "a host name outside the certificate's subjectAltName",
    );
    assert_ne!(error.kind(), ErrorKind::Authentication, "{error}");
    observation(format!(
        "host `127.0.0.1` against a certificate for `localhost` -> {:?}: {}",
        error.kind(),
        error.message()
    ));
}

#[test]
fn requiring_tls_over_a_plaintext_endpoint_is_refused_rather_than_downgraded() {
    // No TLS listener needed: this is the wrapper refusing to open a plaintext
    // session for a profile that says TLS is mandatory. The ordinary TCP DSN is
    // enough to prove it, so this one runs everywhere.
    let params = params().with_tls(TlsMode::Required);
    let error = refusal(&params, "a TCP endpoint under TlsMode::Required");
    assert_eq!(error.kind(), ErrorKind::Configuration, "{error}");
    // No session was ever opened, so there is nothing to validate: the refusal
    // happens while the parameters are being turned into a configuration.
    assert_eq!(error.session_state(), SessionState::Usable);
    observation(format!("TCP endpoint + TlsMode::Required -> {error}"));
}

/// The configured TCPS target rewritten as a full descriptor, with an extra
/// `SECURITY` segment spliced in.
///
/// `None` when the configured DSN is not the `tcps://host:port/service` shape
/// these tests assume, so the caller can skip and say so rather than assert
/// against a descriptor it built out of nothing.
fn tcps_descriptor(security: &str) -> Option<String> {
    let dsn = setting(TCPS_DSN);
    let (host_port, service) = dsn.strip_prefix("tcps://")?.rsplit_once('/')?;
    let (host, port) = host_port.rsplit_once(':')?;
    Some(format!(
        "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST={host})(PORT={port}))\
         (CONNECT_DATA=(SERVICE_NAME={service})){security})"
    ))
}

#[test]
fn a_descriptor_that_pins_a_distinguished_name_is_refused_before_the_network_is_touched() {
    // No TLS listener needed, and the port is deliberately one nothing answers
    // on: a refusal that came from the network would be `Connection` (spike S1),
    // so `Configuration` here is evidence that no socket was opened. U-14 means
    // the parameter would otherwise be forwarded to the server and never
    // applied, and the session would come back weaker than the profile asked
    // for with nothing saying so.
    let error = refusal(
        &params_at(
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=127.0.0.1)(PORT=1))\
             (CONNECT_DATA=(SERVICE_NAME=RELDEX))\
             (SECURITY=(SSL_SERVER_CERT_DN=\"CN=nobody,O=Reldex\")))",
        )
        .with_tls(TlsMode::Required),
        "a descriptor pinning the server certificate's distinguished name",
    );
    assert_eq!(error.kind(), ErrorKind::Configuration, "{error}");
    assert!(error.message().contains("SSL_SERVER_CERT_DN"), "{error}");
    assert!(error.message().contains("subjectAltName"), "{error}");
    assert_eq!(
        error.session_state(),
        SessionState::Usable,
        "no session was opened, so there is nothing to validate"
    );
    assert!(!error.to_string().contains(&setting(PASSWORD)));
    observation(format!("SSL_SERVER_CERT_DN -> {error}"));
}

#[test]
fn a_descriptor_that_sets_dn_matching_opens_a_session_and_reports_the_parameter_as_inert() {
    let Some(base) = tcps() else { return };
    let Some(descriptor) = tcps_descriptor("(SECURITY=(SSL_SERVER_DN_MATCH=YES))") else {
        observation(format!(
            "SKIPPED: {TCPS_DSN} is not the tcps://host:port/service shape this test rewrites"
        ));
        return;
    };
    let params = ConnectionParams::new(
        Endpoint::ConnectString(descriptor),
        Credentials::UserPassword {
            username: setting(USER),
            password: Secret::new(setting(PASSWORD)),
        },
    )
    .with_tls(TlsMode::Required)
    .with_extensions(base.extensions().clone());

    // Never a refusal: what the parameter asks for already happens, by a
    // stricter mechanism.
    let mut connection =
        try_connect(&params).expect("SSL_SERVER_DN_MATCH must not stop a session opening");
    // The finding reaches the caller through the contract's connect-time
    // channel (`take_connect_warnings`, ADR-0002's C-6 amendment), before any
    // statement has run — which is the half the interim mechanism could not
    // cover, because it rode on the first statement that succeeded.
    let connect_warnings = connection.take_connect_warnings();
    let warning = connect_warnings
        .iter()
        .find(|warning| warning.message().contains("SSL_SERVER_DN_MATCH"))
        .expect("the connect-time finding is reported by the connection itself");
    assert_eq!(warning.kind(), WarningKind::Informational);
    assert!(warning.message().contains("subjectAltName"), "{warning:?}");
    observation(format!(
        "SSL_SERVER_DN_MATCH=YES -> session opened; connect warning: {}",
        warning.message()
    ));
    assert!(
        connection.take_connect_warnings().is_empty(),
        "connect findings are taken, not borrowed: the second call must report nothing"
    );

    // And it never appears on a statement, which is what the replaced
    // mechanism did.
    let outcome = connection
        .execute(&Statement::new("SELECT 1 FROM dual"))
        .expect("an ordinary query over TCPS");
    assert!(
        outcome.warnings().is_empty(),
        "a connect-time finding must not travel with a statement any more: {:?}",
        outcome.warnings()
    );
    drop(outcome);
    connection.close().expect("close");
}

#[test]
fn a_connect_time_finding_survives_a_session_that_never_executes_a_statement() {
    // The hole the interim mechanism could not close, now covered: open, ping,
    // close, and the finding still reaches the caller.
    let Some(base) = tcps() else { return };
    let Some(descriptor) = tcps_descriptor("(SECURITY=(SSL_SERVER_DN_MATCH=YES))") else {
        observation(format!(
            "SKIPPED: {TCPS_DSN} is not the tcps://host:port/service shape this test rewrites"
        ));
        return;
    };
    let params = ConnectionParams::new(
        Endpoint::ConnectString(descriptor),
        Credentials::UserPassword {
            username: setting(USER),
            password: Secret::new(setting(PASSWORD)),
        },
    )
    .with_tls(TlsMode::Required)
    .with_extensions(base.extensions().clone());

    let mut connection = try_connect(&params).expect("the session must open");
    let warnings = connection.take_connect_warnings();
    connection.ping().expect("ping");
    connection.close().expect("close");

    assert!(
        warnings
            .iter()
            .any(|warning| warning.message().contains("SSL_SERVER_DN_MATCH")),
        "no statement ran, and the finding still has to arrive: {warnings:?}"
    );
    observation(
        "a connection that is opened, pinged and closed reports its connect-time finding — \
         the case the pre-C-6 mechanism lost entirely",
    );
}

#[test]
fn a_name_failure_under_those_parameters_says_which_name_was_actually_checked() {
    let Some(base) = tcps() else { return };
    // The same numeric-host mismatch as above — the listener's certificate has
    // no IP address in its SAN — but this time the descriptor also sets
    // `SSL_SERVER_DN_MATCH`. That is the configuration where the raw `rustls`
    // text is most misleading: the obvious next move is to adjust the
    // parameter, which does nothing at all (U-14).
    let Some(descriptor) = tcps_descriptor("(SECURITY=(SSL_SERVER_DN_MATCH=YES))") else {
        observation(format!(
            "SKIPPED: {TCPS_DSN} is not the shape this test rewrites"
        ));
        return;
    };
    let numeric = descriptor.replace("(HOST=localhost)", "(HOST=127.0.0.1)");
    if numeric == descriptor {
        observation(format!(
            "SKIPPED: {TCPS_DSN} does not name `localhost`, so there is no mismatch to make"
        ));
        return;
    }
    let params = ConnectionParams::new(
        Endpoint::ConnectString(numeric),
        Credentials::UserPassword {
            username: setting(USER),
            password: Secret::new(setting(PASSWORD)),
        },
    )
    .with_tls(TlsMode::Required)
    .with_extensions(base.extensions().clone());

    let error = refusal(
        &params,
        "a host name outside the certificate's subjectAltName",
    );
    assert!(
        error.message().contains("subjectAltName"),
        "the failure should say which name was checked: {error}"
    );
    assert!(
        error.message().contains("SSL_SERVER_CERT_DN"),
        "and that the Oracle parameters do not influence it: {error}"
    );
    // Upstream's own text is still there, ahead of the explanation.
    assert!(
        error.message().contains("certificate not valid for name"),
        "{error}"
    );
    assert!(!error.to_string().contains(&setting(PASSWORD)));
    observation(format!(
        "name mismatch under SSL_SERVER_DN_MATCH -> {error}"
    ));
}

#[test]
fn tcps_connect_latency_is_measured_against_tcp() {
    let Some(tcps_params) = tcps() else { return };

    // **Same host name on both sides**, or the comparison measures the wrong
    // thing. The ordinary test DSN is `127.0.0.1`, the TCPS one is `localhost`
    // (the certificate has no IP address in its SAN), and resolving `localhost`
    // costs ~2 s per connect on this Windows host — about twenty times the whole
    // of a plaintext connect. Subtracting one from the other would have charged
    // TLS for a name lookup. So the plaintext comparison is rebuilt against the
    // TCPS host, on the TCP port, and the resolution cost is reported as its own
    // number rather than folded into the TLS one.
    let tcps_dsn = setting(TCPS_DSN);
    let tcps_target = tcps_dsn.strip_prefix("tcps://").unwrap_or(&tcps_dsn);
    let (tcps_host, _) = tcps_target.split_once(':').expect("host:port/service");
    let tcp_dsn = setting(common::DSN);
    let (tcp_host_port, tcp_service) = tcp_dsn.rsplit_once('/').expect("host:port/service");
    let (numeric_host, tcp_port) = tcp_host_port.rsplit_once(':').expect("host:port");

    let same_host_tcp = tcp_dsn_for(&format!("{tcps_host}:{tcp_port}/{tcp_service}"));
    let numeric_tcp = tcp_dsn_for(&format!("{numeric_host}:{tcp_port}/{tcp_service}"));

    let mut tls: Vec<Duration> = Vec::with_capacity(10);
    let mut plain: Vec<Duration> = Vec::with_capacity(10);
    let mut numeric: Vec<Duration> = Vec::with_capacity(10);

    // Interleaved rather than one block each, so a slow moment on the container
    // lands on every measurement instead of only the last.
    for _ in 0..10 {
        let started = Instant::now();
        let connection = try_connect(&tcps_params).expect("TCPS connect");
        tls.push(started.elapsed());
        connection.close().expect("close");

        let started = Instant::now();
        let connection = try_connect(&same_host_tcp).expect("TCP connect, same host name");
        plain.push(started.elapsed());
        connection.close().expect("close");

        let started = Instant::now();
        let connection = try_connect(&numeric_tcp).expect("TCP connect, numeric host");
        numeric.push(started.elapsed());
        connection.close().expect("close");
    }
    tls.sort_unstable();
    plain.sort_unstable();
    numeric.sort_unstable();

    let tls_median = tls[tls.len() / 2];
    let plain_median = plain[plain.len() / 2];
    let numeric_median = numeric[numeric.len() / 2];
    measurement("s8.tcps_connect_median", format!("{tls_median:.1?}"));
    measurement("s8.tcps_connect_min", format!("{:.1?}", tls[0]));
    measurement("s8.tcps_connect_max", format!("{:.1?}", tls[tls.len() - 1]));
    measurement(
        "s8.tcp_connect_median_same_host",
        format!("{plain_median:.1?}"),
    );
    measurement(
        "s8.tcp_connect_median_numeric_host",
        format!("{numeric_median:.1?}"),
    );
    measurement(
        "s8.tls_handshake_overhead_median",
        format!("{:.1?}", tls_median.saturating_sub(plain_median)),
    );
    measurement(
        "s8.host_name_resolution_median",
        format!("{:.1?}", plain_median.saturating_sub(numeric_median)),
    );
    // Measurements, not thresholds: nothing is asserted about the numbers.
    assert_eq!(tls.len(), 10);
}

/// Test-user parameters for an arbitrary plaintext DSN.
fn tcp_dsn_for(dsn: &str) -> ConnectionParams {
    ConnectionParams::new(
        Endpoint::ConnectString(dsn.to_owned()),
        Credentials::UserPassword {
            username: setting(USER),
            password: Secret::new(setting(PASSWORD)),
        },
    )
}

#[test]
fn a_statement_still_reports_its_server_error_over_tcps() {
    let Some(params) = tcps() else { return };
    let mut connection = try_connect(&params).expect("TCPS session");
    let error = match connection.execute(&Statement::new("SELECT * FROM no_such_table_s8")) {
        Ok(_) => panic!("a missing table must fail"),
        Err(error) => error,
    };
    let native = error.native().expect("the ORA code survives TLS too");
    assert_eq!(native.code(), 942, "{native}");
    observation(format!("server error over TCPS -> {native}"));
    connection.close().expect("close");
}
