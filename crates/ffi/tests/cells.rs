//! Reading cells through the boundary exactly as the adapter will: one
//! `reldex_batch_column` call per column, then pointer arithmetic per cell,
//! checked against the mock's own `expected_cell` (ADR-0003 D4).

// This crate's tests call its `extern "C"` functions directly, which needs the
// same opt-out `src/lib.rs` documents; the workspace denies `unsafe_code`.
#![allow(
    unsafe_code,
    reason = "these tests drive the crate's own C ABI, which is what they exist to prove"
)]

mod support;

use reldex_driver_mock::{GeneratedQuerySpec, ScriptValue};
use reldex_ffi::{
    ReldexArenaView, ReldexColumnInfo, ReldexColumnKind, ReldexColumnView, ReldexEventKind,
    ReldexFormatOptions, ReldexMockScenarioConfig, ReldexMockStatement, ReldexNumber, ReldexStatus,
    ReldexStr, ReldexTimestamp, ReldexTimestampStyle, reldex_batch_column,
    reldex_batch_column_count, reldex_batch_column_info, reldex_batch_format_column,
    reldex_batch_row_count, reldex_mock_statement, reldex_text_arena_clear,
    reldex_text_arena_count, reldex_text_arena_create, reldex_text_arena_release,
    reldex_text_arena_view,
};

use support::{Harness, OwnedBatch};

const ROWS: u64 = 250;
const SEED: u64 = 7;

fn config() -> ReldexMockScenarioConfig {
    ReldexMockScenarioConfig {
        rows: ROWS,
        seed: SEED,
        ..ReldexMockScenarioConfig::default()
    }
}

/// Whether row `row` of the column is SQL NULL, read the way C will.
///
/// # Safety
///
/// `view` must describe a live batch.
unsafe fn is_null(view: &ReldexColumnView, row: usize) -> bool {
    if view.null_bits.is_null() || row >= view.row_count {
        return false;
    }
    // SAFETY: the view reports how many words the bitmap has, and the batch
    // that owns them is alive.
    let words = unsafe { std::slice::from_raw_parts(view.null_bits, view.null_word_count) };
    (words[row / 64] >> (row % 64)) & 1 == 1
}

/// Reads a text/JSON/unsupported cell without copying anything.
///
/// # Safety
///
/// `view` must describe a live batch.
unsafe fn text_cell(view: &ReldexColumnView, row: usize) -> &str {
    assert!(!view.offsets.is_null(), "a text column has offsets");
    // SAFETY: the view promises `row_count + 1` offsets and `data_len` bytes,
    // both borrowed from a live batch.
    let (offsets, data) = unsafe {
        (
            std::slice::from_raw_parts(view.offsets, view.row_count + 1),
            std::slice::from_raw_parts(view.data, view.data_len),
        )
    };
    std::str::from_utf8(&data[offsets[row]..offsets[row + 1]]).expect("the buffer is UTF-8")
}

/// # Safety
///
/// `view` must describe a live `Number` column.
unsafe fn number_cell(view: &ReldexColumnView, row: usize) -> ReldexNumber {
    assert_eq!(
        view.fixed_stride,
        size_of::<ReldexNumber>(),
        "the header and the library must agree on ReldexNumber's size"
    );
    // SAFETY: the view reports `fixed_len` elements of `fixed_stride` bytes,
    // borrowed from a live batch.
    let values =
        unsafe { std::slice::from_raw_parts(view.fixed.cast::<ReldexNumber>(), view.fixed_len) };
    values[row]
}

/// # Safety
///
/// `view` must describe a live `Timestamp` column.
unsafe fn timestamp_cell(view: &ReldexColumnView, row: usize) -> ReldexTimestamp {
    assert_eq!(view.fixed_stride, size_of::<ReldexTimestamp>());
    // SAFETY: as `number_cell`.
    let values =
        unsafe { std::slice::from_raw_parts(view.fixed.cast::<ReldexTimestamp>(), view.fixed_len) };
    values[row]
}

/// The integer a `ReldexNumber` holds, for the small ids this shape produces.
fn number_as_u64(number: &ReldexNumber) -> u64 {
    let mut value = 0_u64;
    for digit in &number.digits[..usize::from(number.digit_count)] {
        value = value * 10 + u64::from(*digit);
    }
    let shift = i32::from(number.exponent) - i32::from(number.digit_count);
    for _ in 0..shift.max(0) {
        value *= 10;
    }
    value
}

fn column_view(batch: *const reldex_ffi::ReldexBatch, column: usize) -> ReldexColumnView {
    let mut view = ReldexColumnView::default();
    // SAFETY: `batch` is live for the test's duration and `view` is a local.
    let status = unsafe { reldex_batch_column(batch, column, std::ptr::from_mut(&mut view)) };
    assert_eq!(status, ReldexStatus::Ok);
    view
}

fn column_info(batch: *const reldex_ffi::ReldexBatch, column: usize) -> (String, i32) {
    let mut info = ReldexColumnInfo::default();
    // SAFETY: as `column_view`.
    let status = unsafe { reldex_batch_column_info(batch, column, std::ptr::from_mut(&mut info)) };
    assert_eq!(status, ReldexStatus::Ok);
    // SAFETY: the name borrows from the live batch.
    let name = unsafe { info.name.as_str() }.unwrap_or_default().to_owned();
    (name, info.kind)
}

#[test]
fn a_fetched_batch_reads_back_exactly_what_the_mock_generated() {
    let spec = GeneratedQuerySpec::s14_shape(ROWS, SEED);
    let harness = Harness::new();
    let session = harness.open(config());

    assert_eq!(
        harness.execute(session, 10, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let executed = harness.next_event();
    assert_eq!(executed.kind, ReldexEventKind::Executed as i32);
    assert!(executed.error.is_null(), "the generated query must run");
    assert!(executed.has_result);
    assert_eq!(executed.column_count, 3);
    let result = executed.result;

    let mut fetched_rows = 0_u64;
    let mut batches = 0_u32;
    loop {
        assert_eq!(
            harness.fetch(session, 20 + u64::from(batches), result, 100),
            ReldexStatus::Ok
        );
        let event = harness.next_event();
        assert_eq!(event.kind, ReldexEventKind::Fetched as i32);
        assert!(event.error.is_null(), "a fetch of the mock cannot fail");
        let batch = OwnedBatch(event.batch);
        assert!(!batch.0.is_null());
        // SAFETY: the batch is live until `OwnedBatch` drops.
        let rows = unsafe { reldex_batch_row_count(batch.0) };
        assert_eq!(rows, event.row_count, "the event mirrors the batch's rows");
        if rows == 0 {
            break;
        }
        // SAFETY: as above.
        assert_eq!(unsafe { reldex_batch_column_count(batch.0) }, 3);
        assert_eq!(column_info(batch.0, 0).0, "ID");
        assert_eq!(column_info(batch.0, 1).0, "NAME");
        assert_eq!(column_info(batch.0, 2).0, "CREATED");
        assert_eq!(column_info(batch.0, 0).1, ReldexColumnKind::Number as i32);
        assert_eq!(column_info(batch.0, 1).1, ReldexColumnKind::Text as i32);
        assert_eq!(
            column_info(batch.0, 2).1,
            ReldexColumnKind::Timestamp as i32
        );

        let ids = column_view(batch.0, 0);
        let names = column_view(batch.0, 1);
        let created = column_view(batch.0, 2);

        for row in 0..rows {
            let absolute = fetched_rows + row as u64;
            // SAFETY: every view describes the live batch and `row < rows`.
            unsafe {
                assert!(!is_null(&ids, row), "the id column has no NULLs");
                assert_eq!(number_as_u64(&number_cell(&ids, row)), absolute + 1);

                match spec.expected_cell(absolute, 1).expect("row exists") {
                    ScriptValue::Null => assert!(
                        is_null(&names, row),
                        "row {absolute} of NAME must read back as NULL"
                    ),
                    ScriptValue::Text(expected) => {
                        assert!(!is_null(&names, row));
                        assert_eq!(text_cell(&names, row), expected, "row {absolute}");
                    }
                    other => panic!("unexpected NAME cell {other:?}"),
                }

                let ScriptValue::Timestamp(expected) =
                    spec.expected_cell(absolute, 2).expect("row exists")
                else {
                    panic!("CREATED must be a timestamp");
                };
                let cell = timestamp_cell(&created, row);
                assert_eq!(
                    (cell.year, cell.month, cell.day),
                    (expected.year(), expected.month(), expected.day()),
                    "row {absolute}"
                );
                assert!(!cell.has_zone, "a DATE carries no zone");
            }
        }
        fetched_rows += rows as u64;
        batches += 1;
    }
    assert_eq!(fetched_rows, ROWS);
    assert!(batches >= 3, "the result must arrive in several batches");

    assert_eq!(harness.close_result(session, 90, result), ReldexStatus::Ok);
    let closed = harness.next_event();
    assert_eq!(closed.kind, ReldexEventKind::ResultClosed as i32);
    assert!(closed.error.is_null());

    assert_eq!(
        harness.close(session, 91, reldex_ffi::ReldexCloseDisposition::None),
        ReldexStatus::Ok
    );
    let session_closed = harness.next_event();
    assert_eq!(session_closed.kind, ReldexEventKind::SessionClosed as i32);
    assert_eq!(
        session_closed.close_outcome,
        reldex_ffi::ReldexCloseOutcome::Closed as i32
    );
    assert!(!session_closed.session_still_open);
}

#[test]
fn the_bulk_formatter_renders_thai_emoji_and_nulls_the_way_the_caller_asked() {
    let spec = GeneratedQuerySpec::s14_shape(ROWS, SEED);
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.execute(session, 1, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let executed = harness.next_event();
    let result = executed.result;

    // One fetch covering the interesting cadence: row 10 is Thai, row 25 is a
    // non-BMP emoji, row 100 is NULL (0-based: 9, 24, 99).
    assert_eq!(harness.fetch(session, 2, result, 120), ReldexStatus::Ok);
    let event = harness.next_event();
    let batch = OwnedBatch(event.batch);
    // SAFETY: the batch is live.
    let rows = unsafe { reldex_batch_row_count(batch.0) };
    assert_eq!(rows, 120);

    let arena = reldex_text_arena_create();
    assert!(!arena.is_null());
    let null_text = "(null)";
    let options = ReldexFormatOptions {
        null_text: ReldexStr {
            ptr: null_text.as_ptr(),
            len: null_text.len(),
        },
        grouping_separator: u32::from(','),
        timestamp_style: ReldexTimestampStyle::DateOnly as i32,
        ..ReldexFormatOptions::default()
    };

    // The NAME column: Thai, a non-BMP emoji and a NULL all in one window.
    // SAFETY: the batch, the arena and the options (and the string inside
    // them) are all live for the call.
    let status = unsafe {
        reldex_batch_format_column(batch.0, 1, 0, rows, std::ptr::from_ref(&options), arena)
    };
    assert_eq!(status, ReldexStatus::Ok);
    // SAFETY: the arena is live.
    assert_eq!(unsafe { reldex_text_arena_count(arena) }, rows);
    let formatted = read_arena(arena);
    for (row, text) in formatted.iter().enumerate() {
        let expected = match spec.expected_cell(row as u64, 1).expect("row exists") {
            ScriptValue::Null => null_text.to_owned(),
            ScriptValue::Text(value) => value,
            other => panic!("unexpected cell {other:?}"),
        };
        assert_eq!(text, &expected, "row {row}");
    }
    assert!(
        formatted[9].contains('ข') || formatted[9].contains('แ') || formatted[9].contains('ท'),
        "row 10 is Thai: {}",
        formatted[9]
    );
    assert!(
        formatted[24].chars().any(|ch| ch as u32 > 0xFFFF),
        "row 25 carries a non-BMP glyph: {}",
        formatted[24]
    );
    assert_eq!(formatted[99], null_text, "row 100 is NULL");

    // The ID column, with the caller's grouping separator applied.
    // SAFETY: as above.
    unsafe {
        reldex_text_arena_clear(arena);
        let status =
            reldex_batch_format_column(batch.0, 0, 0, rows, std::ptr::from_ref(&options), arena);
        assert_eq!(status, ReldexStatus::Ok);
    }
    let ids = read_arena(arena);
    assert_eq!(ids[0], "1");
    assert_eq!(ids[99], "100");
    assert_eq!(ids[119], "120");

    // The CREATED column, in the style the caller asked for.
    // SAFETY: as above.
    unsafe {
        reldex_text_arena_clear(arena);
        let status =
            reldex_batch_format_column(batch.0, 2, 0, 3, std::ptr::from_ref(&options), arena);
        assert_eq!(status, ReldexStatus::Ok);
    }
    let dates = read_arena(arena);
    assert_eq!(dates.len(), 3);
    assert_eq!(dates[0], "2026-01-02", "one day per row from 2026-01-01");
    assert_eq!(dates[1], "2026-01-03");

    // A window past the end formats what exists and stops.
    // SAFETY: as above.
    unsafe {
        reldex_text_arena_clear(arena);
        let status = reldex_batch_format_column(
            batch.0,
            0,
            rows - 2,
            50,
            std::ptr::from_ref(&options),
            arena,
        );
        assert_eq!(status, ReldexStatus::Ok);
        assert_eq!(reldex_text_arena_count(arena), 2);
    }

    // An unknown column is reported, not guessed at.
    // SAFETY: as above.
    let status = unsafe {
        reldex_batch_format_column(batch.0, 9, 0, 1, std::ptr::from_ref(&options), arena)
    };
    assert_eq!(status, ReldexStatus::NotFound);

    // SAFETY: the arena is live and released exactly once.
    unsafe { reldex_text_arena_release(arena) };
}

fn read_arena(arena: *const reldex_ffi::ReldexTextArena) -> Vec<String> {
    let mut view = ReldexArenaView::default();
    // SAFETY: the arena is live and `view` is a local.
    let status = unsafe { reldex_text_arena_view(arena, std::ptr::from_mut(&mut view)) };
    assert_eq!(status, ReldexStatus::Ok);
    if view.count == 0 {
        return Vec::new();
    }
    // SAFETY: the view promises `count + 1` offsets and `data_len` bytes,
    // borrowed from the live arena.
    let (offsets, data) = unsafe {
        (
            std::slice::from_raw_parts(view.offsets, view.count + 1),
            std::slice::from_raw_parts(view.data, view.data_len),
        )
    };
    (0..view.count)
        .map(|index| {
            std::str::from_utf8(&data[offsets[index]..offsets[index + 1]])
                .expect("the arena is UTF-8")
                .to_owned()
        })
        .collect()
}

#[test]
fn the_scenario_statements_are_nul_terminated_for_c() {
    for kind in [
        ReldexMockStatement::GeneratedQuery,
        ReldexMockStatement::Block,
        ReldexMockStatement::Failing,
        ReldexMockStatement::Panicking,
    ] {
        let text = reldex_mock_statement(kind as i32);
        assert!(text.len > 0);
        // SAFETY: the string is `'static` and this library produced it.
        let bytes = unsafe { std::slice::from_raw_parts(text.ptr, text.len + 1) };
        assert_eq!(bytes[text.len], 0, "C needs the terminator");
    }
    let unknown = reldex_mock_statement(0);
    assert_eq!(unknown.len, 0);
}
