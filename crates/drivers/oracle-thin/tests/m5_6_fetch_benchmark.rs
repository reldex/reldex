//! M5.6 ★ — the fetch-batch benchmark: what one round trip costs against the
//! bytes it carries, across row shapes and links (`phase-1.md` §C.2 M5.6;
//! ADR-0004 RS2, "Bytes per round trip"). The report built from it is
//! `docs/exec-plans/active/phase-1-fetch-benchmark.md`.
//!
//! # Why here, and why an ignored test
//!
//! - **The driver crate, not `db-core`.** The cost under study is the wire
//!   path: `oracledb`'s response parsing (O(packets²) on a pre-23ai server,
//!   U-19) plus this driver's row-to-column conversion. Measuring it through
//!   `oracle-thin` directly keeps `db-core`'s worker, event queue and
//!   compaction out of the per-round-trip number (their cost is ADR-0004
//!   Table 2 and the M5.2 bench). `oracledb` is a dev-dependency here, which
//!   is what lets the `prefetch_rows` knob be measured at all: the driver
//!   pins it to 0 (`conn.rs`, U-3 containment) and exposes no way to change
//!   it.
//! - **A test, not an example.** `tools/oracle-test-db/run-it.sh` is the only
//!   sanctioned way to put the test database's credentials into the
//!   environment, and it runs `cargo test`.
//! - **Ignored.** It runs for tens of minutes, so the `db` stage of
//!   `tools/gates.sh` must never start it. It also does nothing unless
//!   `RELDEX_M56_OUT` names an output directory.
//!
//! # Method
//!
//! The parent test ([`m5_6_fetch_benchmark_matrix`]) runs every measurement
//! in a **fresh child process** (this same test binary, running
//! [`m5_6_fetch_benchmark_child`]), so that each run's CPU time and peak
//! memory are its own. For a latency-injected link the parent also hosts a
//! user-space relay (`m5_6_support/relay.rs`) in front of the listener, so the
//! relay's CPU is never charged to the client. The relay counts TNS packets
//! and bytes; the child prints a mark before and after its timed section and
//! waits for the parent to snapshot those counters while the connection is
//! idle, so the wire figures cover exactly the timed section.
//!
//! One run: connect; seven `ping`s (the link's measured round-trip time);
//! sample CPU and memory; `execute` the shape's statement with
//! `Statement::with_fetch_rows(n)` (the wire array size) and call
//! `fetch_batch(n)` until the statement's rows are all fetched or a time cap
//! passes (with at least five fetches), timing each call; sample again. Each
//! `fetch_batch(n)` is exactly one round trip, because the array size equals
//! the batch size. Every shape's values differ from row to row, because TTC
//! compresses a value equal to the previous row's and would hide the cost
//! (`phase-1-m5-1-data/README.md`). The statements generate rows from `dual`,
//! so nothing is created in the database and nothing is left behind.
//!
//! Runs are interleaved (run 1 of every size, then run 2, …) so that drift in
//! the machine's state spreads across sizes instead of biasing one. Before
//! each run the parent records which build tools are running (`tasklist`).
//!
//! # Running it
//!
//! ```text
//! RELDEX_M56_OUT=/abs/path bash tools/oracle-test-db/run-it.sh m5_6_fetch_benchmark \
//!     --release -- --ignored --nocapture --test-threads=1 m5_6_fetch_benchmark_matrix
//! ```
//!
//! Optional: `RELDEX_M56_LINKS` (default `direct,rtt0,rtt2,rtt10,rtt40`; a
//! link is `direct`, `rtt<ms>` or `rtt<ms>-<n>mbit`), `RELDEX_M56_SHAPES`
//! (default `numbers10,text5date2,wide4k`; also `clob`), `RELDEX_M56_RUNS`
//! (default 3), `RELDEX_M56_SIZES` (rows per round trip for every shape,
//! overriding each shape's list), `RELDEX_M56_CAP_MS` (default 10,000),
//! `RELDEX_M56_EXTRAS` (default `prefetch,sdu`; `none` for neither).
//!
//! Output, all CSV: `runs.csv` (one row per run), `cells.csv` (median and
//! coefficient of variation per link × shape × size), `fit.csv` (per-fetch
//! time against packets per round trip), `shapes.csv`, `groups.csv` (machine
//! state), `prefetch.csv`, `sdu.csv`.

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "benchmark progress and the parent/child protocol are read from standard output"
)]

mod common;
mod m5_6_support;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::{PASSWORD, USER, dsn_address, dsn_service, optional, params_at, setting, try_connect};
use m5_6_support::relay::{LinkModel, Relay, StatsSnapshot};
use m5_6_support::sample::{ProcessSample, cpu_load_percent, machine_state, sample_self};
use reldex_db_driver_api::{
    ColumnData, ColumnMetadata, DatabaseConnection, RowBatch, SqlType, Statement, ValueRef,
};

/// Output directory for the CSVs; the benchmark does nothing without it.
const OUT: &str = "RELDEX_M56_OUT";
/// The child's instructions (`key=value;…`).
const CELL: &str = "RELDEX_M56_CELL";
/// The connect string the child uses: the relay's, or the listener's.
const CELL_DSN: &str = "RELDEX_M56_DSN";
/// The child test's name, which the parent runs with `--exact`.
const CHILD_TEST: &str = "m5_6_fetch_benchmark_child";

/// A run stops at its time cap only after this many fetches.
const MIN_FETCHES: usize = 5;
/// `ping`s per run; the median is the link's round-trip time.
const PINGS: usize = 7;
/// Repetitions inside one prefetch child.
const PREFETCH_REPS: usize = 5;

/// One row shape: a statement whose every column value differs from the
/// previous row's, and the rows per round trip measured for it.
#[derive(Clone, Copy, Debug)]
struct Shape {
    /// Name used in the CSVs.
    name: &'static str,
    /// Rows the statement generates. A run fetches all of them unless its
    /// time cap stops it first.
    rows: usize,
    /// Rows per round trip (the wire array size) measured.
    sizes: &'static [usize],
    /// Rows per round trip for the warm-up child that parses the statement
    /// and measures the shape.
    warm_rows: usize,
    /// What the shape stands for.
    about: &'static str,
}

/// Every shape the benchmark knows.
const SHAPES: [Shape; 4] = [
    Shape {
        name: "numbers10",
        rows: 100_000,
        sizes: &[50, 100, 250, 500, 1_000, 2_000, 5_000, 10_000, 20_000],
        warm_rows: 1_000,
        about: "10 NUMBER columns (one of them LEVEL/7, 40 digits); ADR-0004's numbers10",
    },
    Shape {
        name: "text5date2",
        rows: 100_000,
        sizes: &[25, 50, 100, 250, 500, 1_000, 2_000, 5_000],
        warm_rows: 1_000,
        about: "5 VARCHAR2(100) of 20-100 chars + 2 DATE; ADR-0004's text5date2 (the mixed shape)",
    },
    Shape {
        name: "wide4k",
        rows: 2_000,
        sizes: &[1, 2, 5, 10, 25, 50, 100, 250],
        warm_rows: 10,
        about: "NUMBER + 4 VARCHAR2(4000) filled, distinct per row (~16 KB/row); Issue J's reproducer",
    },
    Shape {
        name: "clob",
        rows: 5_000,
        sizes: &[50, 100, 250, 500, 1_000, 2_000],
        warm_rows: 100,
        about: "NUMBER + one temporary CLOB of 20-200 chars, fetched as a locator (never inline)",
    },
];

impl Shape {
    /// Looks a shape up by name.
    fn named(name: &str) -> Self {
        SHAPES
            .iter()
            .copied()
            .find(|shape| shape.name == name)
            .unwrap_or_else(|| panic!("unknown shape {name:?}"))
    }

    /// The statement. `CONNECT BY` over `dual`: no object is created.
    fn sql(&self) -> String {
        let n = self.rows;
        match self.name {
            "numbers10" => format!(
                "SELECT LEVEL AS n0, MOD(LEVEL * 7919, 100000) AS n1, \
                 MOD(LEVEL * 104729, 100000000) / 100 AS n2, MOD(LEVEL, 1000) + 1 AS n3, \
                 -MOD(LEVEL * 13, 50000) AS n4, MOD(LEVEL * 7, 10000000) / 10000 AS n5, \
                 1000000000000 + LEVEL AS n6, MOD(LEVEL, 2) AS n7, \
                 CASE WHEN MOD(LEVEL, 20) = 0 THEN NULL ELSE MOD(LEVEL, 365) END AS n8, \
                 LEVEL / 7 AS n9 FROM dual CONNECT BY LEVEL <= {n}"
            ),
            "text5date2" => format!(
                "SELECT \
                 CAST(RPAD('r' || LEVEL || 'c0-', 20 + MOD(LEVEL * 7, 81), 'abcdefghij') \
                   AS VARCHAR2(100)) AS t0, \
                 CAST(RPAD('r' || LEVEL || 'c1-', 20 + MOD(LEVEL * 7 + 13, 81), 'klmnopqrst') \
                   AS VARCHAR2(100)) AS t1, \
                 CAST(RPAD('r' || LEVEL || 'c2-', 20 + MOD(LEVEL * 7 + 26, 81), 'uvwxyzABCD') \
                   AS VARCHAR2(100)) AS t2, \
                 CAST(RPAD('r' || LEVEL || 'c3-', 20 + MOD(LEVEL * 7 + 39, 81), 'EFGHIJKLMN') \
                   AS VARCHAR2(100)) AS t3, \
                 CAST(CASE WHEN MOD(LEVEL, 50) = 0 THEN NULL ELSE \
                   RPAD('r' || LEVEL || 'c4-', 20 + MOD(LEVEL * 7 + 52, 81), 'OPQRSTUVWX') END \
                   AS VARCHAR2(100)) AS t4, \
                 DATE '2000-01-01' + MOD(LEVEL, 9000) + MOD(LEVEL, 24) / 24 AS d0, \
                 CASE WHEN MOD(LEVEL, 30) = 0 THEN NULL \
                   ELSE DATE '2020-01-01' + MOD(LEVEL, 1000) END AS d1 \
                 FROM dual CONNECT BY LEVEL <= {n}"
            ),
            "wide4k" => format!(
                "SELECT LEVEL AS id, \
                 CAST(RPAD(TO_CHAR(LEVEL), 4000, CHR(65 + MOD(LEVEL, 26))) AS VARCHAR2(4000)) AS c1, \
                 CAST(RPAD(TO_CHAR(LEVEL), 4000, CHR(65 + MOD(LEVEL + 1, 26))) AS VARCHAR2(4000)) AS c2, \
                 CAST(RPAD(TO_CHAR(LEVEL), 4000, CHR(65 + MOD(LEVEL + 2, 26))) AS VARCHAR2(4000)) AS c3, \
                 CAST(RPAD(TO_CHAR(LEVEL), 4000, CHR(65 + MOD(LEVEL + 3, 26))) AS VARCHAR2(4000)) AS c4 \
                 FROM dual CONNECT BY LEVEL <= {n}"
            ),
            "clob" => format!(
                "SELECT LEVEL AS id, \
                 TO_CLOB(RPAD('r' || LEVEL || '-', 20 + MOD(LEVEL * 7, 181), 'lobtext')) AS doc \
                 FROM dual CONNECT BY LEVEL <= {n}"
            ),
            other => panic!("no statement for shape {other:?}"),
        }
    }
}

/// One link: straight to the listener, or through a relay.
#[derive(Clone, Debug)]
struct Link {
    name: String,
    model: Option<LinkModel>,
}

impl Link {
    /// `direct`, `rtt<ms>` or `rtt<ms>-<n>mbit`.
    fn parse(name: &str) -> Self {
        if name == "direct" {
            return Self {
                name: name.to_owned(),
                model: None,
            };
        }
        let rest = name.strip_prefix("rtt").unwrap_or_else(|| {
            panic!("a link is direct, rtt<ms> or rtt<ms>-<n>mbit, not {name:?}")
        });
        let (rtt, rate) = match rest.split_once('-') {
            Some((rtt, rate)) => (rtt, Some(rate)),
            None => (rest, None),
        };
        let rtt_ms: f64 = rtt
            .parse()
            .unwrap_or_else(|_| panic!("bad round-trip time in {name:?}"));
        let bits_per_second = rate.map(|rate| {
            rate.strip_suffix("mbit")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or_else(|| panic!("bad rate in {name:?}"))
                * 1_000_000
        });
        Self {
            name: name.to_owned(),
            model: Some(LinkModel {
                one_way: Duration::from_secs_f64(rtt_ms / 2_000.0),
                bits_per_second,
            }),
        }
    }

    /// The injected round-trip time, in milliseconds.
    fn injected_rtt_ms(&self) -> f64 {
        self.model
            .map_or(0.0, |model| model.one_way.as_secs_f64() * 2_000.0)
    }

    /// The bottleneck rate, in Mbit/s, or 0 for none.
    fn mbit(&self) -> u64 {
        self.model
            .and_then(|model| model.bits_per_second)
            .map_or(0, |rate| rate / 1_000_000)
    }
}

fn nz(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("a non-zero row count")
}

fn micros(duration: Duration) -> u128 {
    duration.as_micros()
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// `key=value;key=value` → a map.
fn parse_spec(spec: &str) -> BTreeMap<String, String> {
    spec.split(';')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect()
}

/// `key=value key=value` (the child's report lines) → a map.
fn parse_fields(line: &str) -> BTreeMap<String, String> {
    line.split_whitespace()
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

// ---------------------------------------------------------------------------
// The child: one measurement, in its own process
// ---------------------------------------------------------------------------

/// One measurement, driven by [`m5_6_fetch_benchmark_matrix`]. Does nothing on
/// its own.
#[test]
#[ignore = "child of the M5.6 fetch benchmark; m5_6_fetch_benchmark_matrix runs it"]
fn m5_6_fetch_benchmark_child() {
    let Some(spec) = optional(CELL) else {
        return;
    };
    let spec = parse_spec(&spec);
    let dsn = setting(CELL_DSN);
    let shape = Shape::named(spec.get("shape").map_or("", String::as_str));
    let rows: usize = spec
        .get("rows")
        .and_then(|value| value.parse().ok())
        .expect("rows=<n>");
    match spec.get("mode").map(String::as_str) {
        Some("warm") => child_warm(shape, rows, &dsn),
        Some("measure") => {
            let cap_ms: u64 = spec
                .get("cap_ms")
                .and_then(|value| value.parse().ok())
                .unwrap_or(10_000);
            child_measure(shape, rows, Duration::from_millis(cap_ms), &dsn);
        }
        Some("prefetch") => child_prefetch(shape, rows, &dsn),
        other => panic!("unknown child mode {other:?}"),
    }
}

/// Tells the parent where the child is and waits for its answer, so the
/// parent can snapshot the relay's counters while the connection is idle.
fn mark(label: &str) {
    println!("M56 MARK {label}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .expect("the parent answers every mark");
}

/// The median and the minimum of [`PINGS`] round trips.
fn ping_rtt(connection: &mut dyn DatabaseConnection) -> (Duration, Duration) {
    let mut samples: Vec<Duration> = (0..PINGS)
        .map(|_| {
            let started = Instant::now();
            connection.ping().expect("ping");
            started.elapsed()
        })
        .collect();
    samples.sort();
    (samples[PINGS / 2], samples[0])
}

fn connect_to(dsn: &str) -> Box<dyn DatabaseConnection> {
    try_connect(&params_at(dsn))
        .expect("the test database should accept the configured credentials")
}

fn child_measure(shape: Shape, rows: usize, cap: Duration, dsn: &str) {
    let mut connection = connect_to(dsn);
    let (ping_p50, ping_min) = ping_rtt(connection.as_mut());
    let statement = Statement::new(shape.sql()).with_fetch_rows(nz(rows));
    let before = sample_self();

    mark("start");
    let started = Instant::now();
    let mut outcome = connection
        .execute(&statement)
        .unwrap_or_else(|error| panic!("{} failed to execute: {error}", shape.name));
    let execute = started.elapsed();
    let mut cursor = outcome.take_cursor().expect("a query returns a cursor");
    let mut times: Vec<Duration> = Vec::new();
    let mut fetched = 0_usize;
    let mut capped = false;
    loop {
        let fetch_started = Instant::now();
        let batch = cursor
            .fetch_batch(nz(rows))
            .unwrap_or_else(|error| panic!("{} failed to fetch: {error}", shape.name));
        let count = batch.row_count();
        drop(batch);
        let spent = fetch_started.elapsed();
        if count == 0 {
            break;
        }
        times.push(spent);
        fetched += count;
        if fetched >= shape.rows {
            break;
        }
        if started.elapsed() >= cap && times.len() >= MIN_FETCHES {
            capped = true;
            break;
        }
    }
    let total = started.elapsed();
    mark("end");

    let after = sample_self();
    cursor.close().expect("closing a cursor cannot fail");
    drop(connection);

    let mut sorted = times.clone();
    sorted.sort();
    let at = |q: f64| -> Duration {
        let index = ((sorted.len() as f64 - 1.0) * q).round() as usize;
        sorted[index.min(sorted.len() - 1)]
    };
    let sum: Duration = times.iter().sum();
    let mean = sum / u32::try_from(times.len()).expect("fetch count fits");
    let steady = if times.len() > 1 {
        (sum - times[0]) / u32::try_from(times.len() - 1).expect("fetch count fits")
    } else {
        mean
    };
    let mut line = format!(
        "M56 RESULT rows={fetched} fetches={} capped={} execute_us={} first_fetch_us={} \
         fetch_p50_us={} fetch_p90_us={} fetch_max_us={} fetch_mean_us={} steady_mean_us={} \
         total_us={} ping_p50_us={} ping_min_us={}",
        times.len(),
        u8::from(capped),
        micros(execute),
        micros(times[0]),
        micros(at(0.5)),
        micros(at(0.9)),
        micros(at(1.0)),
        micros(mean),
        micros(steady),
        micros(total),
        micros(ping_p50),
        micros(ping_min),
    );
    if let (Some(before), Some(after)) = (before, after) {
        push_samples(&mut line, before, after);
    }
    println!("{line}");
}

fn push_samples(line: &mut String, before: ProcessSample, after: ProcessSample) {
    use std::fmt::Write as _;
    let _ = write!(
        line,
        " cpu_us={} ws_before={} peak_ws_before={} peak_ws_after={} private_before={} \
         peak_private_before={} peak_private_after={}",
        micros(after.cpu.saturating_sub(before.cpu)),
        before.working_set,
        before.peak_working_set,
        after.peak_working_set,
        before.private,
        before.peak_private,
        after.peak_private,
    );
}

fn child_warm(shape: Shape, rows: usize, dsn: &str) {
    let mut connection = connect_to(dsn);
    let server = server_banner(connection.as_mut());
    let statement = Statement::new(shape.sql()).with_fetch_rows(nz(rows));
    let mut outcome = connection
        .execute(&statement)
        .unwrap_or_else(|error| panic!("{} failed to execute: {error}", shape.name));
    let mut cursor = outcome.take_cursor().expect("a query returns a cursor");
    let declared = declared_row_width(cursor.columns());
    let columns = cursor.columns().len();
    let first = cursor.fetch_batch(nz(rows)).expect("the first batch");
    let second = cursor.fetch_batch(nz(rows)).expect("the second batch");
    let sample = if second.row_count() > 0 {
        &second
    } else {
        &first
    };
    let (store, payload) = observed_row_width(sample);
    drop(first);
    drop(second);
    cursor.close().expect("closing a cursor cannot fail");
    println!(
        "M56 SHAPE declared_row_width={declared} store_row_width={store:.1} \
         text_payload_per_row={payload:.1} columns={columns} server={}",
        server.replace(char::is_whitespace, "_"),
    );
}

/// The server's own description of itself, for the environment record.
fn server_banner(connection: &mut dyn DatabaseConnection) -> String {
    for sql in [
        "SELECT banner_full FROM v$version WHERE ROWNUM = 1",
        "SELECT product || ' ' || version_full FROM product_component_version \
         WHERE product LIKE 'Oracle%' AND ROWNUM = 1",
        "SELECT product || ' ' || version FROM product_component_version WHERE ROWNUM = 1",
    ] {
        let Ok(mut outcome) = connection.execute(&Statement::new(sql)) else {
            continue;
        };
        let Some(mut cursor) = outcome.take_cursor() else {
            continue;
        };
        if let Ok(batch) = cursor.fetch_batch(nz(1))
            && let Some(ValueRef::Text(text)) = batch.value(0, 0)
        {
            return text.to_owned();
        }
    }
    "unknown".to_owned()
}

/// The row width the Result Store sizes a **first** round trip by, from the
/// describe alone. A mirror of `declared_row_width` in
/// `crates/db-core/src/store/policy.rs` (a driver crate may not depend on
/// `db-core`): 44 B for `NUMBER`, 16 B for a date or timestamp, the declared
/// maximum plus an 8-byte offset for text, 320 B for a LOB, and one NULL bit
/// per column.
fn declared_row_width(columns: &[ColumnMetadata]) -> usize {
    let variable = |column: &ColumnMetadata| {
        column
            .max_size_bytes()
            .map_or(4_000, |bytes| usize::try_from(bytes).unwrap_or(usize::MAX))
            + 8
    };
    let cells: usize = columns
        .iter()
        .map(|column| match column.sql_type() {
            SqlType::Boolean => 1,
            SqlType::Number => 44,
            SqlType::BinaryFloat => 4,
            SqlType::BinaryDouble => 8,
            SqlType::Date | SqlType::Timestamp | SqlType::TimestampWithTimeZone => 16,
            SqlType::CharacterLob { .. } | SqlType::BinaryLob => 320,
            SqlType::Unsupported => variable(column).max(64 + 8),
            _ => variable(column),
        })
        .sum();
    (cells + columns.len().div_ceil(8)).max(1)
}

/// An estimate of the bytes per row the Result Store retains for this batch
/// once compacted (ADR-0004 RS1): text at its payload plus an 8-byte offset,
/// a `NUMBER` column at 8 B when every value fits a scaled `i64` (scale and
/// digits ≤ 18) and 44 B otherwise, 16 B per date, 320 B per LOB, one NULL
/// bit per cell. Returns the width and the text payload per row. Segment
/// headers (about 0.1 B per row per column at 1,000 rows) are left out.
fn observed_row_width(batch: &RowBatch) -> (f64, f64) {
    let rows = batch.row_count().max(1);
    let mut bytes = 0_usize;
    let mut payload = 0_usize;
    for column in batch.columns() {
        bytes += rows.div_ceil(8);
        bytes += match column.data() {
            ColumnData::Text(text) | ColumnData::Json(text) | ColumnData::Unsupported(text) => {
                payload += text.buffer().len();
                text.buffer().len() + 8 * rows
            }
            ColumnData::Bytes(values) => values.buffer().len() + 8 * rows,
            ColumnData::Number(values) => {
                let (mut scale, mut integer) = (0_i32, 0_i32);
                for (row, value) in values.iter().enumerate() {
                    if column.is_null(row) || value.is_zero() {
                        continue;
                    }
                    let digits = i32::try_from(value.digit_count()).unwrap_or(i32::MAX);
                    let exponent = i32::from(value.exponent());
                    scale = scale.max(digits - exponent);
                    integer = integer.max(exponent);
                }
                if scale <= 18 && integer.max(0) + scale.max(0) <= 18 {
                    8 * rows
                } else {
                    44 * rows
                }
            }
            ColumnData::Timestamp(_) => 16 * rows,
            ColumnData::Lob(_) => 320 * rows,
            ColumnData::Boolean(_) => rows,
            ColumnData::Float(_) => 4 * rows,
            ColumnData::Double(_) => 8 * rows,
            _ => 0,
        };
    }
    (bytes as f64 / rows as f64, payload as f64 / rows as f64)
}

/// `prefetch_rows`, the one fetch knob `oracledb` has besides the array size:
/// rows returned with the execute itself. The driver pins it to 0 (U-3
/// containment), so this goes through raw `oracledb` and only measures what
/// it would buy: the time to the first page of `rows` rows with and without it.
fn child_prefetch(shape: Shape, rows: usize, dsn: &str) {
    let config = oracledb::Config::default()
        .set_connect_string(dsn)
        .expect("a valid connect string")
        .set_credentials(&setting(USER), &setting(PASSWORD));
    let connection = oracledb::connect(config).expect("raw oracledb connects");
    let sql = shape.sql();
    let array = u32::try_from(rows).expect("rows fit u32");
    for rep in 0..PREFETCH_REPS {
        for prefetch in [0, array] {
            let started = Instant::now();
            let mut statement = connection.statement(&sql).expect("prepare");
            statement
                .exclude_from_cache()
                .fetch_array_size(array)
                .prefetch_rows(prefetch);
            let mut cursor = statement.query(&[]).expect("query");
            let executed = started.elapsed();
            let mut got = 0_usize;
            while got < rows {
                match cursor.next() {
                    Some(Ok(_)) => got += 1,
                    Some(Err(error)) => panic!("prefetch={prefetch}: {error}"),
                    None => break,
                }
            }
            let page = started.elapsed();
            drop(cursor);
            println!(
                "M56 PREFETCH rep={rep} prefetch={prefetch} rows={got} execute_us={} page_us={}",
                micros(executed),
                micros(page),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The parent: the matrix
// ---------------------------------------------------------------------------

/// What one child reported.
struct ChildRun {
    ok: bool,
    fields: BTreeMap<String, String>,
    lines: Vec<String>,
    wire: Option<StatsSnapshot>,
    connections: usize,
    transcript: String,
}

impl ChildRun {
    fn number(&self, key: &str) -> Option<f64> {
        self.fields.get(key).and_then(|value| value.parse().ok())
    }
}

/// Runs one child to completion, answering its marks.
fn run_child(spec: &str, dsn: &str, relay: Option<&Relay>) -> ChildRun {
    let exe = std::env::current_exe().expect("the test binary's own path");
    let accepted_before = relay.map_or(0, Relay::accepted);
    let mut child = Command::new(exe)
        .args([
            CHILD_TEST,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CELL, spec)
        .env(CELL_DSN, dsn)
        .env_remove(OUT)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the benchmark child");
    let mut stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let mut stderr = child.stderr.take().expect("child stderr");
    let errors = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

    let mut fields = BTreeMap::new();
    let mut lines = Vec::new();
    let mut transcript = String::new();
    let mut start: Option<StatsSnapshot> = None;
    let mut wire = None;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        transcript.push_str(&line);
        transcript.push('\n');
        if let Some(label) = line.strip_prefix("M56 MARK ") {
            let now = relay.and_then(Relay::latest).map(|stats| stats.snapshot());
            match label.trim() {
                "start" => start = now,
                _ => wire = now.zip(start).map(|(end, begin)| end.since(begin)),
            }
            let _ = writeln!(stdin, "go");
            let _ = stdin.flush();
        } else if let Some(rest) = line
            .strip_prefix("M56 RESULT ")
            .or_else(|| line.strip_prefix("M56 SHAPE "))
        {
            fields.extend(parse_fields(rest));
        } else if line.starts_with("M56 ") {
            lines.push(line);
        }
    }
    drop(stdin);
    let status = child.wait().expect("wait for the child");
    transcript.push_str(&errors.join().unwrap_or_default());
    ChildRun {
        ok: status.success(),
        fields,
        lines,
        wire,
        connections: relay.map_or(1, Relay::accepted) - accepted_before,
        transcript,
    }
}

/// One measured run, as `runs.csv` records it.
struct RunRecord {
    link: Link,
    shape: &'static str,
    rows_per_fetch: usize,
    run: usize,
    machine: String,
    started: u64,
    child: ChildRun,
}

impl RunRecord {
    fn get(&self, key: &str) -> f64 {
        self.child.number(key).unwrap_or(f64::NAN)
    }

    fn rows(&self) -> f64 {
        self.get("rows")
    }

    fn fetches(&self) -> f64 {
        self.get("fetches")
    }

    fn total_ms(&self) -> f64 {
        self.get("total_us") / 1_000.0
    }

    fn fetch_p50_ms(&self) -> f64 {
        self.get("fetch_p50_us") / 1_000.0
    }

    fn rows_per_s(&self) -> f64 {
        self.rows() / (self.total_ms() / 1_000.0)
    }

    /// Wall time for 100,000 rows at this run's rate (measured when the run
    /// fetched 100,000 rows, extrapolated otherwise).
    fn ms_per_100k(&self) -> f64 {
        self.total_ms() * 100_000.0 / self.rows()
    }

    fn cpu_ms(&self) -> f64 {
        self.get("cpu_us") / 1_000.0
    }

    fn cpu_over_wall(&self) -> f64 {
        self.cpu_ms() / self.total_ms()
    }

    fn cpu_ms_per_fetch(&self) -> f64 {
        self.cpu_ms() / self.fetches()
    }

    /// Peak working-set growth over the working set before the run, in KiB.
    fn peak_ws_growth_kib(&self) -> f64 {
        (self.get("peak_ws_after") - self.get("ws_before")) / 1_024.0
    }

    /// Peak private-commit growth over the private commit before the run.
    fn peak_private_growth_kib(&self) -> f64 {
        (self.get("peak_private_after") - self.get("private_before")) / 1_024.0
    }

    /// Whether the peak after the run is still the peak from before it (the
    /// connect), so the growth figure is an upper bound, not a measurement.
    fn peak_is_bound(&self) -> bool {
        self.get("peak_ws_after") <= self.get("peak_ws_before")
    }

    /// Bytes from the server per fetch, the describe included (about one
    /// packet; negligible past a handful of fetches).
    fn wire_bytes_per_fetch(&self) -> f64 {
        self.child.wire.map_or(f64::NAN, |wire| {
            wire.to_client_bytes as f64 / self.fetches()
        })
    }

    fn wire_bytes_per_row(&self) -> f64 {
        self.child
            .wire
            .map_or(f64::NAN, |wire| wire.to_client_bytes as f64 / self.rows())
    }

    /// TNS packets per fetch, less the describe's one packet.
    fn packets_per_fetch(&self) -> f64 {
        self.child.wire.map_or(f64::NAN, |wire| {
            (wire.to_client_packets as f64 - 1.0).max(0.0) / self.fetches()
        })
    }

    const HEADER: &'static str = "link,injected_rtt_ms,mbit,shape,rows_per_fetch,run,started_epoch_s,\
machine_state,ok,rows,fetches,capped,execute_ms,first_fetch_ms,fetch_p50_ms,fetch_p90_ms,fetch_max_ms,\
fetch_mean_ms,steady_mean_ms,total_ms,rows_per_s,ms_per_100k_rows,cpu_ms,cpu_over_wall,cpu_ms_per_fetch,\
ping_p50_ms,ping_min_ms,ws_before_kib,peak_ws_growth_kib,peak_private_growth_kib,peak_is_bound,\
wire_to_client_bytes,wire_to_server_bytes,wire_to_client_packets,wire_data_packets,largest_packet,\
accepted_sdu,accepted_version,framing_lost,wire_bytes_per_row,wire_bytes_per_fetch,packets_per_fetch,\
connections";

    fn csv(&self) -> String {
        let wire = self.child.wire.unwrap_or_default();
        let has_wire = self.child.wire.is_some();
        let opt = |value: u64| {
            if has_wire {
                value.to_string()
            } else {
                String::new()
            }
        };
        [
            self.link.name.clone(),
            format!("{:.1}", self.link.injected_rtt_ms()),
            self.link.mbit().to_string(),
            self.shape.to_owned(),
            self.rows_per_fetch.to_string(),
            self.run.to_string(),
            self.started.to_string(),
            self.machine.clone(),
            u8::from(self.child.ok).to_string(),
            fmt(self.rows(), 0),
            fmt(self.fetches(), 0),
            fmt(self.get("capped"), 0),
            fmt(self.get("execute_us") / 1_000.0, 3),
            fmt(self.get("first_fetch_us") / 1_000.0, 3),
            fmt(self.fetch_p50_ms(), 3),
            fmt(self.get("fetch_p90_us") / 1_000.0, 3),
            fmt(self.get("fetch_max_us") / 1_000.0, 3),
            fmt(self.get("fetch_mean_us") / 1_000.0, 3),
            fmt(self.get("steady_mean_us") / 1_000.0, 3),
            fmt(self.total_ms(), 1),
            fmt(self.rows_per_s(), 0),
            fmt(self.ms_per_100k(), 1),
            fmt(self.cpu_ms(), 1),
            fmt(self.cpu_over_wall(), 3),
            fmt(self.cpu_ms_per_fetch(), 3),
            fmt(self.get("ping_p50_us") / 1_000.0, 3),
            fmt(self.get("ping_min_us") / 1_000.0, 3),
            fmt(self.get("ws_before") / 1_024.0, 0),
            fmt(self.peak_ws_growth_kib(), 0),
            fmt(self.peak_private_growth_kib(), 0),
            u8::from(self.peak_is_bound()).to_string(),
            opt(wire.to_client_bytes),
            opt(wire.to_server_bytes),
            opt(wire.to_client_packets),
            opt(wire.to_client_data_packets),
            opt(wire.largest_to_client_packet),
            opt(wire.accepted_sdu),
            opt(wire.accepted_version),
            if has_wire {
                u8::from(wire.framing_lost).to_string()
            } else {
                String::new()
            },
            fmt(self.wire_bytes_per_row(), 1),
            fmt(self.wire_bytes_per_fetch(), 0),
            fmt(self.packets_per_fetch(), 2),
            self.child.connections.to_string(),
        ]
        .join(",")
    }
}

/// A number for a CSV cell: empty when it is not a number.
fn fmt(value: f64, decimals: usize) -> String {
    if value.is_finite() {
        format!("{value:.decimals$}")
    } else {
        String::new()
    }
}

/// A CSV file written row by row, so a run that dies part-way keeps what it
/// measured.
struct Csv(File);

impl Csv {
    fn create(path: &Path, header: &str) -> Self {
        let mut file = File::create(path).unwrap_or_else(|error| panic!("{path:?}: {error}"));
        writeln!(file, "{header}").expect("write a CSV header");
        Self(file)
    }

    fn row(&mut self, row: &str) {
        writeln!(self.0, "{row}").expect("write a CSV row");
        let _ = self.0.flush();
    }
}

fn list(variable: &str, default: &str) -> Vec<String> {
    optional(variable)
        .unwrap_or_else(|| default.to_owned())
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty() && *item != "none")
        .map(str::to_owned)
        .collect()
}

/// The connect string for a link: the relay's port, or the listener itself.
fn dsn_for(relay: Option<&Relay>) -> String {
    relay.map_or_else(
        || setting(common::DSN),
        |relay| format!("127.0.0.1:{}/{}", relay.addr().port(), dsn_service()),
    )
}

/// The whole matrix. See the file's documentation for the method.
#[test]
#[ignore = "the M5.6 fetch benchmark: tens of minutes against the live database; run on purpose"]
fn m5_6_fetch_benchmark_matrix() {
    let Some(out) = optional(OUT) else {
        println!("M5.6 benchmark skipped: set {OUT} to an absolute output directory");
        return;
    };
    let out = PathBuf::from(out);
    fs::create_dir_all(&out).expect("create the output directory");

    let links: Vec<Link> = list("RELDEX_M56_LINKS", "direct,rtt0,rtt2,rtt10,rtt40")
        .iter()
        .map(|name| Link::parse(name))
        .collect();
    let shapes: Vec<Shape> = list("RELDEX_M56_SHAPES", "numbers10,text5date2,wide4k")
        .iter()
        .map(|name| Shape::named(name))
        .collect();
    let runs: usize = optional("RELDEX_M56_RUNS").map_or(3, |value| value.parse().expect("runs"));
    let cap_ms: u64 =
        optional("RELDEX_M56_CAP_MS").map_or(10_000, |value| value.parse().expect("cap"));
    let sizes_override: Option<Vec<usize>> = optional("RELDEX_M56_SIZES").map(|value| {
        value
            .split(',')
            .map(|size| size.trim().parse().expect("a size"))
            .collect()
    });
    let extras = list("RELDEX_M56_EXTRAS", "prefetch,sdu");
    let upstream = dsn_address();

    let mut runs_csv = Csv::create(&out.join("runs.csv"), RunRecord::HEADER);
    let mut shapes_csv = Csv::create(
        &out.join("shapes.csv"),
        "link,shape,declared_row_width,store_row_width_est,text_payload_per_row,columns,\
         statement_rows,warm_rows_per_fetch,accepted_sdu,accepted_version,\
         server,about,sql",
    );
    let mut groups_csv = Csv::create(
        &out.join("groups.csv"),
        "link,shape,started_epoch_s,cpu_load_percent,machine_state",
    );
    let mut records: Vec<RunRecord> = Vec::new();
    let mut failures = Vec::new();

    for link in &links {
        let relay = link
            .model
            .map(|model| Relay::start(upstream.as_str(), model).expect("start the relay"));
        let dsn = dsn_for(relay.as_ref());
        for shape in &shapes {
            let load = cpu_load_percent().map_or_else(String::new, |load| load.to_string());
            groups_csv.row(&format!(
                "{},{},{},{load},{}",
                link.name,
                shape.name,
                now_epoch(),
                machine_state()
            ));
            let warm = run_child(
                &format!("mode=warm;shape={};rows={}", shape.name, shape.warm_rows),
                &dsn,
                relay.as_ref(),
            );
            if !warm.ok {
                failures.push(format!(
                    "warm {} {}:\n{}",
                    link.name, shape.name, warm.transcript
                ));
                continue;
            }
            shapes_csv.row(&format!(
                "{},{},{},{},{},{},{},{},{},{},{},\"{}\",\"{}\"",
                link.name,
                shape.name,
                warm.fields
                    .get("declared_row_width")
                    .map_or("", String::as_str),
                warm.fields
                    .get("store_row_width")
                    .map_or("", String::as_str),
                warm.fields
                    .get("text_payload_per_row")
                    .map_or("", String::as_str),
                warm.fields.get("columns").map_or("", String::as_str),
                shape.rows,
                shape.warm_rows,
                relay
                    .as_ref()
                    .and_then(Relay::latest)
                    .map_or(String::new(), |stats| stats
                        .snapshot()
                        .accepted_sdu
                        .to_string()),
                relay
                    .as_ref()
                    .and_then(Relay::latest)
                    .map_or(String::new(), |stats| stats
                        .snapshot()
                        .accepted_version
                        .to_string()),
                warm.fields.get("server").map_or("", String::as_str),
                shape.about,
                shape.sql().replace('"', "'"),
            ));
            let sizes = sizes_override
                .clone()
                .unwrap_or_else(|| shape.sizes.to_vec());
            for run in 1..=runs {
                for &rows in &sizes {
                    let machine = machine_state();
                    let started = now_epoch();
                    let child = run_child(
                        &format!(
                            "mode=measure;shape={};rows={rows};cap_ms={cap_ms}",
                            shape.name
                        ),
                        &dsn,
                        relay.as_ref(),
                    );
                    if !child.ok {
                        failures.push(format!(
                            "{} {} {rows} run {run}:\n{}",
                            link.name, shape.name, child.transcript
                        ));
                    }
                    let record = RunRecord {
                        link: link.clone(),
                        shape: shape.name,
                        rows_per_fetch: rows,
                        run,
                        machine,
                        started,
                        child,
                    };
                    runs_csv.row(&record.csv());
                    println!(
                        "M5.6 {} {} rows/fetch={rows} run={run}: fetch p50 {:.2} ms, {:.0} rows/s, \
                         {:.1} ms per 100k",
                        link.name,
                        shape.name,
                        record.fetch_p50_ms(),
                        record.rows_per_s(),
                        record.ms_per_100k(),
                    );
                    records.push(record);
                }
            }
        }
    }

    if extras.iter().any(|extra| extra == "prefetch") {
        prefetch_extra(&out, &links, &upstream, &mut failures);
    }
    if extras.iter().any(|extra| extra == "sdu") {
        sdu_extra(&out, &upstream, runs, cap_ms, &mut failures);
    }

    summarize(&records, &out);
    assert!(
        failures.is_empty(),
        "{} benchmark child(ren) failed; their results are marked ok=0:\n{}",
        failures.len(),
        failures.join("\n---\n")
    );
}

/// `prefetch_rows`: the time to the first page of `rows` rows of the mixed
/// shape with the rows returned by the execute, against the driver's
/// describe-then-fetch.
fn prefetch_extra(out: &Path, links: &[Link], upstream: &str, failures: &mut Vec<String>) {
    let mut csv = Csv::create(
        &out.join("prefetch.csv"),
        "link,injected_rtt_ms,shape,rows,rep,prefetch,rows_returned,execute_ms,first_page_ms",
    );
    let shape = Shape::named("text5date2");
    for link in links {
        let relay = link
            .model
            .map(|model| Relay::start(upstream, model).expect("start the relay"));
        let dsn = dsn_for(relay.as_ref());
        for rows in [100, 1_000] {
            let child = run_child(
                &format!("mode=prefetch;shape={};rows={rows}", shape.name),
                &dsn,
                relay.as_ref(),
            );
            if !child.ok {
                failures.push(format!(
                    "prefetch {} {rows}:\n{}",
                    link.name, child.transcript
                ));
            }
            for line in &child.lines {
                let Some(rest) = line.strip_prefix("M56 PREFETCH ") else {
                    continue;
                };
                let fields = parse_fields(rest);
                let get = |key: &str| fields.get(key).map_or("", String::as_str);
                let micros_to_ms =
                    |key: &str| fmt(get(key).parse::<f64>().unwrap_or(f64::NAN) / 1_000.0, 3);
                csv.row(&format!(
                    "{},{:.1},{},{rows},{},{},{},{},{}",
                    link.name,
                    link.injected_rtt_ms(),
                    shape.name,
                    get("rep"),
                    get("prefetch"),
                    get("rows"),
                    micros_to_ms("execute_us"),
                    micros_to_ms("page_us"),
                ));
            }
            println!("M5.6 prefetch {} rows={rows} done", link.name);
        }
    }
}

/// The SDU: what the listener accepts when the client asks for more than
/// `oracledb`'s default of 8,192 bytes, and what that does to the mixed
/// shape's round trip. Through a zero-delay relay, which reads the `ACCEPT`.
fn sdu_extra(out: &Path, upstream: &str, runs: usize, cap_ms: u64, failures: &mut Vec<String>) {
    let mut csv = Csv::create(
        &out.join("sdu.csv"),
        &format!("requested_sdu,{}", RunRecord::HEADER),
    );
    let link = Link::parse("rtt0");
    let relay = Relay::start(upstream, link.model.expect("a relay link")).expect("start the relay");
    let shape = Shape::named("text5date2");
    for run in 1..=runs {
        for requested in [0_u32, 65_535, 2_097_152] {
            let dsn = if requested == 0 {
                dsn_for(Some(&relay))
            } else {
                format!(
                    "(DESCRIPTION=(SDU={requested})(ADDRESS=(PROTOCOL=TCP)(HOST=127.0.0.1)(PORT={}))\
                     (CONNECT_DATA=(SERVICE_NAME={})))",
                    relay.addr().port(),
                    dsn_service()
                )
            };
            for rows in [1_000, 5_000] {
                let machine = machine_state();
                let started = now_epoch();
                let child = run_child(
                    &format!(
                        "mode=measure;shape={};rows={rows};cap_ms={cap_ms}",
                        shape.name
                    ),
                    &dsn,
                    Some(&relay),
                );
                if !child.ok {
                    failures.push(format!("sdu {requested} {rows}:\n{}", child.transcript));
                }
                let record = RunRecord {
                    link: link.clone(),
                    shape: shape.name,
                    rows_per_fetch: rows,
                    run,
                    machine,
                    started,
                    child,
                };
                csv.row(&format!("{requested},{}", record.csv()));
                println!(
                    "M5.6 sdu requested={requested} rows/fetch={rows} run={run}: accepted {} \
                     largest packet {}, fetch p50 {:.2} ms",
                    record.child.wire.map_or(0, |wire| wire.accepted_sdu),
                    record
                        .child
                        .wire
                        .map_or(0, |wire| wire.largest_to_client_packet),
                    record.fetch_p50_ms(),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Summary: medians, coefficients of variation, and the O(packets²) fit
// ---------------------------------------------------------------------------

fn median(values: &[f64]) -> f64 {
    let mut finite: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if finite.is_empty() {
        return f64::NAN;
    }
    finite.sort_by(f64::total_cmp);
    let mid = finite.len() / 2;
    if finite.len() % 2 == 0 {
        f64::midpoint(finite[mid - 1], finite[mid])
    } else {
        finite[mid]
    }
}

/// Coefficient of variation: sample standard deviation over mean.
fn cov(values: &[f64]) -> f64 {
    let finite: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if finite.len() < 2 {
        return f64::NAN;
    }
    let n = finite.len() as f64;
    let mean = finite.iter().sum::<f64>() / n;
    let variance = finite.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
    variance.sqrt() / mean
}

/// One `cells.csv` row: every run of one link × shape × size.
struct Cell<'a> {
    link: &'a Link,
    shape: &'static str,
    rows_per_fetch: usize,
    runs: Vec<&'a RunRecord>,
}

impl Cell<'_> {
    fn values(&self, metric: impl Fn(&RunRecord) -> f64) -> Vec<f64> {
        self.runs.iter().map(|run| metric(run)).collect()
    }

    fn median(&self, metric: impl Fn(&RunRecord) -> f64) -> f64 {
        median(&self.values(metric))
    }

    fn cov(&self, metric: impl Fn(&RunRecord) -> f64) -> f64 {
        cov(&self.values(metric))
    }
}

fn summarize(records: &[RunRecord], out: &Path) {
    let mut cells: Vec<Cell<'_>> = Vec::new();
    for record in records.iter().filter(|record| record.child.ok) {
        let found = cells.iter_mut().find(|cell| {
            cell.link.name == record.link.name
                && cell.shape == record.shape
                && cell.rows_per_fetch == record.rows_per_fetch
        });
        match found {
            Some(cell) => cell.runs.push(record),
            None => cells.push(Cell {
                link: &record.link,
                shape: record.shape,
                rows_per_fetch: record.rows_per_fetch,
                runs: vec![record],
            }),
        }
    }

    // The wire is the same on every link for one shape and size; the direct
    // link has no relay to count it, so it borrows the relayed links' figure.
    let wire = |shape: &str, rows: usize, metric: fn(&RunRecord) -> f64| -> f64 {
        median(
            &records
                .iter()
                .filter(|r| r.child.ok && r.shape == shape && r.rows_per_fetch == rows)
                .map(metric)
                .collect::<Vec<_>>(),
        )
    };

    let mut csv = Csv::create(
        &out.join("cells.csv"),
        "link,injected_rtt_ms,mbit,shape,rows_per_fetch,runs,capped_runs,wire_bytes_per_fetch,\
         wire_bytes_per_row,packets_per_fetch,fetch_p50_ms,fetch_p50_cov,first_fetch_ms,\
         execute_ms,rows_per_s,rows_per_s_cov,ms_per_100k_rows,ms_per_100k_cov,cpu_over_wall,\
         cpu_ms_per_fetch,peak_ws_growth_kib,peak_private_growth_kib,ping_p50_ms,machine_states",
    );
    for cell in &cells {
        let capped = cell
            .runs
            .iter()
            .filter(|run| run.get("capped") > 0.5)
            .count();
        let mut states: Vec<&str> = cell.runs.iter().map(|run| run.machine.as_str()).collect();
        states.sort_unstable();
        states.dedup();
        csv.row(&format!(
            "{},{:.1},{},{},{},{},{capped},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},\"{}\"",
            cell.link.name,
            cell.link.injected_rtt_ms(),
            cell.link.mbit(),
            cell.shape,
            cell.rows_per_fetch,
            cell.runs.len(),
            fmt(
                wire(
                    cell.shape,
                    cell.rows_per_fetch,
                    RunRecord::wire_bytes_per_fetch
                ),
                0
            ),
            fmt(
                wire(
                    cell.shape,
                    cell.rows_per_fetch,
                    RunRecord::wire_bytes_per_row
                ),
                1
            ),
            fmt(
                wire(
                    cell.shape,
                    cell.rows_per_fetch,
                    RunRecord::packets_per_fetch
                ),
                2
            ),
            fmt(cell.median(RunRecord::fetch_p50_ms), 3),
            fmt(cell.cov(RunRecord::fetch_p50_ms), 3),
            fmt(cell.median(|r| r.get("first_fetch_us") / 1_000.0), 3),
            fmt(cell.median(|r| r.get("execute_us") / 1_000.0), 3),
            fmt(cell.median(RunRecord::rows_per_s), 0),
            fmt(cell.cov(RunRecord::rows_per_s), 3),
            fmt(cell.median(RunRecord::ms_per_100k), 1),
            fmt(cell.cov(RunRecord::ms_per_100k), 3),
            fmt(cell.median(RunRecord::cpu_over_wall), 3),
            fmt(cell.median(RunRecord::cpu_ms_per_fetch), 3),
            fmt(cell.median(RunRecord::peak_ws_growth_kib), 0),
            fmt(cell.median(RunRecord::peak_private_growth_kib), 0),
            fmt(cell.median(|r| r.get("ping_p50_us") / 1_000.0), 3),
            states.join(" | "),
        ));
    }

    // Per link and shape: per-fetch wall time and client CPU against packets
    // per round trip, `y = a + b·P + c·P²`, least squares weighted by 1/y² so
    // that the small round trips count as much as the large ones.
    let mut fit_csv = Csv::create(
        &out.join("fit.csv"),
        "link,shape,metric,points,a_ms,b_ms_per_packet,c_ms_per_packet_sq,max_rel_error",
    );
    let mut groups: Vec<(&Link, &'static str)> = Vec::new();
    for cell in &cells {
        if !groups
            .iter()
            .any(|(link, shape)| link.name == cell.link.name && *shape == cell.shape)
        {
            groups.push((cell.link, cell.shape));
        }
    }
    for (link, shape) in groups {
        let points: Vec<&Cell<'_>> = cells
            .iter()
            .filter(|cell| cell.link.name == link.name && cell.shape == shape)
            .collect();
        for (metric, value) in [
            (
                "fetch_p50_ms",
                RunRecord::fetch_p50_ms as fn(&RunRecord) -> f64,
            ),
            ("cpu_ms_per_fetch", RunRecord::cpu_ms_per_fetch),
        ] {
            let xy: Vec<(f64, f64)> = points
                .iter()
                .map(|cell| {
                    (
                        wire(shape, cell.rows_per_fetch, RunRecord::packets_per_fetch),
                        cell.median(value),
                    )
                })
                .filter(|(x, y)| x.is_finite() && y.is_finite() && *y > 0.0)
                .collect();
            if let Some((a, b, c, error)) = quadratic_fit(&xy) {
                fit_csv.row(&format!(
                    "{},{shape},{metric},{},{a:.4},{b:.5},{c:.7},{error:.3}",
                    link.name,
                    xy.len()
                ));
            }
        }
    }
}

/// Weighted least squares for `y = a + b·x + c·x²` with weights `1/y²`.
/// Returns the coefficients and the largest relative error over the points.
fn quadratic_fit(points: &[(f64, f64)]) -> Option<(f64, f64, f64, f64)> {
    if points.len() < 3 {
        return None;
    }
    // Normal equations: Σ w·[1 x x²]ᵀ[1 x x²] · [a b c]ᵀ = Σ w·y·[1 x x²]ᵀ.
    let mut m = [[0.0_f64; 4]; 3];
    for &(x, y) in points {
        let w = 1.0 / (y * y);
        let basis = [1.0, x, x * x];
        for row in 0..3 {
            for col in 0..3 {
                m[row][col] += w * basis[row] * basis[col];
            }
            m[row][3] += w * basis[row] * y;
        }
    }
    // Gaussian elimination with partial pivoting.
    for col in 0..3 {
        let pivot = (col..3).max_by(|&i, &j| m[i][col].abs().total_cmp(&m[j][col].abs()))?;
        m.swap(col, pivot);
        if m[col][col].abs() < f64::EPSILON {
            return None;
        }
        let pivot_row = m[col];
        for (index, row) in m.iter_mut().enumerate() {
            if index != col {
                let factor = row[col] / pivot_row[col];
                for (target, source) in row.iter_mut().zip(pivot_row.iter()).skip(col) {
                    *target -= factor * source;
                }
            }
        }
    }
    let (a, b, c) = (m[0][3] / m[0][0], m[1][3] / m[1][1], m[2][3] / m[2][2]);
    let error = points
        .iter()
        .map(|&(x, y)| ((a + b * x + c * x * x) - y).abs() / y)
        .fold(0.0, f64::max);
    Some((a, b, c, error))
}

#[test]
fn the_quadratic_fit_recovers_known_coefficients() {
    let points: Vec<(f64, f64)> = [1.0, 2.0, 5.0, 10.0, 40.0, 100.0]
        .iter()
        .map(|&x| (x, 0.5 + 0.2 * x + 0.03 * x * x))
        .collect();
    let (a, b, c, error) = quadratic_fit(&points).expect("a fit");
    assert!((a - 0.5).abs() < 1e-6 && (b - 0.2).abs() < 1e-6 && (c - 0.03).abs() < 1e-8);
    assert!(error < 1e-9);
}

#[test]
fn links_parse_to_the_delay_and_rate_they_name() {
    let direct = Link::parse("direct");
    assert!(direct.model.is_none());
    let ten = Link::parse("rtt10");
    assert_eq!(ten.model.map(|m| m.one_way), Some(Duration::from_millis(5)));
    assert!((ten.injected_rtt_ms() - 10.0).abs() < 1e-9);
    let limited = Link::parse("rtt40-100mbit");
    assert_eq!(limited.mbit(), 100);
    assert_eq!(
        limited.model.and_then(|m| m.bits_per_second),
        Some(100_000_000)
    );
}

#[test]
fn every_shape_has_a_statement_and_sizes() {
    for shape in SHAPES {
        assert!(
            shape.sql().contains("CONNECT BY LEVEL <="),
            "{}",
            shape.name
        );
        assert!(!shape.sizes.is_empty());
        assert!(shape.sizes.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
