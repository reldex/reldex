//! Evidence for ADR-0004 (`docs/decisions/0004-result-store.md`, tasks M5.1
//! and M5.2): what a **retained** fetched prefix costs, per row, in three
//! representations, and what it costs to append a million rows to each.
//!
//! This is a measurement binary (`harness = false`), not a library test and
//! not production code. It prints CSV. The numbers ADR-0004 quotes for the
//! M5.1 prototype are in `docs/exec-plans/active/phase-1-m5-1-data/`; the
//! re-run against the implemented store (M5.2) is in
//! `docs/exec-plans/active/phase-1-m5-2-data/`.
//!
//! ```text
//! cargo bench -p reldex-db-core --bench result_store_shapes -- //!     --csv "$PWD/representation.csv" --mock-csv "$PWD/mock-path.csv"
//! ```
//!
//! Cargo runs a bench from its package directory (`crates/db-core`), so pass
//! absolute paths. `RELDEX_M5_1_REPEATS` (default 3) sets the repeat count.
//!
//! Without the `--bench` flag cargo passes under `cargo bench`, it does
//! nothing and exits, so `cargo test --all-targets` stays cheap.
//!
//! # The three representations
//!
//! - **`rows`** — row-of-values: one `Box<[Value]>` per row, one owned
//!   `String` per text cell. The shape ADR-0002 D6 rejected for batches.
//! - **`batches`** — today's retention: every fetched `RowBatch` kept exactly
//!   as the driver built it (ADR-0003 D4's MVP rule), capacity slack included.
//! - **`store`** — the ADR-0004 store as implemented (M5.2): each batch
//!   becomes one immutable [`ResultSegment`] through
//!   [`ResultSegment::compact`] — text and bytes shrunk to their exact size
//!   in the same buffer-plus-offsets layout, `NUMBER` re-encoded as `i64`
//!   with one decimal scale per segment column when every value in it fits
//!   exactly, otherwise kept as `Number` — counted by
//!   [`ResultSegment::accounted_bytes`] and read through
//!   [`ResultSegment::value`]. (M5.1 measured a prototype of the same
//!   layout, which this replaced.) Every compacted cell is checked against
//!   the batch it came from, `NUMBER` formatting included.
//!
//! # What "bytes" means here
//!
//! `accounted` is the sum of the heap capacities each representation holds
//! plus its fixed struct sizes — for `store`, exactly what the store counts
//! against its byte cap ([`reldex_db_core::ResultStore::retained_bytes`]:
//! every segment's accounted bytes plus its own index).
//! It does not include the allocator's per-allocation overhead. `os` is the
//! process's private bytes (working set on Linux) sampled before and after
//! building the representation in a **fresh child process**, which does
//! include it. Batches are generated in the shape the Oracle thin driver
//! builds them (`crates/drivers/oracle-thin/src/value.rs`: capacity
//! `min(fetch_rows, 4096)` rows, text buffers reserved at 16 bytes per row and
//! grown by doubling), so `batches` carries the same slack a real fetch does.

#![allow(
    clippy::print_stdout,
    reason = "a measurement binary: its output is the result"
)]

use std::env;
use std::fmt::Write as _;
use std::hint::black_box;
use std::mem::{size_of, size_of_val};
use std::num::NonZeroUsize;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reldex_db_core::{
    Cap, CapSource, CellValue, LobHandle, NumberValue, ResultCaps, ResultPolicy, ResultSegment,
    ResultStore, SessionManager, Sourced, Statement, StoreAction,
};
use reldex_db_driver_api::{
    Column, ColumnData, ConnectionParams, Credentials, Endpoint, LobLocator, NullMask, Number,
    RowBatch, TextColumn, Timestamp, Value, ValueRef,
};
use reldex_driver_mock::{Action, GeneratedColumn, GeneratedQuerySpec, MockDriver, Scenario};

// ---------------------------------------------------------------- shapes

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shape {
    /// 10 `NUMBER`: ids, integers, 2- and 4-decimal amounts, a sparse column
    /// and one column of 40-digit quotients (`n / 7`), which no `i64` holds.
    Numbers10,
    /// 5 `VARCHAR2(100 CHAR)` (20–100 characters, every 10th row Thai, 2%
    /// NULL) and 2 `DATE`.
    Text5Date2,
    /// 1 CLOB-like column fetched **inline** as 32 KiB of text per row — what
    /// an extended `VARCHAR2(32767)` or a CLOB converted to text costs.
    ClobInline,
    /// Spike S14/S15's shape: `NUMBER`, `VARCHAR2(40)`, `DATE`.
    S14,
}

impl Shape {
    const ALL: [Self; 4] = [
        Self::Numbers10,
        Self::Text5Date2,
        Self::ClobInline,
        Self::S14,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Numbers10 => "numbers10",
            Self::Text5Date2 => "text5date2",
            Self::ClobInline => "clob32k_inline",
            Self::S14 => "s14",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|shape| shape.name() == text)
    }
}

// ------------------------------------------------------ value generators

fn from_scaled(mantissa: i64, scale: i16) -> Number {
    if mantissa == 0 {
        return Number::ZERO;
    }
    let mut rest = mantissa.unsigned_abs();
    let mut digits = [0_u8; 20];
    let mut len = 0;
    while rest > 0 {
        digits[len] = (rest % 10) as u8;
        rest /= 10;
        len += 1;
    }
    digits[..len].reverse();
    Number::from_digits(mantissa < 0, &digits[..len], len as i16 - scale).expect("in range")
}

/// `numerator / 7` to 40 significant digits: a value like the ones Oracle
/// division produces, which no `i64` can hold exactly.
fn quotient_40(numerator: u64) -> Number {
    let mut digits = Vec::with_capacity(48);
    let whole = numerator / 7;
    let integer_digits = if whole == 0 {
        0
    } else {
        let text = whole.to_string();
        digits.extend(text.bytes().map(|b| b - b'0'));
        text.len()
    };
    let mut remainder = numerator % 7;
    let mut significant = digits.len();
    while significant < 40 {
        remainder *= 10;
        let digit = (remainder / 7) as u8;
        remainder %= 7;
        digits.push(digit);
        if significant > 0 || digit != 0 {
            significant += 1;
        }
    }
    Number::from_digits(false, &digits, integer_digits as i16).expect("40 significant digits")
}

fn number_cell(column: usize, row: u64) -> Option<Number> {
    let r = row as i64;
    Some(match column {
        0 => Number::from(r),
        1 => Number::from((r * 7_919) % 100_000),
        2 => from_scaled((r * 104_729) % 100_000_000, 2),
        3 => Number::from(r % 1_000 + 1),
        4 => Number::from(-((r * 13) % 50_000)),
        5 => from_scaled((r * 7) % 10_000_000, 4),
        6 => Number::from(1_000_000_000_000 + r),
        7 => Number::from(r % 2),
        8 if row % 20 == 0 => return None,
        8 => Number::from(r % 365),
        _ => quotient_40(row),
    })
}

const ASCII_100: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789\
                         abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN";
/// 100 Thai characters, every one of them three bytes in UTF-8.
fn thai_100() -> String {
    "ฐานข้อมูลทดสอบแถว".chars().cycle().take(100).collect()
}

fn text_cell<'a>(
    column: usize,
    row: u64,
    thai: &'a str,
    scratch: &'a mut String,
) -> Option<&'a str> {
    if (row + column as u64 * 11) % 50 == 0 {
        return None;
    }
    let chars = 20 + ((row * 7 + column as u64 * 13) % 81) as usize;
    if (row + column as u64) % 10 == 0 {
        return Some(&thai[..chars * 3]);
    }
    scratch.clear();
    let _ = write!(scratch, "r{row}c{column}-");
    let pad = chars.saturating_sub(scratch.len());
    scratch.push_str(&ASCII_100[..pad]);
    Some(scratch)
}

fn date_cell(column: usize, row: u64) -> Option<Timestamp> {
    if column == 1 && row % 30 == 0 {
        return None;
    }
    Some(
        Timestamp::from_source(
            (2000 + row % 25) as i16,
            (1 + row % 12) as u8,
            (1 + row % 28) as u8,
            (row % 24) as u8,
            (row % 60) as u8,
            ((row / 7) % 60) as u8,
        )
        .expect("valid date"),
    )
}

fn s14_name(row: u64, scratch: &mut String) -> Option<&str> {
    if row % 100 == 0 {
        return None;
    }
    scratch.clear();
    if row % 25 == 0 {
        let _ = write!(scratch, "row {row} 🚀");
    } else if row % 10 == 0 {
        let _ = write!(scratch, "ข้อมูล {row}");
    } else {
        let _ = write!(scratch, "row {row}");
    }
    let chars = scratch.chars().count();
    scratch.extend(std::iter::repeat_n('.', 40_usize.saturating_sub(chars)));
    Some(scratch)
}

// ------------------------------------------ driver-shaped batch building

/// A text column built the way the Oracle thin driver builds one, with its
/// real heap capacity tracked alongside (`TextColumn` does not expose it).
struct TextBuilder {
    column: TextColumn,
    buffer_capacity: usize,
    offsets_capacity: usize,
}

impl TextBuilder {
    fn new(rows_capacity: usize) -> Self {
        Self {
            column: TextColumn::with_capacity(rows_capacity, rows_capacity * 16),
            buffer_capacity: rows_capacity * 16,
            offsets_capacity: rows_capacity + 1,
        }
    }

    fn push(&mut self, text: &str) {
        let needed = self.column.buffer().len() + text.len();
        if needed > self.buffer_capacity {
            // `RawVec::grow_amortized`: max(2 × capacity, required, 8).
            self.buffer_capacity = (self.buffer_capacity * 2).max(needed).max(8);
        }
        self.column.push(text);
    }

    fn heap_bytes(&self) -> usize {
        self.buffer_capacity + self.offsets_capacity * size_of::<usize>()
    }
}

fn mask(rows: usize, nulls: &[usize]) -> NullMask {
    let mut mask = NullMask::new(rows);
    for row in nulls {
        mask.set_null(*row).expect("row in range");
    }
    mask
}

/// One driver-shaped batch of `rows` rows starting at 1-based `first_row`,
/// and its accounted heap bytes as the driver left them.
fn driver_batch(shape: Shape, first_row: u64, rows: usize, fetch_rows: usize) -> (RowBatch, usize) {
    let capacity = fetch_rows.min(4096);
    let mut columns = Vec::new();
    let mut heap = 0_usize;
    let mut scratch = String::new();
    let thai = thai_100();

    let number_column = |cell: &dyn Fn(u64) -> Option<Number>, heap: &mut usize| {
        let mut values = Vec::with_capacity(capacity);
        let mut nulls = Vec::new();
        for i in 0..rows {
            match cell(first_row + i as u64) {
                Some(value) => values.push(value),
                None => {
                    values.push(Number::ZERO);
                    nulls.push(i);
                }
            }
        }
        *heap += values.capacity() * size_of::<Number>();
        let nulls = mask(rows, &nulls);
        *heap += size_of_val(nulls.words());
        Column::new(ColumnData::Number(values), nulls).expect("aligned")
    };

    match shape {
        Shape::Numbers10 => {
            for column in 0..10 {
                columns.push(number_column(&|row| number_cell(column, row), &mut heap));
            }
        }
        Shape::S14 => {
            columns.push(number_column(
                &|row| Some(Number::from(row as i64)),
                &mut heap,
            ));
        }
        Shape::Text5Date2 | Shape::ClobInline => {}
    }

    let text_columns = match shape {
        Shape::Text5Date2 => 5,
        Shape::ClobInline | Shape::S14 => 1,
        Shape::Numbers10 => 0,
    };
    let clob = "x".repeat(32 * 1024);
    for column in 0..text_columns {
        let mut builder = TextBuilder::new(capacity);
        let mut nulls = Vec::new();
        for i in 0..rows {
            let row = first_row + i as u64;
            let cell = match shape {
                Shape::Text5Date2 => text_cell(column, row, &thai, &mut scratch),
                Shape::S14 => s14_name(row, &mut scratch),
                _ => Some(clob.as_str()),
            };
            match cell {
                Some(text) => builder.push(text),
                None => {
                    builder.push("");
                    nulls.push(i);
                }
            }
        }
        heap += builder.heap_bytes();
        let nulls = mask(rows, &nulls);
        heap += size_of_val(nulls.words());
        columns.push(Column::new(ColumnData::Text(builder.column), nulls).expect("aligned"));
    }

    let date_columns = match shape {
        Shape::Text5Date2 => 2,
        Shape::S14 => 1,
        _ => 0,
    };
    for column in 0..date_columns {
        let mut values = Vec::with_capacity(capacity);
        let mut nulls = Vec::new();
        for i in 0..rows {
            let row = first_row + i as u64;
            let cell = if shape == Shape::S14 {
                date_cell(0, row)
            } else {
                date_cell(column, row)
            };
            match cell {
                Some(value) => values.push(value),
                None => {
                    values.push(Timestamp::from_source(1, 1, 1, 0, 0, 0).expect("valid"));
                    nulls.push(i);
                }
            }
        }
        heap += values.capacity() * size_of::<Timestamp>();
        let nulls = mask(rows, &nulls);
        heap += size_of_val(nulls.words());
        columns.push(Column::new(ColumnData::Timestamp(values), nulls).expect("aligned"));
    }

    heap += columns.capacity() * size_of::<Column>() + size_of::<RowBatch>();
    // What `db-core` wraps every batch in on the way out of the worker
    // (`FetchedBatch`: the batch plus an empty LOB-handle vector).
    heap += size_of::<Vec<(usize, usize, LobHandle)>>();
    (RowBatch::new(columns).expect("aligned"), heap)
}

// ------------------------------------------------ the store (ADR-0004)

/// Proves compaction lost nothing, cell by cell, against a fresh copy of the
/// batch it compacted — and that a `NUMBER` held as a scaled `i64` formats
/// byte-identically to the `Number` it came from. Not timed.
fn verify(batch: &RowBatch, segment: &ResultSegment) {
    let mut scratch = String::new();
    let mut original = String::new();
    for (index, column) in batch.columns().iter().enumerate() {
        for row in 0..batch.row_count() {
            let stored = segment.value(row, index).expect("in range");
            match (column.value(row).expect("in range"), stored) {
                (ValueRef::Null, CellValue::Null) => {}
                (ValueRef::Number(expected), CellValue::Number(value)) => {
                    assert_eq!(&value.to_number(), expected, "row {row}: exact");
                    scratch.clear();
                    original.clear();
                    write!(scratch, "{value}").expect("formatting");
                    write!(original, "{expected}").expect("formatting");
                    assert_eq!(scratch, original, "row {row}: formatted identically");
                }
                (ValueRef::Text(expected), CellValue::Text(value)) => {
                    assert_eq!(value, expected, "row {row}");
                }
                (ValueRef::Timestamp(expected), CellValue::Timestamp(value)) => {
                    assert_eq!(value, expected, "row {row}");
                }
                (expected, value) => panic!("row {row}: {value:?} for {expected:?}"),
            }
        }
    }
}

// ----------------------------------------------------- row-of-values

fn to_rows(batch: &RowBatch, out: &mut Vec<Box<[Value]>>, heap: &mut usize) {
    for row in 0..batch.row_count() {
        let cells: Box<[Value]> = batch
            .columns()
            .iter()
            .map(|column| match column.value(row).expect("in range") {
                ValueRef::Null => Value::Null,
                ValueRef::Number(value) => Value::Number(*value),
                ValueRef::Text(text) => {
                    *heap += text.len();
                    Value::Text(text.to_owned())
                }
                ValueRef::Timestamp(value) => Value::Timestamp(value),
                _ => unreachable!("the bench generates no other kind"),
            })
            .collect();
        *heap += cells.len() * size_of::<Value>();
        out.push(cells);
    }
}

// --------------------------------------------------------- measurement

/// This process's private bytes (Windows) or resident set (elsewhere).
fn os_bytes() -> Option<u64> {
    if cfg!(windows) {
        let script = format!(
            "(Get-Process -Id {}).PrivateMemorySize64",
            std::process::id()
        );
        let out = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    } else {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    }
}

struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

const RANDOM_READS: usize = 1_000_000;

/// One child-process measurement: build `representation` for `rows` rows of
/// `shape`, print one CSV line.
fn child(representation: &str, shape: Shape, rows: usize, fetch_rows: usize) {
    // Warm the allocator and the generator before the baseline.
    drop(black_box(driver_batch(
        shape,
        1,
        fetch_rows.min(rows),
        fetch_rows,
    )));
    let before = os_bytes();

    let mut accounted = 0_usize;
    let mut append = Duration::ZERO;
    let mut batches: Vec<RowBatch> = Vec::new();
    // What a `ResultStore` holds: its segments and each one's first row.
    let mut segments: Vec<Arc<ResultSegment>> = Vec::new();
    let mut starts: Vec<usize> = Vec::new();
    let mut row_values: Vec<Box<[Value]>> = Vec::new();
    let mut first = 1_u64;
    let mut left = rows;
    while left > 0 {
        let take = left.min(fetch_rows);
        let (batch, driver_heap) = driver_batch(shape, first, take, fetch_rows);
        match representation {
            "batches" => {
                let start = Instant::now();
                batches.push(batch);
                append += start.elapsed();
                accounted += driver_heap;
            }
            "store" => {
                let start = Instant::now();
                let segment = Arc::new(ResultSegment::compact(batch).expect("no LOB columns"));
                append += start.elapsed();
                accounted += segment.accounted_bytes();
                starts.push(rows - left);
                segments.push(segment);
            }
            _ => {
                let start = Instant::now();
                to_rows(&batch, &mut row_values, &mut accounted);
                append += start.elapsed();
            }
        }
        first += take as u64;
        left -= take;
    }
    accounted += batches.capacity() * size_of::<RowBatch>()
        + segments.capacity() * size_of::<Arc<ResultSegment>>()
        + starts.capacity() * size_of::<usize>()
        + row_values.capacity() * size_of::<Box<[Value]>>();
    let after = os_bytes();

    // Random access: `RANDOM_READS` uniformly random cells.
    let columns = match shape {
        Shape::Numbers10 => 10,
        Shape::Text5Date2 => 7,
        Shape::ClobInline => 1,
        Shape::S14 => 3,
    };
    let mut rng = Xorshift(0x9E37_79B9_7F4A_7C15);
    let mut sink = 0_u64;
    let start = Instant::now();
    for _ in 0..RANDOM_READS {
        let row = (rng.next() % rows as u64) as usize;
        let column = (rng.next() % columns as u64) as usize;
        sink = sink.wrapping_add(read_cell(
            representation,
            &batches,
            &segments,
            &row_values,
            fetch_rows,
            row,
            column,
        ));
    }
    let random_ns = start.elapsed().as_nanos() as f64 / RANDOM_READS as f64;
    black_box(sink);

    let os_delta = match (before, after) {
        (Some(before), Some(after)) => after.saturating_sub(before).to_string(),
        _ => String::new(),
    };
    println!(
        "{},{},{rows},{fetch_rows},{accounted},{:.1},{os_delta},{:.3},{random_ns:.1}",
        shape.name(),
        representation,
        accounted as f64 / rows as f64,
        append.as_secs_f64() * 1e3,
    );

    // Every batch moved into its segment, so check each segment against a
    // fresh copy, after everything above was measured: regenerating and
    // reading every cell while building disturbed the timings (the random
    // reads of 32 KiB rows ran 15x slower). A mismatch fails the child, and
    // with it the run.
    for (segment, start) in segments.iter().zip(&starts) {
        let (check, _) = driver_batch(shape, 1 + *start as u64, segment.row_count(), fetch_rows);
        verify(&check, segment);
    }
}

fn read_cell(
    representation: &str,
    batches: &[RowBatch],
    segments: &[Arc<ResultSegment>],
    rows: &[Box<[Value]>],
    fetch_rows: usize,
    row: usize,
    column: usize,
) -> u64 {
    // Every batch but the last holds exactly `fetch_rows` rows, so the
    // segment is a division, not a search.
    let (index, local) = (row / fetch_rows, row % fetch_rows);
    match representation {
        "batches" => match batches[index].value(local, column) {
            Some(ValueRef::Number(value)) => value.digit_count() as u64,
            Some(ValueRef::Text(text)) => text.len() as u64,
            Some(ValueRef::Timestamp(value)) => u64::from(value.day()),
            _ => 0,
        },
        "store" => match segments[index].value(local, column) {
            Some(CellValue::Number(NumberValue::Scaled(value))) => value.mantissa() as u64,
            Some(CellValue::Number(NumberValue::Decimal(value))) => value.digit_count() as u64,
            Some(CellValue::Text(text)) => text.len() as u64,
            Some(CellValue::Timestamp(value)) => u64::from(value.day()),
            _ => 0,
        },
        _ => match &rows[row][column] {
            Value::Number(value) => value.digit_count() as u64,
            Value::Text(text) => text.len() as u64,
            Value::Timestamp(value) => u64::from(value.day()),
            _ => 0,
        },
    }
}

// ------------------------------------------- through db-core and the mock

/// Fetches `rows` rows of a generated result through a real `db-core`
/// session and its worker thread, and either retains each `FetchedBatch`
/// (`batches`) or feeds a real [`ResultStore`], whose segments are compacted
/// on the worker (`store`, ADR-0004 RS1). Returns one CSV line.
///
/// The store runs with one fetch in flight, like the `batches` loop, and its
/// round-trip byte budget lifted so every request asks for `fetch_rows` rows
/// as the `batches` loop does: the comparison is of retention, not of
/// request sizing. Compaction happens inside each fetch's latency, on the
/// worker, so the `compaction_ms` column is left empty for `store`.
fn mock_path(
    name: &str,
    columns: Vec<GeneratedColumn>,
    rows: u64,
    fetch_rows: usize,
    compacting: bool,
) -> String {
    let scenario = Scenario::new();
    let spec = GeneratedQuerySpec::new(rows, columns, 0).expect("columns");
    scenario.on_sql("SELECT generated", Action::GeneratedQuery(spec));
    let driver = Arc::new(MockDriver::new(scenario));
    let params = ConnectionParams::new(
        Endpoint::ConnectString("mock".to_owned()),
        Credentials::External,
    );
    let session = SessionManager::new()
        .open_session(driver, params)
        .expect("mock session");
    let max = NonZeroUsize::new(fetch_rows).expect("non-zero");

    let start = Instant::now();
    let outcome = session
        .execute(Statement::new("SELECT generated").with_fetch_rows(max))
        .wait()
        .expect("execute");
    let result = outcome.result.expect("a result");
    let mut fetch_latencies = Vec::new();
    let mut accounted = 0_usize;
    let mut retained = Vec::new();
    let mut store = None;
    if compacting {
        let unlimited = Sourced::new(Cap::Unlimited, CapSource::BuiltIn);
        let policy = ResultPolicy::new(ResultCaps::new(unlimited, unlimited))
            .with_fetch_rows(max)
            .with_fetches_in_flight(NonZeroUsize::MIN)
            .with_round_trip_bytes(NonZeroUsize::MAX);
        let mut results = ResultStore::new(result, &outcome.columns, policy);
        results.fetch_all();
        loop {
            let mut answered = Vec::new();
            results
                .pump(|action| {
                    if let StoreAction::Fetch(fetch) = action {
                        let asked = Instant::now();
                        let reply = session.fetch_segment(fetch).wait();
                        fetch_latencies.push(asked.elapsed());
                        answered.push((fetch.ticket(), reply));
                    }
                    Ok(())
                })
                .expect("submitted");
            if answered.is_empty() {
                break;
            }
            for (ticket, reply) in answered {
                results.on_fetched(ticket, reply);
            }
        }
        assert_eq!(results.row_count() as u64, rows, "every row retained");
        accounted = results.retained_bytes();
        store = Some(results);
    } else {
        loop {
            let asked = Instant::now();
            let batch = session.fetch_batch(result, max).wait().expect("fetch");
            fetch_latencies.push(asked.elapsed());
            if batch.is_empty() {
                break;
            }
            retained.push(batch);
        }
    }
    let total = start.elapsed();
    fetch_latencies.sort_unstable();
    let percentile = |p: f64| {
        let index = ((fetch_latencies.len() - 1) as f64 * p).round() as usize;
        fetch_latencies[index].as_secs_f64() * 1e6
    };
    let line = format!(
        "{name},{},{rows},{fetch_rows},{:.1},{:.1},{:.1},{:.1},{},{}",
        if compacting { "store" } else { "batches" },
        total.as_secs_f64() * 1e3,
        percentile(0.5),
        percentile(0.99),
        percentile(1.0),
        if compacting { "" } else { "0.0" },
        accounted,
    );
    black_box((retained, store));
    drop(session);
    line
}

// ------------------------------------------------------------- driver

fn main() {
    let args: Vec<String> = env::args().collect();
    if let Some(position) = args.iter().position(|arg| arg == "--child") {
        let representation = args[position + 1].as_str();
        let shape = Shape::parse(&args[position + 2]).expect("shape");
        let rows = args[position + 3].parse().expect("rows");
        let fetch_rows = args[position + 4].parse().expect("fetch rows");
        child(representation, shape, rows, fetch_rows);
        return;
    }
    if !args.iter().any(|arg| arg == "--bench") {
        // `cargo test --all-targets` builds and runs this with no `--bench`.
        return;
    }
    let option = |name: &str| {
        args.iter()
            .position(|arg| arg == name)
            .and_then(|position| args.get(position + 1).cloned())
    };
    let csv = option("--csv");
    let mock_csv = option("--mock-csv");
    let repeats: usize = env::var("RELDEX_M5_1_REPEATS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3);

    println!(
        "sizes: Number {} B, Timestamp {} B, Value {} B, usize offset {} B, LobHandle {} B, \
         Option<LobLocator> {} B, (usize, usize, LobHandle) {} B",
        size_of::<Number>(),
        size_of::<Timestamp>(),
        size_of::<Value>(),
        size_of::<usize>(),
        size_of::<LobHandle>(),
        size_of::<Option<LobLocator>>(),
        size_of::<(usize, usize, LobHandle)>(),
    );

    let exe = env::current_exe().expect("own path");
    let mut lines = vec![
        "section,shape,representation,rows,fetch_rows,accounted_bytes,accounted_bytes_per_row,\
         os_private_delta_bytes,append_ms,random_read_ns,repeat"
            .to_owned(),
    ];
    let mut run =
        |representation: &str, shape: Shape, rows: usize, fetch_rows: usize, section: &str| {
            for repeat in 0..repeats {
                let out = Command::new(&exe)
                    .args([
                        "--child",
                        representation,
                        shape.name(),
                        &rows.to_string(),
                        &fetch_rows.to_string(),
                    ])
                    .output()
                    .expect("child ran");
                let line = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                assert!(out.status.success(), "child failed: {line}");
                println!("{section},{line},{repeat}");
                lines.push(format!("{section},{line},{repeat}"));
            }
        };

    for shape in Shape::ALL {
        let row_counts: &[usize] = if shape == Shape::ClobInline {
            // 1,000,000 rows of 32 KiB is 32 GiB in any representation.
            &[1_000, 10_000, 100_000]
        } else {
            &[1_000, 100_000, 1_000_000]
        };
        for &rows in row_counts {
            for representation in ["rows", "batches", "store"] {
                run(representation, shape, rows, 1_000, "shape");
            }
        }
    }
    for shape in [Shape::S14, Shape::Text5Date2] {
        for fetch_rows in [100, 10_000] {
            for representation in ["batches", "store"] {
                run(representation, shape, 100_000, fetch_rows, "fetch_size");
            }
        }
    }

    let mut mock_lines = vec![
        "shape,representation,rows,fetch_rows,total_ms,fetch_p50_us,fetch_p99_us,fetch_max_us,\
         compaction_ms,accounted_bytes,repeat"
            .to_owned(),
    ];
    let s14 = || {
        vec![
            GeneratedColumn::Id("ID".to_owned()),
            GeneratedColumn::Name("NAME".to_owned()),
            GeneratedColumn::Created("CREATED".to_owned()),
        ]
    };
    let ids = || {
        (0..10)
            .map(|i| GeneratedColumn::Id(format!("N{i}")))
            .collect::<Vec<_>>()
    };
    for repeat in 0..repeats {
        for compacting in [false, true] {
            for line in [
                mock_path("s14", s14(), 1_000_000, 1_000, compacting),
                mock_path("ids10", ids(), 1_000_000, 1_000, compacting),
            ] {
                println!("mock_path,{line},{repeat}");
                mock_lines.push(format!("{line},{repeat}"));
            }
        }
    }

    for (path, lines) in [(csv, lines), (mock_csv, mock_lines)] {
        if let Some(path) = path {
            std::fs::write(&path, lines.join("\n") + "\n").expect("csv written");
            println!("wrote {path}");
        }
    }
}
