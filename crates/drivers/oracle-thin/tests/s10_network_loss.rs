//! Spike S10 — network loss and reconnect semantics.
//!
//! `SPEC.md` §8 lists "network loss" and "reconnect" in the Phase 0 operations
//! matrix, and §18 forbids ever silently replacing a lost transactional
//! session. Phase 0 had no evidence for either (`phase-0.md`, Workstream F).
//! This file supplies it.
//!
//! The link is simulated **inside the test process** by
//! [`common::proxy::Proxy`]: the session connects to `127.0.0.1:<ephemeral>`,
//! which forwards to the real listener and can be switched to a hard drop
//! (`FIN` both ways) or a black hole (sockets open, nothing moves). Nothing
//! touches Docker, the container or the host's network, so the Phase 0
//! database survives the suite exactly as it was.
//!
//! **Run this file single-threaded.** Several tests deliberately leave a
//! server session that the database still believes in, and two of them measure
//! how long a row lock survives; in parallel they would measure each other.
//!
//! ```text
//! tools/oracle-test-db/run-it.ps1 s10_network_loss -- --test-threads=1
//! tools/oracle-test-db/run-it.sh  s10_network_loss -- --test-threads=1
//! ```

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "spike results are measurements a human reads from the test output"
)]

mod common;

use std::num::NonZeroUsize;
use std::thread;
use std::time::{Duration, Instant};

use common::proxy::{Mode, Proxy};
use common::{
    connect, dsn_address, dsn_service, exec, exec_quietly, measurement, observation, params_at,
    scalar, try_connect, unique, with_watchdog,
};
use reldex_db_driver_api::{
    DatabaseConnection, DbError, ErrorKind, ExtensionValue, Extensions, SessionState, Statement,
};

/// How long a call is given before this file calls it a hang.
///
/// Generous on purpose: the point is to distinguish "slow" from "never", and a
/// wrong answer here would be a false defect report.
const HANG: Duration = Duration::from_secs(30);

/// The deadline armed where a test needs the call to come back.
const DEADLINE: Duration = Duration::from_secs(3);

/// Opens a session whose traffic goes through `proxy`.
fn connect_through(proxy: &Proxy) -> Box<dyn DatabaseConnection> {
    let connect_string = proxy.connect_string(&dsn_service());
    try_connect(&params_at(connect_string)).expect("the proxy should forward the connect")
}

/// A proxy in front of the configured listener.
fn proxy() -> Proxy {
    Proxy::start(dsn_address().as_str()).expect("bind an ephemeral port on 127.0.0.1")
}

/// Renders an error the way every S10 finding is recorded: kind, the session
/// state the driver claims, and the native code if the server produced one.
fn describe(error: &DbError) -> String {
    let native = error.native().map_or_else(
        || "none".to_owned(),
        |native| format!("ORA-{}", native.code()),
    );
    format!(
        "kind={:?} session_state={:?} native={native} retryable={} — {error}",
        error.kind(),
        error.session_state(),
        error.is_retryable()
    )
}

/// Fails the test if the driver claims a session is fine after its link died.
///
/// This is `SPEC.md` §18's rule expressed as an assertion: a `Usable` session
/// is one a pool or a worksheet may go on using, and a transaction that died
/// with the socket must never look like that.
fn assert_not_usable(error: &DbError, what: &str) {
    assert_ne!(
        error.session_state(),
        SessionState::Usable,
        "{what}: the link was gone and the driver still reported a usable session — \
         SPEC.md §18. {}",
        describe(error)
    );
}

// ---------------------------------------------------------------------------
// The proxy itself
// ---------------------------------------------------------------------------

#[test]
fn a_session_works_through_the_forwarding_proxy() {
    let proxy = proxy();
    let mut connection = connect_through(&proxy);
    assert_eq!(scalar(connection.as_mut(), "SELECT 1 FROM dual"), "1");
    connection.ping().expect("ping through the proxy");

    // If the listener had redirected the client to another port — which is what
    // it does on some platforms — the session would work while the proxy saw
    // nothing, and every measurement below would be meaningless.
    assert_eq!(
        proxy.accepted(),
        1,
        "the session did not go through the proxy; this listener redirects and S10's \
         method does not apply to it"
    );
    assert!(proxy.bytes_to_server() > 0 && proxy.bytes_to_client() > 0);
    observation(format!(
        "one session through the proxy moved {} bytes out and {} back; no listener \
         redirect happened",
        proxy.bytes_to_server(),
        proxy.bytes_to_client()
    ));
    connection.close().expect("close");
}

// ---------------------------------------------------------------------------
// An idle session whose link dies
// ---------------------------------------------------------------------------

#[test]
fn an_idle_session_notices_a_hard_drop_on_its_next_call() {
    let proxy = proxy();
    let mut connection = connect_through(&proxy);
    assert_eq!(scalar(connection.as_mut(), "SELECT 1 FROM dual"), "1");

    proxy.set_mode(Mode::HardDrop);
    thread::sleep(Duration::from_millis(50));

    let started = Instant::now();
    let ping = connection.ping();
    let ping_took = started.elapsed();
    let error = ping.expect_err("a ping over a dead socket must fail");
    measurement("s10.hard_drop_ping_detect", format!("{ping_took:.1?}"));
    observation(format!(
        "hard drop, idle session, ping -> {}",
        describe(&error)
    ));
    assert_not_usable(&error, "ping after a hard drop");

    // And the next real statement fails the same way rather than pretending.
    let started = Instant::now();
    let error = connection
        .execute(&Statement::new("SELECT 1 FROM dual"))
        .expect_err("an execute over a dead socket must fail");
    measurement(
        "s10.hard_drop_execute_detect",
        format!("{:.1?}", started.elapsed()),
    );
    observation(format!(
        "hard drop, idle session, execute -> {}",
        describe(&error)
    ));
    assert_not_usable(&error, "execute after a hard drop");
}

#[test]
fn an_idle_session_behind_a_black_hole_never_returns_without_a_deadline() {
    let proxy = proxy();
    let mut connection = connect_through(&proxy);
    assert_eq!(scalar(connection.as_mut(), "SELECT 1 FROM dual"), "1");

    proxy.set_mode(Mode::BlackHole);
    thread::sleep(Duration::from_millis(50));

    let started = Instant::now();
    let outcome = with_watchdog(HANG, move || {
        let result = connection.ping();
        // Keep the connection alive inside the closure so the socket is not
        // closed by a drop racing the measurement.
        drop(connection);
        result.map_err(|error| describe(&error))
    });

    match outcome {
        Some(Ok(())) => panic!("a ping into a black hole reported success"),
        Some(Err(text)) => {
            measurement(
                "s10.black_hole_ping_no_deadline",
                format!("{:.1?}", started.elapsed()),
            );
            observation(format!("black hole, no deadline, ping -> {text}"));
        }
        None => {
            measurement(
                "s10.black_hole_ping_no_deadline",
                format!("did not return within {HANG:?}"),
            );
            observation(format!(
                "FINDING: with the link black-holed and no deadline armed, ping had not \
                 returned after {HANG:?}. `oracledb` sets the socket's read timeout to \
                 None and enables no TCP keepalive, so there is nothing to end the wait \
                 but the operating system"
            ));
        }
    }
}

#[test]
fn a_black_holed_call_with_a_deadline_comes_back_and_says_what_it_knows() {
    let proxy = proxy();
    let mut connection = connect_through(&proxy);
    assert_eq!(scalar(connection.as_mut(), "SELECT 1 FROM dual"), "1");

    proxy.set_mode(Mode::BlackHole);
    thread::sleep(Duration::from_millis(50));

    let started = Instant::now();
    let statement = Statement::new("SELECT 1 FROM dual").with_deadline(DEADLINE);
    let outcome = with_watchdog(HANG, move || {
        let result = connection.execute(&statement);
        drop(connection);
        result
            .map(|_| "returned rows".to_owned())
            .map_err(|error| (error.kind(), error.session_state(), describe(&error)))
    })
    .expect("a deadline must end the call; it did not");
    let elapsed = started.elapsed();

    let (kind, state, text) = outcome.expect_err("a black-holed call cannot succeed");
    measurement("s10.black_hole_with_deadline", format!("{elapsed:.1?}"));
    observation(format!("black hole, {DEADLINE:?} deadline -> {text}"));

    // The contract's promise is honesty, not a particular kind. Against a black
    // hole the deadline fires, upstream tries to reset the connection, the
    // reset waits out the same read timeout again and fails, and what comes
    // back is `NetworkLost` — which is true: the session really is gone. That
    // is U-6 (a fired deadline can cost the session) showing up on the
    // network-loss path, and the honest report of it.
    assert!(
        matches!(kind, ErrorKind::Timeout | ErrorKind::NetworkLost),
        "a black-holed call with a deadline reported {kind:?}"
    );
    assert_ne!(
        state,
        SessionState::Usable,
        "a call that hit a black hole cannot claim the session is fine"
    );
    assert!(
        elapsed >= DEADLINE,
        "the call came back before its own deadline"
    );
    if kind == ErrorKind::NetworkLost {
        observation(format!(
            "FINDING: the deadline fired at {DEADLINE:?} but the call returned after \
             {elapsed:.1?} and the session was destroyed rather than kept. Upstream's \
             recovery path (`receive_data_packet` -> `recover_from_error`) waits out the \
             same read timeout a second time before giving up, so a black hole costs \
             twice the deadline and the connection — U-6 on the network-loss path"
        ));
    }
}

// ---------------------------------------------------------------------------
// Loss in the middle of something
// ---------------------------------------------------------------------------

#[test]
fn a_link_cut_mid_statement_is_reported_not_swallowed() {
    let proxy = proxy();
    let mut connection = connect_through(&proxy);

    // A statement the server will be busy with for a while.
    let statement = Statement::new("BEGIN DBMS_SESSION.SLEEP(20); END;");
    let cut_after = Duration::from_millis(600);

    // The call runs on a worker so this thread is free to cut the link while it
    // is in flight; the worker's report comes back over a channel.
    let started = Instant::now();
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let result = connection.execute(&statement);
        let described = match result {
            Ok(_) => Err("the statement returned normally".to_owned()),
            Err(error) => Ok((error.kind(), error.session_state(), describe(&error))),
        };
        let _ = sender.send(described);
        drop(connection);
    });

    thread::sleep(cut_after);
    proxy.set_mode(Mode::HardDrop);

    let reported = receiver
        .recv_timeout(HANG)
        .expect("the call did not come back after the link was cut");
    let elapsed = started.elapsed();
    let _ = worker.join();

    let (kind, state, text) = reported.expect("the statement must not report success");
    measurement(
        "s10.mid_statement_hard_drop_detect",
        format!("{:.1?}", elapsed.saturating_sub(cut_after)),
    );
    observation(format!("hard drop during a 20 s PL/SQL sleep -> {text}"));
    assert_ne!(
        state,
        SessionState::Usable,
        "a statement whose link died cannot leave the session Usable"
    );
    assert!(
        matches!(kind, ErrorKind::NetworkLost | ErrorKind::DriverInternal),
        "expected a transport failure, got {kind:?}"
    );
}

#[test]
fn a_link_cut_mid_fetch_is_reported_and_the_cursor_is_finished() {
    let proxy = proxy();
    let mut connection = connect_through(&proxy);

    // Small batches on purpose: the result has to span several round trips so
    // the link can die between two of them.
    let batch = NonZeroUsize::new(50).expect("non-zero");
    let statement = Statement::new(
        "SELECT level AS n, RPAD('x', 200, 'x') AS pad FROM dual CONNECT BY level <= 50000",
    )
    .with_fetch_rows(batch);

    let mut outcome = connection.execute(&statement).expect("start the query");
    let mut cursor = outcome.take_cursor().expect("cursor");
    let first = cursor.fetch_batch(batch).expect("the first batch arrives");
    assert_eq!(first.row_count(), 50);

    proxy.set_mode(Mode::HardDrop);
    thread::sleep(Duration::from_millis(50));

    let error = cursor
        .fetch_batch(batch)
        .expect_err("fetching over a dead socket must fail");
    observation(format!("hard drop between batches -> {}", describe(&error)));
    assert_not_usable(&error, "fetch after a hard drop");

    // The cursor is finished, not silently restartable.
    let again = cursor.fetch_batch(batch);
    observation(format!(
        "a second fetch on the same cursor -> {}",
        again
            .as_ref()
            .map_or_else(describe, |batch| format!("{} rows", batch.row_count()))
    ));
    assert!(
        again.is_err(),
        "a cursor whose connection died went on producing rows"
    );

    let closed = cursor.close();
    observation(format!(
        "closing a cursor whose link died -> {}",
        closed.as_ref().map_or_else(describe, |()| "Ok".to_owned())
    ));
}

#[test]
fn a_link_cut_mid_lob_stream_is_reported_and_the_handle_is_finished() {
    // Written through a healthy session so the fixture cannot be the thing that
    // fails; only the read goes through the proxy.
    let table = unique("s10_lob");
    let mut setup = connect();
    exec(setup.as_mut(), &format!("CREATE TABLE {table} (c CLOB)"));
    exec(
        setup.as_mut(),
        &format!(
            "DECLARE c CLOB; k VARCHAR2(1024) := RPAD('x', 1024, 'x'); \
             BEGIN INSERT INTO {table} VALUES (EMPTY_CLOB()) RETURNING c INTO c; \
             FOR i IN 1 .. 8192 LOOP DBMS_LOB.WRITEAPPEND(c, 1024, k); END LOOP; \
             COMMIT; END;"
        ),
    );

    let proxy = proxy();
    let mut connection = connect_through(&proxy);
    let mut outcome = connection
        .execute(&Statement::new(format!("SELECT c FROM {table}")))
        .expect("select the LOB");
    let mut cursor = outcome.take_cursor().expect("cursor");
    let mut first = cursor.fetch_batch(NonZeroUsize::MIN).expect("fetch");
    let mut locator = first
        .column_mut(0)
        .and_then(|column| column.take_lob(0))
        .expect("a CLOB locator");

    let mut buffer = vec![0_u8; 8 * 1024];
    let read = locator.read_chunk(&mut buffer).expect("the first chunk");
    assert!(read > 0, "the stream produced nothing before the cut");

    proxy.set_mode(Mode::HardDrop);
    thread::sleep(Duration::from_millis(50));

    // The driver stages a server-side chunk larger than the caller's buffer, so
    // the first read or two after the cut can still be served from memory. What
    // must not happen is the stream quietly ending, or producing the whole 8 MB
    // from a link that no longer exists.
    let mut served_from_the_buffer = 0_u64;
    let error = loop {
        match locator.read_chunk(&mut buffer) {
            Ok(0) => panic!(
                "the stream reported a clean end of data after its link died; it had \
                 delivered {served_from_the_buffer} of 8388608 bytes"
            ),
            Ok(count) => {
                served_from_the_buffer += count as u64;
                assert!(
                    served_from_the_buffer < 1024 * 1024,
                    "a LOB stream whose connection died went on producing bytes"
                );
            }
            Err(error) => break error,
        }
    };
    measurement("s10.lob_bytes_served_after_the_cut", served_from_the_buffer);
    observation(format!(
        "hard drop mid-LOB-stream: {served_from_the_buffer} further bytes came out of the \
         staging buffer, then -> {}",
        describe(&error)
    ));
    assert_not_usable(&error, "LOB read after a hard drop");

    let again = locator.read_chunk(&mut buffer);
    assert!(
        again.is_err(),
        "a LOB stream whose connection died went on producing bytes"
    );
    observation(format!(
        "a second read on the same locator -> {}",
        again
            .as_ref()
            .map_or_else(describe, |count| format!("{count} bytes"))
    ));

    exec_quietly(setup.as_mut(), &format!("DROP TABLE {table} PURGE"));
    setup.close().expect("close");
}

// ---------------------------------------------------------------------------
// An open transaction when the link dies
// ---------------------------------------------------------------------------

#[test]
fn an_open_transaction_dies_with_its_link_and_a_fresh_session_sees_nothing() {
    let table = unique("s10_tx");
    let mut setup = connect();
    exec(
        setup.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(5), note VARCHAR2(40))"),
    );
    exec(
        setup.as_mut(),
        &format!("INSERT INTO {table} VALUES (1, 'committed')"),
    );
    setup.commit().expect("commit the fixture");

    let proxy = proxy();
    let mut doomed = connect_through(&proxy);
    exec(
        doomed.as_mut(),
        &format!("INSERT INTO {table} VALUES (2, 'uncommitted')"),
    );
    // The driver tracks transaction state conservatively (ADR-0002): after DML
    // it reports `Unknown`, which `may_be_open` treats as open. What matters
    // here is that the core is told to behave as though a transaction were
    // live, not which of the two honest answers it gets.
    let before_the_cut = doomed.transaction_state();
    assert!(
        before_the_cut.may_be_open(),
        "auto-commit is off, so the insert must leave the core treating a transaction as \
         open; got {before_the_cut:?}"
    );
    assert_eq!(
        scalar(doomed.as_mut(), &format!("SELECT COUNT(*) FROM {table}")),
        "2",
        "the doomed session should see its own uncommitted row"
    );

    proxy.set_mode(Mode::HardDrop);
    thread::sleep(Duration::from_millis(50));
    let error = doomed
        .execute(&Statement::new("SELECT 1 FROM dual"))
        .expect_err("the doomed session must fail");
    assert_not_usable(&error, "statement on a session with an open transaction");
    observation(format!(
        "open transaction, hard drop, next statement -> {}",
        describe(&error)
    ));

    // A *new* connection, because the contract forbids resurrecting the old
    // one. Wait for the server to roll the dead session back.
    let started = Instant::now();
    let mut fresh = connect();
    let mut rows = String::new();
    while started.elapsed() < Duration::from_secs(30) {
        rows = scalar(fresh.as_mut(), &format!("SELECT COUNT(*) FROM {table}"));
        if rows == "1" {
            break;
        }
        thread::sleep(Duration::from_millis(250));
    }
    measurement(
        "s10.uncommitted_rows_gone_after",
        format!("{:.1?}", started.elapsed()),
    );
    assert_eq!(
        rows, "1",
        "the uncommitted row was still visible 30 s after the session died"
    );
    observation(
        "after a hard drop the server rolled the dead session back; a fresh session sees \
         only the committed row",
    );

    exec_quietly(fresh.as_mut(), &format!("DROP TABLE {table} PURGE"));
    fresh.close().expect("close");
    setup.close().expect("close");
}

#[test]
fn a_dead_sessions_row_locks_survive_exactly_as_long_as_the_server_believes_in_it() {
    let table = unique("s10_lock");
    let mut setup = connect();
    exec(
        setup.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(5) PRIMARY KEY, note VARCHAR2(40))"),
    );
    exec(
        setup.as_mut(),
        &format!("INSERT INTO {table} VALUES (1, 'a')"),
    );
    setup.commit().expect("commit");

    // How long a second session waits for the lock, per failure shape.
    let mut results: Vec<(&str, String)> = Vec::new();

    for (label, mode, budget) in [
        ("hard drop", Mode::HardDrop, Duration::from_secs(30)),
        ("black hole", Mode::BlackHole, Duration::from_secs(20)),
    ] {
        let proxy = proxy();
        let mut holder = connect_through(&proxy);
        exec(
            holder.as_mut(),
            &format!("UPDATE {table} SET note = '{label}' WHERE id = 1"),
        );

        // Prove the lock is really held before the link dies.
        let blocked = setup.execute(&Statement::new(format!(
            "SELECT id FROM {table} WHERE id = 1 FOR UPDATE NOWAIT"
        )));
        let error = blocked.expect_err("a second session must not get the row while it is locked");
        assert_eq!(
            error.native().map(reldex_db_driver_api::NativeError::code),
            Some(54),
            "expected ORA-00054 (resource busy), got {}",
            describe(&error)
        );

        proxy.set_mode(mode);
        let started = Instant::now();
        let mut freed = None;
        while started.elapsed() < budget {
            if setup
                .execute(&Statement::new(format!(
                    "SELECT id FROM {table} WHERE id = 1 FOR UPDATE NOWAIT"
                )))
                .is_ok()
            {
                freed = Some(started.elapsed());
                break;
            }
            thread::sleep(Duration::from_millis(250));
        }
        setup.rollback().expect("release whatever was taken");

        let answer = freed.map_or_else(
            || format!("still held after {budget:?}"),
            |taken| format!("{taken:.1?}"),
        );
        results.push((label, answer));

        // The holder is dead either way; let it go before the next round.
        drop(proxy);
        drop(holder);
        thread::sleep(Duration::from_millis(500));
    }

    for (label, answer) in &results {
        measurement(&format!("s10.row_lock_released_after.{label}"), answer);
    }
    observation(format!(
        "row locks held by a session whose client vanished: hard drop -> {}, black hole \
         -> {}. The difference is whether the server's socket was closed: with the \
         sockets left open the server has no reason to notice, and the lock outlives the \
         client",
        results[0].1, results[1].1
    ));

    exec_quietly(setup.as_mut(), &format!("DROP TABLE {table} PURGE"));
    setup.close().expect("close");
}

#[test]
fn a_commit_whose_reply_never_arrives_is_reported_as_unknown_not_as_success() {
    let table = unique("s10_doubt");
    let mut setup = connect();
    exec(
        setup.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(5), note VARCHAR2(40))"),
    );
    setup.commit().expect("commit the DDL fixture");

    let proxy = proxy();
    let mut doomed = connect_through(&proxy);
    exec(
        doomed.as_mut(),
        &format!("INSERT INTO {table} VALUES (7, 'in doubt')"),
    );

    // Forward the commit request, then kill the link on the server's reply: the
    // server acted, the client cannot know how.
    proxy.set_mode(Mode::DropOnServerData);
    let result = doomed.commit();
    let claim = match &result {
        Ok(()) => "reported success".to_owned(),
        Err(error) => describe(error),
    };
    observation(format!("commit with the reply dropped -> {claim}"));

    // Whatever the server did, the client must not claim to know.
    let error = result
        .expect_err("a commit whose reply was destroyed cannot be reported as a successful commit");
    assert_not_usable(&error, "commit whose reply never arrived");

    // What actually happened, from a session that can still see the database.
    let started = Instant::now();
    let mut outcome = String::new();
    while started.elapsed() < Duration::from_secs(30) {
        outcome = scalar(setup.as_mut(), &format!("SELECT COUNT(*) FROM {table}"));
        if outcome != "0" {
            break;
        }
        thread::sleep(Duration::from_millis(250));
    }
    measurement("s10.in_doubt_commit_server_outcome", &outcome);
    observation(format!(
        "the server's own answer after the in-doubt commit: {outcome} row(s) present. \
         The client reported an error and a non-usable session, which is the only \
         honest report available to it — SPEC.md §18"
    ));

    exec_quietly(setup.as_mut(), &format!("DROP TABLE {table} PURGE"));
    setup.close().expect("close");
}

// ---------------------------------------------------------------------------
// Reconnect
// ---------------------------------------------------------------------------

#[test]
fn nothing_reconnects_by_itself_and_a_new_connection_is_a_new_session() {
    let proxy = proxy();
    let mut doomed = connect_through(&proxy);

    // A piece of session state that must not survive.
    exec(
        doomed.as_mut(),
        "ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY\"|\"MM\"|\"DD'",
    );
    let formatted = scalar(
        doomed.as_mut(),
        "SELECT TO_CHAR(DATE '2026-09-19') FROM dual",
    );
    assert_eq!(formatted, "2026|09|19");
    let old_identity = scalar(
        doomed.as_mut(),
        "SELECT SYS_CONTEXT('USERENV','SID') || '.' || \
         (SELECT serial# FROM v$session WHERE sid = SYS_CONTEXT('USERENV','SID')) FROM dual",
    );

    proxy.set_mode(Mode::HardDrop);
    thread::sleep(Duration::from_millis(50));

    // Every call on the old connection fails, and keeps failing: nothing behind
    // the contract re-opens a socket.
    for attempt in 0..3 {
        let error = doomed
            .execute(&Statement::new("SELECT 1 FROM dual"))
            .err()
            .unwrap_or_else(|| panic!("attempt {attempt} succeeded after the link died"));
        assert_not_usable(&error, "repeated call on a dead connection");
    }
    let ping = doomed.ping().expect_err("ping must keep failing");
    assert_not_usable(&ping, "repeated ping on a dead connection");
    assert_eq!(
        proxy.accepted(),
        1,
        "the driver opened a second TCP connection by itself — SPEC.md §18 forbids \
         silently replacing a lost session"
    );
    observation(format!(
        "after the loss, three executes and a ping all failed and the proxy saw no new \
         TCP connection; last report {}",
        describe(&ping)
    ));

    // A reconnect is the caller's explicit act, and produces a different
    // session with none of the old one's state.
    let mut fresh = connect();
    let new_identity = scalar(
        fresh.as_mut(),
        "SELECT SYS_CONTEXT('USERENV','SID') || '.' || \
         (SELECT serial# FROM v$session WHERE sid = SYS_CONTEXT('USERENV','SID')) FROM dual",
    );
    assert_ne!(
        old_identity, new_identity,
        "a reconnect handed back the same SID and serial#"
    );
    let default_format = scalar(
        fresh.as_mut(),
        "SELECT TO_CHAR(DATE '2026-09-19') FROM dual",
    );
    assert_ne!(
        default_format, "2026|09|19",
        "the dead session's NLS_DATE_FORMAT survived into the new session"
    );
    observation(format!(
        "reconnect: v$session sid.serial# {old_identity} -> {new_identity}; \
         NLS_DATE_FORMAT reverted to the instance default (TO_CHAR gave {default_format})"
    ));
    fresh.close().expect("close");
}

// ---------------------------------------------------------------------------
// Connect-time behaviour
// ---------------------------------------------------------------------------

#[test]
fn a_connect_into_a_black_hole_is_now_bounded_by_the_connect_timeout() {
    // The TCP handshake succeeds — the proxy accepts — and then nothing moves,
    // which is what a firewall that drops after the SYN looks like. Before the
    // C-5 fix this was the case with no bound at all: the connect was still
    // outstanding after 30 s with a 2 s limit asked for (U-15).
    let proxy = proxy();
    proxy.set_mode(Mode::BlackHole);
    let connect_string = proxy.connect_string(&dsn_service());

    // Short on purpose: this file's budget is measured in seconds, and what is
    // being proven is that the limit is what ends the wait, not what its value
    // is.
    let asked_for = Duration::from_secs(2);
    let started = Instant::now();
    let outcome = with_watchdog(HANG, move || {
        try_connect(&params_at(connect_string).with_connect_timeout(asked_for))
            .map(|connection| {
                let _ = connection.close();
                "connected".to_owned()
            })
            .map_err(|error| describe(&error))
    });
    let elapsed = started.elapsed();

    match outcome {
        None => panic!(
            "connect into a black hole was not bounded by its {asked_for:?} limit: still \
             outstanding after {HANG:?}"
        ),
        Some(Ok(text)) => panic!("a black-holed endpoint reported {text}"),
        Some(Err(text)) => {
            measurement(
                "s10.connect_black_hole_with_2s_timeout",
                format!("{elapsed:.1?}"),
            );
            observation(format!(
                "connect into a black hole with a 2 s connect timeout returned after \
                 {elapsed:.1?} -> {text}"
            ));
            assert!(
                text.contains("kind=Connection"),
                "a connect that ran out of time is a connection failure, not a fired \
                 statement deadline: {text}"
            );
            assert!(
                text.contains("connect limit"),
                "the failure must say the driver's own limit ended it: {text}"
            );
            assert!(
                elapsed < asked_for + Duration::from_secs(5),
                "the limit did not bound the connect: {elapsed:.1?}"
            );
        }
    }
}

#[test]
fn the_default_connect_limit_ends_a_wait_the_operating_system_would_not() {
    // RFC 5737 TEST-NET-1: routable nowhere, and discarded rather than refused
    // on every network this has been run on. S10 originally measured the
    // operating system's own patience here — 22.0 s, a number nothing in
    // Reldex chose. A short explicit limit now ends it instead, and the
    // assertion is that the driver's bound, not the OS's, is what returned:
    // the total live time this test adds is its own limit, not 22 s.
    let asked_for = Duration::from_secs(2);
    let started = Instant::now();
    let outcome = with_watchdog(HANG, move || {
        try_connect(&params_at("192.0.2.1:1521/RELDEX").with_connect_timeout(asked_for))
            .map(|connection| {
                let _ = connection.close();
                "connected".to_owned()
            })
            .map_err(|error| describe(&error))
    });
    let elapsed = started.elapsed();
    match outcome {
        None => panic!("a {asked_for:?} connect limit did not bound an unroutable address"),
        Some(Ok(text)) => panic!("192.0.2.1:1521 reported {text}"),
        Some(Err(error)) => {
            measurement(
                "s10.connect_unroutable_with_2s_limit",
                format!("{elapsed:.1?}"),
            );
            observation(format!(
                "connect to 192.0.2.1:1521 with a 2 s connect limit returned after \
                 {elapsed:.1?} -> {error}"
            ));
            // Two things at once. Upstream calls a socket timeout
            // `CallTimeoutExceeded`, which would read as "the deadline you
            // armed expired" for a connection that never existed (U-16); and
            // the driver's own limit must be a connection failure too, not a
            // statement deadline. Either route has to be `Connection`.
            assert!(
                error.contains("kind=Connection"),
                "a connect that ran out of time must be a Connection failure, not a fired \
                 statement deadline: {error}"
            );
            assert!(
                error.contains("connect limit"),
                "the driver's own limit, not the operating system's 22 s, is what ended \
                 this: {error}"
            );
            assert!(
                elapsed < Duration::from_secs(10),
                "this returned in {elapsed:.1?}, which is the operating system's patience \
                 rather than the limit that was asked for"
            );
        }
    }
}

#[test]
fn a_connect_with_no_limit_at_all_is_still_expressible_and_still_unbounded() {
    // The owner's decision requires "no limit" to be a thing a profile can say
    // (`SPEC.md` §8). It is, through the driver's extension bag — and it means
    // exactly what it says, which is why the UI has to explain it. Proven
    // against a black hole inside the watchdog rather than waited out: what is
    // asserted is that the *driver's* limit did not end it.
    let proxy = proxy();
    proxy.set_mode(Mode::BlackHole);
    let connect_string = proxy.connect_string(&dsn_service());

    let mut extensions = Extensions::new();
    extensions.set(
        reldex_driver_oracle_thin::EXT_CONNECT_TIMEOUT_UNBOUNDED,
        ExtensionValue::Flag(true),
    );
    // Deliberately also set a short timeout, to show the flag wins.
    let params = params_at(connect_string)
        .with_connect_timeout(Duration::from_secs(1))
        .with_extensions(extensions);

    let watched = Duration::from_secs(6);
    let outcome = with_watchdog(watched, move || {
        try_connect(&params)
            .map(|connection| {
                let _ = connection.close();
                "connected".to_owned()
            })
            .map_err(|error| describe(&error))
    });
    match outcome {
        None => {
            measurement(
                "s10.connect_black_hole_unbounded",
                format!("still outstanding after {watched:?}"),
            );
            observation(
                "with \"oracle.connect_timeout_unbounded\" set, a black-holed connect is \
                 still outstanding after the watchdog — the 1 s connect timeout on the same \
                 profile did not apply, which is what the flag is for. The abandoned thread \
                 is left to finish on its own (U-15: nothing can interrupt it)",
            );
        }
        Some(Ok(text)) => panic!("a black-holed endpoint reported {text}"),
        Some(Err(text)) => panic!(
            "an unbounded connect ended after less than {watched:?} with {text}; the \
             extension did not remove the bound"
        ),
    }
}
