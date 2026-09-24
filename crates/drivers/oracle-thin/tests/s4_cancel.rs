//! Spike S4 — cancelling a running statement (ADR-0001, re-scoped).
//!
//! This is the spike the driver decision hangs on. `SPEC.md` §10 and §24.8
//! require that a user can stop a statement they started, and that the session
//! survives it. The four candidates are evaluated **in order**, and each one is
//! measured rather than reasoned about:
//!
//! 1. a deadline armed *before* the call (`set_call_timeout`);
//! 2. a privileged control session issuing `ALTER SYSTEM CANCEL SQL`;
//! 3. what a public upstream break API would need (source study; the drafted
//!    issue text is in `docs/exec-plans/active/phase-0-spike-results.md`);
//! 4. a minimal fork exposing a break handle (assessment, also in that file).
//!
//! Candidates 3 and 4 produce no test: there is nothing to run until the API
//! exists. Candidates 1 and 2 are exercised here against the live database.
//!
//! Two kinds of long-running statement are used deliberately, because they
//! behave differently and the difference is the finding: a **SQL** statement,
//! which the server can interrupt at a fetch boundary, and a **PL/SQL** sleep,
//! which it cannot interrupt until the sleep is over.

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "spike results are measurements a human reads from the test output"
)]

mod common;

use std::num::NonZeroUsize;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use common::{
    DSN, PASSWORD, USER, connect, exec, measurement, observation, params, scalar, setting,
    system_params, try_connect, unique,
};
use reldex_db_driver_api::{
    CancelKind, CancelOutcome, DatabaseConnection, DbError, ErrorKind, NativeError, SessionState,
    Statement,
};

/// A SQL statement that keeps the server busy for many minutes and that the
/// server can interrupt at a row-source boundary. A three-way cartesian join
/// over the data dictionary: it allocates almost nothing, so it runs until it
/// is stopped rather than failing on memory the way a huge `CONNECT BY` does
/// (measured: `CONNECT BY LEVEL <= 2e9` died with ORA-30009 after 1.3s).
const LONG_SQL: &str =
    "SELECT /*+ NO_PARALLEL */ COUNT(*) FROM all_objects a, all_objects b, all_objects c";

/// A PL/SQL sleep. The server will not look at an interrupt until it wakes up,
/// which is what makes this the harder case.
const LONG_PLSQL: &str = "BEGIN DBMS_SESSION.SLEEP(20); END;";

/// Runs a statement and, for a query, drives its first batch.
///
/// **A query's work happens on the fetch.** This driver describes before it
/// fetches — it asks `oracledb` for zero prefetched rows so a select list it
/// cannot decode safely can be refused before any value is decoded (the U-3
/// mitigation) — so `execute` returns as soon as the server has described the
/// result, and the row source only starts producing when rows are asked for. A
/// cancellation test that stopped at `execute` would be timing the describe
/// rather than the statement, which is why every long-SQL case here goes
/// through this helper. For PL/SQL, which produces no cursor, it is exactly
/// `execute`.
fn run_to_first_batch(
    connection: &mut dyn DatabaseConnection,
    statement: &Statement,
) -> Result<(), DbError> {
    let mut outcome = connection.execute(statement)?;
    let Some(mut cursor) = outcome.take_cursor() else {
        return Ok(());
    };
    cursor.fetch_batch(NonZeroUsize::MIN)?;
    cursor.close()
}

/// Reports whether a connection still works after something was done to it.
fn still_usable(connection: &mut dyn DatabaseConnection) -> Result<(), DbError> {
    connection.ping()?;
    let value = scalar(connection, "SELECT 7 FROM dual");
    assert_eq!(value, "7");
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

// ---------------------------------------------------------------------------
// Candidate 1 — a deadline armed before the call
// ---------------------------------------------------------------------------

#[test]
fn a_deadline_stops_a_long_sql_statement_and_reports_the_session_honestly() {
    // **Which outcome this produces is not deterministic, and that is the
    // finding.** U-6: when the deadline fires, `Client::recover_from_error`
    // sends an interrupt marker and then reads the reset reply from the same
    // socket *with the expired timeout still armed*. Whether the server answers
    // inside another full window decides everything:
    //
    // - answered in time → `Timeout`, and the session survives;
    // - not answered     → a second timeout, `unrecoverable_error` closes the
    //                      transport, and the caller gets `NetworkLost`.
    //
    // The 2-second deadline and the 2- or 4-second stop below are the two
    // shapes, and the second one has been observed on a **serial** run against
    // an idle container, not only under load. So this test asserts what the
    // driver owes the caller either way — the statement stops, and the reported
    // session state matches reality — and records which way it went. Asserting
    // the happy outcome would be asserting a coin flip, and Reldex must not
    // build on one: this is why `SPEC.md` §24.8 is NO-GO on this upstream
    // version.
    let mut connection = connect();
    let deadline = Duration::from_secs(2);

    let started = Instant::now();
    let error = match run_to_first_batch(
        connection.as_mut(),
        &Statement::new(LONG_SQL).with_deadline(deadline),
    ) {
        Ok(()) => panic!("the deadline did not stop the statement"),
        Err(error) => error,
    };
    let elapsed = started.elapsed();

    measurement("s4.sql_deadline_armed", format!("{deadline:.1?}"));
    measurement("s4.sql_deadline_stopped_after", format!("{elapsed:.1?}"));
    measurement(
        "s4.sql_deadline_overshoot",
        format!("{:.1?}", elapsed.saturating_sub(deadline)),
    );
    observation(format!("SQL deadline fired -> {}", describe(&error)));
    assert!(
        elapsed < Duration::from_secs(30),
        "the statement was not stopped at all"
    );

    match error.kind() {
        ErrorKind::Timeout => {
            // A deadline the caller armed is a timeout, not a cancellation
            // someone requested, and the session is left needing validation
            // rather than declared lost.
            assert_eq!(error.session_state(), SessionState::NeedsValidation);
            still_usable(connection.as_mut()).unwrap_or_else(|error| {
                panic!(
                    "the driver reported Timeout, which promises a recoverable \
                     session, but the session did not survive: {}",
                    describe(&error)
                )
            });
            observation(
                "recovery completed inside the remaining window: the session survived \
                 its own deadline and is usable",
            );
        }
        ErrorKind::NetworkLost => {
            // U-6's other outcome. The driver must not dress it up: a lost
            // session is reported as lost, so `db-core` can surface the lost
            // transaction instead of silently opening a new one
            // (`SPEC.md` §18).
            assert_eq!(error.session_state(), SessionState::Lost);
            assert!(error.session_may_be_unusable());
            assert!(
                elapsed >= deadline,
                "a lost session should follow at least one full timeout"
            );
            observation(
                "UPSTREAM GAP U-6: recovery timed out as well, so the connection was \
                 closed and the deadline cost the session. Reported as NetworkLost / \
                 Lost, which is the honest answer",
            );
        }
        other => panic!("a fired deadline must be Timeout or NetworkLost, not {other:?}: {error}"),
    }

    let _ = connection.close();
}

#[test]
fn a_deadline_on_a_plsql_sleep_usually_destroys_the_session() {
    // This is the case that breaks, and **how often** it breaks is not fixed.
    // The server will not answer the interrupt marker until the sleep is over,
    // so upstream's `recover_from_error` — which reads the reset reply from the
    // same socket with the same timeout still armed — usually times out as
    // well, `unrecoverable_error` closes the transport, and the caller loses
    // the session instead of getting a timeout. Sometimes the recovery lands
    // inside the remaining window and the session survives.
    //
    // This test used to assert `NetworkLost` outright. That assertion was
    // **flaky**, which is worse than either outcome: measured on 2026-09-19,
    // three runs of this test on its own gave `NetworkLost` three times, while
    // two runs of the whole file serially gave `NetworkLost` once and `Timeout`
    // once — the earlier tests in the file leave the container busy enough to
    // flip it. The results file's claim that "serial runs are stable" was true
    // for the rest of the file and not for this test. A re-run is not evidence,
    // so what is asserted here is the invariant that must hold **either way**:
    // the call comes back, it is classified as one of the two honest kinds, and
    // the session state it reports matches what the session actually does.
    let mut connection = connect();
    let deadline = Duration::from_secs(2);

    let started = Instant::now();
    let error = match connection.execute(&Statement::new(LONG_PLSQL).with_deadline(deadline)) {
        Ok(_) => panic!("the deadline did not stop the sleep"),
        Err(error) => error,
    };
    let elapsed = started.elapsed();

    measurement("s4.plsql_deadline_stopped_after", format!("{elapsed:.1?}"));
    observation(format!("PL/SQL deadline fired -> {}", describe(&error)));
    assert!(
        elapsed < Duration::from_secs(20),
        "the sleep ran to completion"
    );
    assert!(
        elapsed >= deadline,
        "the call returned before its own deadline"
    );

    match error.kind() {
        ErrorKind::NetworkLost => {
            assert_eq!(error.session_state(), SessionState::Lost);
            let after = still_usable(connection.as_mut())
                .expect_err("a session reported Lost must not still work");
            observation(format!(
                "UPSTREAM GAP U-6, usual outcome: the session did NOT survive the PL/SQL \
                 deadline: {}",
                describe(&after)
            ));
        }
        ErrorKind::Timeout => {
            assert_eq!(error.session_state(), SessionState::NeedsValidation);
            still_usable(connection.as_mut()).unwrap_or_else(|error| {
                panic!(
                    "the driver reported Timeout, which promises a recoverable session, \
                     but the session did not survive: {}",
                    describe(&error)
                )
            });
            observation(
                "UPSTREAM GAP U-6, the less common outcome: recovery landed inside the \
                 remaining window and the session survived its own deadline. Load \
                 decides which of the two a user gets, which is the whole problem",
            );
        }
        other => panic!("a fired deadline must be Timeout or NetworkLost, not {other:?}: {error}"),
    }
    let _ = connection.close();
}

#[test]
fn a_statement_that_finishes_inside_its_deadline_is_unaffected() {
    let mut connection = connect();
    let outcome = connection
        .execute(&Statement::new("SELECT 1 FROM dual").with_deadline(Duration::from_secs(10)))
        .expect("a fast statement must not be disturbed by a generous deadline");
    assert!(outcome.has_cursor());
    drop(outcome);

    // And the armed deadline does not leak into the next statement.
    //
    // **House rule: no absolute timing bounds — and this one was worse than
    // that, it was a test defect, not a driver bug.** Investigated 2026-09-24
    // after this assertion (previously `elapsed >= Duration::from_secs(3)`
    // around a bare `DBMS_SESSION.SLEEP(3)`) failed 3/5 whole-file runs. Two
    // problems, found by instrumenting the old assertion to print the actual
    // elapsed time before removing it:
    //
    // 1. **The pairing could never have caught the bug it claims to test.**
    //    The statement above arms a 10s deadline; even if that value leaked
    //    into the call below completely unnoticed, 10s is longer than the 3s
    //    this sleep asked for, so "leaked" and "correctly cleared" were
    //    observationally identical — both let a 3s sleep finish normally. A
    //    leak can only be caught by pairing a *shorter* deadline with a
    //    *longer*, undeadlined call after it, so a real leak produces an
    //    unambiguous error instead of a coincidence of timing.
    // 2. **The boundary had no margin against real timer behaviour, solo or
    //    not.** `DBMS_SESSION.SLEEP(3)`, measured client-side on this
    //    container: 5/5 solo runs (`--test-threads=1`, this test alone, no
    //    contention possible) landed at 2.985-2.999s and failed the old
    //    assertion every time; 2/2 whole-file runs landed at 3.004s and
    //    passed. No run in either mode ever produced an error — only a clean
    //    completion a few milliseconds either side of the 3.000s line. That
    //    is deterministic timer-precision straddling a boundary with ~0
    //    margin, not "the rest of the suite" adding load-dependent noise: the
    //    solo runs alone falsify the original comment's theory, since nothing
    //    else was running to leak anything into them.
    //
    // The driver itself is not implicated: `apply_deadline`
    // (`crates/drivers/oracle-thin/src/conn.rs:795-802`) is called
    // unconditionally on every `execute()` with `statement.deadline()` and
    // disarms to `None` before a call that asks for none, and no timeout
    // error was observed in any of the 7 runs above — a genuinely leaked
    // *shorter* deadline could not produce a clean early success, only an
    // error.
    //
    // So: arm a short deadline that the first statement still finishes well
    // inside, then run something *longer* than that deadline with none of its
    // own armed. If the short deadline leaked, the longer call fails long
    // before it would otherwise finish — an unambiguous error — which needs
    // no wall-clock lower bound at all.
    let leak_probe_deadline = Duration::from_secs(1);
    let outcome = connection
        .execute(&Statement::new("SELECT 1 FROM dual").with_deadline(leak_probe_deadline))
        .expect("a fast statement must not be disturbed by a deadline it finishes well inside");
    assert!(outcome.has_cursor());
    drop(outcome);

    let leak_probe_started = Instant::now();
    connection
        .execute(&Statement::new("BEGIN DBMS_SESSION.SLEEP(5); END;"))
        .unwrap_or_else(|error| {
            panic!(
                "the previous statement's {leak_probe_deadline:?} deadline leaked into this \
                 unrelated, undeadlined 5s sleep and cut it short after {:.3?}: {error}",
                leak_probe_started.elapsed()
            )
        });
    measurement(
        "s4.leak_probe_undeadlined_sleep5_elapsed",
        format!("{:.3?}", leak_probe_started.elapsed()),
    );
    observation(format!(
        "a {leak_probe_deadline:?} deadline on the previous statement did not leak into an \
         unrelated, undeadlined 5s sleep after it (no error, whatever it took)"
    ));
    connection.close().expect("close");
}

#[test]
fn the_cancel_handle_answers_immediately_while_a_statement_is_running() {
    // The contract's rule is that `request_cancel` must never block on the
    // connection's own call lock. This calls it repeatedly *while* a statement
    // is running: it must answer at once, and it must say that the statement
    // cannot be interrupted rather than pretend a cancel was sent.
    let mut connection = connect();
    let handle = connection.cancel_handle();
    assert_eq!(handle.kind(), CancelKind::PreArmedDeadline);

    let barrier = Arc::new(Barrier::new(2));
    let probe_barrier = Arc::clone(&barrier);
    let probe = thread::spawn(move || {
        probe_barrier.wait();
        let mut worst = Duration::ZERO;
        let mut last = None;
        for _ in 0..20 {
            let started = Instant::now();
            let outcome = handle.request_cancel().expect("never fails");
            worst = worst.max(started.elapsed());
            last = Some(outcome);
            thread::sleep(Duration::from_millis(50));
        }
        (worst, last)
    });

    barrier.wait();
    let result =
        connection.execute(&Statement::new(LONG_PLSQL).with_deadline(Duration::from_millis(1500)));
    let (worst, last) = probe.join().expect("the probe thread must not panic");

    assert!(
        result.is_err(),
        "the deadline should have stopped the sleep"
    );
    measurement("s4.request_cancel_worst_case", format!("{worst:.1?}"));
    assert!(
        worst < Duration::from_millis(20),
        "request_cancel blocked for {worst:.1?}; it must never wait on the call lock"
    );
    match last.expect("at least one call") {
        CancelOutcome::NotInterruptible { deadline_remaining } => {
            observation(format!(
                "while the statement ran, request_cancel answered NotInterruptible \
                 with {deadline_remaining:?} left, in at most {worst:.1?}"
            ));
        }
        other => panic!("this driver must never claim to have requested a cancel: {other:?}"),
    }

    let _ = connection.close();
}

// ---------------------------------------------------------------------------
// Candidate 1, the other half — why a cancel cannot be delivered on demand
// ---------------------------------------------------------------------------

#[test]
fn asking_the_upstream_client_to_arm_a_deadline_mid_call_blocks_until_the_call_ends() {
    // `oracledb::Connection::set_call_timeout` takes `&self` and does
    // `self.client_ref.lock().unwrap()`. `Client::perform_round_trip` holds
    // that same mutex for the whole request/response cycle. So arming a
    // deadline *after* a statement has started cannot take effect until the
    // statement is over — which is precisely why this driver reports
    // `CancelKind::PreArmedDeadline` and not `Native`.
    //
    // This is the one test that uses the upstream crate directly, because the
    // fact being measured is invisible through this crate's own API.
    let config = oracledb::Config::default()
        .set_connect_string(&setting(DSN))
        .expect("a valid connect string")
        .set_credentials(&setting(USER), &setting(PASSWORD));
    let connection = Arc::new(oracledb::connect(config).expect("connect"));

    let sleeping = Arc::clone(&connection);
    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let worker = thread::spawn(move || {
        worker_barrier.wait();
        let started = Instant::now();
        let _ = sleeping.execute("BEGIN DBMS_SESSION.SLEEP(5); END;", &[]);
        started.elapsed()
    });

    barrier.wait();
    // Give the round trip time to take the lock.
    thread::sleep(Duration::from_millis(500));
    let started = Instant::now();
    let armed = connection.set_call_timeout(Some(Duration::from_millis(100)));
    let blocked_for = started.elapsed();
    let ran_for = worker.join().expect("the worker thread must not panic");
    let _ = connection.set_call_timeout(None);

    measurement(
        "s4.set_call_timeout_blocked_for",
        format!("{blocked_for:.1?}"),
    );
    measurement("s4.sleep_ran_for", format!("{ran_for:.1?}"));
    assert!(armed.is_ok());
    assert!(
        blocked_for > Duration::from_secs(3),
        "set_call_timeout returned after {blocked_for:.1?}; the round-trip lock was \
         expected to hold it for the rest of the 5-second statement"
    );
    observation(format!(
        "arming a deadline mid-call blocked for {blocked_for:.1?} — the whole \
         remainder of the statement. There is no way to deliver a cancel to a \
         running call through the public API"
    ));
}

// ---------------------------------------------------------------------------
// Candidate 2 — a privileged control session
// ---------------------------------------------------------------------------

/// Runs `sql` on a tagged session and reports what stopped it.
struct ControlResult {
    /// What the statement's own connection saw, if anything.
    client_error: Option<DbError>,
    /// How long the statement's `execute` call took to return.
    ran_for: Duration,
    /// Whether that connection was usable afterwards.
    after: Result<(), DbError>,
    /// How long the server took to stop the call, as `V$SESSION` reports it.
    server_stopped_after: Option<Duration>,
}

fn cancel_from_a_control_session(
    sql: &'static str,
    control_statement: fn(&str) -> String,
) -> Option<ControlResult> {
    let control_params = system_params()?;
    let tag = unique("s4tag");
    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let worker_tag = tag.clone();

    let worker = thread::spawn(move || {
        let mut connection = match try_connect(&params()) {
            Ok(connection) => connection,
            Err(error) => panic!("worker connect failed: {error}"),
        };
        // Tag the session so the control session can find it without guessing.
        exec(
            connection.as_mut(),
            &format!("BEGIN DBMS_APPLICATION_INFO.SET_CLIENT_INFO('{worker_tag}'); END;"),
        );
        worker_barrier.wait();
        let started = Instant::now();
        // A generous deadline is armed purely so a failed test cannot hang the
        // suite; it is far longer than the cancel should take.
        let result = run_to_first_batch(
            connection.as_mut(),
            &Statement::new(sql).with_deadline(SAFETY_DEADLINE),
        );
        let elapsed = started.elapsed();
        let after = still_usable(connection.as_mut());
        let _ = connection.close();
        (result.err(), elapsed, after)
    });

    let mut control = match try_connect(&control_params) {
        Ok(connection) => connection,
        Err(error) => panic!("privileged connect failed: {error}"),
    };
    barrier.wait();
    // Long enough that the statement is genuinely executing, not still being
    // parsed: `ALTER SYSTEM CANCEL SQL` only affects the SQL running *now*.
    thread::sleep(Duration::from_secs(3));

    // The SQL_ID is included: `ALTER SYSTEM CANCEL SQL` names a cursor, and
    // without it the statement is accepted but nothing is cancelled (measured).
    let target = scalar(
        control.as_mut(),
        &format!(
            "SELECT s.sid || ',' || s.serial# || ',@' || i.instance_number || ',' || s.sql_id \
             FROM v$session s CROSS JOIN v$instance i \
             WHERE s.client_info = '{tag}' AND s.status = 'ACTIVE' AND s.sql_id IS NOT NULL"
        ),
    );
    assert!(
        target.contains(','),
        "the worker session was not found by its tag; got {target}"
    );
    observation(format!("found the running session as {target}"));

    let issued = Instant::now();
    // `ALTER SYSTEM KILL SESSION` raises ORA-00031 ("session marked for kill")
    // when the session cannot be ended at once, which is a normal outcome and
    // not a failure of the test.
    let control_sql = control_statement(&target);
    if let Err(error) = control.execute(&Statement::new(&control_sql)) {
        assert_eq!(
            error.native().map(NativeError::code),
            Some(31),
            "the control statement failed: {}",
            describe(&error)
        );
        observation("the control session reported ORA-00031: session marked for kill");
    }
    measurement(
        "s4.control_statement_returned_in",
        format!("{:.1?}", issued.elapsed()),
    );

    // Watch the server's own view: if the session stops being ACTIVE, the
    // cancel worked server-side and only the client failed to notice — a very
    // different conclusion from the cancel not working at all.
    let mut became_idle = None;
    for _ in 0..24 {
        thread::sleep(Duration::from_millis(500));
        let status = scalar(
            control.as_mut(),
            &format!(
                "SELECT NVL(MAX(status), 'GONE') FROM v$session                  WHERE client_info = '{tag}'"
            ),
        );
        if status != "ACTIVE" {
            became_idle = Some((issued.elapsed(), status));
            break;
        }
    }
    match &became_idle {
        Some((after, status)) => observation(format!(
            "server side: the session left ACTIVE after {after:.1?} (now {status})"
        )),
        None => {
            observation("server side: the session was still ACTIVE 15s after the control statement")
        }
    }
    control.close().expect("close");

    let (client_error, ran_for, after) = worker.join().expect("worker must not panic");
    Some(ControlResult {
        client_error,
        ran_for,
        after,
        server_stopped_after: became_idle.map(|(after, _)| after),
    })
}

/// A deadline armed on the worker purely so a failed test cannot hang the
/// suite. It is far longer than any cancel should take.
const SAFETY_DEADLINE: Duration = Duration::from_secs(20);

#[test]
fn a_privileged_cancel_reaches_the_server_but_not_the_client() {
    let Some(result) =
        cancel_from_a_control_session(LONG_SQL, |t| format!("ALTER SYSTEM CANCEL SQL '{t}'"))
    else {
        observation(
            "NOT RUN: no privileged credentials in the environment, so \
             ALTER SYSTEM CANCEL SQL was not exercised",
        );
        return;
    };

    measurement(
        "s4.cancel_sql_server_stopped_after",
        result
            .server_stopped_after
            .map_or_else(|| "never".to_owned(), |d| format!("{d:.1?}")),
    );
    measurement(
        "s4.cancel_sql_client_returned_after",
        format!("{:.1?}", result.ran_for),
    );

    // Half of this works. `ALTER SYSTEM CANCEL SQL` is accepted in a couple of
    // milliseconds and the server really does end the call: `V$SESSION` leaves
    // ACTIVE well before the client would ever find out on its own.
    //
    // **No absolute timing bound here — house rule: a fixed wall-clock upper
    // bound just moves the flake to a slower CI runner or a busier local DB.**
    // An earlier version of this assertion tried `stopped < 5s`, then
    // `stopped < 10s` after two back-to-back failures under
    // `tools/gates.sh --only db` on 2026-09-24 (see
    // `docs/exec-plans/active/phase-0-spike-results.md`, "S4 addendum
    // (2026-09-24)", for the related-but-distinct candidate-1 finding this is
    // NOT the same mechanism as — that one is the client's own pre-armed
    // deadline firing mid-statement, not an externally issued cancel).
    // Measured on this machine: run alone (`--test-threads=1`), `stopped` is a
    // steady ~1.5s; run as part of this file at cargo's default parallelism —
    // exactly what `tools/oracle-test-db/run-it.sh` and `tools/gates.sh
    // --only db` use — several of this file's *other* tests
    // (`a_deadline_stops_a_long_sql_statement_and_reports_the_session_honestly`,
    // `killing_a_session_is_a_different_thing_with_different_consequences`)
    // also drive CPU-bound work or their own `ALTER SYSTEM` command
    // concurrently against the same single-instance container, and the server
    // takes longer to notice and act on this test's own cancel: 6.1-6.6s
    // across 4 such runs (3 whole-file + 1 full `gates.sh --only db`). Both
    // absolute bounds (5s, then 10s) were just numbers that happened to have
    // margin over what had been measured so far.
    //
    // What the test actually needs to prove does not require a wall-clock
    // figure at all: the server really did end the call (`server_stopped_after`
    // being `Some` already proves that — it only becomes `Some` inside the
    // polling loop's own 12s observation window above, `for _ in 0..24 {
    // sleep(500ms) }`; the `None` arm panics separately, below), and it did so
    // *before* the client would ever have found out about the cancel on its
    // own by timing out. That ordering is the actual contrast this test
    // exists to show, it is a relative fact rather than an absolute one, and
    // it holds with enormous margin under every load level measured so far:
    // single-digit-second server reaction against the client's
    // `SAFETY_DEADLINE`-driven ~24-29s recovery failure (this comparison is
    // slightly conservative rather than exact — `stopped` is measured from
    // when the cancel was issued, a few seconds after `ran_for`'s own clock
    // started, so it understates the true gap between the two wall-clock
    // moments; that only makes the assertion harder to satisfy, never easier).
    let stopped = result
        .server_stopped_after
        .unwrap_or_else(|| panic!("the server never stopped the statement"));
    assert!(
        stopped < result.ran_for,
        "the server took {stopped:.1?} to stop the statement, which is not clearly ahead of \
         the {:.1?} the client itself took to notice the cancel — the server-side stop is no \
         longer a distinct, earlier event",
        result.ran_for
    );

    // The other half does not. The client stays blocked in its read until its
    // own deadline fires, and then reports the connection as unrecoverable. It
    // never sees ORA-01013. So a privileged cancel is not a usable
    // cancellation mechanism with this driver version either: the user's
    // statement does stop on the server, but the application only finds out by
    // losing the session.
    let error = result
        .client_error
        .unwrap_or_else(|| panic!("the client returned success for a cancelled statement"));
    observation(format!("the client eventually saw: {}", describe(&error)));
    assert!(
        result.ran_for >= SAFETY_DEADLINE,
        "the client noticed the cancel after {:.1?}; if it now returns promptly, \
         re-run this spike and revisit the S4 conclusion",
        result.ran_for
    );
    assert_ne!(
        error.native().map(NativeError::code),
        Some(1013),
        "the client now receives ORA-01013; the S4 conclusion should be revisited"
    );
    assert!(
        result.after.is_err(),
        "the connection unexpectedly survived"
    );
    observation(
        "ALTER SYSTEM CANCEL SQL stopped the statement on the server in well under a \
         second, but the client remained blocked until its own deadline expired and \
         then lost the session",
    );
}

#[test]
fn killing_a_session_is_a_different_thing_with_different_consequences() {
    // Recorded for completeness: `ALTER SYSTEM KILL SESSION` is **not** a
    // cancel. It ends the session and rolls its transaction back, and — unlike
    // a cancel — the client does find out, because the socket is torn down.
    // Reldex must never offer it as "stop this statement"; the difference is
    // what this test pins down.
    let Some(result) = cancel_from_a_control_session(LONG_SQL, |t| {
        // KILL SESSION takes only the session, not the cursor.
        let session = t.split(",@").next().unwrap_or(t);
        format!("ALTER SYSTEM KILL SESSION '{session}' IMMEDIATE")
    }) else {
        observation("NOT RUN: no privileged credentials for the KILL SESSION comparison");
        return;
    };

    measurement(
        "s4.kill_session_client_returned_after",
        format!("{:.1?}", result.ran_for),
    );
    let error = result
        .client_error
        .unwrap_or_else(|| panic!("the killed statement completed normally"));
    observation(format!(
        "ALTER SYSTEM KILL SESSION -> {}; the session afterwards was {}",
        describe(&error),
        if result.after.is_ok() {
            "usable"
        } else {
            "unusable"
        }
    ));
    assert!(
        result.ran_for < SAFETY_DEADLINE,
        "the client should notice a killed session at once, not at its deadline"
    );
    assert_eq!(
        error.native().map(NativeError::code),
        Some(28),
        "ORA-00028 expected"
    );
    assert_ne!(
        error.kind(),
        ErrorKind::Cancelled,
        "a killed session must not be reported as a cancelled statement"
    );
    assert!(
        result.after.is_err(),
        "a killed session must not look usable afterwards"
    );
}
