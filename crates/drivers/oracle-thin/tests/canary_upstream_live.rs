//! Upstream canaries that need the live Oracle test database.
//!
//! The companion of `canary_upstream_offline.rs`; read that file's header
//! first — it explains what a canary is and why a **failing** canary is good
//! news. The procedure both files serve is
//! `docs/exec-plans/active/oracledb-upgrade-checklist.md`.
//!
//! Every test here drives **raw `oracledb`**, never `OracleThinDriver`: a
//! canary has to record what upstream does, not how this wrapper reacts to it.
//! That is the opposite of the `s*.rs` spikes, which exist to test the wrapper.
//!
//! # The abort canaries
//!
//! U-2 and U-3 end the process rather than failing a test (U-4), so they
//! cannot be asserted in the process that runs the suite. Each is therefore a
//! **pair**: an `#[ignore]`d `child_*` test that does the dangerous thing and
//! nothing else, and a parent that re-invokes this same test binary for it and
//! asserts on how the child died. The child does nothing at all unless the
//! parent armed it through `RELDEX_CANARY_CHILD`, so `--ignored` on its own is
//! safe.
//!
//! Run the whole file:
//!
//! ```text
//! tools/oracle-test-db/run-it.ps1 canary_upstream_live -- --test-threads=1
//! ```

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "a canary's observation is something a human reads from the test output"
)]

mod common;

use std::fs;
use std::io::Write as _;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::{DSN, PASSWORD, USER, observation, setting, unique};

/// What to do when a canary's premise no longer holds.
const UPGRADE_PROCEDURE: &str =
    "follow docs/exec-plans/active/oracledb-upgrade-checklist.md before changing any guard";

/// A connection through raw `oracledb`, bypassing this crate entirely.
fn raw_connect() -> oracledb::Connection {
    let config = oracledb::Config::default()
        .set_connect_string(&setting(DSN))
        .expect("a valid connect string")
        .set_credentials(&setting(USER), &setting(PASSWORD));
    oracledb::connect(config).expect("the test database should accept the configured credentials")
}

/// Runs a statement through raw `oracledb`, with the SQL in the failure.
fn raw_exec(connection: &oracledb::Connection, sql: &str) {
    if let Err(error) = connection.execute(sql, &[]) {
        panic!("{sql}\n  failed: {error}");
    }
}

/// Runs a statement whose failure does not matter (cleanup).
fn raw_exec_quietly(connection: &oracledb::Connection, sql: &str) {
    let _ = connection.execute(sql, &[]);
}

// ---------------------------------------------------------------------------
// U-1 — a bound NUMBER stored ten times too large
// ---------------------------------------------------------------------------

/// One value whose leading-zero count makes `decimal_point_index` odd and
/// negative, with the value upstream stores instead.
struct ScaledCase {
    /// What is bound.
    bound: &'static str,
    /// What `26.0.0-beta.3` puts in the column: ten times too much.
    scaled_by_ten: &'static str,
}

/// U-1: binding a decimal below `0.1` whose leading-zero count is odd still
/// stores a value ten times too large, with no error anywhere.
///
/// The comparison is made by the **server**, against literals in the statement
/// text, because binding the expected value would run into the same defect.
#[test]
fn u1_a_bound_number_with_an_odd_leading_zero_count_is_still_stored_ten_times_too_large() {
    const CASES: [ScaledCase; 3] = [
        ScaledCase {
            bound: "0.05",
            scaled_by_ten: "0.5",
        },
        ScaledCase {
            bound: "0.0005",
            scaled_by_ten: "0.005",
        },
        ScaledCase {
            bound: "-0.05",
            scaled_by_ten: "-0.5",
        },
    ];

    let connection = raw_connect();
    let table = unique("can_u1");
    raw_exec(
        &connection,
        &format!("CREATE TABLE {table} (id NUMBER(3), v NUMBER)"),
    );

    // Collect first, drop the table, assert afterwards: a canary that fails
    // must still leave the test schema clean.
    let mut verdicts = Vec::with_capacity(CASES.len());
    for (id, case) in CASES.iter().enumerate() {
        let number: oracledb::OracleNumber = case
            .bound
            .parse()
            .expect("a legal Oracle NUMBER, and from_str accepts it");
        let insert = format!("INSERT INTO {table} (id, v) VALUES ({id}, :1)");
        if let Err(error) = connection.execute(&insert, &[&number]) {
            panic!("{insert}\n  failed: {error}");
        }
        let sql = format!(
            "SELECT CASE WHEN v = {scaled} THEN 'TEN_TIMES_TOO_LARGE' \
                         WHEN v = {bound} THEN 'EXACT' \
                         ELSE 'SOMETHING_ELSE' END \
             FROM {table} WHERE id = {id}",
            scaled = case.scaled_by_ten,
            bound = case.bound,
        );
        let row = connection
            .query_row(&sql, &[])
            .unwrap_or_else(|error| panic!("{sql}\n  failed: {error}"));
        let verdict: String = row
            .get(0)
            .unwrap_or_else(|error| panic!("{sql}\n  returned nothing readable: {error}"));
        verdicts.push(verdict);
    }
    raw_exec_quietly(&connection, &format!("DROP TABLE {table} PURGE"));

    for (case, verdict) in CASES.iter().zip(&verdicts) {
        assert_eq!(
            verdict,
            "TEN_TIMES_TOO_LARGE",
            "U-1 appears FIXED upstream (oracle/rust-oracledb#21): binding {bound} no longer \
             stores {scaled}, the server says {verdict}. Re-evaluate the NUMBER bind refusal \
             in binds.rs::encoder_defect (EncoderDefect::ScaledByTen) — half of all decimals \
             below 0.1 are currently unbindable because of this. {UPGRADE_PROCEDURE}",
            bound = case.bound,
            scaled = case.scaled_by_ten,
        );
    }
    observation(format!(
        "U-1 still present: {:?} are stored ten times too large",
        CASES.map(|case| case.bound)
    ));
}

// ---------------------------------------------------------------------------
// U-5 — TIMESTAMP WITH TIME ZONE returned without applying its offset
// ---------------------------------------------------------------------------

/// U-5: a `TIMESTAMP WITH TIME ZONE` still arrives as the wire's **UTC**
/// fields carrying the zone offset beside them, so `Display` renders a
/// different instant from the one the server holds.
///
/// `13:45:30 +07:00` on the server must still read back as `06:45:30 +07:00`.
#[test]
fn u5_a_timestamp_with_time_zone_is_still_returned_without_its_offset_applied() {
    let connection = raw_connect();
    let sql = "SELECT TO_TIMESTAMP_TZ('2026-09-19 13:45:30.123456789 +07:00',
                                      'YYYY-MM-DD HH24:MI:SS.FF9 TZH:TZM') FROM dual";
    let row = connection
        .query_row(sql, &[])
        .unwrap_or_else(|error| panic!("{sql}\n  failed: {error}"));
    let value: oracledb::OracleTimestamp = row
        .get(0)
        .unwrap_or_else(|error| panic!("{sql}\n  did not decode: {error}"));

    assert_eq!(
        (value.hour(), value.minute(), value.tz_hour_offset()),
        (6, 45, 7),
        "U-5 appears FIXED upstream (not submitted as an issue): a TIMESTAMP WITH TIME ZONE \
         now reads back as the instant the server holds ({value}) instead of its UTC fields \
         with the offset beside them. Re-evaluate value.rs::to_timestamp and \
         binds.rs::to_oracle_timestamp, which currently apply the offset on read and its \
         inverse on write — leaving them in place would now shift every value by its zone. \
         {UPGRADE_PROCEDURE}"
    );
    observation(format!(
        "U-5 still present: the server's 13:45:30+07:00 reads back as {value}"
    ));
}

// ---------------------------------------------------------------------------
// U-8 — the server's error code and error position are discarded
// ---------------------------------------------------------------------------

/// U-8: a server error still arrives as `ErrorKind::DbError(String)` with the
/// `ORA-` code only inside the message text and no character offset at all.
#[test]
fn u8_the_servers_error_code_and_position_are_still_only_in_the_message_text() {
    let connection = raw_connect();
    let missing = unique("can_u8");
    let sql = format!("SELECT 1 FROM {missing}");
    let error = connection
        .query_row(&sql, &[])
        .err()
        .unwrap_or_else(|| panic!("{sql}\n  must not succeed: the table does not exist"));

    match error.kind() {
        oracledb::ErrorKind::DbError(text) => {
            assert!(
                text.contains("ORA-00942"),
                "U-8 CHANGED in an unexpected way: DbError no longer carries the ORA- code in \
                 its text either, so error.rs's native-code recovery has nothing left to parse. \
                 Message was: {text}"
            );
            observation(format!(
                "U-8 still present: the only structure is the text — {text:?}"
            ));
        }
        other => panic!(
            "U-8 appears FIXED upstream (not submitted as an issue): a server error is no longer \
             ErrorKind::DbError(String) but {other:?}, so the ORA- code and possibly the wire \
             offset are now structured. Re-evaluate error.rs — it recovers the code by parsing \
             `ORA-nnnnn` back out of the message, and SPEC.md §24.14's \"highlight the offending \
             token\" may now be achievable for ordinary SQL errors. {UPGRADE_PROCEDURE}"
        ),
    }
}

// ---------------------------------------------------------------------------
// U-18 — `CREATE TRIGGER` is impossible: `:NEW` is parsed as a placeholder
// ---------------------------------------------------------------------------

/// U-18: the SQL parser still scans DDL for `:name` placeholders, so a trigger
/// body that mentions `:NEW` is rejected client-side before the server ever
/// sees it.
#[test]
fn u18_a_trigger_body_that_mentions_new_is_still_parsed_as_a_bind_placeholder() {
    let connection = raw_connect();
    let table = unique("can_u18");
    let trigger = unique("can_u18t");
    raw_exec(
        &connection,
        &format!("CREATE TABLE {table} (id NUMBER(3), made DATE)"),
    );

    let sql = format!(
        "CREATE TRIGGER {trigger} BEFORE INSERT ON {table} FOR EACH ROW \
         BEGIN :NEW.made := SYSDATE; END;"
    );
    let outcome = connection.execute(&sql, &[]);
    // Clean up whichever way it went: if upstream fixed the parser the trigger
    // now exists, and the canary must not leave it behind.
    raw_exec_quietly(&connection, &format!("DROP TRIGGER {trigger}"));
    raw_exec_quietly(&connection, &format!("DROP TABLE {table} PURGE"));

    let error = match outcome {
        Ok(_) => panic!(
            "U-18 appears FIXED upstream (Issue G, drafted — not yet submitted): CREATE TRIGGER \
             with :NEW now executes. Re-evaluate error.rs::explain_parsed_placeholders, which \
             rewrites the upstream message into one naming the cause and the \
             EXECUTE IMMEDIATE workaround, and the U-18 row in SPEC.md §16's object groups. \
             {UPGRADE_PROCEDURE}"
        ),
        Err(error) => error,
    };
    match error.kind() {
        oracledb::ErrorKind::WrongNumPositionalBinds(required, provided) => {
            assert_eq!(
                (*required, *provided),
                (1, 0),
                "U-18 CHANGED in an unexpected way (Issue G, drafted): the parser still reads \
                 :NEW as a placeholder but now counts {required} of them where one was expected. \
                 Re-derive error.rs::explain_parsed_placeholders' \"the caller declared no \
                 binds\" condition before trusting it. Error was: {error}"
            );
            observation(format!("U-18 still present: {error}"));
        }
        other => panic!(
            "U-18 appears FIXED or CHANGED upstream (Issue G, drafted — not yet submitted): \
             CREATE TRIGGER with :NEW now fails as {other:?} rather than a missing positional \
             bind. Re-evaluate error.rs::explain_parsed_placeholders, which only rewrites the \
             missing-bind message. Error was: {error}. {UPGRADE_PROCEDURE}"
        ),
    }
}

// ---------------------------------------------------------------------------
// U-14 — SSL_SERVER_DN_MATCH is parsed, sent, and never used
// ---------------------------------------------------------------------------

/// U-14: `SSL_SERVER_DN_MATCH=OFF` — the escape hatch every other Oracle
/// client offers — is still accepted by the descriptor parser and still has no
/// effect on the client, which keeps verifying the descriptor's `HOST` against
/// the certificate's `subjectAltName`.
///
/// The probe aims at `127.0.0.1`, which the test listener's certificate does
/// not carry (it names `localhost` only, which is what spike S8 found), and
/// asks for name matching to be turned off. A control connection over the same
/// listener runs first, so a failure here cannot be mistaken for a listener
/// that is simply down.
#[test]
fn u14_ssl_server_dn_match_off_is_still_parsed_sent_and_ignored() {
    let (Some(dsn), Some(ca_dir)) = (
        common::optional(common::TCPS_DSN),
        common::optional(common::TCPS_CA_DIR),
    ) else {
        observation(format!(
            "SKIPPED: {} or {} is not set; run tools/oracle-test-db/startup/10_enable_tcps.sh \
             and use run-it.ps1/.sh",
            common::TCPS_DSN,
            common::TCPS_CA_DIR
        ));
        return;
    };
    let Some((port, service)) = tcps_port_and_service(&dsn) else {
        observation(format!(
            "SKIPPED: {dsn} does not name `localhost`, so there is no name to mismatch"
        ));
        return;
    };

    // A process-wide prerequisite, not the behaviour under test: `rustls` needs
    // a default crypto provider installed before anything builds a client
    // configuration, and this binary reaches TLS only through raw `oracledb`.
    reldex_driver_oracle_thin::install_default_crypto_provider();

    let with_wallet = |connect_string: &str| {
        oracledb::Config::default()
            .set_connect_string(connect_string)
            .expect("a valid TCPS connect string")
            .set_credentials(&setting(USER), &setting(PASSWORD))
            .set_wallet_location(ca_dir.clone())
    };

    let control = oracledb::connect(with_wallet(&dsn));
    assert!(
        control.is_ok(),
        "the U-14 canary did not run: the control TCPS connection to {dsn} failed, so a \
         refusal below would say nothing about SSL_SERVER_DN_MATCH. Error was: {:?}",
        control.err().map(|error| error.to_string())
    );
    drop(control);

    let numeric = format!(
        "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=127.0.0.1)(PORT={port}))\
         (CONNECT_DATA=(SERVICE_NAME={service}))(SECURITY=(SSL_SERVER_DN_MATCH=OFF)))"
    );
    match oracledb::connect(with_wallet(&numeric)) {
        Ok(_) => panic!(
            "U-14 appears FIXED upstream (oracle/rust-oracledb#25): SSL_SERVER_DN_MATCH=OFF now \
             turns host-name verification off, so a certificate that does not name the host is \
             accepted. Re-evaluate what Reldex offers here — the driver currently cannot expose \
             the escape hatch at all, and TlsMode/`lib.rs`'s transport-security section says so. \
             Note this also makes the strict default overridable, which S8 relied on. \
             {UPGRADE_PROCEDURE}"
        ),
        Err(error) => observation(format!(
            "U-14 still present: SSL_SERVER_DN_MATCH=OFF was accepted by the parser and \
             ignored by the client — {error}"
        )),
    }
}

/// The port and service name of a `tcps://localhost:PORT/SERVICE` connect
/// string, or `None` when it does not name `localhost`.
fn tcps_port_and_service(dsn: &str) -> Option<(String, String)> {
    let without_scheme = dsn.rsplit("://").next()?;
    let (authority, service) = without_scheme.split_once('/')?;
    let (host, port) = authority.split_once(':')?;
    (host == "localhost").then(|| (port.to_owned(), service.to_owned()))
}

// ---------------------------------------------------------------------------
// U-2, U-3, U-4 — the defects that end the process
// ---------------------------------------------------------------------------

/// The variable the parent sets to arm exactly one child canary.
const CHILD_MARKER: &str = "RELDEX_CANARY_CHILD";

/// How long a child gets before it is killed and reported as hung.
///
/// Generous: the child pays for a process start, a `cargo`-free but still cold
/// binary load and a connect. The abort itself arrives in well under a second.
const CHILD_LIMIT: Duration = Duration::from_secs(60);

/// How the child process ended.
#[derive(Debug)]
enum ChildEnding {
    /// It reached the dangerous call and the process died abnormally — the
    /// current behaviour, and what U-4 does to an upstream panic.
    Aborted {
        /// The exit status as the operating system reported it.
        status: String,
    },
    /// It reached the dangerous call and reported an ordinary test failure.
    /// The panic still happened but no longer took the process with it.
    Panicked,
    /// It reached the dangerous call and finished cleanly.
    Survived,
    /// It never reached the dangerous call, so its ending says nothing about
    /// upstream. Something else — a connect, the environment — failed first.
    NeverArmed,
    /// It was still running when [`CHILD_LIMIT`] expired.
    TimedOut,
}

/// Everything the parent learned about one child run.
struct ChildRun {
    /// How the child ended.
    ending: ChildEnding,
    /// What the child wrote to standard output.
    stdout: String,
    /// What the child wrote to standard error, including the panic message.
    stderr: String,
    /// How long the child took.
    elapsed: Duration,
}

impl ChildRun {
    /// The child's own output, for a failure message.
    fn transcript(&self) -> String {
        format!(
            "child ran for {:.2?}\n--- child stdout ---\n{}\n--- child stderr ---\n{}",
            self.elapsed,
            self.stdout.trim(),
            self.stderr.trim()
        )
    }
}

/// The line a child prints immediately before the call that may end it.
///
/// The parent requires it: without it, a child that died because it could not
/// connect would look exactly like one that aborted where it was meant to.
fn sentinel(child_test: &str) -> String {
    format!("CANARY-ARMED {child_test}")
}

/// Returns whether this process is the child the parent asked for, and says so
/// when it is not.
fn armed_as(child_test: &str) -> bool {
    if std::env::var(CHILD_MARKER).is_ok_and(|value| value == child_test) {
        return true;
    }
    println!(
        "SKIPPED: {CHILD_MARKER} is not set to {child_test}; this test is driven by its \
         parent canary and does nothing on its own"
    );
    false
}

/// Announces that the dangerous call is next, and flushes so the line survives
/// an abort.
fn arm(child_test: &str) {
    println!("{}", sentinel(child_test));
    let _ = std::io::stdout().flush();
}

/// Runs one `#[ignore]`d child canary in a process of its own.
///
/// The child is this same test binary, re-invoked with a filter — so no extra
/// binary target is needed and the child inherits the connection settings
/// `run-it.ps1` put in the environment. Its output goes to files rather than
/// pipes so that the parent can poll for the deadline without risking a full
/// pipe buffer.
fn run_child_canary(child_test: &str) -> ChildRun {
    let exe = std::env::current_exe().expect("a test binary knows its own path");
    let stem = format!("reldex_canary_{}_{child_test}", std::process::id());
    let out_path = std::env::temp_dir().join(format!("{stem}.out"));
    let err_path = std::env::temp_dir().join(format!("{stem}.err"));
    let out = fs::File::create(&out_path).expect("a temporary file for the child's stdout");
    let err = fs::File::create(&err_path).expect("a temporary file for the child's stderr");

    let started = Instant::now();
    let mut child = Command::new(exe)
        .args([
            child_test,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_MARKER, child_test)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .expect("the canary child process should start");

    let mut status: Option<ExitStatus> = None;
    while started.elapsed() < CHILD_LIMIT {
        match child.try_wait() {
            Ok(Some(finished)) => {
                status = Some(finished);
                break;
            }
            Ok(None) => thread::sleep(Duration::from_millis(25)),
            Err(error) => panic!("could not wait for the canary child process: {error}"),
        }
    }
    if status.is_none() {
        let _ = child.kill();
    }
    // Reap on every path. After a `try_wait` that already succeeded this just
    // returns the status it cached.
    let _ = child.wait();
    let elapsed = started.elapsed();

    let stdout = fs::read_to_string(&out_path).unwrap_or_default();
    let stderr = fs::read_to_string(&err_path).unwrap_or_default();
    let _ = fs::remove_file(&out_path);
    let _ = fs::remove_file(&err_path);

    let ending = match status {
        None => ChildEnding::TimedOut,
        Some(_) if !stdout.contains(&sentinel(child_test)) => ChildEnding::NeverArmed,
        Some(finished) if finished.success() => ChildEnding::Survived,
        // 101 is what libtest exits with after an ordinary, caught panic.
        Some(finished) if finished.code() == Some(101) => ChildEnding::Panicked,
        Some(finished) => ChildEnding::Aborted {
            status: describe_status(finished),
        },
    };
    ChildRun {
        ending,
        stdout,
        stderr,
        elapsed,
    }
}

/// The exit status in a form worth recording: Windows reports an abort as an
/// `NTSTATUS` (`0xC0000409` is `STATUS_STACK_BUFFER_OVERRUN`), Unix as a
/// signal, and the C runtime's own `abort()` as plain 3.
fn describe_status(status: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return format!("killed by signal {signal}");
        }
    }
    match status.code() {
        Some(code) => format!("exit code {code} (0x{:08X})", code as u32),
        None => format!("no exit code ({status})"),
    }
}

/// U-2 with U-4: binding 40 significant digits with an odd, positive
/// decimal-point index still reads past the encoder's 40-byte buffer, and the
/// resulting panic still takes the process with it.
///
/// `1.234567890123456789012345678901234567891` is of magnitude **one** — the
/// trigger is the shape, not the size.
#[test]
fn u2_a_forty_digit_bind_with_an_odd_decimal_point_index_still_aborts_the_process() {
    let child = CHILD_U2;
    let run = run_child_canary(child);
    match &run.ending {
        ChildEnding::Aborted { status } => observation(format!(
            "U-2 with U-4 still present: the child process {status} after {:.2?}",
            run.elapsed
        )),
        ChildEnding::Survived => panic!(
            "U-2 appears FIXED upstream (oracle/rust-oracledb#22): binding 40 digits with an \
             odd decimal-point index no longer ends the process. Re-evaluate \
             binds.rs::encoder_defect (EncoderDefect::ReadsPastItsDigitBuffer) and the \
             digit-count arm of its predicate. {UPGRADE_PROCEDURE}\n{}",
            run.transcript()
        ),
        ChildEnding::Panicked => panic!(
            "U-4 appears FIXED upstream (oracle/rust-oracledb#22): the child reported an \
             ordinary test failure instead of aborting, so a panic raised inside a round trip \
             now unwinds and StatementHolder::drop no longer panics on the poisoned mutex. \
             U-2 itself may well still be present — read the child's panic below. This is the \
             one that changes what a wrapper can do: with unwinding contained, \
             binds.rs::encoder_defect could become a catch rather than a refusal. \
             {UPGRADE_PROCEDURE}\n{}",
            run.transcript()
        ),
        ChildEnding::NeverArmed => panic!(
            "the U-2 canary did not run: the child never printed `{}`, so it failed before the \
             bind. This says nothing about upstream — fix the child's failure and re-run.\n{}",
            sentinel(child),
            run.transcript()
        ),
        ChildEnding::TimedOut => panic!(
            "the U-2 canary did not finish: the child was still running after {CHILD_LIMIT:?} \
             and was killed. If upstream now blocks rather than aborting, that is a third \
             outcome this canary does not describe — investigate before changing any guard.\n{}",
            run.transcript()
        ),
    }
}

/// The child of [`u2_a_forty_digit_bind_with_an_odd_decimal_point_index_still_aborts_the_process`].
const CHILD_U2: &str = "child_u2_binds_forty_digits_with_an_odd_decimal_point_index";

/// Binds the shape U-2 mishandles, through raw `oracledb`, and ends the
/// process doing it. Driven by its parent; does nothing on its own.
#[test]
#[ignore = "child of the U-2 canary; aborts the process on oracledb 26.0.0-beta.3"]
fn child_u2_binds_forty_digits_with_an_odd_decimal_point_index() {
    if !armed_as(CHILD_U2) {
        return;
    }
    let connection = raw_connect();
    let number: oracledb::OracleNumber = "1.234567890123456789012345678901234567891"
        .parse()
        .expect("a legal Oracle NUMBER, and from_str accepts it");
    arm(CHILD_U2);
    let survived = connection
        .execute("SELECT :1 FROM dual", &[&number])
        .is_ok();
    println!("CANARY-SURVIVED the bind returned ok={survived}");
}

/// U-3 with U-4: decoding a `TIMESTAMP WITH TIME ZONE` whose zone is a named
/// region still reaches a `todo!()`, and the panic still takes the process.
#[test]
fn u3_a_named_time_zone_region_still_aborts_the_process() {
    let child = CHILD_U3;
    let run = run_child_canary(child);
    match &run.ending {
        ChildEnding::Aborted { status } => observation(format!(
            "U-3 with U-4 still present: the child process {status} after {:.2?}",
            run.elapsed
        )),
        ChildEnding::Survived => panic!(
            "U-3 appears FIXED upstream (oracle/rust-oracledb#22): a region-encoded \
             TIMESTAMP WITH TIME ZONE now decodes. Re-evaluate the whole containment — \
             cursor.rs::timestamp_with_time_zone_is_refused (the describe-time column \
             refusal), the prefetch_rows(0) and exclude_from_cache() calls in conn.rs that \
             exist to make that refusal possible, and lib.rs::EXT_ALLOW_TIMESTAMP_WITH_TIME_ZONE. \
             Dropping prefetch_rows(0) would also return the one extra round trip a small \
             result currently pays. {UPGRADE_PROCEDURE}\n{}",
            run.transcript()
        ),
        ChildEnding::Panicked => panic!(
            "U-4 appears FIXED upstream (oracle/rust-oracledb#22): the child reported an \
             ordinary test failure instead of aborting, so the todo!() in \
             ora_type/timestamp.rs now unwinds without taking the process. U-3 itself is \
             probably still present — read the child's panic below — but a contained panic \
             changes what cursor.rs could do about it. {UPGRADE_PROCEDURE}\n{}",
            run.transcript()
        ),
        ChildEnding::NeverArmed => panic!(
            "the U-3 canary did not run: the child never printed `{}`, so it failed before the \
             query. This says nothing about upstream — fix the child's failure and re-run.\n{}",
            sentinel(child),
            run.transcript()
        ),
        ChildEnding::TimedOut => panic!(
            "the U-3 canary did not finish: the child was still running after {CHILD_LIMIT:?} \
             and was killed. Investigate before changing any guard.\n{}",
            run.transcript()
        ),
    }
}

/// The child of [`u3_a_named_time_zone_region_still_aborts_the_process`].
const CHILD_U3: &str = "child_u3_reads_a_named_time_zone_region";

/// Reads a region-encoded `TIMESTAMP WITH TIME ZONE` through raw `oracledb`,
/// which ends the process. Driven by its parent; does nothing on its own.
#[test]
#[ignore = "child of the U-3 canary; aborts the process on oracledb 26.0.0-beta.3"]
fn child_u3_reads_a_named_time_zone_region() {
    if !armed_as(CHILD_U3) {
        return;
    }
    let connection = raw_connect();
    arm(CHILD_U3);
    let survived = connection
        .query_row(
            "SELECT TO_TIMESTAMP_TZ('2026-09-19 13:45:30 Asia/Bangkok',
                                    'YYYY-MM-DD HH24:MI:SS TZR') FROM dual",
            &[],
        )
        .is_ok();
    println!("CANARY-SURVIVED the query returned ok={survived}");
}
