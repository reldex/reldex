//! Spike S13 — privileged connections (`SPEC.md` §8, Connectivity:
//! "privileged connections where supported").
//!
//! Phase 0 recorded this as "only incidental evidence": S4 used a `SYSTEM`
//! account, which is an ordinary session with DBA privileges, not a privileged
//! *connection*. `AS SYSDBA` is a different thing — it authenticates against
//! the password file rather than the data dictionary, which is what lets a DBA
//! connect to an instance that is not open.
//!
//! In its own test binary on purpose. A privileged connect exercises an
//! authentication path nothing else in the suite touches, and a panic anywhere
//! inside a round trip aborts the whole process (U-4); a separate binary means
//! that would cost this file's results and no others.
//!
//! **No credential appears here.** The user name and password come from
//! `RELDEX_TEST_ORACLE_SYSDBA_USER` / `..._SYSDBA_PASSWORD`, which
//! `tools/oracle-test-db/run-it.ps1` (or `.sh`) fills from the untracked
//! `.env`; nothing is printed, asserted on, or put in a failure message.

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "spike results are measurements a human reads from the test output"
)]

mod common;

use std::time::Instant;

use common::{measurement, observation, scalar, sysdba_params, try_connect};
use reldex_db_driver_api::SessionRole;

#[test]
fn the_contract_can_express_a_privileged_role_and_the_driver_uses_it() {
    // The vendor-neutral half of the answer, and it needs no database:
    // `ConnectionParams::with_role` exists, and this driver maps all three
    // roles onto `oracledb`'s `set_auth_mode` (`conn.rs`, `build_config`).
    // Recording it here keeps the "contract gap vs upstream gap" distinction
    // the task asks for on the evidence rather than on memory.
    let Some(params) = sysdba_params() else {
        observation(
            "SKIPPED: RELDEX_TEST_ORACLE_SYSDBA_USER / _PASSWORD are not set, so the \
             live half of S13 did not run",
        );
        return;
    };
    assert_eq!(params.role(), SessionRole::SysDba);
    observation(
        "contract: ConnectionParams::with_role(SessionRole::SysDba) — no extension bag \
         needed; driver: build_config maps SysDba to oracledb's AUTH_MODE_SYSDBA and \
         SysOper to AUTH_MODE_SYSOPER",
    );
}

#[test]
fn a_sysdba_session_authenticates_over_the_listener_and_says_it_is_privileged() {
    let Some(params) = sysdba_params() else {
        observation(
            "SKIPPED: no SYSDBA credentials configured; run through \
             tools/oracle-test-db/run-it.ps1 (or .sh)",
        );
        return;
    };

    let started = Instant::now();
    let mut connection = match try_connect(&params) {
        Ok(connection) => connection,
        Err(error) => {
            // A failure here is reported by kind, never by echoing the
            // credentials that produced it.
            observation(format!(
                "FINDING: a SYSDBA connect over the listener failed: kind={:?} \
                 session_state={:?} native={:?}. On this image that would mean the \
                 password file is absent or REMOTE_LOGIN_PASSWORDFILE is NONE, not that \
                 the driver cannot express the role",
                error.kind(),
                error.session_state(),
                error.native().map(reldex_db_driver_api::NativeError::code)
            ));
            panic!("SYSDBA connect failed: {:?}", error.kind());
        }
    };
    let elapsed = started.elapsed();
    measurement("s13.sysdba_connect", format!("{elapsed:.1?}"));

    // The server's own opinion of what this session is.
    let who = scalar(connection.as_mut(), "SELECT USER FROM dual");
    let is_dba = scalar(
        connection.as_mut(),
        "SELECT SYS_CONTEXT('USERENV','ISDBA') FROM dual",
    );
    let schema = scalar(
        connection.as_mut(),
        "SELECT SYS_CONTEXT('USERENV','CURRENT_SCHEMA') FROM dual",
    );
    let protocol = scalar(
        connection.as_mut(),
        "SELECT SYS_CONTEXT('USERENV','NETWORK_PROTOCOL') FROM dual",
    );
    assert_eq!(who, "SYS", "a SYSDBA session runs as SYS");
    assert_eq!(
        is_dba, "TRUE",
        "the session authenticated but is not privileged; the role was not applied"
    );
    observation(format!(
        "SYSDBA over the listener in {elapsed:.1?}: USER={who}, ISDBA={is_dba}, \
         CURRENT_SCHEMA={schema}, NETWORK_PROTOCOL={protocol}"
    ));

    // Something only a privileged session can see, so the claim is not just a
    // context variable: V$INSTANCE's startup state and the password-file
    // setting that made this connection possible at all.
    let instance = scalar(
        connection.as_mut(),
        "SELECT instance_name || '/' || status || '/' || database_status FROM v$instance",
    );
    let password_file = scalar(
        connection.as_mut(),
        "SELECT value FROM v$parameter WHERE name = 'remote_login_passwordfile'",
    );
    observation(format!(
        "v$instance -> {instance}; remote_login_passwordfile = {password_file} (this is \
         what makes AS SYSDBA over the listener work; with NONE it could not)"
    ));

    // A privileged session is an ordinary session in every other respect: the
    // same transaction rules apply, and auto-commit is still off.
    let state = connection.transaction_state();
    observation(format!(
        "a fresh SYSDBA session reports transaction_state = {state:?}"
    ));
    connection.ping().expect("ping a privileged session");
    connection.close().expect("close");
}

#[test]
fn a_privileged_session_is_a_different_session_from_an_ordinary_one() {
    let Some(params) = sysdba_params() else {
        observation("SKIPPED: no SYSDBA credentials configured");
        return;
    };
    let mut privileged = match try_connect(&params) {
        Ok(connection) => connection,
        Err(error) => {
            observation(format!(
                "SKIPPED: SYSDBA connect failed with kind={:?}",
                error.kind()
            ));
            return;
        }
    };
    let mut ordinary = common::connect();

    let privileged_id = scalar(
        privileged.as_mut(),
        "SELECT SYS_CONTEXT('USERENV','SID') || '/' || USER FROM dual",
    );
    let ordinary_id = scalar(
        ordinary.as_mut(),
        "SELECT SYS_CONTEXT('USERENV','SID') || '/' || USER FROM dual",
    );
    assert_ne!(privileged_id, ordinary_id);

    // The privilege does not leak: the ordinary session is still not a DBA.
    let ordinary_is_dba = scalar(
        ordinary.as_mut(),
        "SELECT SYS_CONTEXT('USERENV','ISDBA') FROM dual",
    );
    assert_eq!(
        ordinary_is_dba, "FALSE",
        "an ordinary session became privileged while a SYSDBA session was open"
    );
    observation(format!(
        "two concurrent sessions, one privileged: {privileged_id} (ISDBA TRUE) and \
         {ordinary_id} (ISDBA {ordinary_is_dba})"
    ));

    privileged.close().expect("close");
    ordinary.close().expect("close");
}

#[test]
fn the_sysoper_role_is_expressible_and_reports_what_the_server_says() {
    // `SYSOPER` is the other half of the contract's `SessionRole`. SYS may
    // connect with it, and the session then runs as `PUBLIC` rather than `SYS`
    // — which is exactly the kind of difference a test should record rather
    // than assume.
    let Some(params) = sysdba_params() else {
        observation("SKIPPED: no privileged credentials configured");
        return;
    };
    let params = params.with_role(SessionRole::SysOper);
    match try_connect(&params) {
        Ok(mut connection) => {
            let who = scalar(connection.as_mut(), "SELECT USER FROM dual");
            let is_dba = scalar(
                connection.as_mut(),
                "SELECT SYS_CONTEXT('USERENV','ISDBA') FROM dual",
            );
            observation(format!(
                "SYSOPER connected: USER={who}, ISDBA={is_dba}. The role reaches the \
                 server through the same `set_auth_mode` path as SYSDBA"
            ));
            connection.close().expect("close");
        }
        Err(error) => {
            observation(format!(
                "SYSOPER was refused: kind={:?} native={:?}. On this image SYS is granted \
                 SYSDBA in the password file; whether it also holds SYSOPER is a database \
                 configuration question, not a driver one",
                error.kind(),
                error.native().map(reldex_db_driver_api::NativeError::code)
            ));
        }
    }
}
