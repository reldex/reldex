//! The store's policy and state machine, driven without a session: every
//! reply is built here, so each transition ADR-0004 names is exercised
//! exactly. `crates/db-core/tests/result_store.rs` drives the same store
//! through real sessions and the mock driver.

use std::num::NonZeroUsize;
use std::sync::Arc;

use reldex_db_driver_api::{
    Column, ColumnData, ColumnMetadata, ConnectionId, DbError, DbResult, ErrorKind, LobKind,
    LobLocator, LobStream, NullMask, Number, ResultSetId, RowBatch, SqlType, StatementKind,
    TextColumn,
};

use super::{
    EndCause, FetchMore, FetchRequest, Fetched, LimitKind, LobCell, LobUnavailable, MoreRows,
    ResultPhase, ResultStore, SegmentReply, StoreAction,
};
use crate::ids::{ResultId, SessionId};
use crate::session::{ExecuteOutcome, OutValues};
use crate::store::policy::{Cap, CapSource, ResultCaps, ResultPolicy, Sourced};
use crate::store::scaled::NumberValue;
use crate::store::segment::{CellValue, ResultSegment, SegmentData, compact_batch};

fn nz(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("non-zero")
}

fn at(value: usize) -> Cap {
    Cap::At(nz(value))
}

fn result_id() -> ResultId {
    ResultId::new(SessionId::allocate(), ResultSetId::allocate())
}

fn caps(rows: Cap, bytes: Cap) -> ResultCaps {
    ResultCaps::new(
        Sourced::new(rows, CapSource::Application),
        Sourced::new(bytes, CapSource::BuiltIn),
    )
}

/// One `NUMBER` column declared as such, one text column declared at
/// `text_width` bytes.
fn columns(text_width: u32) -> Vec<ColumnMetadata> {
    vec![
        ColumnMetadata::new("ID", SqlType::Number),
        ColumnMetadata::new(
            "NAME",
            SqlType::Text {
                national: false,
                fixed_length: false,
            },
        )
        .with_max_size_bytes(text_width),
    ]
}

/// Rows `first..first + count` of the result every test fetches from: the id,
/// and a short text.
fn segment(first: usize, count: usize) -> Arc<ResultSegment> {
    let mut ids = Vec::with_capacity(count);
    let mut names = TextColumn::with_capacity(count, count * 16);
    for row in first..first + count {
        ids.push(Number::from(i64::try_from(row).expect("small")));
        names.push(&format!("row {row}"));
    }
    let batch = RowBatch::new(vec![
        Column::not_null(ColumnData::Number(ids)),
        Column::not_null(ColumnData::Text(names)),
    ])
    .expect("batch");
    Arc::new(ResultSegment::compact(batch).expect("no LOBs"))
}

fn empty_segment() -> Arc<ResultSegment> {
    Arc::new(ResultSegment::compact(RowBatch::empty()).expect("empty"))
}

/// Every action now due, taken as submitted.
fn pump(store: &mut ResultStore) -> Vec<StoreAction> {
    let mut actions = Vec::new();
    store
        .pump(|action| {
            actions.push(action);
            Ok(())
        })
        .expect("submitting never fails here");
    actions
}

fn fetches(actions: &[StoreAction]) -> Vec<FetchRequest> {
    actions
        .iter()
        .filter_map(|action| match action {
            StoreAction::Fetch(fetch) => Some(*fetch),
            StoreAction::CloseResult(_) => None,
        })
        .collect()
}

/// A result of `total` rows behind a forward-only cursor, answering fetches
/// in order the way the worker does.
struct FakeCursor {
    total: usize,
    position: usize,
}

impl FakeCursor {
    const fn new(total: usize) -> Self {
        Self { total, position: 0 }
    }

    fn answer(&mut self, fetch: FetchRequest) -> DbResult<SegmentReply> {
        let count = fetch.max_rows().get().min(self.total - self.position);
        let first = self.position;
        self.position += count;
        let segment = if count == 0 {
            empty_segment()
        } else {
            segment(first, count)
        };
        Ok(SegmentReply::new(segment, self.position == self.total))
    }
}

/// Pumps and answers until nothing is due, returning every action taken.
fn run_actions(store: &mut ResultStore, cursor: &mut FakeCursor) -> Vec<StoreAction> {
    let mut taken = Vec::new();
    loop {
        let actions = pump(store);
        let due = fetches(&actions);
        taken.extend(actions);
        if due.is_empty() {
            return taken;
        }
        for fetch in due {
            let reply = cursor.answer(fetch);
            assert!(
                matches!(
                    store.on_fetched(fetch.ticket(), reply),
                    Fetched::Appended { .. }
                ),
                "{:?}",
                store.state().phase()
            );
        }
    }
}

/// [`run_actions`], returning the fetches' sizes.
fn run(store: &mut ResultStore, cursor: &mut FakeCursor) -> Vec<usize> {
    fetches(&run_actions(store, cursor))
        .iter()
        .map(|fetch| fetch.max_rows().get())
        .collect()
}

fn store(
    total_caps: ResultCaps,
    fetch_rows: usize,
    in_flight: usize,
    text_width: u32,
) -> ResultStore {
    ResultStore::new(
        result_id(),
        &columns(text_width),
        ResultPolicy::new(total_caps)
            .with_fetch_rows(nz(fetch_rows))
            .with_fetches_in_flight(nz(in_flight)),
    )
}

fn unlimited() -> ResultCaps {
    caps(Cap::Unlimited, Cap::Unlimited)
}

fn text(store: &ResultStore, row: usize) -> String {
    match store.value(row, 1) {
        Some(CellValue::Text(value)) => value.to_owned(),
        other => panic!("row {row}: {other:?}"),
    }
}

fn outcome(kind: StatementKind) -> ExecuteOutcome {
    ExecuteOutcome {
        result: None,
        columns: Vec::new(),
        rows_affected: None,
        statement_kind: kind,
        committed_implicitly: kind.commits_implicitly(),
        warnings: Vec::new(),
        out_values: OutValues::None,
    }
}

// ------------------------------------------------------------- the policy

#[test]
fn the_first_fetch_is_due_at_once_and_sized_by_the_declared_widths() {
    // 256 KiB / (44 + 4008 + 1) = 64 rows: a declared VARCHAR2(4000) keeps
    // the blind first round trip small.
    let mut wide = store(unlimited(), 1_000, 2, 4_000);
    wide.fetch_all();
    let due = fetches(&pump(&mut wide));
    assert_eq!(
        due.len(),
        1,
        "one first fetch, whatever the demand, and none beside it until it          is answered: only one request is sized blind"
    );
    assert_eq!(due[0].max_rows().get(), 64);
    assert_eq!(due[0].ticket().sequence(), 0);
    assert!(matches!(wide.state().phase(), ResultPhase::Fetching));

    // A narrow declared row is bounded by `results.fetch_rows` instead.
    let mut narrow = store(unlimited(), 1_000, 2, 40);
    assert_eq!(fetches(&pump(&mut narrow))[0].max_rows().get(), 1_000);
}

#[test]
fn the_observed_width_replaces_the_declared_one_after_the_first_segment() {
    let mut store = store(unlimited(), 1_000, 1, 4_000);
    let mut cursor = FakeCursor::new(10_000);
    let first = fetches(&pump(&mut store))[0];
    assert_eq!(first.max_rows().get(), 64);
    store.on_fetched(first.ticket(), cursor.answer(first));
    store.set_demand(64);
    let second = fetches(&pump(&mut store));
    assert_eq!(second.len(), 1);
    // The rows are ~30 bytes each, far under the declared 4 KB: the next
    // round trip is bounded by `fetch_rows` again, not by the declaration.
    assert_eq!(second[0].max_rows().get(), 1_000);
}

#[test]
fn demand_keeps_one_fetch_of_read_ahead_within_the_fetches_in_flight() {
    let mut store = store(unlimited(), 100, 2, 40);
    let mut cursor = FakeCursor::new(100_000);
    assert_eq!(run(&mut store, &mut cursor), [100], "first page only");
    assert!(matches!(store.state().phase(), ResultPhase::Open));
    assert!(fetches(&pump(&mut store)).is_empty(), "no demand, no fetch");

    // The view shows rows 0..30: 100 retained < 31 + 100, so one more.
    store.set_demand(31);
    assert_eq!(run(&mut store, &mut cursor), [100]);
    assert_eq!(store.row_count(), 200);

    // A scrollbar drag to row 5,000: at most two fetches outstanding.
    store.set_demand(5_000);
    let due = fetches(&pump(&mut store));
    assert_eq!(due.len(), 2);
    assert_eq!(store.state().fetches_in_flight(), 2);
    assert!(
        fetches(&pump(&mut store)).is_empty(),
        "the in-flight bound holds"
    );
    for fetch in due {
        store.on_fetched(fetch.ticket(), cursor.answer(fetch));
    }
    run(&mut store, &mut cursor);
    assert!(store.row_count() >= 5_000 && store.row_count() < 5_000 + 200 + 100);
    assert_eq!(text(&store, 4_999), "row 4999");
}

#[test]
fn a_result_that_ends_is_complete_and_keeps_every_row() {
    let mut store = store(unlimited(), 100, 2, 40);
    store.fetch_all();
    let mut cursor = FakeCursor::new(250);
    run(&mut store, &mut cursor);
    let state = store.state();
    assert!(
        matches!(state.phase(), ResultPhase::Complete),
        "{:?}",
        state.phase()
    );
    assert_eq!(state.rows(), 250);
    assert_eq!(state.retained_rows(), 250);
    assert_eq!(text(&store, 249), "row 249");
    assert!(store.value(250, 0).is_none());
    assert!(
        fetches(&pump(&mut store)).is_empty(),
        "nothing after the end"
    );

    // An empty result is complete with no rows.
    let mut empty = store_with_cursor_of(0);
    assert!(matches!(empty.state().phase(), ResultPhase::Complete));
    assert_eq!(empty.row_count(), 0);
    assert!(fetches(&pump(&mut empty)).is_empty());
}

fn store_with_cursor_of(total: usize) -> ResultStore {
    let mut store = store(unlimited(), 100, 2, 40);
    store.fetch_all();
    run(&mut store, &mut FakeCursor::new(total));
    store
}

// ---------------------------------------------------------------- the caps

#[test]
fn the_row_cap_is_exact_and_its_lookahead_says_more_rows_exist() {
    let mut store = store(caps(at(10), Cap::Unlimited), 4, 2, 40);
    store.fetch_all();
    let sizes = run(&mut store, &mut FakeCursor::new(100));
    assert_eq!(sizes, [4, 4, 3], "the last request asks for room + 1");
    let state = store.state();
    assert!(matches!(
        state.phase(),
        ResultPhase::AtLimit {
            limit: LimitKind::Rows,
            more: MoreRows::Yes,
            cursor_open: true
        }
    ));
    assert_eq!(state.rows(), 10, "the lookahead is not shown");
    assert_eq!(state.retained_rows(), 11, "but it is held, and counted");
    assert!(store.value(10, 0).is_none());
    assert_eq!(
        state.caps().max_rows(),
        Sourced::new(at(10), CapSource::Application)
    );

    // Exactly as many rows as the cap: complete, and no limit message.
    let mut exact = store_capped(10);
    run(&mut exact, &mut FakeCursor::new(10));
    assert!(matches!(exact.state().phase(), ResultPhase::Complete));
    assert_eq!(exact.row_count(), 10);
}

fn store_capped(rows: usize) -> ResultStore {
    let mut store = store(caps(at(rows), Cap::Unlimited), 4, 2, 40);
    store.fetch_all();
    store
}

#[test]
fn the_byte_cap_shrinks_requests_and_stops_with_more_rows_unknown() {
    let cap = 40 * 1024;
    let mut store = store(caps(Cap::Unlimited, at(cap)), 500, 2, 40);
    store.fetch_all();
    let sizes = run(&mut store, &mut FakeCursor::new(1_000_000));
    let state = store.state();
    assert!(
        matches!(
            state.phase(),
            ResultPhase::AtLimit {
                limit: LimitKind::Bytes,
                more: MoreRows::Unknown,
                ..
            }
        ),
        "{:?}",
        state.phase()
    );
    // Requests shrank to what fit as the cap came near.
    assert!(sizes.windows(2).any(|pair| pair[1] < pair[0]), "{sizes:?}");
    // The overshoot is bounded by what the fetches in flight carried: here
    // two round trips of at most `fetch_rows` rows at the declared width.
    let declared = 44 + 48 + 1;
    assert!(
        state.retained_bytes() <= cap + 2 * 500 * declared,
        "{} bytes against a {cap}-byte cap",
        state.retained_bytes()
    );
    assert!(state.retained_bytes() > cap / 2);
}

#[test]
fn a_byte_cap_below_one_declared_row_still_fetches_the_first_row() {
    let mut store = store(caps(Cap::Unlimited, at(1_000)), 500, 2, 4_000);
    let due = fetches(&pump(&mut store));
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].max_rows().get(), 1);
    let mut cursor = FakeCursor::new(1_000);
    store.on_fetched(due[0].ticket(), cursor.answer(due[0]));
    store.fetch_all();
    run(&mut store, &mut cursor);
    assert!(matches!(
        store.state().phase(),
        ResultPhase::AtLimit {
            limit: LimitKind::Bytes,
            more: MoreRows::Unknown,
            ..
        }
    ));
    assert!(store.row_count() >= 1);
    assert_eq!(text(&store, 0), "row 0");
}

#[test]
fn fetch_more_continues_the_same_cursor_one_step_or_without_limit() {
    let mut store = store_capped(10);
    let mut cursor = FakeCursor::new(100);
    run(&mut store, &mut cursor);
    assert_eq!(store.row_count(), 10);

    assert!(store.fetch_more(FetchMore::Step));
    assert_eq!(store.row_count(), 11, "the lookahead row shows at once");
    run(&mut store, &mut cursor);
    let state = store.state();
    assert_eq!(state.rows(), 20, "one more step of the same size");
    assert_eq!(
        state.caps().max_rows(),
        Sourced::new(at(20), CapSource::FetchMore)
    );
    assert_eq!(text(&store, 19), "row 19", "the same cursor, not a re-run");

    assert!(store.fetch_more(FetchMore::Unlimited));
    run(&mut store, &mut cursor);
    assert!(matches!(store.state().phase(), ResultPhase::Complete));
    assert_eq!(store.row_count(), 100);
    assert!(!store.fetch_more(FetchMore::Step), "only at a cap");
}

#[test]
fn with_close_cursor_at_limit_the_cursor_is_closed_and_fetch_more_is_gone() {
    let caps = caps(at(10), Cap::Unlimited)
        .with_close_cursor_at_limit(Sourced::new(true, CapSource::Profile));
    let mut store = store(caps, 4, 2, 40);
    store.fetch_all();
    let actions = run_actions(&mut store, &mut FakeCursor::new(100));
    // The close is the last action taken, once, right after the cap.
    assert_eq!(
        actions.last(),
        Some(&StoreAction::CloseResult(store.result()))
    );
    assert_eq!(fetches(&actions).len(), actions.len() - 1);
    assert!(pump(&mut store).is_empty(), "closed once");
    let state = store.state();
    assert!(matches!(
        state.phase(),
        ResultPhase::AtLimit {
            limit: LimitKind::Rows,
            more: MoreRows::Yes,
            cursor_open: false
        }
    ));
    assert_eq!(state.lobs_unavailable(), Some(LobUnavailable::ResultClosed));
    assert_eq!(state.rows(), 10, "the rows stay");
    assert!(!store.fetch_more(FetchMore::Step));
}

// ------------------------------------------------------ stop and discard

#[test]
fn stop_takes_effect_at_once_and_keeps_what_is_already_in_flight() {
    let mut store = store(unlimited(), 10, 2, 40);
    let mut cursor = FakeCursor::new(1_000);
    run(&mut store, &mut cursor);
    store.fetch_all();
    let in_flight = fetches(&pump(&mut store));
    assert_eq!(in_flight.len(), 2);
    store.stop();
    assert!(store.state().stopped());
    for fetch in in_flight {
        assert_eq!(
            store.on_fetched(fetch.ticket(), cursor.answer(fetch)),
            Fetched::Appended { rows: 10 },
            "rows already paid for are kept"
        );
    }
    assert_eq!(store.row_count(), 30);
    store.set_demand(1_000);
    assert!(
        fetches(&pump(&mut store)).is_empty(),
        "demand does not undo a stop"
    );
    assert!(matches!(store.state().phase(), ResultPhase::Open));
    store.resume();
    assert_eq!(fetches(&pump(&mut store)).len(), 2);
}

#[test]
fn a_discarded_store_drops_replies_still_in_flight_and_keeps_its_prefix() {
    let mut store = store(unlimited(), 10, 2, 40);
    let mut cursor = FakeCursor::new(1_000);
    run(&mut store, &mut cursor);
    store.fetch_all();
    let in_flight = fetches(&pump(&mut store));
    store.discard();
    for fetch in in_flight {
        assert_eq!(
            store.on_fetched(fetch.ticket(), cursor.answer(fetch)),
            Fetched::Stale
        );
    }
    let state = store.state();
    assert!(matches!(
        state.phase(),
        ResultPhase::Ended {
            cause: EndCause::Discarded
        }
    ));
    assert_eq!(state.rows(), 10);
    assert_eq!(state.fetches_in_flight(), 0);
    assert!(pump(&mut store).is_empty(), "the caller submits the close");
}

// ---------------------------------------- the transaction and the session

#[test]
fn a_transaction_end_command_pauses_then_ends_or_resumes_the_store() {
    let mut store = store(unlimited(), 10, 2, 40);
    let mut cursor = FakeCursor::new(1_000);
    run(&mut store, &mut cursor);
    store.fetch_all();
    let in_flight = fetches(&pump(&mut store));
    assert_eq!(in_flight.len(), 2);

    // A commit is submitted: nothing new is submitted behind it...
    store.transaction_end_submitted();
    for fetch in in_flight {
        store.on_fetched(fetch.ticket(), cursor.answer(fetch));
        assert!(fetches(&pump(&mut store)).is_empty());
    }
    assert_eq!(
        store.row_count(),
        30,
        "...and the fetches ahead of it are kept"
    );

    // A failed commit released nothing: the store resumes.
    store.transaction_end_answered(false);
    let resumed = fetches(&pump(&mut store));
    assert_eq!(resumed.len(), 2);
    for fetch in resumed {
        store.on_fetched(fetch.ticket(), cursor.answer(fetch));
    }

    // A successful one ends it.
    store.transaction_end_submitted();
    store.transaction_end_answered(true);
    let state = store.state();
    assert!(matches!(
        state.phase(),
        ResultPhase::Ended {
            cause: EndCause::TransactionEnded
        }
    ));
    assert_eq!(state.rows(), 50, "the prefix stays readable");
    assert_eq!(text(&store, 49), "row 49");
    assert_eq!(
        state.lobs_unavailable(),
        Some(LobUnavailable::TransactionEnded)
    );
    assert!(pump(&mut store).is_empty(), "no fetch after the end");
}

#[test]
fn a_statement_that_ends_the_transaction_ends_the_store_and_others_do_not() {
    for (kind, ends) in [
        (StatementKind::TransactionControl, true),
        (StatementKind::Ddl, true),
        (StatementKind::Dml, false),
        (StatementKind::Query, false),
        (StatementKind::PlSqlBlock, false),
        (StatementKind::SessionControl, false),
        (StatementKind::Other, false),
    ] {
        let mut store = store(unlimited(), 10, 2, 40);
        run(&mut store, &mut FakeCursor::new(1_000));
        store.statement_executed(&outcome(kind));
        let ended = matches!(
            store.state().phase(),
            ResultPhase::Ended {
                cause: EndCause::TransactionEnded
            }
        );
        assert_eq!(ended, ends, "{kind:?}");
        assert_eq!(store.row_count(), 10, "{kind:?}");
    }
}

#[test]
fn a_fetch_error_after_the_end_is_stale_never_a_failure() {
    let mut store = store(unlimited(), 10, 1, 40);
    run(&mut store, &mut FakeCursor::new(1_000));
    store.fetch_all();
    let fetch = fetches(&pump(&mut store))[0];
    store.statement_executed(&outcome(StatementKind::TransactionControl));
    let invalidated =
        DbError::internal("reldex-db-core: unknown, closed or invalidated result handle");
    assert_eq!(
        store.on_fetched(fetch.ticket(), Err(invalidated)),
        Fetched::Stale
    );
    assert!(matches!(
        store.state().phase(),
        ResultPhase::Ended {
            cause: EndCause::TransactionEnded
        }
    ));
}

#[test]
fn the_session_ending_keeps_every_prefix_and_ends_what_was_not_complete() {
    let mut open = store(unlimited(), 10, 2, 40);
    run(&mut open, &mut FakeCursor::new(1_000));
    open.session_ended();
    let state = open.state();
    assert!(matches!(
        state.phase(),
        ResultPhase::Ended {
            cause: EndCause::SessionEnded
        }
    ));
    assert_eq!(state.rows(), 10);
    assert_eq!(state.lobs_unavailable(), Some(LobUnavailable::SessionEnded));

    let mut complete = store_with_cursor_of(25);
    complete.session_ended();
    assert!(matches!(complete.state().phase(), ResultPhase::Complete));
    assert_eq!(complete.row_count(), 25);
    assert_eq!(
        complete.state().lobs_unavailable(),
        Some(LobUnavailable::SessionEnded)
    );
}

#[test]
fn a_failed_fetch_keeps_the_rows_before_it_and_later_replies_are_stale() {
    let mut store = store(unlimited(), 10, 2, 40);
    let mut cursor = FakeCursor::new(1_000);
    run(&mut store, &mut cursor);
    store.fetch_all();
    let due = fetches(&pump(&mut store));
    let error = DbError::new(ErrorKind::Transaction, "ORA-01555: snapshot too old");
    assert_eq!(
        store.on_fetched(due[0].ticket(), Err(error)),
        Fetched::Failed
    );
    assert_eq!(
        store.on_fetched(due[1].ticket(), cursor.answer(due[1])),
        Fetched::Stale
    );
    match store.state().phase() {
        ResultPhase::Failed { after, error } => {
            assert_eq!(after, 10);
            assert!(error.message().contains("ORA-01555"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        store.state().lobs_unavailable(),
        Some(LobUnavailable::ResultClosed)
    );
    assert!(pump(&mut store).is_empty());
}

// ------------------------------------------------------------ the sequence

#[test]
fn a_reply_for_another_result_changes_nothing() {
    let mut store = store(unlimited(), 10, 2, 40);
    let mut other = self::store(unlimited(), 10, 2, 40);
    let foreign = fetches(&pump(&mut other))[0];
    let _mine = fetches(&pump(&mut store))[0];
    assert_eq!(
        store.on_fetched(
            foreign.ticket(),
            Ok(SegmentReply::new(segment(0, 10), false))
        ),
        Fetched::NotThisResult
    );
    assert_eq!(store.state().fetches_in_flight(), 1);
}

/// A reply out of sequence is a core bug: asserted in a debug build, and
/// reported as the result's failure — never appended — in a release build.
#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "was expected"))]
fn a_reply_out_of_sequence_is_asserted_and_never_appended() {
    let mut store = store(unlimited(), 10, 2, 40);
    let mut cursor = FakeCursor::new(1_000);
    run(&mut store, &mut cursor);
    store.fetch_all();
    let due = fetches(&pump(&mut store));
    let second = due[1];
    assert_eq!(
        store.on_fetched(
            second.ticket(),
            Ok(SegmentReply::new(segment(20, 10), false))
        ),
        Fetched::OutOfSequence
    );
    assert_eq!(store.row_count(), 10, "nothing appended");
    assert!(matches!(store.state().phase(), ResultPhase::Failed { .. }));
    assert_eq!(
        store.on_fetched(due[0].ticket(), cursor.answer(due[0])),
        Fetched::Stale
    );
}

// ------------------------------------------------------------- the lookup

/// Appends segments of the given sizes to a fresh store, in order.
fn store_with_segments(sizes: &[usize]) -> ResultStore {
    let mut store = store(unlimited(), 10, 1, 40);
    store.fetch_all();
    extend_segments(&mut store, sizes, &mut FakeCursor::new(1_000));
    store
}

fn extend_segments(store: &mut ResultStore, sizes: &[usize], cursor: &mut FakeCursor) {
    for &size in sizes {
        let fetch = fetches(&pump(store))[0];
        let fetch = FetchRequest {
            max_rows: nz(size),
            ..fetch
        };
        store.on_fetched(fetch.ticket(), cursor.answer(fetch));
    }
}

fn assert_every_row_found(store: &ResultStore) {
    for row in 0..store.row_count() {
        assert_eq!(text(store, row), format!("row {row}"), "row {row}");
        let (segment, first) = store.segment_for_row(row).expect("in range");
        assert!(row >= first && row < first + segment.row_count());
    }
    assert!(store.segment_for_row(store.row_count()).is_none());
}

#[test]
fn rows_map_to_segments_by_division_until_a_short_segment_forces_a_search() {
    // Equal segments and a last one of any size keep division valid.
    let mut store = store_with_segments(&[10, 10, 10, 7]);
    assert!(store.lookup_is_constant_time());
    assert_every_row_found(&store);

    // So does a first segment of its own size: the first request is sized
    // by declared widths, the rest by what was observed.
    let first_differs = store_with_segments(&[4, 10, 10, 10, 3]);
    assert!(first_differs.lookup_is_constant_time());
    assert_eq!(first_differs.row_count(), 37);
    assert_every_row_found(&first_differs);

    // The short segment stops being last: rows now need a binary search, and
    // every one must still land in the right segment.
    let mut cursor = FakeCursor::new(1_000);
    cursor.position = 37;
    extend_segments(&mut store, &[10, 3, 10], &mut cursor);
    assert!(!store.lookup_is_constant_time());
    assert_eq!(store.row_count(), 60);
    assert_every_row_found(&store);
}

// ----------------------------------------------------- values and accounting

#[test]
fn a_number_reads_the_same_across_segments_of_different_scales() {
    let number = |text: &str| -> Number { text.parse().expect("valid") };
    let batch = |values: &[&str]| {
        let data: Vec<Number> = values.iter().map(|text| number(text)).collect();
        let len = data.len();
        let mut names = TextColumn::new();
        for _ in 0..len {
            names.push("x");
        }
        Arc::new(
            ResultSegment::compact(
                RowBatch::new(vec![
                    Column::not_null(ColumnData::Number(data)),
                    Column::not_null(ColumnData::Text(names)),
                ])
                .expect("batch"),
            )
            .expect("no LOBs"),
        )
    };
    let mut store = store(unlimited(), 10, 1, 40);
    store.fetch_all();
    // Scale 2 (because of 2.25) in the first segment, 1 in the second.
    for values in [&["1.5", "2.25"][..], &["1.5", "-0.5"][..]] {
        let fetch = fetches(&pump(&mut store))[0];
        store.on_fetched(fetch.ticket(), Ok(SegmentReply::new(batch(values), false)));
    }
    let scales: Vec<u8> = store
        .segments()
        .map(
            |(segment, _)| match segment.column(0).expect("column").data() {
                SegmentData::ScaledNumber { scale, .. } => scale,
                other => panic!("{other:?}"),
            },
        )
        .collect();
    assert_eq!(scales, [2, 1]);
    let shown: Vec<String> = (0..4)
        .map(|row| match store.value(row, 0) {
            Some(CellValue::Number(value)) => value.to_string(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(shown, ["1.5", "2.25", "1.5", "-0.5"]);
    let (Some(CellValue::Number(first)), Some(CellValue::Number(third))) =
        (store.value(0, 0), store.value(2, 0))
    else {
        panic!("numbers");
    };
    assert_ne!(first, third, "held at different scales");
    assert_eq!(first.to_number(), third.to_number());
    assert!(matches!(first, NumberValue::Scaled(_)));
}

#[test]
fn accounted_bytes_are_the_capacities_the_segments_hold() {
    let mut store = store(unlimited(), 100, 1, 40);
    store.fetch_all();
    run(&mut store, &mut FakeCursor::new(250));
    let mut sum = 0;
    for (segment, _) in store.segments() {
        let mut columns = 0;
        for column in segment.columns() {
            columns += size_of_val(column.nulls().words());
            columns += match column.data() {
                SegmentData::ScaledNumber { values, .. } => size_of_val(values),
                SegmentData::Text(text) => text.heap_bytes(),
                other => panic!("{other:?}"),
            };
        }
        assert!(
            segment.accounted_bytes() >= columns,
            "a segment counts at least its columns' heap"
        );
        assert!(
            segment.accounted_bytes() - columns < 512,
            "and a small fixed header on top: {} vs {columns}",
            segment.accounted_bytes()
        );
        sum += segment.accounted_bytes();
    }
    let state = store.state();
    assert!(state.retained_bytes() >= sum);
    assert!(state.retained_bytes() - sum < 256, "the store's own index");
    // Scaled ids (8 B), short text (~8 B + 8 B offset), a NULL bit: well
    // under what the driver's batch held (44 B per id alone).
    assert!(
        state.retained_bytes() / 250 < 40,
        "{}",
        state.retained_bytes() / 250
    );
}

// ------------------------------------------------------------------ LOBs

struct Blob;

impl LobStream for Blob {
    fn connection_id(&self) -> ConnectionId {
        ConnectionId::from_raw(1)
    }

    fn kind(&self) -> LobKind {
        LobKind::Binary
    }

    fn size_hint(&self) -> Option<u64> {
        Some(0)
    }

    fn read_chunk(&mut self, _buf: &mut [u8]) -> DbResult<usize> {
        Ok(0)
    }
}

#[test]
fn lob_cells_are_handles_until_the_transaction_ends_and_never_null_after() {
    let session = SessionId::allocate();
    let result = ResultId::new(session, ResultSetId::allocate());
    let mut nulls = NullMask::new(3);
    nulls.set_null(1).expect("in range");
    let locators = vec![
        Some(LobLocator::new(Box::new(Blob))),
        None,
        Some(LobLocator::new(Box::new(Blob))),
    ];
    let batch = RowBatch::new(vec![
        Column::new(ColumnData::Lob(locators), nulls).expect("aligned"),
    ])
    .expect("batch");
    let mut next = 40_u64;
    let segment = compact_batch(session, batch, |_, _locator| {
        next += 1;
        Ok(next)
    })
    .expect("compacted");
    let SegmentData::Lob(ids) = segment.column(0).expect("column").data() else {
        panic!("a LOB column");
    };
    assert_eq!(ids, &[41, 0, 42]);
    // 8 B per id plus the nominal charge for the two present cells.
    assert!(segment.accounted_bytes() >= 3 * 8 + 2 * (256 - 8));

    let mut store = ResultStore::new(
        result,
        &[ColumnMetadata::new("DOC", SqlType::BinaryLob)],
        ResultPolicy::new(unlimited()),
    );
    let fetch = fetches(&pump(&mut store))[0];
    store.on_fetched(
        fetch.ticket(),
        Ok(SegmentReply::new(Arc::new(segment), true)),
    );
    let Some(LobCell::Readable(handle)) = store.lob(0, 0) else {
        panic!("row 0 holds a readable LOB");
    };
    assert_eq!(handle.owner(), session);
    assert_eq!(store.lob(1, 0), None, "SQL NULL has no LOB");
    assert_eq!(store.value(1, 0), Some(CellValue::Null));
    assert_eq!(
        store
            .value(2, 0)
            .map(|cell| matches!(cell, CellValue::Lob(_))),
        Some(true)
    );

    store.statement_executed(&outcome(StatementKind::TransactionControl));
    assert!(
        matches!(store.state().phase(), ResultPhase::Complete),
        "complete stays complete"
    );
    assert_eq!(
        store.lob(0, 0),
        Some(LobCell::Unavailable(LobUnavailable::TransactionEnded))
    );
    assert_eq!(
        store.value(2, 0),
        Some(CellValue::LobUnavailable(LobUnavailable::TransactionEnded)),
        "unavailable, never NULL"
    );
    assert_eq!(store.value(1, 0), Some(CellValue::Null));
}
