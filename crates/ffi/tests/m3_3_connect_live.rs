//! M3.3: the first real database session through the C ABI, against the live
//! Oracle 19c test database.
//!
//! Behind the `oracle-it` feature, so `cargo test --workspace` stays green
//! with no database; run it with
//!
//! ```text
//! RELDEX_IT_PACKAGE=reldex-ffi bash tools/oracle-test-db/run-it.sh m3_3_connect_live -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! which loads `tools/oracle-test-db/.env` into the environment. Each test is
//! also `#[ignore]`d, and skips itself — and says so — when
//! `RELDEX_TEST_ORACLE_DSN`/`_USER`/`_PASSWORD` are not set. No credential is
//! printed: a password goes from the environment straight into
//! `reldex_secret_from_utf8`, the only way one crosses this ABI.
//!
//! Every path goes the way the adapter's does: a profile in an in-memory
//! workspace, `reldex_workspace_prepare_connect` on its service thread, then
//! `reldex_hub_open_session` with `RELDEX_DRIVER_KIND_ORACLE`.
#![cfg(feature = "oracle-it")]
#![allow(
    unsafe_code,
    reason = "these tests drive the crate's own C ABI, which is what they exist to prove"
)]
#![allow(
    clippy::print_stdout,
    reason = "the measurements are read by a human from the test output"
)]

mod support;

use std::env;
use std::time::{Duration, Instant};

use reldex_ffi::{
    ReldexAbandonOutcome, ReldexCancelKind, ReldexCancelOutcome, ReldexCloseDisposition,
    ReldexDriverKind, ReldexErrorKind, ReldexEvent, ReldexEventKind, ReldexOpenOptions,
    ReldexPasswordSourceKind, ReldexPasswordStorageKind, ReldexStatus, reldex_batch_column_count,
    reldex_batch_row_count, reldex_connect_summary_release, reldex_hub_open_session,
    reldex_secret_from_utf8, reldex_secret_release, reldex_session_request_cancel,
};

use support::workspace::{TestWorkspace, profile_details, str_of};
use support::{Harness, release_batch, take_error};

/// The test database, as `run-it.sh` exports it.
struct LiveDb {
    host: String,
    port: u16,
    service: String,
    user: String,
    password: String,
}

impl LiveDb {
    /// The database, or `None` — and the test skips — when the environment
    /// names none.
    fn from_env(test: &str) -> Option<Self> {
        Self::from_env_as(
            test,
            "RELDEX_TEST_ORACLE_USER",
            "RELDEX_TEST_ORACLE_PASSWORD",
        )
    }

    fn from_env_as(test: &str, user_variable: &str, password_variable: &str) -> Option<Self> {
        let variable = |name: &str| env::var(name).ok().filter(|value| !value.is_empty());
        let (Some(dsn), Some(user), Some(password)) = (
            variable("RELDEX_TEST_ORACLE_DSN"),
            variable(user_variable),
            variable(password_variable),
        ) else {
            println!(
                "{test}: skipped, RELDEX_TEST_ORACLE_DSN/{user_variable}/{password_variable} are \
                 not set"
            );
            return None;
        };
        // `host:port/service`, the form `run-it.sh` exports.
        let (address, service) = dsn.split_once('/').expect("DSN is host:port/service");
        let (host, port) = address.rsplit_once(':').expect("DSN is host:port/service");
        Some(Self {
            host: host.to_owned(),
            port: port.parse().expect("the DSN's port is a number"),
            service: service.to_owned(),
            user,
            password,
        })
    }
}

/// A workspace holding one profile for `db`, whose password is asked for at
/// every connect (so nothing is ever stored).
struct Profile {
    workspace: TestWorkspace,
    id: [u8; 16],
}

impl Profile {
    fn new(db: &LiveDb) -> Self {
        let workspace = TestWorkspace::open();
        let mut details = profile_details(&db.host, db.port, &db.service, &db.user);
        details.password_storage = ReldexPasswordStorageKind::PromptEachTime as i32;
        let id = workspace.create_profile(&details);
        Self { workspace, id }
    }

    /// Prepares a connect with `password` typed in, and opens it on `harness`
    /// as an Oracle session. Returns the session id.
    fn open(&self, harness: &Harness, password: &str, request: u64) -> u64 {
        // SAFETY: `password` is valid UTF-8, alive for the call.
        let typed = unsafe { reldex_secret_from_utf8(str_of(password)) };
        assert!(!typed.is_null());
        let reply = self.workspace.prepare_connect(&self.id, typed);
        // SAFETY: owned here, released once; the summary holds its own copy.
        unsafe { reldex_secret_release(typed) };
        assert!(reply.error.is_null(), "prepare_connect failed");
        assert_eq!(
            reply.password_source_kind,
            ReldexPasswordSourceKind::Supplied as i32
        );
        assert!(!reply.connect.is_null());
        let options = ReldexOpenOptions {
            driver: ReldexDriverKind::Oracle as i32,
            connect: reply.connect,
            ..ReldexOpenOptions::default()
        };
        let mut session = 0_u64;
        // SAFETY: the hub is live; `options` borrows a live summary for the
        // call.
        let status = unsafe {
            reldex_hub_open_session(
                harness.hub(),
                std::ptr::from_ref(&options),
                request,
                std::ptr::from_mut(&mut session),
            )
        };
        // SAFETY: owned here, released once, after the call returned.
        unsafe { reldex_connect_summary_release(reply.connect) };
        assert_eq!(status, ReldexStatus::Ok);
        session
    }
}

/// Opens a session and waits for its `OPENED`, asserting it connected.
fn connected(harness: &Harness, profile: &Profile, db: &LiveDb) -> (u64, ReldexEvent) {
    let session = profile.open(harness, &db.password, 1);
    let opened = harness.next_event();
    assert_eq!(opened.kind, ReldexEventKind::Opened as i32);
    assert_eq!(opened.session, session);
    if let Some(error) = take_error(&opened) {
        panic!("the test database refused the session: {error:?}");
    }
    (session, opened)
}

/// Closes `session` with `disposition` and drains through its `TERMINAL`.
fn close(harness: &Harness, session: u64, disposition: ReldexCloseDisposition) {
    assert_eq!(harness.close(session, 900, disposition), ReldexStatus::Ok);
    let closed = harness.next_event();
    assert_eq!(closed.kind, ReldexEventKind::SessionClosed as i32);
    if let Some(error) = take_error(&closed) {
        panic!("the session closes cleanly: {error:?}");
    }
    let terminal = harness.next_event();
    assert_eq!(terminal.kind, ReldexEventKind::Terminal as i32);
    assert_eq!(terminal.session, session);
    assert!(!terminal.abandoned);
    assert!(
        !terminal.transaction_possibly_lost,
        "a close that named its disposition loses nothing"
    );
    // SAFETY: the hub is live; a null `out` asks only for the count.
    let listed =
        unsafe { reldex_ffi::reldex_hub_list_sessions(harness.hub(), std::ptr::null_mut(), 0) };
    assert_eq!(
        listed,
        harness.session_count(),
        "the closed session is retired"
    );
}

/// Runs `sql` and returns its reply, asserting it succeeded.
fn run(harness: &Harness, session: u64, request: u64, sql: &str) -> ReldexEvent {
    assert_eq!(
        harness.execute_text(session, request, sql),
        ReldexStatus::Ok
    );
    let reply = harness.next_event();
    assert_eq!(reply.request, request, "{sql}");
    if let Some(error) = take_error(&reply) {
        panic!("`{sql}` failed: {error:?}");
    }
    reply
}

fn p50(samples: &mut [Duration]) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

#[test]
#[ignore = "needs the Oracle test database; run through tools/oracle-test-db/run-it.sh"]
fn the_ffi_opens_a_real_session_and_runs_select_1_from_dual() {
    let Some(db) = LiveDb::from_env("select_1_from_dual") else {
        return;
    };
    let profile = Profile::new(&db);
    let harness = Harness::new();
    let (session, opened) = connected(&harness, &profile, &db);
    assert_eq!(
        opened.cancel_kind,
        ReldexCancelKind::PreArmedDeadline as i32,
        "the thin driver can only enforce a deadline armed before the statement"
    );

    let executed = run(&harness, session, 2, "select 1 from dual");
    assert_eq!(executed.kind, ReldexEventKind::Executed as i32);
    assert!(executed.has_result);
    assert_eq!(executed.column_count, 1);

    assert_eq!(
        harness.fetch(session, 3, executed.result, 10),
        ReldexStatus::Ok
    );
    let fetched = harness.next_event();
    assert_eq!(fetched.kind, ReldexEventKind::Fetched as i32);
    assert!(!fetched.batch.is_null());
    // SAFETY: the batch is live until `release_batch` below.
    let (rows, columns) = unsafe {
        (
            reldex_batch_row_count(fetched.batch),
            reldex_batch_column_count(fetched.batch),
        )
    };
    assert_eq!((rows, columns), (1, 1));
    release_batch(&fetched);

    assert_eq!(
        harness.close_result(session, 4, executed.result),
        ReldexStatus::Ok
    );
    let result_closed = harness.next_event();
    assert_eq!(result_closed.kind, ReldexEventKind::ResultClosed as i32);

    // The thin driver tracks transactions conservatively: after any
    // statement but DDL a transaction *may* be open (`SELECT … FOR UPDATE`
    // opens one), so a close that names no disposition is refused rather
    // than silently committing or rolling back (`SPEC.md` §10). The session
    // stays open for the caller to decide.
    assert_eq!(
        harness.close(session, 5, ReldexCloseDisposition::None),
        ReldexStatus::Ok
    );
    let refused = harness.next_event();
    assert_eq!(refused.kind, ReldexEventKind::SessionClosed as i32);
    let error = take_error(&refused).expect("a close with no decision is refused");
    assert_eq!(error.kind, ReldexErrorKind::Transaction as i32, "{error:?}");
    assert!(
        refused.session_still_open,
        "the refusal leaves the session open"
    );
    close(&harness, session, ReldexCloseDisposition::Rollback);
}

#[test]
#[ignore = "needs the Oracle test database; run through tools/oracle-test-db/run-it.sh"]
fn auto_commit_is_off_an_insert_is_invisible_elsewhere_until_committed() {
    let Some(db) = LiveDb::from_env("auto_commit_off") else {
        return;
    };
    let profile = Profile::new(&db);
    let harness = Harness::new();
    let (writer, _) = connected(&harness, &profile, &db);
    let table = format!("M33_AC_{}", std::process::id());
    run(
        &harness,
        writer,
        10,
        &format!("create table {table} (id number)"),
    );

    assert_eq!(
        harness.execute_text(writer, 11, &format!("insert into {table} values (1)")),
        ReldexStatus::Ok
    );
    let mut inserted = None;
    let mut transaction_open = false;
    while inserted.is_none() || !transaction_open {
        let event = harness.next_any_event();
        if event.kind == ReldexEventKind::TransactionState as i32 {
            transaction_open = event.transaction_possibly_active;
        } else if event.request == 11 && event.kind != ReldexEventKind::Executing as i32 {
            if let Some(error) = take_error(&event) {
                panic!("the insert failed: {error:?}");
            }
            inserted = Some(event);
        }
    }
    let inserted = inserted.expect("the insert replied");
    assert!(inserted.has_rows_affected);
    assert_eq!(inserted.rows_affected, 1);
    assert!(
        !inserted.committed_implicitly,
        "nothing committed the insert behind the user's back"
    );

    // A second session sees nothing: the row was not committed behind the
    // user's back.
    let (reader, _) = {
        let session = profile.open(&harness, &db.password, 20);
        let opened = harness.next_event();
        assert_eq!(opened.session, session);
        assert!(take_error(&opened).is_none());
        (session, opened)
    };
    let counted = run(
        &harness,
        reader,
        21,
        &format!("select count(*) from {table}"),
    );
    assert_eq!(
        harness.fetch(reader, 22, counted.result, 1),
        ReldexStatus::Ok
    );
    let fetched = harness.next_event();
    assert!(!fetched.batch.is_null());
    // SAFETY: the batch is live until `release_batch` below.
    let rows = unsafe { reldex_batch_row_count(fetched.batch) };
    assert_eq!(rows, 1);
    let count = support::format_cell(fetched.batch, 0, 0);
    release_batch(&fetched);
    assert_eq!(
        count, "0",
        "an uncommitted row is invisible to another session"
    );
    assert_eq!(
        harness.close_result(reader, 24, counted.result),
        ReldexStatus::Ok
    );
    harness.next_event();

    close(&harness, writer, ReldexCloseDisposition::Rollback);
    run(&harness, reader, 25, &format!("drop table {table} purge"));
    close(&harness, reader, ReldexCloseDisposition::None);
}

#[test]
#[ignore = "needs the Oracle test database; run through tools/oracle-test-db/run-it.sh"]
fn a_wrong_password_is_authentication_with_ora_01017() {
    let Some(db) = LiveDb::from_env("wrong_password") else {
        return;
    };
    let profile = Profile::new(&db);
    let harness = Harness::new();
    let session = profile.open(&harness, "Wrong_password_m33_0", 1);
    let opened = harness.next_event();
    assert_eq!(opened.kind, ReldexEventKind::Opened as i32);
    assert_eq!(opened.session, session);
    let error = take_error(&opened).expect("a refused login explains itself");
    assert_eq!(
        error.kind,
        ReldexErrorKind::Authentication as i32,
        "{error:?}"
    );
    assert_eq!(error.native_code, Some(1017), "{error:?}");
    let terminal = harness.next_event();
    assert_eq!(terminal.kind, ReldexEventKind::Terminal as i32);
    take_error(&terminal);
    assert_eq!(harness.session_count(), 0);
}

#[test]
#[ignore = "needs the Oracle test database; run through tools/oracle-test-db/run-it.sh"]
fn an_unreachable_host_fails_within_the_connect_limit() {
    let Some(db) = LiveDb::from_env("unreachable_host") else {
        return;
    };
    const LIMIT_SECONDS: u32 = 2;
    // TEST-NET-1 (RFC 5737): never routed, so the connect hangs until
    // something gives up — the driver's connect limit, or the network saying
    // no sooner.
    let unreachable = LiveDb {
        host: "192.0.2.1".to_owned(),
        port: 1521,
        ..db
    };
    let profile = Profile::new(&unreachable);
    profile
        .workspace
        .set_profile_connect_timeout(&profile.id, LIMIT_SECONDS);
    let harness = Harness::new();
    let started = Instant::now();
    let session = profile.open(&harness, &unreachable.password, 1);
    let opened = harness.next_event();
    let elapsed = started.elapsed();
    assert_eq!(opened.session, session);
    let error = take_error(&opened).expect("an unreachable host explains itself");
    assert_eq!(error.kind, ReldexErrorKind::Connection as i32, "{error:?}");
    let limit = Duration::from_secs(LIMIT_SECONDS.into());
    assert!(
        elapsed < limit + Duration::from_secs(3),
        "the connect limit bounds the attempt: {elapsed:?}"
    );
    let by_the_limit = error.message.contains("connect limit");
    println!(
        "unreachable host: failed after {elapsed:?} ({}), limit {limit:?}",
        if by_the_limit {
            "the connect limit ended it"
        } else {
            "the network refused first"
        }
    );
    if elapsed >= limit {
        assert!(
            by_the_limit,
            "a limit-length wait names the limit: {error:?}"
        );
    }
    let terminal = harness.next_event();
    assert_eq!(terminal.kind, ReldexEventKind::Terminal as i32);
    take_error(&terminal);
}

#[test]
#[ignore = "needs the Oracle test database; run through tools/oracle-test-db/run-it.sh"]
fn abandoning_a_real_connect_returns_at_once_and_adopts_nothing() {
    let Some(db) = LiveDb::from_env("abandon_connect") else {
        return;
    };
    // The server-side observer connects first (on its own hub), so it can
    // watch from the moment of the abandon. Optional: it needs the SYSTEM
    // login `run-it.sh` exports.
    let observer = LiveDb::from_env_as(
        "abandon_connect (server-side check)",
        "RELDEX_TEST_ORACLE_SYSTEM_USER",
        "RELDEX_TEST_ORACLE_SYSTEM_PASSWORD",
    )
    .map(|system| {
        let profile = Profile::new(&system);
        let harness = Harness::new();
        let (session, _) = connected(&harness, &profile, &system);
        (harness, session)
    });

    let profile = Profile::new(&db);
    let harness = Harness::new();
    let session = profile.open(&harness, &db.password, 1);
    let started = Instant::now();
    let (status, outcome, lost) = harness.abandon(session);
    let abandon_took = started.elapsed();
    assert_eq!(status, ReldexStatus::Ok);
    assert!(!lost);
    println!("abandon while connecting: outcome {outcome}, returned in {abandon_took:?}");
    if outcome == ReldexAbandonOutcome::Connecting as i32 {
        let opened = harness.next_event();
        assert_eq!(opened.kind, ReldexEventKind::Opened as i32);
        let error = take_error(&opened).expect("an abandoned connect is never reported open");
        assert_eq!(error.kind, ReldexErrorKind::Cancelled as i32, "{error:?}");
    } else {
        // The connect won the race: abandon released an open session.
        assert_eq!(outcome, ReldexAbandonOutcome::Open as i32);
        let opened = harness.next_event();
        assert_eq!(opened.kind, ReldexEventKind::Opened as i32);
        take_error(&opened);
    }
    let terminal = harness.next_event();
    assert_eq!(terminal.kind, ReldexEventKind::Terminal as i32);
    assert!(terminal.abandoned);
    take_error(&terminal);
    assert_eq!(harness.session_count(), 0);
    assert_eq!(
        harness.execute_text(session, 2, "select 1 from dual"),
        ReldexStatus::NotFound,
        "the abandoned id is retired, never adopted"
    );
    support::free_error(reldex_ffi::reldex_last_error_take());

    // Server side: whatever connected late was closed, not left behind.
    let Some((observer_hub, observer)) = observer else {
        return;
    };
    let user = db.user.to_uppercase();
    let mut request = 100;
    let mut count_sessions = || {
        request += 3;
        count_user_sessions(&observer_hub, observer, request, &user)
    };
    // Watch the server for a while after the abandon: a connect the driver
    // could not stop may still arrive (U-15), and must then be closed, not
    // kept. The window is far longer than a local connect takes (~150 ms).
    let watch = Duration::from_secs(3);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut peak = 0_u64;
    let mut polls = 0_u32;
    loop {
        let count = count_sessions();
        polls += 1;
        peak = peak.max(count);
        if count == 0 && started.elapsed() >= watch {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{count} {user} session(s) still on the server 30 s after the abandon"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    println!(
        "abandon while connecting: {polls} polls over {:?}; at most {peak} late {user} \
         session(s) seen, none left",
        started.elapsed()
    );
    // Control: the same query does see a session this test keeps.
    let (kept, _) = connected(&harness, &profile, &db);
    assert!(
        count_sessions() >= 1,
        "the observer's query sees a live {user} session"
    );
    close(&harness, kept, ReldexCloseDisposition::None);
    close(&observer_hub, observer, ReldexCloseDisposition::Rollback);
}

/// How many sessions `user` has on the server, asked on `observer`'s session
/// (which needs `v$session`, hence the SYSTEM login). Uses requests
/// `request..request + 3`.
fn count_user_sessions(harness: &Harness, observer: u64, request: u64, user: &str) -> u64 {
    let counted = run(
        harness,
        observer,
        request,
        &format!("select count(*) from v$session where username = '{user}'"),
    );
    assert_eq!(
        harness.fetch(observer, request + 1, counted.result, 1),
        ReldexStatus::Ok
    );
    let fetched = harness.next_event();
    let count = support::format_cell(fetched.batch, 0, 0)
        .parse()
        .expect("a count");
    release_batch(&fetched);
    assert_eq!(
        harness.close_result(observer, request + 2, counted.result),
        ReldexStatus::Ok
    );
    harness.next_event();
    count
}

#[test]
#[ignore = "needs the Oracle test database; run through tools/oracle-test-db/run-it.sh"]
fn a_statement_deadline_ends_a_long_call_and_cancel_says_it_cannot_interrupt() {
    let Some(db) = LiveDb::from_env("deadline_and_cancel") else {
        return;
    };
    let profile = Profile::new(&db);
    let harness = Harness::new();
    let (session, _) = connected(&harness, &profile, &db);

    let started = Instant::now();
    // SAFETY: the hub is live; the SQL text is alive for the call.
    let status = unsafe {
        reldex_ffi::reldex_session_execute(
            harness.hub(),
            session,
            2,
            str_of("begin dbms_session.sleep(10); end;"),
            1_500,
        )
    };
    assert_eq!(status, ReldexStatus::Ok);
    let mut cancel_outcome = -1_i32;
    // SAFETY: the hub is live; `cancel_outcome` is a real local.
    let status = unsafe {
        reldex_session_request_cancel(
            harness.hub(),
            session,
            std::ptr::from_mut(&mut cancel_outcome),
        )
    };
    assert_eq!(status, ReldexStatus::Ok);
    assert_eq!(
        cancel_outcome,
        ReldexCancelOutcome::NotInterruptible as i32,
        "the thin driver cannot interrupt a running call; the UI must not pretend"
    );
    let reply = harness.next_event();
    let elapsed = started.elapsed();
    assert_eq!(reply.request, 2);
    let error = take_error(&reply).expect("the deadline ends the call");
    println!(
        "statement deadline 1.5 s: the reply arrived after {elapsed:?}, kind {}",
        error.kind
    );
    assert!(
        elapsed < Duration::from_secs(9),
        "the deadline, not the sleep, ended the call: {elapsed:?}"
    );
    if error.kind == ReldexErrorKind::NetworkLost as i32 {
        // What `oracledb` does today (ADR-0002 T4): a call timeout during a
        // blocked call loses the connection. The loss is reported, not
        // hidden: a `TERMINAL` follows on its own, before any close.
        let terminal = harness.next_event();
        assert_eq!(terminal.kind, ReldexEventKind::Terminal as i32);
        assert!(!terminal.abandoned);
        take_error(&terminal).expect("a lost session's TERMINAL carries the cause");
    } else {
        assert_eq!(error.kind, ReldexErrorKind::Timeout as i32, "{error:?}");
        close(&harness, session, ReldexCloseDisposition::Rollback);
    }
    assert_eq!(harness.session_count(), 0);
}

#[test]
#[ignore = "needs the Oracle test database; run through tools/oracle-test-db/run-it.sh"]
fn connect_and_first_reply_times() {
    let Some(db) = LiveDb::from_env("connect_and_first_reply_times") else {
        return;
    };
    let runs = support::iterations("RELDEX_M33_RUNS", 10);
    let profile = Profile::new(&db);
    let harness = Harness::new();
    let mut prepare = Vec::with_capacity(runs);
    let mut connect = Vec::with_capacity(runs);
    let mut first_reply = Vec::with_capacity(runs);
    let mut total = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        // SAFETY: the password is valid UTF-8, alive for the call.
        let typed = unsafe { reldex_secret_from_utf8(str_of(&db.password)) };
        let reply = profile.workspace.prepare_connect(&profile.id, typed);
        // SAFETY: owned here, released once.
        unsafe { reldex_secret_release(typed) };
        let prepared = started.elapsed();
        let options = ReldexOpenOptions {
            driver: ReldexDriverKind::Oracle as i32,
            connect: reply.connect,
            ..ReldexOpenOptions::default()
        };
        let mut session = 0_u64;
        let opening = Instant::now();
        // SAFETY: the hub is live; `options` borrows a live summary.
        let status = unsafe {
            reldex_hub_open_session(
                harness.hub(),
                std::ptr::from_ref(&options),
                1,
                std::ptr::from_mut(&mut session),
            )
        };
        // SAFETY: owned here, released once, after the call returned.
        unsafe { reldex_connect_summary_release(reply.connect) };
        assert_eq!(status, ReldexStatus::Ok);
        let opened = harness.next_event();
        assert!(take_error(&opened).is_none(), "the session connects");
        let connected_after = opening.elapsed();
        let executing = Instant::now();
        let executed = run(&harness, session, 2, "select 1 from dual");
        let replied_after = executing.elapsed();
        let total_after = started.elapsed();
        assert_eq!(
            harness.close_result(session, 3, executed.result),
            ReldexStatus::Ok
        );
        harness.next_event();
        close(&harness, session, ReldexCloseDisposition::Rollback);
        prepare.push(prepared);
        connect.push(connected_after);
        first_reply.push(replied_after);
        total.push(total_after);
    }
    let max = |samples: &[Duration]| samples.iter().max().copied().unwrap_or_default();
    println!(
        "n={runs}: prepare p50 {:?} (max {:?}); connect p50 {:?} (max {:?}); \
         first reply p50 {:?} (max {:?}); prepare+connect+first reply p50 {:?} (max {:?})",
        p50(&mut prepare),
        max(&prepare),
        p50(&mut connect),
        max(&connect),
        p50(&mut first_reply),
        max(&first_reply),
        p50(&mut total),
        max(&total),
    );
}
