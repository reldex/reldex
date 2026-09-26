//! What the event path delivers since it drains `db-core`'s event queue
//! (M2.15, ABI 3.2): a `TERMINAL` for every way a session can end — including
//! a loss mid-statement that nobody closed —, `abandon` through the real
//! session registry, `EXECUTING`/`TRANSACTION_STATE` progress, server output
//! ahead of the reply that follows it, and a queue bound that refuses a
//! caller who stops draining instead of blocking a worker.
//!
//! Every test runs on the mock driver; nothing here needs a database, and
//! nothing asserts a duration.

#![allow(
    unsafe_code,
    reason = "these tests drive the crate's own C ABI, which is what they exist to prove"
)]

mod support;

use reldex_ffi::{
    ReldexAbandonOutcome, ReldexCloseDisposition, ReldexCloseOutcome, ReldexCompletedOperation,
    ReldexErrorKind, ReldexEvent, ReldexEventKind, ReldexMockFailure, ReldexMockScenarioConfig,
    ReldexMockStatement, ReldexOpenOptions, ReldexSessionState, ReldexStatus,
    reldex_hub_open_session, reldex_hub_pending_events, reldex_mock_statement,
    reldex_server_output_lines_count, reldex_server_output_lines_get, reldex_session_execute,
};

use support::{ErrorSnapshot, Harness, take_error, wait_until};

fn config() -> ReldexMockScenarioConfig {
    ReldexMockScenarioConfig {
        rows: 100,
        seed: 3,
        ..ReldexMockScenarioConfig::default()
    }
}

/// An event with everything it owned taken out of it.
struct Seen {
    kind: i32,
    request: u64,
    session_state: i32,
    error: Option<ErrorSnapshot>,
    lines: Vec<String>,
    event: ReldexEvent,
}

impl Seen {
    fn is(&self, kind: ReldexEventKind) -> bool {
        self.kind == kind as i32
    }
}

fn seen(event: ReldexEvent) -> Seen {
    let lines = (0..unsafe_lines_count(&event))
        .map(|index| {
            // SAFETY: the lines are live until released below.
            let line = unsafe { reldex_server_output_lines_get(event.server_output_lines, index) };
            // SAFETY: the string borrows from the still-live lines.
            unsafe { line.as_str() }.unwrap_or_default().to_owned()
        })
        .collect();
    let error = take_error(&event);
    support::release_batch(&event);
    Seen {
        kind: event.kind,
        request: event.request,
        session_state: event.session_state,
        error,
        lines,
        event,
    }
}

fn unsafe_lines_count(event: &ReldexEvent) -> usize {
    // SAFETY: null or live lines from a drained event.
    unsafe { reldex_server_output_lines_count(event.server_output_lines) }
}

/// Every event, of any kind, until (and including) the session's `TERMINAL`.
fn until_terminal(harness: &Harness) -> Vec<Seen> {
    let mut events = Vec::new();
    loop {
        let event = seen(harness.next_any_event());
        let done = event.is(ReldexEventKind::Terminal);
        events.push(event);
        if done {
            return events;
        }
    }
}

/// Every event, of any kind, until (and including) the reply to `request`.
fn until_reply(harness: &Harness, request: u64) -> Vec<Seen> {
    let mut events = Vec::new();
    loop {
        let event = seen(harness.next_any_event());
        let done = event.request == request
            && !event.is(ReldexEventKind::Executing)
            && !event.is(ReldexEventKind::Terminal);
        events.push(event);
        if done {
            return events;
        }
    }
}

fn execute_with_deadline(
    harness: &Harness,
    session: u64,
    request: u64,
    statement: ReldexMockStatement,
    deadline_ms: u64,
) -> ReldexStatus {
    let sql = reldex_mock_statement(statement as i32);
    // SAFETY: the hub is live and `sql` is a `'static` string.
    unsafe { reldex_session_execute(harness.hub(), session, request, sql, deadline_ms) }
}

fn last_error_kind() -> i32 {
    let error = reldex_ffi::reldex_last_error_take();
    assert!(!error.is_null(), "a refused call records why");
    let mut view = reldex_ffi::ReldexErrorView::default();
    // SAFETY: the error is live until freed below.
    let status = unsafe { reldex_ffi::reldex_error_view(error, std::ptr::from_mut(&mut view)) };
    assert_eq!(status, ReldexStatus::Ok);
    support::free_error(error);
    view.kind
}

// ---------------------------------------------------------------- TERMINAL

#[test]
fn a_session_lost_mid_statement_ends_with_terminal_without_a_close() {
    let harness = Harness::new();
    let session = harness.open(config());
    // A transaction the loss will take with it.
    assert_eq!(
        harness.execute(session, 2, ReldexMockStatement::Dml),
        ReldexStatus::Ok
    );
    let dml = until_reply(&harness, 2);
    assert!(dml.last().is_some_and(|reply| reply.error.is_none()));

    assert_eq!(
        harness.execute(session, 3, ReldexMockStatement::LoseSession),
        ReldexStatus::Ok
    );
    let events = until_terminal(&harness);
    let failed = events
        .iter()
        .find(|event| event.is(ReldexEventKind::Executed))
        .expect("the statement is answered");
    assert_eq!(failed.request, 3);
    let error = failed
        .error
        .as_ref()
        .expect("the loss is the reply's error");
    assert_eq!(error.kind, ReldexErrorKind::NetworkLost as i32);
    assert_eq!(error.native_code, Some(3113));
    assert_eq!(failed.session_state, ReldexSessionState::Lost as i32);

    let terminal = events.last().expect("until_terminal ends on it");
    assert_eq!(terminal.event.session, session);
    assert_eq!(terminal.request, 0, "TERMINAL answers no request");
    assert_eq!(terminal.session_state, ReldexSessionState::Lost as i32);
    let cause = terminal.error.as_ref().expect("a loss names its cause");
    assert_eq!(cause.native_code, Some(3113));
    assert!(
        terminal.event.transaction_possibly_lost,
        "the DML's transaction went with the connection, and the UI must say so"
    );
    assert!(!terminal.event.abandoned);

    assert_eq!(
        harness.execute(session, 4, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::NotFound,
        "draining TERMINAL retired the id"
    );
    support::free_error(reldex_ffi::reldex_last_error_take());
    assert!(harness.poll_event().is_none());
}

#[test]
fn a_clean_close_is_followed_by_one_terminal_that_lost_nothing() {
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.execute(session, 2, ReldexMockStatement::Dml),
        ReldexStatus::Ok
    );
    until_reply(&harness, 2);
    assert_eq!(
        harness.close(session, 3, ReldexCloseDisposition::Commit),
        ReldexStatus::Ok
    );
    let events = until_terminal(&harness);
    let kinds: Vec<i32> = events
        .iter()
        .filter(|event| !event.is(ReldexEventKind::TransactionState))
        .map(|event| event.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            ReldexEventKind::SessionClosed as i32,
            ReldexEventKind::Terminal as i32
        ]
    );
    let closed = &events[events.len() - 2];
    assert_eq!(
        closed.event.close_outcome,
        ReldexCloseOutcome::Closed as i32
    );
    let terminal = events.last().expect("ends on TERMINAL");
    assert_eq!(terminal.session_state, ReldexSessionState::Closed as i32);
    assert!(terminal.error.is_none());
    assert!(!terminal.event.transaction_possibly_lost);
    assert!(!terminal.event.abandoned);
}

#[test]
fn closing_a_session_that_was_lost_reports_failed_never_closed() {
    // `SPEC.md` §10: a close must not report success for a session whose
    // transaction the server already rolled back. Submitted after the loss
    // but before its TERMINAL is drained, the close is accepted — and its
    // reply, which may follow the TERMINAL, says FAILED.
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.execute(session, 2, ReldexMockStatement::LoseSession),
        ReldexStatus::Ok
    );
    let failed = seen(harness.next_event());
    assert_eq!(failed.request, 2);
    assert_eq!(
        harness.close(session, 3, ReldexCloseDisposition::Commit),
        ReldexStatus::Ok
    );
    let mut close = None;
    let mut terminals = 0;
    while close.is_none() || terminals == 0 {
        let event = seen(harness.next_event());
        if event.is(ReldexEventKind::Terminal) {
            terminals += 1;
        } else {
            assert!(event.is(ReldexEventKind::SessionClosed));
            close = Some(event);
        }
    }
    let close = close.expect("the loop ends only once the close is answered");
    assert_eq!(close.request, 3);
    assert_eq!(close.event.close_outcome, ReldexCloseOutcome::Failed as i32);
    assert!(!close.event.session_still_open);
    assert!(close.error.is_some(), "a failed close says why");
    assert_eq!(terminals, 1, "exactly one TERMINAL");
    assert!(harness.poll_event().is_none());
}

#[test]
fn a_failed_open_answers_opened_with_the_error_then_terminal() {
    let harness = Harness::new();
    let (session, opened) = harness.open_raw(
        ReldexMockScenarioConfig {
            connect_failure: ReldexMockFailure::Unreachable as i32,
            ..config()
        },
        7,
    );
    let opened = seen(opened);
    assert!(opened.is(ReldexEventKind::Opened));
    assert_eq!(opened.request, 7);
    let error = opened.error.as_ref().expect("a failed connect is an error");
    assert_eq!(error.kind, ReldexErrorKind::Connection as i32);
    assert_eq!(error.native_code, Some(12541));
    assert_eq!(opened.session_state, ReldexSessionState::Lost as i32);

    // Before its TERMINAL is drained the id is known, but nothing is accepted.
    assert_eq!(
        harness.execute(session, 8, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::InvalidState
    );
    support::free_error(reldex_ffi::reldex_last_error_take());

    let terminal = seen(harness.next_event());
    assert!(terminal.is(ReldexEventKind::Terminal));
    assert_eq!(terminal.session_state, ReldexSessionState::Lost as i32);
    assert!(
        !terminal.event.transaction_possibly_lost,
        "nothing connected, so nothing was lost"
    );
    assert_eq!(
        harness.execute(session, 9, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::NotFound
    );
    support::free_error(reldex_ffi::reldex_last_error_take());
}

#[test]
fn a_ping_that_finds_the_connection_gone_ends_the_session() {
    let harness = Harness::new();
    let session = harness.open(ReldexMockScenarioConfig {
        ping_failure: ReldexMockFailure::Lost as i32,
        ..config()
    });
    assert_eq!(harness.ping(session, 2), ReldexStatus::Ok);
    let events = until_terminal(&harness);
    let ping = events.first().expect("the ping is answered first");
    assert!(ping.is(ReldexEventKind::Completed));
    assert_eq!(
        ping.event.completed_operation,
        ReldexCompletedOperation::Ping as i32
    );
    assert_eq!(
        ping.error.as_ref().and_then(|error| error.native_code),
        Some(3113)
    );
    assert_eq!(events.len(), 2, "the ping's reply, then TERMINAL");
}

// ------------------------------------------------------------------ abandon

#[test]
fn abandoning_an_open_session_never_commits_and_says_so() {
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.execute(session, 2, ReldexMockStatement::Dml),
        ReldexStatus::Ok
    );
    until_reply(&harness, 2);

    let (status, outcome, early) = harness.abandon(session);
    assert_eq!(status, ReldexStatus::Ok);
    assert_eq!(outcome, ReldexAbandonOutcome::Open as i32);
    assert!(early, "a transaction may be open, so warn at once");

    assert_eq!(
        harness.execute(session, 3, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::InvalidState,
        "nothing is accepted after an abandon"
    );
    support::free_error(reldex_ffi::reldex_last_error_take());
    let (status, again, _) = harness.abandon(session);
    assert_eq!(status, ReldexStatus::Ok);
    assert_eq!(again, ReldexAbandonOutcome::AlreadyEnded as i32);

    let events = until_terminal(&harness);
    let terminal = events.last().expect("ends on TERMINAL");
    assert!(
        terminal.event.abandoned,
        "the TERMINAL says the caller ended it"
    );
    assert!(
        terminal.event.transaction_possibly_lost,
        "the authoritative answer: the server rolled the DML back"
    );
    assert_eq!(harness.session_count(), 0);
    let (status, _, _) = harness.abandon(session);
    assert_eq!(status, ReldexStatus::NotFound);
    support::free_error(reldex_ffi::reldex_last_error_take());
}

#[test]
fn abandoning_a_connect_that_has_not_returned_answers_it_at_once() {
    let harness = Harness::new();
    let options = ReldexOpenOptions {
        mock: ReldexMockScenarioConfig {
            block_connect: true,
            ..config()
        },
        ..ReldexOpenOptions::default()
    };
    let mut session = 0_u64;
    // SAFETY: the hub is live; `options` and `session` are real locals.
    let status = unsafe {
        reldex_hub_open_session(
            harness.hub(),
            std::ptr::from_ref(&options),
            5,
            std::ptr::from_mut(&mut session),
        )
    };
    assert_eq!(status, ReldexStatus::Ok);

    let (status, outcome, early) = harness.abandon(session);
    assert_eq!(status, ReldexStatus::Ok);
    assert_eq!(outcome, ReldexAbandonOutcome::Connecting as i32);
    assert!(!early);

    let opened = seen(harness.next_event());
    assert!(opened.is(ReldexEventKind::Opened));
    assert_eq!(opened.request, 5);
    assert_eq!(
        opened.error.as_ref().map(|error| error.kind),
        Some(ReldexErrorKind::Cancelled as i32)
    );
    let terminal = seen(harness.next_event());
    assert!(terminal.is(ReldexEventKind::Terminal));
    assert!(terminal.event.abandoned);
    assert!(!terminal.event.transaction_possibly_lost);
    // Retiring the session released the parked connect; nothing is adopted.
    assert_eq!(harness.session_count(), 0);
    assert!(harness.poll_event().is_none());
}

#[test]
fn abandoning_a_session_inside_a_statement_waits_for_nothing() {
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.execute(session, 2, ReldexMockStatement::Block),
        ReldexStatus::Ok
    );
    let started = seen(harness.next_any_event());
    assert!(started.is(ReldexEventKind::Executing));

    // Returns while the worker is still inside the driver call.
    let (status, outcome, early) = harness.abandon(session);
    assert_eq!(status, ReldexStatus::Ok);
    assert_eq!(outcome, ReldexAbandonOutcome::Open as i32);
    assert!(
        early,
        "a statement in flight could have opened a transaction"
    );

    // Whether the abandon's cancel reached it or the release does, the
    // statement still gets its one reply, then the session its TERMINAL.
    assert_eq!(harness.release_block(session), ReldexStatus::Ok);
    let events = until_terminal(&harness);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.is(ReldexEventKind::Executed) && event.request == 2)
            .count(),
        1
    );
    assert!(
        events
            .last()
            .is_some_and(|terminal| terminal.event.abandoned)
    );
}

// ------------------------------------------------ EXECUTING / TRANSACTION_STATE

#[test]
fn executing_opens_a_statement_and_transaction_state_tracks_the_transaction() {
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        execute_with_deadline(&harness, session, 2, ReldexMockStatement::Dml, 5_000),
        ReldexStatus::Ok
    );
    let mut events = until_reply(&harness, 2);
    let started = events.first().expect("at least the reply");
    assert!(
        started.is(ReldexEventKind::Executing),
        "EXECUTING comes first"
    );
    assert_eq!(started.request, 2, "and names the statement it is about");
    assert!(started.event.has_deadline);
    assert_eq!(started.event.deadline_ms, 5_000);
    assert!(
        events
            .last()
            .is_some_and(|reply| reply.is(ReldexEventKind::Executed))
    );
    // The flip may be queued just before or just after the reply.
    if !events
        .iter()
        .any(|event| event.is(ReldexEventKind::TransactionState))
    {
        events.push(seen(harness.next_any_event()));
    }
    let opened = events
        .iter()
        .find(|event| event.is(ReldexEventKind::TransactionState))
        .expect("the DML opened a transaction");
    assert_eq!(opened.request, 0, "unsolicited");
    assert!(opened.event.transaction_possibly_active);

    assert_eq!(harness.commit(session, 3), ReldexStatus::Ok);
    let mut events = until_reply(&harness, 3);
    if !events
        .iter()
        .any(|event| event.is(ReldexEventKind::TransactionState))
    {
        events.push(seen(harness.next_any_event()));
    }
    assert!(
        events
            .iter()
            .all(|event| !event.is(ReldexEventKind::Executing))
    );
    let ended = events
        .iter()
        .find(|event| event.is(ReldexEventKind::TransactionState))
        .expect("the commit ended it");
    assert!(!ended.event.transaction_possibly_active);

    // No deadline armed, none echoed.
    assert_eq!(
        harness.execute(session, 4, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let events = until_reply(&harness, 4);
    assert!(events[0].is(ReldexEventKind::Executing));
    assert!(!events[0].event.has_deadline);
}

// ---------------------------------------------------------------- server output

#[test]
fn server_output_arrives_ahead_of_the_reply_that_follows_it() {
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.set_server_output(session, 2, true),
        ReldexStatus::Ok
    );
    let configured = seen(harness.next_event());
    assert!(configured.is(ReldexEventKind::ServerOutputConfigured));

    assert_eq!(
        harness.execute(session, 3, ReldexMockStatement::ServerOutput),
        ReldexStatus::Ok
    );
    let events = until_reply(&harness, 3);
    let kinds: Vec<i32> = events.iter().map(|event| event.kind).collect();
    assert_eq!(
        kinds,
        [
            ReldexEventKind::Executing as i32,
            ReldexEventKind::ServerOutput as i32,
            ReldexEventKind::Executed as i32,
        ],
        "the statement's output lies between its EXECUTING and its EXECUTED"
    );
    let output = &events[1];
    assert_eq!(output.request, 0);
    assert_eq!(output.event.session, session);
    assert_eq!(
        output.lines,
        [
            "reldex: first line",
            "",
            "reldex: \u{FFFD} arrived as invalid UTF-8"
        ]
    );
    assert_eq!(output.event.server_output_invalid_utf8_lines, 1);
    assert_eq!(output.event.server_output_dropped, 0);
    assert!(output.error.is_none());
}

/// Submits `statements` server-output blocks without draining, and waits
/// until the worker has run them all.
fn flood_output(harness: &Harness, session: u64, statements: u64) {
    for request in 0..statements {
        assert_eq!(
            harness.execute(session, 100 + request, ReldexMockStatement::ServerOutput),
            ReldexStatus::Ok
        );
    }
    // EXECUTING + EXECUTED per statement, plus the 256 output events the
    // queue keeps for one session; the rest are dropped and counted. The
    // statement opens no transaction, so nothing else is unsolicited.
    let expected = usize::try_from(statements * 2).expect("fits") + 256;
    wait_until("every flooded statement to be answered", || {
        // SAFETY: the hub is live.
        unsafe { reldex_hub_pending_events(harness.hub()) == expected }
    });
}

#[test]
fn server_output_past_the_cap_is_dropped_and_counted_on_the_next_output() {
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.set_server_output(session, 2, true),
        ReldexStatus::Ok
    );
    seen(harness.next_event());

    flood_output(&harness, session, 300);
    let mut outputs = 0;
    while let Some(event) = harness.poll_event() {
        let event = seen(event);
        if event.is(ReldexEventKind::ServerOutput) {
            outputs += 1;
            assert_eq!(event.event.server_output_dropped, 0);
        }
    }
    assert_eq!(outputs, 256, "the per-session cap on undrained output");

    assert_eq!(
        harness.execute(session, 999, ReldexMockStatement::ServerOutput),
        ReldexStatus::Ok
    );
    let events = until_reply(&harness, 999);
    let output = events
        .iter()
        .find(|event| event.is(ReldexEventKind::ServerOutput))
        .expect("the next statement's output");
    assert_eq!(
        output.event.server_output_dropped,
        44 * 3,
        "44 statements' three lines each were dropped, and the UI must say so"
    );
}

#[test]
fn server_output_dropped_with_nothing_after_it_is_reported_on_terminal() {
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.set_server_output(session, 2, true),
        ReldexStatus::Ok
    );
    seen(harness.next_event());

    flood_output(&harness, session, 260);
    while let Some(event) = harness.poll_event() {
        seen(event);
    }
    assert_eq!(
        harness.close(session, 3, ReldexCloseDisposition::Rollback),
        ReldexStatus::Ok
    );
    let events = until_terminal(&harness);
    let terminal = events.last().expect("ends on TERMINAL");
    assert_eq!(terminal.event.server_output_dropped, 4 * 3);
}

// --------------------------------------------------------- bound and waker

#[test]
fn a_caller_that_stops_draining_is_refused_at_the_bound_and_never_blocks_a_worker() {
    let harness = Harness::new();
    let session = harness.open(config());
    let before = harness.signal.wakes();

    // 1,024 undrained replies is the per-session bound (db-core's
    // `max_outstanding_requests`). Every submit below returns at once; the
    // worker runs every statement while nothing is drained.
    for request in 0..1_024_u64 {
        assert_eq!(
            harness.execute(session, 10 + request, ReldexMockStatement::ServerOutput),
            ReldexStatus::Ok
        );
    }
    wait_until("the worker to run all 1,024 statements undrained", || {
        // SAFETY: the hub is live.
        unsafe { reldex_hub_pending_events(harness.hub()) == 2_048 }
    });
    assert_eq!(
        harness.signal.wakes(),
        before + 1,
        "2,048 events onto an empty queue are one wake"
    );

    // One more is refused, accepting nothing.
    assert_eq!(
        harness.execute(session, 5_000, ReldexMockStatement::ServerOutput),
        ReldexStatus::Error
    );
    assert_eq!(last_error_kind(), ReldexErrorKind::Resource as i32);
    // SAFETY: the hub is live.
    assert_eq!(unsafe { reldex_hub_pending_events(harness.hub()) }, 2_048);

    // Draining one reply gives one slot back.
    let started = seen(harness.next_any_event());
    assert!(started.is(ReldexEventKind::Executing));
    let reply = seen(harness.next_any_event());
    assert_eq!(reply.request, 10);
    assert_eq!(
        harness.execute(session, 5_001, ReldexMockStatement::ServerOutput),
        ReldexStatus::Ok
    );
    let mut answered = 0;
    loop {
        let event = seen(harness.next_any_event());
        if event.is(ReldexEventKind::Executed) {
            answered += 1;
            if event.request == 5_001 {
                break;
            }
        }
    }
    assert_eq!(answered, 1_024, "every accepted request answered, once");
}
