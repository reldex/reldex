//! A result's columns, available from the moment `EXECUTED` is drained.
//!
//! What the Qt adapter (M1.6) had to do without this: read `column_count` off
//! the `EXECUTED` event, put an unnamed header up, then reset the model when
//! the first batch arrived with the names. That reset is a mid-stream model
//! rebuild on every query, and on a result with columns and no rows the names
//! never arrive at all.
//!
//! Also covers the two smaller things the same consumer found: a `FETCHED`
//! event that does not say which result it belongs to, and the absence of a
//! mock statement that returns an empty result set.

#![allow(
    unsafe_code,
    reason = "these tests drive the crate's own C ABI, which is what they exist to prove"
)]

mod support;

use reldex_ffi::{
    ReldexColumnInfo, ReldexColumnKind, ReldexEventKind, ReldexMockScenarioConfig,
    ReldexMockStatement, ReldexStatus, reldex_batch_column_info, reldex_session_result_column,
    reldex_session_result_column_count,
};

use support::{Harness, OwnedBatch};

fn config() -> ReldexMockScenarioConfig {
    ReldexMockScenarioConfig {
        rows: 32,
        seed: 7,
        ..ReldexMockScenarioConfig::default()
    }
}

/// `(name, kind, native type)` of one column of a live result.
fn described(harness: &Harness, session: u64, result: u64, column: usize) -> (String, i32, String) {
    let mut info = ReldexColumnInfo::default();
    // SAFETY: the hub is live and `info` is a real local with its
    // `struct_size` set.
    let status = unsafe {
        reldex_session_result_column(
            harness.hub(),
            session,
            result,
            column,
            std::ptr::from_mut(&mut info),
        )
    };
    assert_eq!(status, ReldexStatus::Ok);
    read(&info)
}

/// The same triple, read from a batch.
fn described_by_batch(
    batch: *const reldex_ffi::ReldexBatch,
    column: usize,
) -> (String, i32, String) {
    let mut info = ReldexColumnInfo::default();
    // SAFETY: the batch is live for the test's duration.
    let status = unsafe { reldex_batch_column_info(batch, column, std::ptr::from_mut(&mut info)) };
    assert_eq!(status, ReldexStatus::Ok);
    read(&info)
}

fn read(info: &ReldexColumnInfo) -> (String, i32, String) {
    // SAFETY: both strings borrow from storage this library keeps alive for
    // the call's documented lifetime, and both are NUL-terminated.
    let (name, native) = unsafe {
        assert_eq!(
            *info.name.ptr.add(info.name.len),
            0,
            "the boundary promises NUL termination"
        );
        (
            info.name.as_str().expect("a name is UTF-8").to_owned(),
            info.native_type_name
                .as_str()
                .expect("a native type name is UTF-8")
                .to_owned(),
        )
    };
    (name, info.kind, native)
}

fn column_count(harness: &Harness, session: u64, result: u64) -> usize {
    // SAFETY: the hub is live.
    unsafe { reldex_session_result_column_count(harness.hub(), session, result) }
}

#[test]
fn a_results_columns_are_described_before_any_row_arrives() {
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.execute(session, 10, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let executed = harness.next_event();
    assert_eq!(executed.kind, ReldexEventKind::Executed as i32);
    assert!(executed.error.is_null());
    assert!(executed.has_result);
    let result = executed.result;
    assert_eq!(executed.column_count, 3);

    // No fetch has been submitted: this is the moment a grid wants its header.
    assert_eq!(column_count(&harness, session, result), 3);
    let headers: Vec<_> = (0..3)
        .map(|column| described(&harness, session, result, column))
        .collect();
    assert_eq!(
        headers
            .iter()
            .map(|(name, _, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["ID", "NAME", "CREATED"]
    );
    assert_eq!(
        headers.iter().map(|(_, kind, _)| *kind).collect::<Vec<_>>(),
        [
            ReldexColumnKind::Number as i32,
            ReldexColumnKind::Text as i32,
            ReldexColumnKind::Timestamp as i32,
        ]
    );

    // And a batch says exactly the same thing, so an adapter may use either.
    assert_eq!(harness.fetch(session, 20, result, 32), ReldexStatus::Ok);
    let fetched = harness.next_event();
    assert_eq!(fetched.kind, ReldexEventKind::Fetched as i32);
    let batch = OwnedBatch(fetched.batch);
    assert!(!batch.0.is_null());
    for (column, expected) in headers.iter().enumerate() {
        assert_eq!(
            described_by_batch(batch.0, column),
            *expected,
            "column {column}"
        );
    }
}

#[test]
fn an_empty_result_still_has_columns() {
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.execute(session, 10, ReldexMockStatement::EmptyQuery),
        ReldexStatus::Ok
    );
    let executed = harness.next_event();
    assert!(executed.error.is_null());
    assert!(executed.has_result, "an empty result is still a result");
    let result = executed.result;
    assert_eq!(executed.column_count, 3);
    assert_eq!(column_count(&harness, session, result), 3);
    assert_eq!(described(&harness, session, result, 1).0, "NAME");

    // The only batch it will ever produce carries no rows, which is exactly
    // why the header cannot be built from one.
    assert_eq!(harness.fetch(session, 20, result, 100), ReldexStatus::Ok);
    let fetched = harness.next_event();
    assert_eq!(fetched.kind, ReldexEventKind::Fetched as i32);
    assert_eq!(fetched.row_count, 0, "the result is exhausted immediately");
    let batch = OwnedBatch(fetched.batch);
    assert!(!batch.0.is_null());
    // SAFETY: the batch is live.
    assert_eq!(unsafe { reldex_ffi::reldex_batch_row_count(batch.0) }, 0);
}

#[test]
fn a_fetch_reply_says_which_result_it_belongs_to() {
    let harness = Harness::new();
    let session = harness.open(config());

    // Two results open at once: the id on the reply is the only thing that
    // distinguishes them without the caller keeping its own request map.
    assert_eq!(
        harness.execute(session, 10, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let first = harness.next_event().result;
    assert_eq!(
        harness.execute(session, 11, ReldexMockStatement::EmptyQuery),
        ReldexStatus::Ok
    );
    let second = harness.next_event().result;
    assert_ne!(first, second);

    assert_eq!(harness.fetch(session, 20, second, 8), ReldexStatus::Ok);
    assert_eq!(harness.fetch(session, 21, first, 8), ReldexStatus::Ok);

    for expected in [second, first] {
        let event = harness.next_event();
        assert_eq!(event.kind, ReldexEventKind::Fetched as i32);
        assert!(event.has_result);
        assert_eq!(event.result, expected);
        support::release_batch(&event);
    }

    assert_eq!(harness.close_result(session, 30, first), ReldexStatus::Ok);
    let closed = harness.next_event();
    assert_eq!(closed.kind, ReldexEventKind::ResultClosed as i32);
    assert!(closed.has_result);
    assert_eq!(closed.result, first, "the reply names what ended");

    // And the description goes with it, while the other result keeps its own.
    assert_eq!(column_count(&harness, session, first), 0);
    assert_eq!(column_count(&harness, session, second), 3);
}

#[test]
fn an_unknown_result_or_column_reports_why() {
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.execute(session, 10, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let result = harness.next_event().result;

    let mut info = ReldexColumnInfo::default();
    // SAFETY: the hub is live and `info` is a real local.
    let missing = unsafe {
        reldex_session_result_column(
            harness.hub(),
            session,
            result + 1_000,
            0,
            std::ptr::from_mut(&mut info),
        )
    };
    assert_eq!(missing, ReldexStatus::NotFound);
    support::free_error(reldex_ffi::reldex_last_error_take());

    // SAFETY: as above.
    let out_of_range = unsafe {
        reldex_session_result_column(
            harness.hub(),
            session,
            result,
            3,
            std::ptr::from_mut(&mut info),
        )
    };
    assert_eq!(out_of_range, ReldexStatus::NotFound);
    support::free_error(reldex_ffi::reldex_last_error_take());

    // SAFETY: a null `out` is exactly what this must refuse.
    let refused = unsafe {
        reldex_session_result_column(harness.hub(), session, result, 0, std::ptr::null_mut())
    };
    assert_eq!(refused, ReldexStatus::InvalidArgument);
    support::free_error(reldex_ffi::reldex_last_error_take());

    // An unknown result has no columns, and says so without failing.
    assert_eq!(column_count(&harness, session, result + 1_000), 0);
}
