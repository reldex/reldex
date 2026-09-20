//! Two properties the M1.3 review found missing, both about a `ReldexBatch*`
//! after it has been handed to the caller:
//!
//! 1. every string it hands out is really NUL-terminated at `ptr[len]` — the
//!    boundary promises it and C will believe it;
//! 2. reading it from several threads at once is sound and gives every thread
//!    the same answer, including the first read of a `NUMBER` or `TIMESTAMP`
//!    column, which is the one that builds a cache.
//!
//! Neither asserts a timing bound; both are deterministic.

#![allow(
    unsafe_code,
    reason = "these tests drive the crate's own C ABI, which is what they exist to prove"
)]

mod support;

use std::sync::{Arc, Barrier};

use reldex_ffi::{
    ReldexBatch, ReldexColumnInfo, ReldexColumnKind, ReldexColumnView, ReldexEventKind,
    ReldexMockScenarioConfig, ReldexMockStatement, ReldexStatus, ReldexStr, reldex_batch_column,
    reldex_batch_column_count, reldex_batch_column_info, reldex_batch_row_count,
};

use support::{Harness, OwnedBatch};

fn config() -> ReldexMockScenarioConfig {
    ReldexMockScenarioConfig {
        rows: 64,
        seed: 3,
        ..ReldexMockScenarioConfig::default()
    }
}

/// Runs the S14 query and hands back its first batch.
fn first_batch(harness: &Harness) -> (u64, OwnedBatch) {
    let session = harness.open(config());
    assert_eq!(
        harness.execute(session, 10, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let executed = harness.next_event();
    assert_eq!(executed.kind, ReldexEventKind::Executed as i32);
    assert!(executed.error.is_null());
    let result = executed.result;

    assert_eq!(harness.fetch(session, 20, result, 64), ReldexStatus::Ok);
    let event = harness.next_event();
    assert_eq!(event.kind, ReldexEventKind::Fetched as i32);
    assert!(!event.batch.is_null());
    (session, OwnedBatch(event.batch))
}

fn info(batch: *const ReldexBatch, column: usize) -> ReldexColumnInfo {
    let mut info = ReldexColumnInfo::default();
    // SAFETY: the batch is live for the test's duration.
    let status = unsafe { reldex_batch_column_info(batch, column, std::ptr::from_mut(&mut info)) };
    assert_eq!(status, ReldexStatus::Ok);
    info
}

fn view(batch: *const ReldexBatch, column: usize) -> ReldexColumnView {
    let mut view = ReldexColumnView::default();
    // SAFETY: as `info`.
    let status = unsafe { reldex_batch_column(batch, column, std::ptr::from_mut(&mut view)) };
    assert_eq!(status, ReldexStatus::Ok);
    view
}

/// Reads the byte at `ptr[len]`.
///
/// # Safety
///
/// Only call this on a string **this library produced**, which is where the
/// NUL-termination promise applies. Doing it to a caller-built `ReldexStr`
/// would be the very out-of-bounds read this test exists to rule out.
unsafe fn terminator(text: ReldexStr) -> u8 {
    assert!(!text.ptr.is_null(), "an outbound ReldexStr is never null");
    // SAFETY: delegated to this function's contract.
    unsafe { *text.ptr.add(text.len) }
}

#[test]
fn every_string_a_batch_hands_out_is_nul_terminated() {
    // The bug this replaces: column names came straight from `ColumnMetadata`,
    // whose `name` is a `Box<str>`. A `Box<str>` has no trailing NUL, so
    // `ptr[len]` was one byte past the allocation — it read as zero often
    // enough to look fine and was undefined behaviour every time. The fix
    // copies each name once per result set, so the byte below is one this
    // library owns and the read here is defined.
    let harness = Harness::new();
    let (_session, batch) = first_batch(&harness);

    // SAFETY: the batch is live.
    let columns = unsafe { reldex_batch_column_count(batch.0) };
    assert_eq!(columns, 3, "the S14 shape has three columns");

    let mut names = Vec::new();
    for column in 0..columns {
        let described = info(batch.0, column);
        // SAFETY: both strings came from this library, which terminates them.
        unsafe {
            assert_eq!(
                terminator(described.name),
                0,
                "column {column}'s name must be NUL-terminated at ptr[len]"
            );
            assert_eq!(
                terminator(described.native_type_name),
                0,
                "column {column}'s native type name must be NUL-terminated"
            );
            names.push(described.name.as_str().unwrap_or_default().to_owned());
        }
        assert!(described.name.len > 0, "column {column} must report a name");
    }
    assert_eq!(names, ["ID", "NAME", "CREATED"]);

    // Same again after the mirrors have been built, in case describing a
    // column ever starts touching the strings.
    let _ = view(batch.0, 0);
    let described = info(batch.0, 0);
    // SAFETY: as above.
    assert_eq!(unsafe { terminator(described.name) }, 0);
}

#[test]
fn an_empty_string_still_points_at_a_readable_nul() {
    // `ReldexStr::empty()` is what an absent optional string crosses as. A C
    // caller doing `strlen(s.ptr)` on one must not read unmapped memory.
    let empty = ReldexStr::empty();
    assert_eq!(empty.len, 0);
    // SAFETY: the empty string is a `'static` NUL byte in this library.
    assert_eq!(unsafe { terminator(empty) }, 0);
}

/// A raw batch pointer a thread may take a *shared* reference through.
///
/// The library documents concurrent read-only calls on one batch as sound
/// (`ReldexBatch` is `Sync`, asserted in `src/batch.rs`); Rust still needs to
/// be told that this pointer may cross a thread boundary.
#[derive(Clone, Copy)]
struct Shared(*const ReldexBatch);

// SAFETY: only ever used to call the read-only entry points, which take a
// shared reference to a `Sync` type. The batch outlives every thread below —
// they are joined before it is released.
unsafe impl Send for Shared {}
// SAFETY: as above.
unsafe impl Sync for Shared {}

#[test]
fn several_threads_may_describe_the_same_batch_at_once() {
    // The bug this replaces: the per-column mirror cache was a `RefCell`, so
    // two threads describing a `NUMBER` column at the same time could hit
    // "already mutably borrowed" — a panic in the lucky case and a data race
    // in the unlucky one, from a function whose C signature is `const`. It is
    // now a `OnceLock` per column.
    //
    // The barrier makes the first call on each column genuinely simultaneous,
    // which is the only moment the cache is built. No timing is asserted.
    const THREADS: usize = 4;

    let harness = Harness::new();
    let (_session, batch) = first_batch(&harness);
    let shared = Shared(batch.0);
    // SAFETY: the batch is live.
    let rows = unsafe { reldex_batch_row_count(batch.0) };
    assert!(rows > 0, "the fetch must have produced rows");

    let barrier = Arc::new(Barrier::new(THREADS));
    let mut handles = Vec::with_capacity(THREADS);
    for _ in 0..THREADS {
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let shared = shared;
            barrier.wait();
            // Column 0 is NUMBER and column 2 is TIMESTAMP: the two kinds that
            // build a mirror. Column 1 is text and borrows directly.
            let numbers = view(shared.0, 0);
            let timestamps = view(shared.0, 2);
            let name = info(shared.0, 0);
            (
                numbers.fixed as usize,
                numbers.fixed_len,
                timestamps.fixed as usize,
                timestamps.fixed_len,
                name.name.ptr as usize,
            )
        }));
    }

    let seen: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("no thread may panic"))
        .collect();

    let first = seen[0];
    assert_ne!(first.0, 0, "the NUMBER mirror must exist");
    assert_ne!(first.2, 0, "the TIMESTAMP mirror must exist");
    assert_eq!(first.1, rows, "the mirror covers every row");
    assert_eq!(first.3, rows);
    for (index, observed) in seen.iter().enumerate() {
        assert_eq!(
            *observed, first,
            "thread {index} saw a different pointer than thread 0; the cache must be built once \
             and shared"
        );
    }

    // And the batch is still usable from this thread afterwards.
    let after = view(batch.0, 0);
    assert_eq!(after.fixed as usize, first.0);
    assert_eq!(after.kind, ReldexColumnKind::Number as i32);
}

#[test]
fn an_out_of_range_column_reports_why_rather_than_leaving_a_stale_error() {
    let harness = Harness::new();
    let (_session, batch) = first_batch(&harness);
    // Leave a recognisable failure behind first, so a stale one is visible.
    let mut view = ReldexColumnView::default();
    // SAFETY: a null batch is exactly what this call must refuse.
    let refused =
        unsafe { reldex_batch_column(std::ptr::null(), 0, std::ptr::from_mut(&mut view)) };
    assert_eq!(refused, ReldexStatus::InvalidArgument);
    support::free_error(reldex_ffi::reldex_last_error_take());

    // SAFETY: the batch is live; column 99 does not exist.
    let status = unsafe { reldex_batch_column(batch.0, 99, std::ptr::from_mut(&mut view)) };
    assert_eq!(status, ReldexStatus::NotFound);
    let error = reldex_ffi::reldex_last_error_take();
    assert!(
        !error.is_null(),
        "a NOT_FOUND must record why, not leave the previous failure in place"
    );
    let mut described = reldex_ffi::ReldexErrorView::default();
    // SAFETY: `error` is live until it is freed below.
    unsafe {
        assert_eq!(
            reldex_ffi::reldex_error_view(error, std::ptr::from_mut(&mut described)),
            ReldexStatus::Ok
        );
        let message = described.message.as_str().unwrap_or_default();
        assert!(
            message.contains("column 99"),
            "the message must name the column: {message}"
        );
    }
    support::free_error(error);
}
