//! Server output (`DBMS_OUTPUT` and its kind) through `db-core` — M2.7,
//! ADR-0002 amendment T — against the mock driver, which buffers output only
//! while it is enabled, prints before a statement finishes, and counts every
//! `set_server_output` / `take_server_output` call it receives.
//!
//! The acceptance criterion this file exists for: **a session whose output
//! pane is off makes no server-output call at all.** The mock's counters are
//! the proof, because a call that fails still counts.
//!
//! Nothing here synchronises with a sleep, and nothing asserts an upper bound
//! on time: every wait is on an event, a gate or a counter, under the suite's
//! hang guard.

mod support;

use std::sync::Arc;

use reldex_db_core::{
    CloseDisposition, EventCaps, RequestId, ServerOutputBuffer, ServerOutputLog,
    ServerOutputSetting, SessionEvent, SessionLimits, Statement,
};
use reldex_db_driver_api::{Capabilities, ErrorKind, SessionState, StatementKind};
use reldex_driver_mock::{
    Action, BlockGate, BlockSpec, QueryPlan, QuerySource, Scenario, ScriptValue, ScriptedError,
    ServerOutputCounts,
};
use support::{ReplyPath, both_paths};

const BLOCK: &str = "BEGIN print_some; END;";
const FAILING_BLOCK: &str = "BEGIN print_then_raise; END;";
const INSERT: &str = "INSERT INTO t VALUES (1)";
const SELECT: &str = "SELECT 1 FROM dual";
const PARK: &str = "BEGIN park; END;";

const ON: ServerOutputSetting = ServerOutputSetting::Enabled(ServerOutputBuffer::Unlimited);

/// A scenario whose driver **can** collect output — so a test that sees no
/// call proves the core chose not to make one, not that it could not.
fn scenario() -> Arc<Scenario> {
    let scenario = support::scenario();
    scenario.set_capabilities(scenario.capabilities().with_server_output(true));
    scenario.on_sql(
        BLOCK,
        Action::Execute {
            statement_kind: StatementKind::PlSqlBlock,
            rows_affected: None,
            opens_transaction: false,
        },
    );
    scenario.on_sql_output(BLOCK, lines("line", 3));
    scenario.on_sql(
        FAILING_BLOCK,
        Action::Fail(
            ScriptedError::new(ErrorKind::Other, "user-defined exception")
                .with_native(20_001, "ORA-20001: raised after printing"),
        ),
    );
    scenario.on_sql_output(FAILING_BLOCK, lines("before the raise", 2));
    scenario.on_sql(
        INSERT,
        Action::Dml {
            rows_affected: 1,
            insert: Some(("t".to_owned(), vec![ScriptValue::from(1_i64)])),
        },
    );
    scenario.on_sql_output(INSERT, lines("trigger says", 1));
    scenario.on_sql(
        SELECT,
        Action::query(QuerySource::Fixed(QueryPlan::new(Vec::new(), Vec::new()))),
    );
    scenario.on_sql_output(SELECT, lines("function in select list", 1));
    scenario
}

fn lines(prefix: &str, count: usize) -> Vec<String> {
    (1..=count).map(|i| format!("{prefix} {i}")).collect()
}

fn counts(scenario: &Scenario) -> ServerOutputCounts {
    scenario.server_output_counts()
}

fn enable(session: &support::Session) {
    let effective = session
        .set_server_output(ON)
        .wait()
        .expect("enabling output succeeds on a capable driver");
    assert_eq!(effective, ON);
}

// ------------------------------------------------------------ never enabled

fn a_session_that_never_enabled_output_makes_no_server_output_call(path: ReplyPath) {
    let scenario = scenario();
    let session = support::open_on(&scenario, path);

    // Every kind of request, including a failing statement, a commit and a
    // fetch: none of them may cost a server-output round trip.
    session
        .execute(Statement::new(BLOCK))
        .wait()
        .expect("block");
    let failed = session.execute(Statement::new(FAILING_BLOCK)).wait();
    assert!(failed.is_err());
    session
        .execute(Statement::new(INSERT))
        .wait()
        .expect("insert");
    let query = session
        .execute(Statement::new(SELECT))
        .wait()
        .expect("select");
    let result = query.result.expect("a result");
    let _ = session
        .fetch_batch(result, support::n(10))
        .wait()
        .expect("fetch");
    session.commit().wait().expect("commit");
    session.ping().wait().expect("ping");

    assert_eq!(
        counts(&scenario),
        ServerOutputCounts::default(),
        "no set and no take: the pane is off, so there is no extra round trip"
    );
    assert!(session.take_output().is_empty());
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("close");
}

// ------------------------------------------------------- enable and disable

fn enabling_and_disabling_cost_one_driver_call_each_and_are_answered(path: ReplyPath) {
    let scenario = scenario();
    let session = support::open_on(&scenario, path);

    enable(&session);
    assert_eq!(counts(&scenario).sets, 1);
    assert_eq!(counts(&scenario).takes, 0, "enabling reads nothing");

    let off = session
        .set_server_output(ServerOutputSetting::Disabled)
        .wait()
        .expect("disable");
    assert_eq!(off, ServerOutputSetting::Disabled);
    assert_eq!(counts(&scenario).sets, 2);
    assert_eq!(counts(&scenario).takes, 0);
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("close");
}

fn disabling_stops_the_reads(path: ReplyPath) {
    let scenario = scenario();
    let session = support::open_on(&scenario, path);
    enable(&session);
    session
        .execute(Statement::new(BLOCK))
        .wait()
        .expect("block");
    let reads_while_on = counts(&scenario).takes;
    assert!(reads_while_on >= 1);
    assert_eq!(session.take_output().lines, lines("line", 3));

    session
        .set_server_output(ServerOutputSetting::Disabled)
        .wait()
        .expect("disable");
    session
        .execute(Statement::new(BLOCK))
        .wait()
        .expect("block");
    session
        .execute(Statement::new(INSERT))
        .wait()
        .expect("insert");
    assert_eq!(
        counts(&scenario).takes,
        reads_while_on,
        "once off, no statement reads output again"
    );
    assert!(session.take_output().is_empty());
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("close");
}

fn a_driver_without_the_capability_is_refused_without_a_call(path: ReplyPath) {
    let scenario = scenario();
    scenario.set_capabilities(Capabilities::none().with_exact_transaction_state(true));
    let session = support::open_on(&scenario, path);

    let refused = session
        .set_server_output(ON)
        .wait()
        .expect_err("no capability, no output");
    assert_eq!(refused.kind(), ErrorKind::Unsupported);
    session
        .execute(Statement::new(BLOCK))
        .wait()
        .expect("block");
    assert_eq!(
        counts(&scenario),
        ServerOutputCounts::default(),
        "refused in the core: the driver was never asked"
    );
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("close");
}

fn a_failed_enable_leaves_output_off(path: ReplyPath) {
    let scenario = scenario();
    scenario.fail_set_server_output(ScriptedError::new(
        ErrorKind::Permission,
        "insufficient privileges",
    ));
    let session = support::open_on(&scenario, path);

    let failed = session
        .set_server_output(ON)
        .wait()
        .expect_err("the enable failed");
    assert_eq!(failed.kind(), ErrorKind::Permission);
    assert_eq!(counts(&scenario).sets, 1);

    session
        .execute(Statement::new(BLOCK))
        .wait()
        .expect("block");
    assert_eq!(
        counts(&scenario).takes,
        0,
        "the reply said nothing changed, so nothing is read"
    );
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("close");
}

// ------------------------------------------------------------- delivery

fn a_statements_output_is_delivered_by_the_time_its_reply_is(path: ReplyPath) {
    let scenario = scenario();
    let session = support::open_on(&scenario, path);
    enable(&session);

    session
        .execute(Statement::new(BLOCK))
        .wait()
        .expect("block");
    let output = session.take_output();
    assert_eq!(output.lines, lines("line", 3));
    assert_eq!(output.dropped, 0);
    assert_eq!(output.failures, 0);
    assert_eq!(
        counts(&scenario).takes,
        1,
        "three lines fit one read, and the driver said it was drained"
    );
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("close");
}

fn a_failing_statement_still_delivers_what_it_printed(path: ReplyPath) {
    let scenario = scenario();
    let session = support::open_on(&scenario, path);
    enable(&session);

    let error = session
        .execute(Statement::new(FAILING_BLOCK))
        .wait()
        .expect_err("the block raises");
    // The statement's own error, untouched by the read that followed it.
    assert_eq!(error.kind(), ErrorKind::Other);
    assert_eq!(error.native().map(|native| native.code()), Some(20_001));
    let output = session.take_output();
    assert_eq!(
        output.lines,
        lines("before the raise", 2),
        "the lines printed before the raise are the ones that explain it"
    );
    assert_eq!(output.failures, 0);
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("close");
}

fn large_output_is_read_in_bounded_chunks(path: ReplyPath) {
    const TOTAL: usize = 1_000;
    const CHUNK: usize = 64;
    let scenario = scenario();
    let large = "BEGIN print_a_lot; END;";
    scenario.on_sql(
        large,
        Action::Execute {
            statement_kind: StatementKind::PlSqlBlock,
            rows_affected: None,
            opens_transaction: false,
        },
    );
    scenario.on_sql_output(large, lines("many", TOTAL));
    let limits = SessionLimits::new().with_server_output_chunk_lines(support::n(CHUNK));
    let session = support::open_on_with_limits(&scenario, limits, path);
    enable(&session);

    session
        .execute(Statement::new(large))
        .wait()
        .expect("large block");
    let output = session.take_output();
    assert_eq!(output.lines, lines("many", TOTAL), "every line, in order");
    let reads = TOTAL.div_ceil(CHUNK);
    assert_eq!(
        counts(&scenario).takes,
        reads,
        "one read per chunk, and none after the driver said it was drained"
    );
    if path == ReplyPath::Events {
        assert_eq!(output.events, reads, "one event per read, each bounded");
    }
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("close");
}

// ------------------------------------------------------------- failures

fn a_failed_read_does_not_mask_the_statements_result(path: ReplyPath) {
    let scenario = scenario();
    scenario.fail_take_server_output(ScriptedError::new(
        ErrorKind::DataConversion,
        "server output could not be decoded",
    ));
    let session = support::open_on(&scenario, path);
    enable(&session);

    // A statement that succeeded still succeeds.
    let outcome = session
        .execute(Statement::new(BLOCK))
        .wait()
        .expect("the statement's own result is Ok");
    assert_eq!(outcome.statement_kind, StatementKind::PlSqlBlock);
    let output = session.take_output();
    assert_eq!(output.failures, 1, "the failed read is reported, once");
    assert_eq!(
        output
            .first_failure
            .as_ref()
            .map(reldex_db_core::DbError::kind),
        Some(ErrorKind::DataConversion)
    );

    // A statement that failed still fails with its own error, not the read's.
    let error = session
        .execute(Statement::new(FAILING_BLOCK))
        .wait()
        .expect_err("the block raises");
    assert_eq!(error.native().map(|native| native.code()), Some(20_001));
    let output = session.take_output();
    assert_eq!(output.failures, 1);
    assert!(
        session.session_state().is_usable(),
        "a read that failed without losing the connection leaves the session usable"
    );
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("close");
}

fn a_read_that_finds_the_connection_gone_loses_the_session_like_any_call(path: ReplyPath) {
    let scenario = scenario();
    let session = support::open_on(&scenario, path);
    enable(&session);
    scenario.fail_take_server_output(ScriptedError::new(
        ErrorKind::NetworkLost,
        "connection reset during the read",
    ));

    // The INSERT reached the server and succeeded; the read after it is what
    // found the connection gone. The statement's answer is still its own.
    let outcome = session
        .execute(Statement::new(INSERT))
        .wait()
        .expect("the insert itself succeeded");
    assert_eq!(outcome.rows_affected, Some(1));
    let output = session.take_output();
    assert_eq!(output.failures, 1);
    assert_eq!(
        output
            .first_failure
            .as_ref()
            .map(|error| error.session_state()),
        Some(SessionState::Lost)
    );
    assert!(
        session.is_lost(),
        "the loss went through the ordinary loss path"
    );

    // Nothing more is attempted on a lost session: the next statement fails
    // fast and reads nothing.
    let takes = counts(&scenario).takes;
    let next = session.execute(Statement::new(BLOCK)).wait();
    assert!(next.is_err());
    assert_eq!(counts(&scenario).takes, takes);

    if path == ReplyPath::Events {
        session.await_terminal();
        let terminal = session
            .stashed()
            .iter()
            .find_map(|event| match event {
                SessionEvent::Terminal {
                    cause,
                    transaction_possibly_lost,
                    ..
                } => Some((
                    cause.as_ref().map(reldex_db_core::DbError::kind),
                    *transaction_possibly_lost,
                )),
                _ => None,
            })
            .expect("a Terminal");
        assert_eq!(terminal.0, Some(ErrorKind::NetworkLost));
        assert!(
            terminal.1,
            "the INSERT's transaction went with the connection, and Terminal says so"
        );
    }
}

fn a_statement_that_lost_the_session_is_not_followed_by_a_read(path: ReplyPath) {
    let scenario = scenario();
    let dying = "BEGIN die; END;";
    scenario.on_sql(
        dying,
        Action::Fail(ScriptedError::new(ErrorKind::NetworkLost, "gone")),
    );
    scenario.on_sql_output(dying, lines("printed before dying", 1));
    let session = support::open_on(&scenario, path);
    enable(&session);

    let _ = session.execute(Statement::new(dying)).wait();
    assert!(session.is_lost());
    assert_eq!(
        counts(&scenario).takes,
        0,
        "there is no connection left to read from; the output went with it"
    );
}

fn output_is_not_read_on_a_session_that_needs_validation_until_it_is_pinged(path: ReplyPath) {
    let scenario = scenario();
    let flaky = "BEGIN time_out; END;";
    scenario.on_sql(
        flaky,
        Action::Fail(
            ScriptedError::new(ErrorKind::Timeout, "deadline elapsed")
                .with_session_state(SessionState::NeedsValidation),
        ),
    );
    scenario.on_sql_output(flaky, lines("printed before the timeout", 1));
    let session = support::open_on(&scenario, path);
    enable(&session);

    let _ = session.execute(Statement::new(flaky)).wait();
    assert_eq!(
        counts(&scenario).takes,
        0,
        "a session that must be pinged is not used again before the ping"
    );
    // The next statement pings first, then runs, then reads both statements'
    // output: nothing was lost, only delayed.
    session
        .execute(Statement::new(BLOCK))
        .wait()
        .expect("block");
    let output = session.take_output();
    let mut expected = lines("printed before the timeout", 1);
    expected.extend(lines("line", 3));
    assert_eq!(output.lines, expected);
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("close");
}

// ------------------------------------------------------------- lifecycle

fn the_setting_belongs_to_one_session_and_dies_with_it(path: ReplyPath) {
    let scenario = scenario();
    let first = support::open_on(&scenario, path);
    enable(&first);
    first.execute(Statement::new(BLOCK)).wait().expect("block");
    let takes = counts(&scenario).takes;
    let _ = first.take_output();
    first
        .close(Some(CloseDisposition::Rollback))
        .expect("close");

    // A new session on the same driver starts with output off.
    let second = support::open_on(&scenario, path);
    second.execute(Statement::new(BLOCK)).wait().expect("block");
    assert_eq!(counts(&scenario).takes, takes);
    assert!(second.take_output().is_empty());
    second
        .close(Some(CloseDisposition::Rollback))
        .expect("close");
}

both_paths!(
    a_session_that_never_enabled_output_makes_no_server_output_call,
    enabling_and_disabling_cost_one_driver_call_each_and_are_answered,
    disabling_stops_the_reads,
    a_driver_without_the_capability_is_refused_without_a_call,
    a_failed_enable_leaves_output_off,
    a_statements_output_is_delivered_by_the_time_its_reply_is,
    a_failing_statement_still_delivers_what_it_printed,
    large_output_is_read_in_bounded_chunks,
    a_failed_read_does_not_mask_the_statements_result,
    a_read_that_finds_the_connection_gone_loses_the_session_like_any_call,
    a_statement_that_lost_the_session_is_not_followed_by_a_read,
    output_is_not_read_on_a_session_that_needs_validation_until_it_is_pinged,
    the_setting_belongs_to_one_session_and_dies_with_it,
);

// ------------------------------------------------- event path: ordering

/// Where each event of one execute sits, as a compact label.
fn label(event: &SessionEvent) -> &'static str {
    match event {
        SessionEvent::Executing { .. } => "Executing",
        SessionEvent::ServerOutput { .. } => "ServerOutput",
        SessionEvent::Executed { .. } => "Executed",
        SessionEvent::TransactionStateChanged { .. } => "TransactionStateChanged",
        SessionEvent::ServerOutputConfigured { .. } => "ServerOutputConfigured",
        _ => "other",
    }
}

fn submit_and_collect(
    session: &reldex_db_core::DatabaseSession,
    queue: &reldex_db_core::EventQueue,
    request: u64,
    sql: &str,
) -> Vec<SessionEvent> {
    session
        .submit_execute(RequestId(request), Statement::new(sql))
        .expect("accepted");
    support::drain_until(queue, |seen| {
        seen.iter().any(|event| {
            matches!(event, SessionEvent::Executed { request: done, .. } if *done == RequestId(request))
        })
    })
}

#[test]
fn output_sits_between_its_statements_executing_and_executed() {
    let scenario = scenario();
    let large = "BEGIN print_a_lot; END;";
    scenario.on_sql(
        large,
        Action::Execute {
            statement_kind: StatementKind::PlSqlBlock,
            rows_affected: None,
            opens_transaction: false,
        },
    );
    scenario.on_sql_output(large, lines("chunked", 10));
    let limits = SessionLimits::new().with_server_output_chunk_lines(support::n(3));
    let (session, queue) = support::open_events_with(&scenario, limits, EventCaps::new());
    session
        .submit_set_server_output(RequestId(1), ON)
        .expect("accepted");
    let configured = support::drain_n(&queue, 1);
    assert!(matches!(
        configured.as_slice(),
        [SessionEvent::ServerOutputConfigured { request: RequestId(1), result: Ok(setting), .. }]
            if *setting == ON
    ));

    let seen = submit_and_collect(&session, &queue, 2, large);
    let labels: Vec<&str> = seen
        .iter()
        .map(label)
        .filter(|label| *label != "TransactionStateChanged")
        .collect();
    assert_eq!(
        labels,
        [
            "Executing",
            "ServerOutput",
            "ServerOutput",
            "ServerOutput",
            "ServerOutput",
            "Executed"
        ],
        "four bounded chunks, all inside the statement's window"
    );
    let delivered: Vec<String> = seen
        .into_iter()
        .filter_map(|event| match event {
            SessionEvent::ServerOutput { lines, .. } => Some(lines),
            _ => None,
        })
        .flatten()
        .map(String::from)
        .collect();
    assert_eq!(delivered, lines("chunked", 10));
}

#[test]
fn output_of_a_statement_that_needs_validation_arrives_in_the_next_executes_window() {
    // The second documented exception to "a statement's output sits in its
    // own window": no read is made on a session that needs validation, so
    // what the failed statement printed is read after the next execute,
    // ahead of that execute's own lines — delayed and not attributable to
    // the right statement, but not lost.
    let scenario = scenario();
    let flaky = "BEGIN time_out; END;";
    scenario.on_sql(
        flaky,
        Action::Fail(
            ScriptedError::new(ErrorKind::Timeout, "deadline elapsed")
                .with_session_state(SessionState::NeedsValidation),
        ),
    );
    scenario.on_sql_output(flaky, lines("printed by the timed-out block", 1));
    let (session, queue) = support::open_events(&scenario);
    session
        .submit_set_server_output(RequestId(1), ON)
        .expect("accepted");
    let _ = support::drain_n(&queue, 1);

    let failed = submit_and_collect(&session, &queue, 2, flaky);
    let labels: Vec<&str> = failed
        .iter()
        .map(label)
        .filter(|label| *label != "TransactionStateChanged")
        .collect();
    assert_eq!(labels, ["Executing", "Executed"], "no read before the ping");

    let next = submit_and_collect(&session, &queue, 3, BLOCK);
    let labels: Vec<&str> = next
        .iter()
        .map(label)
        .filter(|label| *label != "TransactionStateChanged")
        .collect();
    assert_eq!(labels, ["Executing", "ServerOutput", "Executed"]);
    let delivered: Vec<String> = next
        .into_iter()
        .filter_map(|event| match event {
            SessionEvent::ServerOutput { lines, .. } => Some(lines),
            _ => None,
        })
        .flatten()
        .map(String::from)
        .collect();
    let mut expected = lines("printed by the timed-out block", 1);
    expected.extend(lines("line", 3));
    assert_eq!(delivered, expected);
}

#[test]
fn a_statements_transaction_state_change_precedes_its_output() {
    // Exact transaction state (the mock default), so the INSERT flips
    // "possibly active" to true: that is recorded before the read, so the
    // change is announced before the output and both before the reply.
    let scenario = scenario();
    let (session, queue) = support::open_events(&scenario);
    session
        .submit_set_server_output(RequestId(1), ON)
        .expect("accepted");
    let _ = support::drain_n(&queue, 1);

    let seen = submit_and_collect(&session, &queue, 2, INSERT);
    let labels: Vec<&str> = seen.iter().map(label).collect();
    assert_eq!(
        labels,
        [
            "Executing",
            "TransactionStateChanged",
            "ServerOutput",
            "Executed"
        ]
    );
}

#[test]
fn a_fetch_is_not_followed_by_a_read() {
    // Fetches are not followed by a read (a round trip per batch would be the
    // price), so output a select-list function prints while rows are fetched
    // stays on the server until the next statement's read (ADR-0002 T). The
    // mock cannot print during a fetch, so what is checked here is the half
    // that is the core's decision: a fetch issues no read.
    let scenario = scenario();
    let (session, queue) = support::open_events(&scenario);
    session
        .submit_set_server_output(RequestId(1), ON)
        .expect("accepted");
    let _ = support::drain_n(&queue, 1);
    let seen = submit_and_collect(&session, &queue, 2, SELECT);
    let result = seen
        .iter()
        .find_map(|event| match event {
            SessionEvent::Executed {
                outcome: Ok(outcome),
                ..
            } => outcome.result,
            _ => None,
        })
        .expect("a result");
    let takes = counts(&scenario).takes;
    session
        .submit_fetch(RequestId(3), result, support::n(10))
        .expect("accepted");
    let _ = support::drain_until(&queue, |seen| {
        seen.iter()
            .any(|event| matches!(event, SessionEvent::Fetched { .. }))
    });
    assert_eq!(counts(&scenario).takes, takes, "a fetch reads no output");
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("close");
}

// ------------------------------------------------- event path: back-pressure

#[test]
fn at_the_cap_output_is_dropped_and_counted_so_delivered_plus_dropped_is_exact() {
    const TOTAL: usize = 100;
    let scenario = scenario();
    let large = "BEGIN print_a_lot; END;";
    scenario.on_sql(
        large,
        Action::Execute {
            statement_kind: StatementKind::PlSqlBlock,
            rows_affected: None,
            opens_transaction: false,
        },
    );
    scenario.on_sql_output(large, lines("flood", TOTAL));
    let gate = BlockGate::new();
    scenario.on_sql(PARK, Action::Block(BlockSpec::new(Arc::clone(&gate))));
    let limits = SessionLimits::new().with_server_output_chunk_lines(support::n(10));
    let caps = EventCaps::new().with_max_unsolicited_per_session(support::n(4));
    let (session, queue) = support::open_events_with(&scenario, limits, caps);

    session
        .submit_set_server_output(RequestId(1), ON)
        .expect("accepted");
    let _ = support::drain_n(&queue, 1);

    // Nothing is drained while the flood is produced: the statement behind it
    // parks the worker, and reaching the park means the flood's read loop and
    // its reply are both done.
    session
        .submit_execute(RequestId(2), Statement::new(large))
        .expect("accepted");
    session
        .submit_execute(RequestId(3), Statement::new(PARK))
        .expect("accepted");
    assert!(gate.wait_until_blocked(support::HANG_GUARD));
    assert_eq!(
        counts(&scenario).takes,
        TOTAL / 10,
        "the whole buffer was read"
    );

    let mut delivered = 0_usize;
    let mut reported = 0_u64;
    while let Some(event) = queue.next() {
        if let SessionEvent::ServerOutput { lines, dropped, .. } = event {
            delivered += lines.len();
            reported += u64::from(dropped);
        }
    }
    reported += u64::from(queue.pending_dropped_lines(session.id()));
    assert!(delivered < TOTAL, "the cap really did refuse some output");
    assert!(queue.dropped_unsolicited() > 0);
    assert_eq!(
        delivered as u64 + reported,
        TOTAL as u64,
        "every line is either delivered or counted as dropped: {delivered} + {reported}"
    );

    gate.release();
    let _ = support::drain_until(&queue, |seen| {
        seen.iter().any(|event| {
            matches!(
                event,
                SessionEvent::Executed {
                    request: RequestId(3),
                    ..
                }
            )
        })
    });
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("close");
}

// --------------------------------------------- event path: invalid UTF-8

#[test]
fn invalid_utf8_lines_the_driver_reports_ride_on_the_server_output_event() {
    // M2.12's fix round: `ServerOutputChunk::invalid_utf8_lines()` must not be
    // silently discarded at the `db-core` boundary. It rides on
    // `SessionEvent::ServerOutput` exactly the way `dropped` does.
    let scenario = scenario();
    let invalid_block = "BEGIN print_invalid; END;";
    scenario.on_sql(
        invalid_block,
        Action::Execute {
            statement_kind: StatementKind::PlSqlBlock,
            rows_affected: None,
            opens_transaction: false,
        },
    );
    scenario.on_sql_output_with_invalid_utf8_lines(invalid_block, lines("bad", 3), 2);
    let (session, queue) = support::open_events(&scenario);
    session
        .submit_set_server_output(RequestId(1), ON)
        .expect("accepted");
    let _ = support::drain_n(&queue, 1);

    let seen = submit_and_collect(&session, &queue, 2, invalid_block);
    let (delivered, invalid_total) = seen.iter().fold(
        (0_usize, 0_u32),
        |(delivered, invalid), event| match event {
            SessionEvent::ServerOutput {
                lines,
                invalid_utf8_lines,
                ..
            } => (delivered + lines.len(), invalid + invalid_utf8_lines),
            _ => (delivered, invalid),
        },
    );
    assert_eq!(delivered, 3, "every scripted line was still delivered");
    assert_eq!(
        invalid_total, 2,
        "the driver's per-chunk count survives the trip through db-core"
    );
}

// ------------------------------------------------- completion path: bound

#[test]
fn the_completion_path_log_is_bounded_and_counts_what_it_refused() {
    let extra = 25;
    let total = ServerOutputLog::MAX_RETAINED_LINES + extra;
    let scenario = scenario();
    let large = "BEGIN print_a_lot; END;";
    scenario.on_sql(
        large,
        Action::Execute {
            statement_kind: StatementKind::PlSqlBlock,
            rows_affected: None,
            opens_transaction: false,
        },
    );
    scenario.on_sql_output(large, lines("x", total));
    let session = support::open(&scenario);
    assert_eq!(session.set_server_output(ON).wait().expect("enable"), ON);
    session
        .execute(Statement::new(large))
        .wait()
        .expect("large block");

    let log = session.take_server_output();
    assert_eq!(log.lines.len(), ServerOutputLog::MAX_RETAINED_LINES);
    assert_eq!(log.dropped as usize, extra, "the refused lines are counted");
    assert_eq!(
        log.lines.first().map(AsRef::as_ref),
        Some("x 1"),
        "the start is kept"
    );
    assert!(
        session.take_server_output().is_empty(),
        "taking empties the log"
    );
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("close");
}

#[test]
fn the_completion_path_log_counts_invalid_utf8_lines_past_the_truncation_bound() {
    // The rule stated on `ServerOutputLog::invalid_utf8_lines`: the count is
    // the full count of invalid lines among everything drained from the
    // server, not only the ones the bounded log had room to retain. Every
    // line the trailing `extra` count marks invalid is one of the lines
    // `the_completion_path_log_is_bounded_and_counts_what_it_refused` shows
    // get refused by the log's bound, so this is the same scenario with the
    // refused lines scripted invalid.
    let extra = 5;
    let total = ServerOutputLog::MAX_RETAINED_LINES + extra;
    let scenario = scenario();
    let large = "BEGIN print_a_lot_invalid; END;";
    scenario.on_sql(
        large,
        Action::Execute {
            statement_kind: StatementKind::PlSqlBlock,
            rows_affected: None,
            opens_transaction: false,
        },
    );
    scenario.on_sql_output_with_invalid_utf8_lines(large, lines("x", total), extra as u32);
    let session = support::open(&scenario);
    assert_eq!(session.set_server_output(ON).wait().expect("enable"), ON);
    session
        .execute(Statement::new(large))
        .wait()
        .expect("large block");

    let log = session.take_server_output();
    assert_eq!(log.lines.len(), ServerOutputLog::MAX_RETAINED_LINES);
    assert_eq!(log.dropped as usize, extra, "the refused lines are counted");
    assert_eq!(
        log.invalid_utf8_lines, extra as u32,
        "counted in full even though the affected lines were themselves refused"
    );
    assert!(
        session.take_server_output().is_empty(),
        "taking empties the invalid-UTF-8 count along with everything else"
    );
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("close");
}

// ------------------------------------------------- abandon during a drain

#[test]
fn abandon_during_a_drain_does_not_hang_and_no_further_read_is_made() {
    support::with_timeout_guard(support::HANG_GUARD, || {
        let scenario = scenario();
        let large = "BEGIN print_a_lot; END;";
        scenario.on_sql(
            large,
            Action::Execute {
                statement_kind: StatementKind::PlSqlBlock,
                rows_affected: None,
                opens_transaction: false,
            },
        );
        scenario.on_sql_output(large, lines("never finished", 5));
        let limits = SessionLimits::new().with_server_output_chunk_lines(support::n(1));
        let (registry, queue) = support::registry_with(limits, EventCaps::new());
        let id = registry.open(support::driver(&scenario), support::params(), RequestId(1));
        let _ = support::drain_n(&queue, 1);
        let session = registry.get(id).expect("open");
        session
            .submit_set_server_output(RequestId(2), ON)
            .expect("accepted");
        let _ = support::drain_n(&queue, 1);

        let gate = BlockGate::new();
        scenario.block_take_server_output(Arc::clone(&gate));
        session
            .submit_execute(RequestId(3), Statement::new(large))
            .expect("accepted");
        assert!(gate.wait_until_blocked(support::HANG_GUARD));
        drop(session);

        // The worker is inside the first read. Abandon returns without
        // waiting for it — it cannot interrupt the read, and must not try.
        let _ = registry.abandon(id);
        gate.release();

        let seen = support::drain_until_terminal_of(&queue, id);
        assert!(
            seen.iter().any(|event| matches!(
                event,
                SessionEvent::Executed {
                    request: RequestId(3),
                    outcome: Ok(_),
                    ..
                }
            )),
            "the statement is still answered, with its own result: {seen:#?}"
        );
        let delivered: usize = seen
            .iter()
            .map(|event| match event {
                SessionEvent::ServerOutput { lines, .. } => lines.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(
            delivered, 1,
            "the read in progress completes and is delivered"
        );
        assert_eq!(
            counts(&scenario).takes,
            1,
            "and no further read is made once the session is being abandoned"
        );
        assert!(registry.retire(id));
    });
}
