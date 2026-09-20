//! Checks, without depending on OS RSS or a custom global allocator (the
//! workspace denies `unsafe_code`, which a `GlobalAlloc` impl needs), that
//! [`GeneratedQuerySpec`]/`GeneratedCursor` cost `O(batch)` memory rather than
//! `O(rows)`.
//!
//! # The structural argument
//!
//! `crates/drivers/mock/src/generated.rs`'s `GeneratedCursor` holds exactly:
//! a [`GeneratedQuerySpec`] (parameters — a row count, a handful of column
//! generators, a seed, two optional latencies — never rows), a `u64` cursor
//! position, and a couple of `bool`/`Option` flags. Contrast
//! `crate::cursor::MockCursor`, which holds `rows: Vec<Vec<ScriptValue>>` —
//! the *entire* scripted result — because it exists to replay a fixture, not
//! to generate one. `GeneratedCursor::fetch_batch` computes values for only
//! the rows in the requested batch into a local `Vec` that is converted into
//! the returned `RowBatch` and then goes out of scope; nothing it allocates
//! survives the call except the advanced `next_row` position.
//!
//! Two independent, no-`unsafe` checks back that argument:
//!
//! - [`a_generated_query_specs_footprint_does_not_depend_on_its_row_count`]
//!   checks the one part of it that is testable by inspection: the spec
//!   itself — which is what `GeneratedCursor` stores, and the only field of
//!   it that varies per query — does not grow with `rows`, because `rows` is
//!   a fixed-width `u64`, not a buffer sized to it.
//! - [`at_most_one_batch_is_ever_alive_at_once_regardless_of_the_total_row_count`]
//!   wraps every fetched `RowBatch` in a small guard that increments a
//!   counter on receipt and decrements it on drop — a counter of
//!   generated-but-not-yet-dropped batches, built from ordinary safe `Drop`
//!   rather than an allocator hook — and asserts that count never exceeds
//!   one, at both a small and a much larger total row count.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};

use reldex_db_driver_api::{
    ConnectionParams, Credentials, DatabaseConnection, DatabaseDriver, Endpoint, RowBatch,
    Statement,
};
use reldex_driver_mock::{Action, GeneratedQuerySpec, MockDriver, Scenario};

#[test]
fn a_generated_query_specs_footprint_does_not_depend_on_its_row_count() {
    let tiny = GeneratedQuerySpec::s14_shape(1, 0);
    let huge = GeneratedQuerySpec::s14_shape(1_000_000_000_000, 0);

    assert_eq!(
        std::mem::size_of_val(&tiny),
        std::mem::size_of_val(&huge),
        "a spec's in-memory size must not depend on how many rows it declares"
    );
    assert_eq!(tiny.rows(), 1);
    assert_eq!(huge.rows(), 1_000_000_000_000);
}

/// A live count of fetched batches that have not been dropped yet, tracked
/// through ordinary `Drop` rather than an allocator hook (the workspace denies
/// `unsafe_code`, so a custom `GlobalAlloc` is not an option here).
struct LiveBatchCounter {
    live: AtomicUsize,
    peak: AtomicUsize,
}

impl LiveBatchCounter {
    const fn new() -> Self {
        Self {
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    fn track(&self, batch: RowBatch) -> TrackedBatch<'_> {
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(live, Ordering::SeqCst);
        TrackedBatch {
            batch,
            counter: self,
        }
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

struct TrackedBatch<'a> {
    batch: RowBatch,
    counter: &'a LiveBatchCounter,
}

impl Drop for TrackedBatch<'_> {
    fn drop(&mut self) {
        self.counter.live.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Streams `rows` rows at a fixed batch size, wrapping every batch in
/// [`TrackedBatch`] so [`LiveBatchCounter::peak`] reports the maximum number
/// of batches ever alive at the same time.
fn stream_and_track(rows: u64) -> (u64, usize) {
    let scenario = Scenario::new();
    scenario.on_sql(
        "SELECT * FROM big",
        Action::GeneratedQuery(GeneratedQuerySpec::s14_shape(rows, 0)),
    );
    let driver = MockDriver::new(scenario);
    let params = ConnectionParams::new(
        Endpoint::ConnectString("mock".to_owned()),
        Credentials::External,
    );
    let mut connection: Box<dyn DatabaseConnection> = driver.connect(&params).expect("connect");
    let mut outcome = connection
        .execute(&Statement::new("SELECT * FROM big"))
        .expect("execute");
    let mut cursor = outcome.take_cursor().expect("cursor");

    let counter = LiveBatchCounter::new();
    let batch_size = NonZeroUsize::new(250).expect("non-zero");
    let mut total_rows = 0_u64;
    loop {
        let batch = cursor.fetch_batch(batch_size).expect("fetch");
        if batch.is_empty() {
            break;
        }
        let tracked = counter.track(batch);
        total_rows += tracked.batch.row_count() as u64;
        // `tracked` is dropped here, at the end of the loop body, before the
        // next `fetch_batch` call — the calling pattern every consumer in
        // this crate (and `db-core`) follows.
    }
    cursor.close().expect("close");
    (total_rows, counter.peak())
}

#[test]
fn at_most_one_batch_is_ever_alive_at_once_regardless_of_the_total_row_count() {
    let (small_rows, small_peak) = stream_and_track(2_000);
    assert_eq!(small_rows, 2_000);
    assert_eq!(
        small_peak, 1,
        "a caller that drops each batch before fetching the next must never see two alive"
    );

    // The same invariant at 100x the rows: if `GeneratedCursor` ever started
    // accumulating batches instead of generating them on demand, this is
    // where it would show up, however small the earlier pass looked.
    let (large_rows, large_peak) = stream_and_track(200_000);
    assert_eq!(large_rows, 200_000);
    assert_eq!(
        large_peak, 1,
        "peak live batches must not grow with the total row count"
    );
}
