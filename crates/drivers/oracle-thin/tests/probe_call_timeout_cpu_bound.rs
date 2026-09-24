//! Probe for upstream issue #23 (`oracle/rust-oracledb`): re-tests the
//! maintainer's explanation against **CPU-bound** server work, not just
//! `dbms_session.sleep`.
//!
//! Background: we filed
//! <https://github.com/oracle/rust-oracledb/issues/23> after S4
//! (`docs/exec-plans/active/phase-0-spike-results.md`, "### S4") found that a
//! fired call timeout can close the connection instead of reporting a plain
//! timeout (U-6/U-7 in `docs/exec-plans/active/oracledb-upgrade-checklist.md`).
//! The maintainer's answer, 2026-09-20: expected behaviour. Our repro used
//! `dbms_session.sleep(10)` with a 2s call timeout; a sleeping server never
//! looks at the in-band interrupt the client sends when the timeout fires, so
//! the client's own recovery read times out too and the socket is deemed
//! unusable. `sleep(3)` (server can answer inside the 2s window) or a timeout
//! `> 10s` would give a plain call-timeout error with the session intact. OOB
//! break is unavailable on Rust/Windows. Recommendation: use a pool.
//!
//! What that answer does not cover, and what matters for a database IDE whose
//! users cancel **heavy queries**, not sleeps: does a server that is busy on
//! **CPU** (not suspended in a wait) observe the in-band interrupt any better
//! than a sleeping one? This file measures six scenarios, each run 3+ times.
//!
//! ## Why each scenario behaves the way it does (upstream source)
//!
//! Traced in `oracledb-26.0.0-beta.3/src/client/mod.rs` (paths relative to
//! `~/.cargo/registry/src/*/oracledb-26.0.0-beta.3/`):
//!
//! - `Client::receive_data_packet` (`client/mod.rs:134-150`): a socket read
//!   that times out (`err.is_call_timeout_exceeded()`) calls
//!   `recover_from_error` instead of failing outright.
//! - `Client::recover_from_error` (`client/mod.rs:214-219`): sends
//!   `MARKER_TYPE_INTERRUPT` (`constants.rs:143`, value 3), then calls `reset`,
//!   mapping any failure of *either* step to `unrecoverable_error` — and then
//!   still returns the **original** timeout error.
//! - `Client::reset` (`client/mod.rs:226-237`): sends `MARKER_TYPE_RESET`
//!   (`constants.rs:142`, value 2) and loops reading packets from
//!   `self.transport` **with whatever read timeout is already armed** — there
//!   is no re-arming to a fresh/longer window before this read. That read
//!   timeout is the same one `Statement::with_deadline` set
//!   (`crates/drivers/oracle-thin/src/conn.rs:795-800` calls
//!   `set_call_timeout`, which is `transport.set_read_timeout`,
//!   `client/mod.rs:853-858`).
//! - `Client::unrecoverable_error` (`client/mod.rs:275-278`): closes the
//!   transport and returns a fatal error. This is what turns a fired deadline
//!   into a dead connection.
//!
//! So the deciding factor is never "was the server sleeping or computing" in
//! the abstract — it is narrower: **can the server get back to checking its
//! input stream and answer the reset marker within one more full deadline
//! window** after the interrupt lands? A `dbms_session.sleep` cannot, because
//! it is parked in a timed wait unrelated to the client socket. A tight
//! PL/SQL loop or a row-generating join *can*, in principle, notice an
//! interrupt between iterations the way Ctrl-C does in SQL*Plus — whether
//! `oracledb`'s in-band marker actually reaches that check while the CPU is
//! busy is exactly what scenarios 3-5 measure.
//!
//! Also relevant: `crates/drivers/oracle-thin/src/conn.rs:610-616` — the
//! deadline is a **socket read timeout**, not a wall-clock end time, so it is
//! re-applied in full on every round trip (every `fetch_batch`, not just
//! `execute`). Scenario 5 depends on this: several cheap rows can be fetched
//! successfully before a later, expensive row's round trip is the one that
//! exceeds the deadline.
//!
//! ## Canaries for U-6/U-7
//!
//! `docs/exec-plans/active/oracledb-upgrade-checklist.md` §2 points at two
//! tests here as the deterministic replacement for its former "manual check"
//! entries: `scenario_1_control_a_sleep_10_dies_as_the_issue_describes`
//! (U-6: a fired deadline on suspended server work costs the session —
//! `NetworkLost`, and the session is independently confirmed unusable, not
//! just self-reported) and `scenario_3_cpu_bound_plsql_loop` (the contrasting
//! case this file exists to add: a fired deadline on CPU-bound server work
//! does not — `Timeout`, session independently confirmed usable). Both assert
//! on the real invariant (a follow-up `SELECT 1 FROM dual`), not only on the
//! error's own self-reported kind, and neither asserts a timing upper bound.
//! These stayed in this file rather than moving into `canary_upstream_live.rs`
//! because that file's own header commits it to driving **raw `oracledb`**,
//! never `OracleThinDriver` — the opposite of every test in this file, which
//! exists to measure the wrapper's `Statement::with_deadline` contract.
//!
//! ## Method for "did the server-side statement keep running"
//!
//! A privileged control connection (`system_params()`, the same `SYSTEM`
//! account S4 uses for `ALTER SYSTEM CANCEL SQL`) polls
//! `v$session.status` for the worker's `client_info` tag roughly every 250ms,
//! starting when the call starts and continuing until the worker's call has
//! returned **and** the status has left `ACTIVE`, or a 25s safety bound trips.
//! This is read-only — no `ALTER SYSTEM` statement is issued anywhere in this
//! file. It also serves the "leave nothing running" requirement: every
//! scenario function waits for the polled status to settle before moving on.

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "probe results are measurements a human reads from the test output"
)]

mod common;

use std::num::NonZeroUsize;
use std::thread;
use std::time::{Duration, Instant};

use common::{connect, exec, exec_quietly, measurement, observation, scalar, system_params, try_connect, unique};
use reldex_db_driver_api::{
    ConnectionParams, DatabaseConnection, DbError, ErrorKind, NativeError, SessionState, Statement,
};

/// Minimum number of runs per scenario the task asks for.
const RUNS: usize = 3;

/// The call timeout armed for every scenario except the sanity check (6).
const SHORT_DEADLINE: Duration = Duration::from_secs(2);

/// Runs a query statement to its first batch, the way a UI would drive a
/// result grid. Mirrors `s4_cancel.rs`'s helper of the same name: `execute`
/// only describes (this driver asks for zero prefetched rows), so a query's
/// actual server-side work happens on the first `fetch_batch`.
fn run_to_first_batch(connection: &mut dyn DatabaseConnection, statement: &Statement) -> Result<(), DbError> {
    let mut outcome = connection.execute(statement)?;
    let Some(mut cursor) = outcome.take_cursor() else {
        return Ok(());
    };
    cursor.fetch_batch(NonZeroUsize::MIN)?;
    cursor.close()
}

/// Whether the connection still works, checked exactly as the task asks:
/// `SELECT 1 FROM dual`.
fn still_usable(connection: &mut dyn DatabaseConnection) -> Result<(), DbError> {
    connection.ping()?;
    let value = scalar(connection, "SELECT 1 FROM dual");
    assert_eq!(value, "1");
    Ok(())
}

fn describe(error: &DbError) -> String {
    format!(
        "kind {:?}, session {:?}, native {:?}: {}",
        error.kind(),
        error.session_state(),
        error.native().map(NativeError::code),
        error.message()
    )
}

fn usable_text(result: &Result<(), DbError>) -> String {
    match result {
        Ok(()) => "usable".to_owned(),
        Err(error) => format!("NOT usable ({})", describe(error)),
    }
}

/// A fired deadline must report one of the two honest kinds, with the session
/// state the contract promises for each (`s4_cancel.rs` establishes the same
/// invariant). This is the one structural assertion every erroring scenario
/// shares; which of the two it picks is exactly the open question.
fn assert_deadline_kind_is_honest(error: &DbError) {
    match error.kind() {
        ErrorKind::Timeout => assert_eq!(
            error.session_state(),
            SessionState::NeedsValidation,
            "Timeout must report NeedsValidation, not something stronger or weaker"
        ),
        ErrorKind::NetworkLost => assert_eq!(
            error.session_state(),
            SessionState::Lost,
            "NetworkLost must report Lost"
        ),
        other => panic!("a fired deadline must be Timeout or NetworkLost, not {other:?}: {error}"),
    }
}

/// Polls `v$session.status` for `tag` on its own control connection until
/// `worker` has returned and the status has left `ACTIVE`, or 25s have
/// passed. Read-only: no `ALTER SYSTEM` statement is issued. Returns `(time
/// since polling started, status)` samples; empty when no privileged
/// credentials are configured or the control connection could not be made.
fn poll_server_status<T>(
    control_params: &Option<ConnectionParams>,
    tag: &str,
    worker: &thread::JoinHandle<T>,
) -> Vec<(Duration, String)> {
    let mut history = Vec::new();
    let Some(params) = control_params else {
        return history;
    };
    let Ok(mut control) = try_connect(params) else {
        return history;
    };
    let poll_start = Instant::now();
    loop {
        let status = scalar(
            control.as_mut(),
            &format!("SELECT NVL(MAX(status), 'GONE') FROM v$session WHERE client_info = '{tag}'"),
        );
        let elapsed = poll_start.elapsed();
        let settled = worker.is_finished() && status != "ACTIVE";
        history.push((elapsed, status));
        if settled || elapsed > Duration::from_secs(25) {
            break;
        }
        thread::sleep(Duration::from_millis(250));
    }
    let _ = control.close();
    history
}

/// Renders when (if ever) the polled status left `ACTIVE`, for the printed
/// report.
fn server_status_summary(history: &[(Duration, String)]) -> String {
    if history.is_empty() {
        return "not observed (no privileged control connection)".to_owned();
    }
    match history.iter().find(|(_, status)| status != "ACTIVE") {
        Some((left_at, status)) => {
            format!("left ACTIVE after {left_at:.1?} (now {status}); {} samples", history.len())
        }
        None => {
            let last = history.last().expect("checked non-empty above");
            format!("still ACTIVE after {:.1?} when observation stopped; {} samples", last.0, history.len())
        }
    }
}

/// One measured attempt at `sql` with `deadline` armed, plus what the
/// privileged control connection saw happening server-side across the same
/// span. `want_cursor` distinguishes a query (execute + first fetch) from a
/// non-query PL/SQL call (plain `execute`).
struct Probe {
    error: Option<DbError>,
    elapsed: Duration,
    still_usable_after: Result<(), DbError>,
    server_status: Vec<(Duration, String)>,
}

fn probe_call(sql: &'static str, deadline: Duration, want_cursor: bool) -> Probe {
    let control_params = system_params();
    let tag = unique("probe23");
    let worker_tag = tag.clone();
    let worker = thread::Builder::new()
        .spawn(move || {
            let mut connection = connect();
            exec(
                connection.as_mut(),
                &format!("BEGIN DBMS_APPLICATION_INFO.SET_CLIENT_INFO('{worker_tag}'); END;"),
            );
            let statement = Statement::new(sql).with_deadline(deadline);
            let started = Instant::now();
            let result = if want_cursor {
                run_to_first_batch(connection.as_mut(), &statement)
            } else {
                connection.execute(&statement).map(|_| ())
            };
            let elapsed = started.elapsed();
            let (error, still_usable_after) = match result {
                Ok(()) => (None, still_usable(connection.as_mut())),
                Err(error) => {
                    let after = still_usable(connection.as_mut());
                    (Some(error), after)
                }
            };
            let _ = connection.close();
            (error, elapsed, still_usable_after)
        })
        .expect("spawn probe worker");

    let server_status = poll_server_status(&control_params, &tag, &worker);
    let (error, elapsed, still_usable_after) = worker.join().expect("probe worker must not panic");
    Probe { error, elapsed, still_usable_after, server_status }
}

fn report(scenario: &str, run: usize, probe: &Probe) {
    let outcome = probe.error.as_ref().map_or_else(|| "no error (call succeeded)".to_owned(), describe);
    measurement(&format!("probe23.{scenario}.elapsed"), format!("{:.1?}", probe.elapsed));
    observation(format!(
        "{scenario} run {run}/{RUNS}: {outcome}; session afterwards: {}; server-side: {}",
        usable_text(&probe.still_usable_after),
        server_status_summary(&probe.server_status),
    ));
}

// ---------------------------------------------------------------------------
// Scenario 1 — control A: `dbms_session.sleep(10)`, 2s deadline. The issue's
// own repro; expected to destroy the connection.
// ---------------------------------------------------------------------------

#[test]
fn scenario_1_control_a_sleep_10_dies_as_the_issue_describes() {
    for run in 1..=RUNS {
        let probe = probe_call("BEGIN DBMS_SESSION.SLEEP(10); END;", SHORT_DEADLINE, false);
        report("control_a_sleep10", run, &probe);
        let error = probe
            .error
            .as_ref()
            .unwrap_or_else(|| panic!("run {run}: sleep(10) with a 2s deadline must fire; it did not"));
        assert_deadline_kind_is_honest(error);
        assert!(
            probe.elapsed >= SHORT_DEADLINE,
            "run {run}: a fired deadline should take at least the deadline itself"
        );
        // Canary for U-6/U-7 (`oracledb-upgrade-checklist.md` §2): a
        // suspended server must leave the session genuinely unusable, not
        // merely self-reported as `Lost` — this is the independent
        // `SELECT 1 FROM dual` check, not the error's own claim.
        assert!(
            probe.still_usable_after.is_err(),
            "run {run}: NetworkLost must leave the session genuinely unusable, not just \
             self-reported — but `SELECT 1 FROM dual` still succeeded"
        );
    }
}

// ---------------------------------------------------------------------------
// Scenario 2 — control B: `dbms_session.sleep(3)`, 2s deadline. The
// maintainer's prediction: plain call-timeout error, session survives.
// ---------------------------------------------------------------------------

#[test]
fn scenario_2_control_b_sleep_3_tests_the_maintainers_prediction() {
    for run in 1..=RUNS {
        let probe = probe_call("BEGIN DBMS_SESSION.SLEEP(3); END;", SHORT_DEADLINE, false);
        report("control_b_sleep3", run, &probe);
        let error = probe
            .error
            .as_ref()
            .unwrap_or_else(|| panic!("run {run}: sleep(3) with a 2s deadline must fire; it did not"));
        assert_deadline_kind_is_honest(error);
        assert!(
            probe.elapsed >= SHORT_DEADLINE,
            "run {run}: a fired deadline should take at least the deadline itself"
        );
    }
}

// ---------------------------------------------------------------------------
// Scenario 3 — CPU-bound PL/SQL, ~10s of pure computation, no sleep, no I/O.
// ---------------------------------------------------------------------------

const CPU_PLSQL_SQL: &str = "DECLARE t NUMBER := DBMS_UTILITY.GET_TIME; x NUMBER := 0; BEGIN LOOP x := x + 1; \
     EXIT WHEN DBMS_UTILITY.GET_TIME - t > 1000; END LOOP; END;";

#[test]
fn scenario_3_cpu_bound_plsql_loop() {
    for run in 1..=RUNS {
        let probe = probe_call(CPU_PLSQL_SQL, SHORT_DEADLINE, false);
        report("cpu_plsql", run, &probe);
        let error = probe
            .error
            .as_ref()
            .unwrap_or_else(|| panic!("run {run}: a 10s CPU loop with a 2s deadline must fire; it did not"));
        assert_deadline_kind_is_honest(error);
        assert!(
            probe.elapsed >= SHORT_DEADLINE,
            "run {run}: a fired deadline should take at least the deadline itself"
        );
        // Canary for U-6/U-7: CPU-bound work must leave the session
        // genuinely usable, not just self-reported — the independent
        // `SELECT 1 FROM dual` check, not the error's own claim.
        probe.still_usable_after.as_ref().unwrap_or_else(|error| {
            panic!(
                "run {run}: Timeout must leave the session genuinely usable, not just \
                 self-reported: {}",
                describe(error)
            )
        });
    }
}

// ---------------------------------------------------------------------------
// Scenario 4 — CPU-bound SQL that returns rows only at the end. Calibrated on
// this test database (see below) to run ~15s: a nested-loop cartesian over
// 500 rows of `all_objects` (63,587 rows) against the full table, counted by
// a single aggregate so nothing streams until the whole join is done. No
// sort/hash, so no meaningful temp space.
// ---------------------------------------------------------------------------

/// Calibration (sqlplus, `set timing on`, this container, 2026-09-23): 100
/// rows -> 3.5s, 500 rows -> 15.0-17.2s (three runs), 1000 rows -> 31.2s.
/// 500 was chosen for a comfortable 10-20s window.
const CPU_SQL: &str = "SELECT /*+ NO_PARALLEL */ COUNT(*) FROM \
     (SELECT /*+ NO_MERGE */ * FROM all_objects WHERE ROWNUM <= 500) x, all_objects b";

#[test]
fn scenario_4_cpu_bound_sql_returning_rows_only_at_the_end() {
    for run in 1..=RUNS {
        let probe = probe_call(CPU_SQL, SHORT_DEADLINE, true);
        report("cpu_sql", run, &probe);
        let error = probe
            .error
            .as_ref()
            .unwrap_or_else(|| panic!("run {run}: a ~15s CPU join with a 2s deadline must fire; it did not"));
        assert_deadline_kind_is_honest(error);
        assert!(
            probe.elapsed >= SHORT_DEADLINE,
            "run {run}: a fired deadline should take at least the deadline itself"
        );
    }
}

// ---------------------------------------------------------------------------
// Scenario 5 — a streaming SELECT whose rows arrive at very different speeds:
// the first 5 rows cost ~0.05s of CPU each (fetch comfortably inside the 2s
// deadline), rows 6+ cost ~3s of CPU each (comfortably over it). `fetch_rows`
// is pinned to 1 so every row is its own round trip, and the deadline —
// which this driver re-arms as a plain socket read timeout on every
// `fetch_batch`, not once for the whole result set (`conn.rs:610-616`) —
// gets a fresh 2s budget each time. This asks whether the interrupt lands
// differently when it fires on a fetch that already delivered rows, versus
// firing on the very first round trip (scenarios 1-4).
// ---------------------------------------------------------------------------

struct StreamProbe {
    rows_before_error: usize,
    error: Option<DbError>,
    elapsed: Duration,
    still_usable_after: Result<(), DbError>,
    server_status: Vec<(Duration, String)>,
}

fn probe_streaming(sql: String, deadline: Duration, max_rows_to_try: usize) -> StreamProbe {
    let control_params = system_params();
    let tag = unique("probe23s");
    let worker_tag = tag.clone();
    let worker = thread::Builder::new()
        .spawn(move || {
            let mut connection = connect();
            exec(
                connection.as_mut(),
                &format!("BEGIN DBMS_APPLICATION_INFO.SET_CLIENT_INFO('{worker_tag}'); END;"),
            );
            let statement = Statement::new(sql).with_deadline(deadline).with_fetch_rows(NonZeroUsize::MIN);
            let started = Instant::now();
            let mut rows_before_error = 0_usize;
            let mut error = None;
            match connection.execute(&statement) {
                Ok(mut outcome) => {
                    if let Some(mut cursor) = outcome.take_cursor() {
                        for _ in 0..max_rows_to_try {
                            match cursor.fetch_batch(NonZeroUsize::MIN) {
                                Ok(batch) if batch.row_count() > 0 => rows_before_error += 1,
                                Ok(_) => break,
                                Err(err) => {
                                    error = Some(err);
                                    break;
                                }
                            }
                        }
                        let _ = cursor.close();
                    }
                }
                Err(err) => error = Some(err),
            }
            let elapsed = started.elapsed();
            let still_usable_after = still_usable(connection.as_mut());
            let _ = connection.close();
            (rows_before_error, error, elapsed, still_usable_after)
        })
        .expect("spawn stream worker");

    let server_status = poll_server_status(&control_params, &tag, &worker);
    let (rows_before_error, error, elapsed, still_usable_after) =
        worker.join().expect("stream worker must not panic");
    StreamProbe { rows_before_error, error, elapsed, still_usable_after, server_status }
}

#[test]
fn scenario_5_streaming_select_with_slow_rows() {
    for run in 1..=RUNS {
        let function = unique("probe23_row");
        let mut setup = connect();
        exec(
            setup.as_mut(),
            &format!(
                "CREATE OR REPLACE FUNCTION {function}(p IN NUMBER) RETURN NUMBER AS \
                 t NUMBER := DBMS_UTILITY.GET_TIME; \
                 budget NUMBER := CASE WHEN p <= 5 THEN 5 ELSE 300 END; \
                 BEGIN LOOP EXIT WHEN DBMS_UTILITY.GET_TIME - t > budget; END LOOP; RETURN p; END;"
            ),
        );
        setup.close().expect("close setup connection");

        let sql = format!("SELECT {function}(LEVEL) FROM dual CONNECT BY LEVEL <= 10");
        let probe = probe_streaming(sql, SHORT_DEADLINE, 10);

        measurement("probe23.streaming.rows_before_error", probe.rows_before_error);
        measurement("probe23.streaming.elapsed", format!("{:.1?}", probe.elapsed));
        observation(format!(
            "streaming run {run}/{RUNS}: {} rows fetched before {}; session afterwards: {}; server-side: {}",
            probe.rows_before_error,
            probe.error.as_ref().map_or_else(|| "no error (all 10 rows arrived)".to_owned(), describe),
            usable_text(&probe.still_usable_after),
            server_status_summary(&probe.server_status),
        ));

        let mut cleanup = connect();
        exec_quietly(cleanup.as_mut(), &format!("DROP FUNCTION {function}"));
        let _ = cleanup.close();

        // The cheap rows (1-5, ~0.05s each) must not be starved by the 2s
        // deadline — only the invariant we are sure of, not which row number
        // the failure lands on (server load can shift it by one or two).
        if let Some(error) = &probe.error {
            assert_deadline_kind_is_honest(error);
            assert!(
                probe.rows_before_error >= 1,
                "run {run}: not even the first cheap row was fetched before the deadline fired \
                 ({} rows, {})",
                probe.rows_before_error,
                describe(error)
            );
        } else {
            observation(format!(
                "streaming run {run}/{RUNS}: all 10 rows arrived without the deadline ever firing \
                 mid-fetch — re-check the per-row budget if this recurs"
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Scenario 6 — sanity check: the same CPU-bound PL/SQL loop as scenario 3,
// with a 15s deadline against ~10s of work. No error expected.
// ---------------------------------------------------------------------------

#[test]
fn scenario_6_generous_deadline_on_cpu_bound_work_is_unaffected() {
    for run in 1..=RUNS {
        let probe = probe_call(CPU_PLSQL_SQL, Duration::from_secs(15), false);
        report("cpu_plsql_generous_deadline", run, &probe);
        assert!(
            probe.error.is_none(),
            "run {run}: a 15s deadline on ~10s of CPU work must not fire: {}",
            probe.error.as_ref().map_or_else(String::new, describe)
        );
        probe
            .still_usable_after
            .as_ref()
            .unwrap_or_else(|error| panic!("run {run}: session should be unaffected: {}", describe(error)));
    }
}
