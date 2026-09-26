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
    ReldexColumnInfo, ReldexColumnKind, ReldexColumnView, ReldexEventKind,
    ReldexMockScenarioConfig, ReldexMockStatement, ReldexStatus, reldex_batch_column_fixed,
    reldex_batch_column_info, reldex_session_result_column, reldex_session_result_column_count,
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

/// The documented lifetime lists its invalidators, all caller-initiated. "A
/// session lost mid-statement" is not one of them — not even once its
/// `TERMINAL` has been drained and the session retired, which is when this
/// crate lets go of everything else the session held.
///
/// The names are compared against a copy taken before the loss, so a failure
/// shows up as wrong content rather than only as a sanitizer report — and
/// nothing here reads memory whose life the contract does not guarantee.
#[test]
fn losing_a_session_mid_statement_does_not_free_its_column_names() {
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.execute(session, 10, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let executed = harness.next_event();
    assert!(executed.error.is_null());
    let result = executed.result;

    // The pointers the caller holds across the panic, and an owned copy of
    // what they must still say afterwards.
    let mut info = ReldexColumnInfo::default();
    // SAFETY: the hub is live and `info` is a real local with `struct_size`.
    let status = unsafe {
        reldex_session_result_column(
            harness.hub(),
            session,
            result,
            0,
            std::ptr::from_mut(&mut info),
        )
    };
    assert_eq!(status, ReldexStatus::Ok);
    let expected = read(&info);
    assert_eq!(expected.0, "ID");

    // Lose the session. Nothing is submitted for `result`.
    assert_eq!(
        harness.execute(session, 11, ReldexMockStatement::LoseSession),
        ReldexStatus::Ok
    );
    let failed = harness.next_event();
    assert_eq!(failed.request, 11);
    assert_eq!(
        failed.session_state,
        reldex_ffi::ReldexSessionState::Lost as i32,
        "the statement must report the session lost"
    );
    support::take_error(&failed);
    let terminal = harness.next_event();
    assert_eq!(terminal.kind, ReldexEventKind::Terminal as i32);
    support::take_error(&terminal);

    // The pointers taken before the panic must still read the same. `read`
    // also checks the NUL at `ptr[len]`, which a freed buffer would not have
    // reliably kept.
    assert_eq!(
        read(&info),
        expected,
        "a session lost to an internal panic must not end the documented lifetime of a result's column names"
    );

    // The result itself is gone — the session is lost and, its `TERMINAL`
    // drained, retired — so the accessor reports that rather than pretending.
    // The two facts are separate on purpose: what was already handed out
    // stays readable; what was not is not invented.
    assert_eq!(column_count(&harness, session, result), 0);
    support::free_error(reldex_ffi::reldex_last_error_take());
    let mut after = ReldexColumnInfo::default();
    // SAFETY: the hub is live and `after` is a real local.
    let refused = unsafe {
        reldex_session_result_column(
            harness.hub(),
            session,
            result,
            0,
            std::ptr::from_mut(&mut after),
        )
    };
    assert_eq!(refused, ReldexStatus::NotFound);
    support::free_error(reldex_ffi::reldex_last_error_take());

    // And the pointers are still good after that refusal too.
    assert_eq!(read(&info), expected);
}

/// The explicit mirror call on a column that has no mirror, and on the
/// arguments that must be refused rather than dereferenced.
#[test]
fn asking_for_a_fixed_array_that_does_not_exist_is_answered_not_invented() {
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.execute(session, 10, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let result = harness.next_event().result;
    assert_eq!(harness.fetch(session, 20, result, 32), ReldexStatus::Ok);
    let fetched = harness.next_event();
    let batch = OwnedBatch(fetched.batch);
    assert!(!batch.0.is_null());

    let mut view = ReldexColumnView::default();
    let out = std::ptr::from_mut(&mut view);

    // Column 1 is VARCHAR2: it borrows `data`/`offsets` and has no fixed-width
    // elements at all. That is an answer, not a failure.
    // SAFETY: the batch is live and `view` is a local.
    let text = unsafe { reldex_batch_column_fixed(batch.0, 1, out) };
    assert_eq!(text, ReldexStatus::Ok);
    assert!(view.fixed.is_null(), "a text column has no element array");
    assert_eq!(view.fixed_len, 0);
    assert_eq!(view.fixed_stride, 0, "and no element width to report");
    assert_eq!(view.kind, ReldexColumnKind::Text as i32);

    // SAFETY: as above; column 9 does not exist.
    let out_of_range = unsafe { reldex_batch_column_fixed(batch.0, 9, out) };
    assert_eq!(out_of_range, ReldexStatus::NotFound);
    support::free_error(reldex_ffi::reldex_last_error_take());

    // SAFETY: a null batch is exactly what this must refuse.
    let no_batch = unsafe { reldex_batch_column_fixed(std::ptr::null(), 0, out) };
    assert_eq!(no_batch, ReldexStatus::InvalidArgument);
    support::free_error(reldex_ffi::reldex_last_error_take());

    // SAFETY: a null `out` is likewise what this must refuse.
    let no_out = unsafe { reldex_batch_column_fixed(batch.0, 0, std::ptr::null_mut()) };
    assert_eq!(no_out, ReldexStatus::InvalidArgument);
    support::free_error(reldex_ffi::reldex_last_error_take());
}

/// The terminal batch that reports a result exhausted carries no columns, so
/// an adapter that reads its headers per batch must skip it — which is the
/// reason `reldex_session_result_column` exists.
#[test]
fn an_exhausted_batch_describes_no_columns() {
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.execute(session, 10, ReldexMockStatement::EmptyQuery),
        ReldexStatus::Ok
    );
    let result = harness.next_event().result;
    assert_eq!(harness.fetch(session, 20, result, 16), ReldexStatus::Ok);
    let fetched = harness.next_event();
    assert_eq!(fetched.row_count, 0);
    let batch = OwnedBatch(fetched.batch);
    assert!(!batch.0.is_null());

    // SAFETY: the batch is live.
    assert_eq!(unsafe { reldex_ffi::reldex_batch_column_count(batch.0) }, 0);
    let mut info = ReldexColumnInfo::default();
    // SAFETY: as above.
    let missing = unsafe { reldex_batch_column_info(batch.0, 0, std::ptr::from_mut(&mut info)) };
    assert_eq!(missing, ReldexStatus::NotFound);
    support::free_error(reldex_ffi::reldex_last_error_take());

    // The result itself still knows, which is the point.
    assert_eq!(column_count(&harness, session, result), 3);
}

/// A second execute leaves the first result open and describable: nothing is
/// closed implicitly, which is also why a caller that never closes results
/// accumulates them.
#[test]
fn executing_again_does_not_close_the_previous_result() {
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.execute(session, 10, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let first = harness.next_event().result;
    assert_eq!(
        harness.execute(session, 11, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let second = harness.next_event().result;
    assert_ne!(first, second);

    assert_eq!(column_count(&harness, session, first), 3);
    assert_eq!(described(&harness, session, first, 0).0, "ID");
    // And it can still be fetched from.
    assert_eq!(harness.fetch(session, 20, first, 4), ReldexStatus::Ok);
    let event = harness.next_event();
    assert_eq!(event.result, first);
    assert!(event.row_count > 0);
    support::release_batch(&event);
}
