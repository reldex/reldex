//! Behavioural tests for [`Action::GeneratedQuery`] / [`GeneratedQuerySpec`]:
//! the lazily produced result the S15 FFI/UI spike needs to scroll a
//! 1,000,000-row `TableView` without the mock materialising the whole thing.
//!
//! DB-free and fast by construction: everything here runs against
//! `reldex-driver-mock` directly, and every latency used is measured in
//! milliseconds, never seconds, so the suite stays quick. The one exception is
//! [`a_million_rows_stream_through_the_mock_driver_at_a_measured_rate`], which
//! is `#[ignore]`d and meant to be run once by hand in release mode.

#![allow(
    clippy::print_stdout,
    reason = "the smoke test's numbers are measurements a human reads from the test output"
)]

mod support;

use std::time::{Duration, Instant};

use reldex_db_driver_api::{Cursor, DatabaseConnection, RowBatch, Statement, ValueRef};
use reldex_driver_mock::{Action, GeneratedQuerySpec, Scenario, ScriptValue};
use support::{connect, one};

fn run(sql: &str, spec: GeneratedQuerySpec) -> (Box<dyn DatabaseConnection>, Box<dyn Cursor>) {
    let scenario = Scenario::new();
    scenario.on_sql(sql, Action::GeneratedQuery(spec));
    let mut connection = connect(&scenario);
    let mut outcome = connection.execute(&Statement::new(sql)).expect("execute");
    let cursor = outcome
        .take_cursor()
        .expect("a generated query has a cursor");
    (connection, cursor)
}

fn assert_cell_matches(expected: &ScriptValue, actual: ValueRef<'_>) {
    match expected {
        ScriptValue::Null => assert!(actual.is_null(), "expected NULL, got {actual:?}"),
        ScriptValue::Number(expected) => {
            assert_eq!(
                actual.as_number(),
                Some(expected),
                "wrong number, got {actual:?}"
            );
        }
        ScriptValue::Text(expected) => {
            assert_eq!(
                actual.as_str(),
                Some(expected.as_str()),
                "wrong text, got {actual:?}"
            );
        }
        ScriptValue::Timestamp(expected) => {
            assert_eq!(
                actual.as_timestamp(),
                Some(*expected),
                "wrong timestamp, got {actual:?}"
            );
        }
        other => panic!("this test does not expect a {other:?} cell"),
    }
}

#[test]
fn a_generated_query_streams_exactly_rows_rows_at_every_batch_size() {
    // 12_345 divides evenly into none of the batch sizes below, so every size
    // but 1 ends on a short last batch.
    const ROWS: u64 = 12_345;
    let scenario = Scenario::new();
    scenario.on_sql(
        "SELECT * FROM big",
        Action::GeneratedQuery(GeneratedQuerySpec::s14_shape(ROWS, 7)),
    );
    let mut connection = connect(&scenario);

    for batch_size in [1_usize, 100, 1_000, 10_000] {
        let mut outcome = connection
            .execute(&Statement::new("SELECT * FROM big"))
            .expect("execute");
        let mut cursor = outcome.take_cursor().expect("cursor");

        let mut total_rows = 0_u64;
        let mut batch_count = 0_u64;
        loop {
            let batch = cursor.fetch_batch(one(batch_size)).expect("fetch succeeds");
            if batch.is_empty() {
                break;
            }
            assert!(
                batch.row_count() <= batch_size,
                "batch size {batch_size}: got {} rows, more than requested",
                batch.row_count()
            );
            total_rows += batch.row_count() as u64;
            batch_count += 1;
        }

        assert_eq!(
            total_rows, ROWS,
            "batch size {batch_size} lost or duplicated rows"
        );
        assert_eq!(
            batch_count,
            ROWS.div_ceil(batch_size as u64),
            "batch size {batch_size}: unexpected number of batches"
        );
        assert!(cursor.is_exhausted());
        // Exhaustion is sticky: fetching again must not resurrect rows.
        assert!(
            cursor
                .fetch_batch(one(batch_size))
                .expect("still legal")
                .is_empty()
        );
        cursor.close().expect("close after exhaustion");
    }
}

#[test]
fn expected_cell_matches_fetched_data_at_first_last_null_thai_and_emoji_rows() {
    const ROWS: u64 = 500;
    let spec = GeneratedQuerySpec::s14_shape(ROWS, 99);
    let (mut _connection, mut cursor) = run("SELECT * FROM big", spec.clone());

    // 1-based row numbers hitting every cadence in the S14 shape's NAME
    // column, plus the very first and very last row of the whole result.
    let interesting_row_numbers: std::collections::HashSet<u64> =
        [1, 10, 25, 100, ROWS].into_iter().collect();
    let mut checked = std::collections::HashSet::new();

    let mut next_row = 0_u64; // 0-based offset of the batch about to be read
    loop {
        let batch = cursor.fetch_batch(one(64)).expect("fetch");
        if batch.is_empty() {
            break;
        }
        for offset in 0..batch.row_count() {
            let row = next_row + offset as u64; // 0-based
            let row_number = row + 1;
            if interesting_row_numbers.contains(&row_number) {
                for column in 0..batch.column_count() {
                    let expected = spec
                        .expected_cell(row, column)
                        .unwrap_or_else(|| panic!("row {row} column {column} is in range"));
                    let actual = batch
                        .value(offset, column)
                        .unwrap_or_else(|| panic!("row {row} column {column} exists"));
                    assert_cell_matches(&expected, actual);
                }
                checked.insert(row_number);
            }
        }
        next_row += batch.row_count() as u64;
    }

    assert_eq!(
        checked, interesting_row_numbers,
        "every interesting row must actually have been reached while streaming"
    );
    cursor.close().expect("close");
}

#[test]
fn expected_cell_returns_none_outside_the_result_and_column_range() {
    let spec = GeneratedQuerySpec::s14_shape(10, 0);
    assert!(
        spec.expected_cell(10, 0).is_none(),
        "row 10 is out of range for 10 rows"
    );
    assert!(
        spec.expected_cell(0, 3).is_none(),
        "the S14 shape has 3 columns"
    );
    assert!(
        spec.expected_cell(9, 2).is_some(),
        "the last row is in range"
    );
}

#[test]
fn per_fetch_latency_occupies_the_calling_thread_on_every_fetch() {
    // Coarse lower-bound only, per the "DB-free, fast" test bar: no assertion
    // on an upper bound, which would be flaky under load.
    let per_fetch = Duration::from_millis(20);
    let spec = GeneratedQuerySpec::s14_shape(30, 0).with_per_fetch_latency(per_fetch);
    let (_connection, mut cursor) = run("SELECT * FROM big", spec);

    let started = Instant::now();
    let first = cursor.fetch_batch(one(10)).expect("first batch");
    let after_first = started.elapsed();
    assert_eq!(first.row_count(), 10);
    assert!(
        after_first >= per_fetch,
        "the first fetch must pay the per-fetch latency too: took {after_first:?}"
    );

    let started = Instant::now();
    let second = cursor.fetch_batch(one(10)).expect("second batch");
    let after_second = started.elapsed();
    assert_eq!(second.row_count(), 10);
    assert!(
        after_second >= per_fetch,
        "every fetch pays the per-fetch latency: took {after_second:?}"
    );
    cursor.close().expect("close");
}

#[test]
fn first_batch_latency_applies_once_on_top_of_any_per_fetch_latency() {
    // No upper bound on any single fetch: a loaded CI runner can turn a 10ms
    // sleep into 90ms (see the flake this replaced), so a tight
    // `second_elapsed < first_batch` check on one sample is not safe at any
    // realistic margin. Instead: widely separated durations, plus a vote
    // across several later fetches — a stall would have to hit *every one*
    // of them to produce a false failure, which a generic scheduler stall
    // does not do.
    let first_batch = Duration::from_millis(200);
    let per_fetch = Duration::from_millis(5);
    const LATER_FETCHES: usize = 5;
    let spec = GeneratedQuerySpec::s14_shape(100, 0)
        .with_first_batch_latency(first_batch)
        .with_per_fetch_latency(per_fetch);
    let (_connection, mut cursor) = run("SELECT * FROM big", spec);

    let started = Instant::now();
    cursor.fetch_batch(one(10)).expect("first batch");
    let first_elapsed = started.elapsed();
    assert!(
        first_elapsed >= first_batch + per_fetch,
        "the first fetch should pay both latencies: took {first_elapsed:?}"
    );

    let mut later_elapsed = Vec::with_capacity(LATER_FETCHES);
    for _ in 0..LATER_FETCHES {
        let started = Instant::now();
        cursor.fetch_batch(one(10)).expect("later batch");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= per_fetch,
            "every fetch pays the per-fetch latency: took {elapsed:?}"
        );
        later_elapsed.push(elapsed);
    }

    let fastest_later = later_elapsed
        .iter()
        .copied()
        .min()
        .expect("LATER_FETCHES is non-zero");
    assert!(
        fastest_later < first_batch,
        "the fastest of {LATER_FETCHES} later fetches ({fastest_later:?}) was not faster than \
         the first-batch latency ({first_batch:?}); the first-batch latency looks like it is \
         being paid again instead of only once. All later fetch times: {later_elapsed:?}"
    );

    cursor.close().expect("close");
}

#[test]
fn closing_a_generated_cursor_early_is_fine_and_frees_its_state() {
    let scenario = Scenario::new();
    scenario.on_sql(
        "SELECT * FROM big",
        Action::GeneratedQuery(GeneratedQuerySpec::s14_shape(1_000, 3)),
    );
    let mut connection = connect(&scenario);
    let mut outcome = connection
        .execute(&Statement::new("SELECT * FROM big"))
        .expect("execute");
    let mut cursor = outcome.take_cursor().expect("cursor");

    // Fetch a couple of batches, nowhere near exhaustion, then close.
    let batch = cursor.fetch_batch(one(10)).expect("first batch");
    assert_eq!(batch.row_count(), 10);
    let batch = cursor.fetch_batch(one(10)).expect("second batch");
    assert_eq!(batch.row_count(), 10);
    assert!(!cursor.is_exhausted());

    cursor.close().expect("closing early must succeed");
    assert_eq!(scenario.counts().cursors_opened, 1);
    assert_eq!(scenario.counts().cursors_closed, 1);

    connection.close().expect("connection still closes cleanly");
}

#[test]
fn a_zero_row_generated_query_is_immediately_exhausted() {
    let (_connection, mut cursor) = run("SELECT * FROM big", GeneratedQuerySpec::s14_shape(0, 0));
    let batch = cursor
        .fetch_batch(one(10))
        .expect("fetch on an empty result");
    assert!(batch.is_empty());
    assert!(cursor.is_exhausted());
    cursor.close().expect("close");
}

#[test]
#[ignore = "run by hand in release mode: `cargo test --release -p reldex-driver-mock \
            --test generated_query -- --ignored --nocapture \
            a_million_rows_stream_through_the_mock_driver_at_a_measured_rate`"]
fn a_million_rows_stream_through_the_mock_driver_at_a_measured_rate() {
    // Mirrors `crates/drivers/oracle-thin/tests/s14_large_result.rs`, but
    // against the mock's `GeneratedQuerySpec` rather than a live database.
    // `reldex-driver-mock` must not depend on `reldex-db-core`
    // (`crates/db-core/tests/dependency_rules.rs`), so this streams straight
    // through the driver contract (execute -> fetch_batch loop) instead of
    // through a `db-core` session; the numbers are the generator's raw
    // throughput, not `db-core`'s dispatch overhead on top of it.
    const ROWS: u64 = 1_000_000;
    let batch_size = one(1_000);
    let (_connection, mut cursor) =
        run("SELECT * FROM big", GeneratedQuerySpec::s14_shape(ROWS, 0));

    let started = Instant::now();
    let mut rows = 0_u64;
    let mut batches = 0_u64;
    loop {
        let batch: RowBatch = cursor.fetch_batch(batch_size).expect("fetch a batch");
        let count = batch.row_count() as u64;
        drop(batch);
        if count == 0 {
            break;
        }
        rows += count;
        batches += 1;
    }
    let elapsed = started.elapsed();
    assert_eq!(rows, ROWS);
    cursor.close().expect("close");

    let per_second = rows as f64 / elapsed.as_secs_f64();
    println!(
        "generated_query smoke: {rows} rows in {batches} batches, {elapsed:?} elapsed, \
         {per_second:.0} rows/s"
    );
}
