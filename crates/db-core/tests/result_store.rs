//! The Result Store (ADR-0004) through real sessions and the mock driver.
//!
//! `src/store/result_store/tests.rs` drives the store's state machine with
//! hand-built replies; this file holds it to the same promises against a
//! real worker, on both reply paths, and — through [`SessionResults`] — on
//! the event path the product uses, where several fetches really are in
//! flight while a commit, a typed `COMMIT`, a DDL statement or the loss of the
//! session overtakes them.

mod support;

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::thread;

use reldex_db_core::{
    Cap, CapSource, CellValue, CompletedOperation, DatabaseSession, EndCause, EventQueue,
    ExecuteOutcome, FetchMore, Fetched, LimitKind, LobCell, LobUnavailable, MoreRows, Observed,
    RequestId, ResultCaps, ResultId, ResultPhase, ResultPolicy, ResultStore, SavepointName,
    SessionEvent, SessionResults, Sourced, Statement, StoreAction,
};
use reldex_db_driver_api::{ErrorKind, LobKind, NativeError, SqlType, StatementKind};
use reldex_driver_mock::{
    Action, ColumnSpec, GeneratedQuerySpec, QueryPlan, QuerySource, Scenario, ScriptValue,
    ScriptedError,
};

const SELECT_BIG: &str = "SELECT id, name, created FROM big";
const SELECT_DOCS: &str = "SELECT doc FROM docs";
const TYPED_COMMIT: &str = "COMMIT";
const CREATE_TABLE: &str = "CREATE TABLE t (n NUMBER)";
const LOSE_SESSION: &str = "SELECT 1 FROM dual";

fn big(rows: u64) -> GeneratedQuerySpec {
    GeneratedQuerySpec::s14_shape(rows, 7)
}

/// A scenario serving [`SELECT_BIG`] from `spec`, and the statements that end
/// a transaction or the session.
fn scenario(spec: &GeneratedQuerySpec) -> Arc<Scenario> {
    let scenario = support::scenario();
    scenario.on_sql(SELECT_BIG, Action::GeneratedQuery(spec.clone()));
    scenario.on_sql(
        TYPED_COMMIT,
        Action::Execute {
            statement_kind: StatementKind::TransactionControl,
            rows_affected: None,
            opens_transaction: false,
        },
    );
    scenario.on_sql(CREATE_TABLE, Action::Ddl);
    scenario.on_sql(
        LOSE_SESSION,
        Action::Fail(
            ScriptedError::new(ErrorKind::NetworkLost, "connection reset")
                .with_native(3113, "ORA-03113: end-of-file on communication channel"),
        ),
    );
    scenario
}

fn nz(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("non-zero")
}

fn policy(rows: Cap, fetch_rows: usize, in_flight: usize) -> ResultPolicy {
    ResultPolicy::new(ResultCaps::new(
        Sourced::new(rows, CapSource::Application),
        Sourced::new(Cap::Unlimited, CapSource::BuiltIn),
    ))
    .with_fetch_rows(nz(fetch_rows))
    .with_fetches_in_flight(nz(in_flight))
}

/// Whether the store's `row` reads exactly what the generator produced.
fn assert_row(store: &ResultStore, spec: &GeneratedQuerySpec, row: usize) {
    for column in 0..3 {
        let expected = spec
            .expected_cell(u64::try_from(row).expect("small"), column)
            .expect("in range");
        let actual = store.value(row, column).expect("the store holds the row");
        let same = match (&expected, actual) {
            (ScriptValue::Null, CellValue::Null) => true,
            (ScriptValue::Number(expected), CellValue::Number(actual)) => {
                actual.to_number() == *expected && actual.to_string() == expected.to_string()
            }
            (ScriptValue::Text(expected), CellValue::Text(actual)) => actual == expected,
            (ScriptValue::Timestamp(expected), CellValue::Timestamp(actual)) => actual == *expected,
            _ => false,
        };
        assert!(
            same,
            "row {row} column {column}: {actual:?} vs {expected:?}"
        );
    }
}

// ------------------------------------------------ one store, both paths

/// Runs every fetch the store makes due, answering each through `session`,
/// until nothing is due. Returns what the store made of every reply.
fn drive(session: &support::Session, store: &mut ResultStore) -> Vec<Fetched> {
    let mut outcomes = Vec::new();
    loop {
        let mut answered = Vec::new();
        store
            .pump(|action| match action {
                StoreAction::Fetch(fetch) => {
                    answered.push((fetch.ticket(), session.fetch_segment(fetch).wait()));
                    Ok(())
                }
                StoreAction::CloseResult(result) => session.close_result(result).wait(),
                other => panic!("an action this test does not know: {other:?}"),
            })
            .expect("every submit is accepted");
        if answered.is_empty() {
            return outcomes;
        }
        for (ticket, reply) in answered {
            outcomes.push(store.on_fetched(ticket, reply));
        }
    }
}

fn open_store(session: &support::Session, policy: ResultPolicy) -> ResultStore {
    let outcome = session
        .execute(Statement::new(SELECT_BIG))
        .wait()
        .expect("execute");
    ResultStore::new(outcome.result.expect("a cursor"), &outcome.columns, policy)
}

fn a_store_fetches_on_demand_to_the_end_on_the_worker(path: support::ReplyPath) {
    let spec = big(1_000);
    let scenario = scenario(&spec);
    let session = support::open_on(&scenario, path);
    let mut store = open_store(&session, policy(Cap::Unlimited, 100, 2));

    // The first page only: nothing is fetched that nobody asked for.
    drive(&session, &mut store);
    assert_eq!(store.row_count(), 100);
    assert!(matches!(store.state().phase(), ResultPhase::Open));

    store.set_demand(450);
    drive(&session, &mut store);
    assert!(
        (450..=650).contains(&store.row_count()),
        "{}",
        store.row_count()
    );

    store.fetch_all();
    let outcomes = drive(&session, &mut store);
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome, Fetched::Appended { .. }))
    );
    assert!(matches!(store.state().phase(), ResultPhase::Complete));
    assert_eq!(store.row_count(), 1_000);
    for row in [0, 1, 9, 24, 99, 100, 499, 999] {
        assert_row(&store, &spec, row);
    }

    // Every driver call ran on the worker, the compaction included.
    let seen = scenario.thread_ids_seen(session.connection_id());
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_ne!(seen[0], thread::current().id());
}

fn the_row_cap_stops_exactly_and_fetch_more_continues_the_same_cursor(path: support::ReplyPath) {
    let spec = big(10_000);
    let scenario = scenario(&spec);
    let session = support::open_on(&scenario, path);
    let mut store = open_store(&session, policy(Cap::At(nz(1_000)), 300, 2));
    store.fetch_all();
    drive(&session, &mut store);

    let state = store.state();
    assert!(matches!(
        state.phase(),
        ResultPhase::AtLimit {
            limit: LimitKind::Rows,
            more: MoreRows::Yes,
            cursor_open: true
        }
    ));
    assert_eq!(state.rows(), 1_000);
    assert_eq!(state.retained_rows(), 1_001, "the lookahead row");
    assert!(
        store.value(1_000, 0).is_none(),
        "the lookahead is not shown"
    );

    assert!(store.fetch_more(FetchMore::Step));
    drive(&session, &mut store);
    assert_eq!(store.row_count(), 2_000);
    assert_eq!(
        store.state().caps().max_rows(),
        Sourced::new(Cap::At(nz(2_000)), CapSource::FetchMore)
    );
    // The same cursor, not a re-run: the rows continue where they stopped.
    for row in [999, 1_000, 1_001, 1_999] {
        assert_row(&store, &spec, row);
    }
}

fn closing_the_cursor_at_the_cap_releases_it(path: support::ReplyPath) {
    let spec = big(10_000);
    let scenario = scenario(&spec);
    let session = support::open_on(&scenario, path);
    let caps = ResultCaps::new(
        Sourced::new(Cap::At(nz(500)), CapSource::Profile),
        Sourced::new(Cap::Unlimited, CapSource::BuiltIn),
    )
    .with_close_cursor_at_limit(Sourced::new(true, CapSource::Profile));
    let mut store = open_store(&session, ResultPolicy::new(caps).with_fetch_rows(nz(200)));
    let closed_before = scenario.counts().cursors_closed;
    store.fetch_all();
    drive(&session, &mut store);

    assert!(matches!(
        store.state().phase(),
        ResultPhase::AtLimit {
            limit: LimitKind::Rows,
            more: MoreRows::Yes,
            cursor_open: false
        }
    ));
    assert_eq!(scenario.counts().cursors_closed, closed_before + 1);
    assert!(
        !store.fetch_more(FetchMore::Step),
        "only a re-run continues"
    );
    assert_eq!(store.row_count(), 500);
    assert_row(&store, &spec, 499);
}

fn a_failed_fetch_keeps_the_rows_before_it(path: support::ReplyPath) {
    let scenario = support::scenario();
    let rows = (0..50_i64).map(|n| vec![ScriptValue::from(n)]).collect();
    let plan = QueryPlan::new(vec![ColumnSpec::new("N", SqlType::Number)], rows)
        .with_fail_on_batch(
            3,
            ScriptedError::new(ErrorKind::Transaction, "snapshot too old")
                .with_native(1555, "ORA-01555: snapshot too old"),
        );
    scenario.on_sql("SELECT n FROM t", Action::query(QuerySource::Fixed(plan)));
    let session = support::open_on(&scenario, path);
    let outcome = session
        .execute(Statement::new("SELECT n FROM t"))
        .wait()
        .expect("execute");
    let mut store = ResultStore::new(
        outcome.result.expect("a cursor"),
        &outcome.columns,
        policy(Cap::Unlimited, 10, 1),
    );
    store.fetch_all();
    let outcomes = drive(&session, &mut store);
    assert_eq!(outcomes.last(), Some(&Fetched::Failed));
    match store.state().phase() {
        ResultPhase::Failed { after, error } => {
            assert_eq!(after, 20);
            assert_eq!(error.kind(), ErrorKind::Transaction);
            assert_eq!(error.native().map(NativeError::code), Some(1555));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(store.row_count(), 20, "the prefix stays");
    assert!(matches!(store.value(19, 0), Some(CellValue::Number(n)) if n.to_string() == "19"));
}

// ------------------------------------------------------------------ LOBs

fn lob_scenario() -> Arc<Scenario> {
    let scenario = support::scenario();
    let rows = vec![
        vec![ScriptValue::Lob {
            kind: LobKind::Character,
            bytes: b"the first document".to_vec(),
        }],
        vec![ScriptValue::Null],
        vec![ScriptValue::Lob {
            kind: LobKind::Character,
            bytes: b"the third".to_vec(),
        }],
    ];
    scenario.on_sql(
        SELECT_DOCS,
        Action::query(QuerySource::Fixed(QueryPlan::new(
            vec![ColumnSpec::new(
                "DOC",
                SqlType::CharacterLob { national: false },
            )],
            rows,
        ))),
    );
    scenario
}

fn read_all(session: &support::Session, cell: Option<LobCell>) -> String {
    let Some(LobCell::Readable(lob)) = cell else {
        panic!("a readable LOB, got {cell:?}");
    };
    let mut collected = Vec::new();
    loop {
        let chunk = session
            .read_lob_chunk(lob, nz(4))
            .wait()
            .expect("read a chunk");
        if chunk.is_empty() {
            return String::from_utf8(collected).expect("UTF-8");
        }
        collected.extend_from_slice(&chunk);
    }
}

/// ADR-0002 K1, extended to the store: a segment holds handle ids, never a
/// locator, so the store and its segments may be dropped on any thread while
/// every driver call — compaction, LOB reads, releases — stays on the worker.
fn only_plain_data_crosses_threads_in_a_store(path: support::ReplyPath) {
    let scenario = lob_scenario();
    let session = support::open_on(&scenario, path);
    let outcome = session
        .execute(Statement::new(SELECT_DOCS))
        .wait()
        .expect("execute");
    let mut store = ResultStore::new(
        outcome.result.expect("a cursor"),
        &outcome.columns,
        policy(Cap::Unlimited, 10, 1),
    );
    drive(&session, &mut store);
    assert!(matches!(store.state().phase(), ResultPhase::Complete));

    assert_eq!(store.value(1, 0), Some(CellValue::Null));
    assert_eq!(store.lob(1, 0), None, "SQL NULL has nothing to read");
    assert_eq!(read_all(&session, store.lob(0, 0)), "the first document");

    // A segment shared with another thread and dropped there, then the whole
    // store: plain data, so no driver code runs.
    let (segment, _) = store.segment_for_row(0).expect("row 0");
    let segment = Arc::clone(segment);
    thread::spawn(move || drop(segment))
        .join()
        .expect("a segment may be dropped anywhere");
    let third = store.lob(2, 0);
    thread::spawn(move || drop(store))
        .join()
        .expect("a store may be dropped anywhere");

    // Its handles still read on the worker until the transaction ends.
    assert_eq!(read_all(&session, third), "the third");

    let seen = scenario.thread_ids_seen(session.connection_id());
    assert_eq!(
        seen.len(),
        1,
        "one thread runs driver code for a connection, however stores and segments move: \
         {seen:?}"
    );
    assert_ne!(seen[0], thread::current().id());
}

fn a_commit_makes_lob_cells_unavailable_never_null(path: support::ReplyPath) {
    let scenario = lob_scenario();
    let session = support::open_on(&scenario, path);
    let outcome = session
        .execute(Statement::new(SELECT_DOCS))
        .wait()
        .expect("execute");
    let mut store = ResultStore::new(
        outcome.result.expect("a cursor"),
        &outcome.columns,
        policy(Cap::Unlimited, 10, 1),
    );
    drive(&session, &mut store);
    let Some(LobCell::Readable(handle)) = store.lob(0, 0) else {
        panic!("row 0 is readable before the commit");
    };

    store.transaction_end_submitted();
    let committed = session.commit().wait();
    store.transaction_end_answered(committed.is_ok());
    committed.expect("commit");

    assert!(matches!(store.state().phase(), ResultPhase::Complete));
    assert_eq!(
        store.lob(0, 0),
        Some(LobCell::Unavailable(LobUnavailable::TransactionEnded))
    );
    assert_eq!(
        store.value(2, 0),
        Some(CellValue::LobUnavailable(LobUnavailable::TransactionEnded))
    );
    assert_eq!(store.value(1, 0), Some(CellValue::Null));
    // And the worker agrees: the handle was released with the transaction.
    session
        .read_lob_chunk(handle, nz(4))
        .wait()
        .expect_err("the commit released every parked LOB");
}

support::both_paths! {
    a_store_fetches_on_demand_to_the_end_on_the_worker,
    the_row_cap_stops_exactly_and_fetch_more_continues_the_same_cursor,
    closing_the_cursor_at_the_cap_releases_it,
    a_failed_fetch_keeps_the_rows_before_it,
    only_plain_data_crosses_threads_in_a_store,
    a_commit_makes_lob_cells_unavailable_never_null,
}

// ------------------------------------ the event path, with SessionResults

/// The product's shape: one session's events drained on one thread, every
/// event handed to [`SessionResults::observe`], and the stores pumped after
/// each.
struct Consumer {
    session: DatabaseSession,
    queue: EventQueue,
    results: SessionResults,
    next_request: u64,
    fetched: Vec<Fetched>,
}

impl Consumer {
    fn new(scenario: &Arc<Scenario>) -> Self {
        let (session, queue) = support::open_events(scenario);
        let results = SessionResults::new(session.id());
        Self {
            session,
            queue,
            results,
            next_request: 0,
            fetched: Vec::new(),
        }
    }

    fn request(&mut self) -> RequestId {
        self.next_request += 1;
        RequestId(self.next_request)
    }

    fn pump(&mut self) {
        let next = &mut self.next_request;
        self.results
            .submit_events(&self.session, || {
                *next += 1;
                RequestId(*next)
            })
            .expect("the session accepts every fetch");
    }

    /// Drains, observing and pumping, until `wanted` matches an event the
    /// stores handed back; returns it.
    fn until(&mut self, wanted: impl Fn(&SessionEvent) -> bool) -> SessionEvent {
        let deadline = std::time::Instant::now() + support::HANG_GUARD;
        loop {
            self.pump();
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "the awaited event never arrived");
            let Some(event) = self.queue.wait_timeout(remaining) else {
                continue;
            };
            match self.results.observe(event) {
                Observed::Segment { fetched, .. } => {
                    self.fetched.extend(fetched);
                }
                Observed::Event(event) if wanted(&event) => return event,
                Observed::Event(_) => {}
                other => panic!("{other:?}"),
            }
        }
    }

    /// Drains until the reply to `request` arrives.
    fn reply(&mut self, request: RequestId) -> SessionEvent {
        self.until(|event| event.is_reply() && event.request() == Some(request))
    }

    /// Drains until no store has a fetch in flight and none is due.
    fn idle(&mut self) {
        let deadline = std::time::Instant::now() + support::HANG_GUARD;
        loop {
            self.pump();
            if self
                .results
                .iter()
                .all(|store| store.state().fetches_in_flight() == 0)
            {
                return;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "the fetches never finished");
            if let Some(event) = self.queue.wait_timeout(remaining) {
                match self.results.observe(event) {
                    Observed::Segment { fetched, .. } => self.fetched.extend(fetched),
                    Observed::Event(_) => {}
                    other => panic!("{other:?}"),
                }
            }
        }
    }

    fn execute(&mut self, sql: &str) -> ExecuteOutcome {
        let request = self.request();
        self.session
            .submit_execute(request, Statement::new(sql))
            .expect("submit");
        match self.reply(request) {
            SessionEvent::Executed { outcome, .. } => outcome.expect("the statement runs"),
            other => panic!("expected Executed, got {other:?}"),
        }
    }

    /// Runs [`SELECT_BIG`] and opens a store on its result, idle after its
    /// first page.
    fn open(&mut self, policy: ResultPolicy) -> ResultId {
        let outcome = self.execute(SELECT_BIG);
        let result = self
            .results
            .open(&outcome, policy)
            .expect("the query opened a result")
            .result();
        self.idle();
        result
    }

    fn store(&self, result: ResultId) -> &ResultStore {
        self.results.get(result).expect("the store is open")
    }

    fn store_mut(&mut self, result: ResultId) -> &mut ResultStore {
        self.results.get_mut(result).expect("the store is open")
    }

    /// Submits a transaction-ending command the way the product must:
    /// announced to the stores once the session accepted it, before the
    /// next pump.
    fn end_transaction(
        &mut self,
        submit: impl FnOnce(&DatabaseSession, RequestId),
    ) -> SessionEvent {
        let request = self.request();
        submit(&self.session, request);
        self.results.transaction_end_submitted();
        self.reply(request)
    }
}

fn ended_by_transaction(store: &ResultStore) -> bool {
    matches!(
        store.state().phase(),
        ResultPhase::Ended {
            cause: EndCause::TransactionEnded
        }
    )
}

#[test]
fn every_transaction_end_ends_every_open_store_and_keeps_its_prefix() {
    type Trigger = fn(&mut Consumer);
    let triggers: [(&str, Trigger); 5] = [
        ("commit command", |consumer| {
            consumer.end_transaction(|session, request| {
                session.submit_commit(request).expect("submit");
            });
        }),
        ("rollback command", |consumer| {
            consumer.end_transaction(|session, request| {
                session.submit_rollback(request).expect("submit");
            });
        }),
        ("rollback to savepoint", |consumer| {
            consumer.end_transaction(|session, request| {
                session
                    .submit_rollback_to_savepoint(
                        request,
                        SavepointName::new("before_query").expect("valid"),
                    )
                    .expect("submit");
            });
        }),
        ("typed COMMIT", |consumer| {
            let outcome = consumer.execute(TYPED_COMMIT);
            assert_eq!(outcome.statement_kind, StatementKind::TransactionControl);
        }),
        ("DDL", |consumer| {
            let outcome = consumer.execute(CREATE_TABLE);
            assert!(outcome.committed_implicitly);
        }),
    ];
    for (name, trigger) in triggers {
        let spec = big(10_000);
        let scenario = scenario(&spec);
        let mut consumer = Consumer::new(&scenario);
        let savepoint = consumer.request();
        consumer
            .session
            .submit_savepoint(
                savepoint,
                SavepointName::new("before_query").expect("valid"),
            )
            .expect("submit");
        consumer.reply(savepoint);

        let first = consumer.open(policy(Cap::Unlimited, 100, 2));
        let second = consumer.open(policy(Cap::Unlimited, 50, 2));
        trigger(&mut consumer);
        consumer.idle();

        for (result, rows) in [(first, 100), (second, 50)] {
            let store = consumer.store(result);
            assert!(
                ended_by_transaction(store),
                "{name}: {:?}",
                store.state().phase()
            );
            assert_eq!(store.row_count(), rows, "{name}: the prefix stays");
            assert_row(store, &spec, rows - 1);
        }
        // Nothing is fetched after the end, whatever the demand.
        consumer.store_mut(first).set_demand(5_000);
        consumer.store_mut(first).fetch_all();
        consumer.idle();
        assert_eq!(consumer.store(first).row_count(), 100, "{name}");

        // A store opened after the end is not ended by it.
        let third = consumer.open(policy(Cap::Unlimited, 100, 2));
        assert!(
            matches!(consumer.store(third).state().phase(), ResultPhase::Open),
            "{name}"
        );
    }
}

#[test]
fn fetches_submitted_behind_a_typed_commit_are_stale_never_failures() {
    let spec = big(10_000);
    let scenario = scenario(&spec);
    let mut consumer = Consumer::new(&scenario);
    let result = consumer.open(policy(Cap::Unlimited, 10, 4));
    consumer.fetched.clear();

    // Two fetches ahead of the COMMIT...
    consumer.store_mut(result).set_demand(15);
    consumer.pump();
    let request = consumer.request();
    consumer
        .session
        .submit_execute(request, Statement::new(TYPED_COMMIT))
        .expect("submit");
    // ...and two the store submits behind it, because nothing announced it.
    consumer.store_mut(result).set_demand(35);
    consumer.pump();
    assert_eq!(consumer.store(result).state().fetches_in_flight(), 4);

    consumer.reply(request);
    consumer.idle();
    assert_eq!(
        consumer.fetched,
        [
            Fetched::Appended { rows: 10 },
            Fetched::Appended { rows: 10 },
            Fetched::Stale,
            Fetched::Stale
        ]
    );
    let store = consumer.store(result);
    assert!(ended_by_transaction(store), "{:?}", store.state().phase());
    assert_eq!(store.row_count(), 30);
    assert_row(store, &spec, 29);
}

#[test]
fn a_commit_command_waits_for_the_fetches_ahead_of_it_and_submits_none_behind_it() {
    let spec = big(10_000);
    let scenario = scenario(&spec);
    let mut consumer = Consumer::new(&scenario);
    let result = consumer.open(policy(Cap::Unlimited, 10, 4));
    consumer.fetched.clear();
    consumer.store_mut(result).fetch_all();
    consumer.pump();
    assert_eq!(consumer.store(result).state().fetches_in_flight(), 4);

    consumer.end_transaction(|session, request| {
        session.submit_commit(request).expect("submit");
    });
    consumer.idle();
    assert_eq!(consumer.fetched, [Fetched::Appended { rows: 10 }; 4]);
    let store = consumer.store(result);
    assert!(ended_by_transaction(store));
    assert_eq!(store.row_count(), 50);
}

#[test]
fn a_failed_commit_releases_nothing_and_the_store_carries_on() {
    let spec = big(1_000);
    let scenario = scenario(&spec);
    scenario.fail_commit(ScriptedError::new(ErrorKind::Transaction, "commit refused"));
    let mut consumer = Consumer::new(&scenario);
    let result = consumer.open(policy(Cap::Unlimited, 100, 2));

    let reply = consumer.end_transaction(|session, request| {
        session.submit_commit(request).expect("submit");
    });
    assert!(matches!(
        reply,
        SessionEvent::Completed {
            operation: CompletedOperation::Commit,
            result: Err(_),
            ..
        }
    ));
    assert!(matches!(
        consumer.store(result).state().phase(),
        ResultPhase::Open
    ));

    consumer.store_mut(result).fetch_all();
    consumer.idle();
    let store = consumer.store(result);
    assert!(matches!(store.state().phase(), ResultPhase::Complete));
    assert_eq!(store.row_count(), 1_000);
    assert_row(store, &spec, 999);
}

#[test]
fn losing_the_session_keeps_every_prefix() {
    let spec = big(10_000);
    let scenario = scenario(&spec);
    let mut consumer = Consumer::new(&scenario);
    let result = consumer.open(policy(Cap::Unlimited, 100, 2));

    let request = consumer.request();
    consumer
        .session
        .submit_execute(request, Statement::new(LOSE_SESSION))
        .expect("submit");
    consumer.until(SessionEvent::is_terminal);
    consumer.idle();

    let store = consumer.store(result);
    let state = store.state();
    assert!(matches!(
        state.phase(),
        ResultPhase::Ended {
            cause: EndCause::SessionEnded
        }
    ));
    assert_eq!(state.rows(), 100);
    assert_eq!(state.lobs_unavailable(), Some(LobUnavailable::SessionEnded));
    assert_row(store, &spec, 99);
}

/// The network drops under a fetch while others are in flight (review of
/// M5.2): the result is `Failed { after }` with the loss's classification,
/// replies behind it are stale, the `Terminal` that follows leaves the phase,
/// and the LOB cells are unavailable because the session ended.
#[test]
fn losing_the_session_mid_fetch_fails_the_result_and_ends_its_lobs_with_the_session() {
    let scenario = support::scenario();
    let rows = (0..1_000_i64).map(|n| vec![ScriptValue::from(n)]).collect();
    let plan = QueryPlan::new(vec![ColumnSpec::new("N", SqlType::Number)], rows)
        .with_fail_on_batch(
            3,
            ScriptedError::new(ErrorKind::NetworkLost, "connection reset mid-fetch")
                .with_native(3113, "ORA-03113: end-of-file on communication channel"),
        );
    scenario.on_sql("SELECT n FROM t", Action::query(QuerySource::Fixed(plan)));
    let mut consumer = Consumer::new(&scenario);
    let outcome = consumer.execute("SELECT n FROM t");
    let result = consumer
        .results
        .open(&outcome, policy(Cap::Unlimited, 10, 2))
        .expect("a result")
        .result();
    consumer.idle();
    consumer.fetched.clear();

    consumer.store_mut(result).fetch_all();
    consumer.until(SessionEvent::is_terminal);
    consumer.idle();

    assert_eq!(
        &consumer.fetched[..2],
        [Fetched::Appended { rows: 10 }, Fetched::Failed],
        "{:?}",
        consumer.fetched
    );
    assert!(
        consumer.fetched[2..]
            .iter()
            .all(|fetched| *fetched == Fetched::Stale),
        "every reply behind the loss is stale: {:?}",
        consumer.fetched
    );
    let store = consumer.store(result);
    let state = store.state();
    match state.phase() {
        ResultPhase::Failed { after, error } => {
            assert_eq!(after, 20);
            assert_eq!(error.kind(), ErrorKind::NetworkLost);
            assert_eq!(error.native().map(NativeError::code), Some(3113));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(state.lobs_unavailable(), Some(LobUnavailable::SessionEnded));
    assert!(
        matches!(store.value(19, 0), Some(CellValue::Number(n)) if n.to_string() == "19"),
        "the prefix stays readable"
    );
}

#[test]
fn a_discarded_result_drops_the_replies_still_in_flight() {
    let spec = big(10_000);
    let scenario = scenario(&spec);
    let mut consumer = Consumer::new(&scenario);
    let kept = consumer.open(policy(Cap::Unlimited, 10, 4));
    let removed = consumer.open(policy(Cap::Unlimited, 10, 4));
    consumer.fetched.clear();
    consumer.store_mut(kept).fetch_all();
    consumer.store_mut(removed).fetch_all();
    consumer.pump();

    // One store is discarded and kept to read its prefix, the other
    // discarded and taken out; both cursors are closed behind the fetches.
    for result in [kept, removed] {
        consumer.store_mut(result).discard();
        let request = consumer.request();
        consumer
            .session
            .submit_close_result(request, result)
            .expect("submit");
    }
    let mut gone = consumer.results.remove(removed).expect("open");
    assert!(matches!(
        gone.state().phase(),
        ResultPhase::Ended {
            cause: EndCause::Discarded
        }
    ));
    gone.pump(|_| panic!("a discarded store submits nothing"))
        .expect("nothing to submit");

    let closed = consumer.request();
    consumer.session.submit_ping(closed).expect("submit");
    consumer.reply(closed);
    assert_eq!(
        consumer.fetched,
        [Fetched::Stale; 4],
        "the kept store's in-flight replies are stale; the removed one's reach no store"
    );
    let store = consumer.store(kept);
    assert_eq!(store.row_count(), 10);
    assert_eq!(store.state().fetches_in_flight(), 0);
    assert_row(store, &spec, 9);
}
