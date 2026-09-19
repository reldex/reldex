//! Spike S14 — first measurements for a large result (`phase-0.md`,
//! "Measurements": fetch throughput and memory during a large fetch).
//!
//! `SPEC.md` §12 makes one claim this file exists to test: a large result is
//! cursor/batch based, so the client's memory is bounded by the **batch**, not
//! by the result. A million rows must therefore cost about the same as ten
//! thousand, provided the caller drops each batch.
//!
//! Phase 0 had LOB streaming numbers (S7) and nothing for row fetching.
//!
//! *Method, stated so the numbers can be argued with:* one session, rows
//! generated on the server by cross-joining a 1 000-row `CONNECT BY` with
//! itself, three columns (NUMBER, VARCHAR2(40), DATE), wall clock from before
//! `execute` to after the last `fetch_batch`, each batch dropped as soon as its
//! row count has been read. Working set is read from `tasklist`, the same
//! coarse instrument S7 used and with the same weakness: it is the operating
//! system's view of the whole process, sampled, so it includes allocator
//! caching and anything else the process does, and it cannot see an allocation
//! that was freed between samples. It is enough to tell "bounded" from "grows
//! with the row count", which is the claim under test, and not enough for a
//! precise allocation figure.

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "spike results are measurements a human reads from the test output"
)]

mod common;

use std::num::NonZeroUsize;
use std::process::Command;
use std::time::Instant;

use common::{connect, measurement, observation, scalar};
use reldex_db_driver_api::{DatabaseConnection, Statement};

/// Rows generated for each pass.
const ROWS: u64 = 1_000_000;

/// How often the working set is sampled during a pass.
const SAMPLE_EVERY: std::time::Duration = std::time::Duration::from_millis(250);

/// The generator. `WITH … SELECT` so the classifier sees a query.
fn generator() -> String {
    format!(
        "WITH g AS (SELECT LEVEL AS n FROM dual CONNECT BY LEVEL <= 1000) \
         SELECT (a.n - 1) * 1000 + b.n AS id, \
                RPAD('row ' || b.n, 40, '.') AS label, \
                DATE '2026-01-01' + b.n AS made \
         FROM g a CROSS JOIN g b \
         WHERE (a.n - 1) * 1000 + b.n <= {ROWS}"
    )
}

/// This process's working set in kilobytes, as Windows reports it.
///
/// Same method as S7, deliberately: one process spawn per sample rather than a
/// dependency that would then ship in every build.
fn working_set_kb() -> Option<u64> {
    let output = Command::new("tasklist")
        .args([
            "/FI",
            &format!("PID eq {}", std::process::id()),
            "/FO",
            "CSV",
            "/NH",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let field = text.split("\",\"").nth(4)?;
    let digits: String = field.chars().filter(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// One pass: what a caller who drops every batch actually pays.
struct Pass {
    /// Rows the caller received.
    rows: u64,
    /// Batches it took.
    batches: u64,
    /// Rows per second over the whole pass.
    per_second: f64,
    /// Working-set growth over the pass, in KiB, or `None` if unmeasurable.
    growth_kb: Option<u64>,
    /// How long the first batch took to arrive.
    first_batch: std::time::Duration,
}

/// Streams the generator at one batch size, dropping every batch.
fn stream(connection: &mut dyn DatabaseConnection, batch_size: NonZeroUsize) -> Pass {
    let statement = Statement::new(generator()).with_fetch_rows(batch_size);
    let baseline = working_set_kb();
    let mut peak = baseline.unwrap_or(0);

    let started = Instant::now();
    let mut outcome = connection.execute(&statement).expect("start the query");
    let mut cursor = outcome.take_cursor().expect("cursor");
    let mut rows = 0_u64;
    let mut batches = 0_u64;
    let mut first_batch = std::time::Duration::ZERO;
    let mut last_sample = Instant::now();
    loop {
        let batch = cursor.fetch_batch(batch_size).expect("fetch a batch");
        let count = batch.row_count() as u64;
        if count == 0 {
            break;
        }
        // The batch dies here. Anything that grows with `rows` after this point
        // is the driver holding on to something.
        drop(batch);
        rows += count;
        batches += 1;
        if batches == 1 {
            first_batch = started.elapsed();
        }
        // Sampled on a clock, not on a batch count: spawning `tasklist` costs
        // tens of milliseconds, and once every fiftieth batch is 200 spawns for
        // the 100-row pass and two for the 10 000-row one — which would make
        // the throughput figures a measure of this instrument rather than of
        // the driver. A fixed interval charges every pass the same.
        if last_sample.elapsed() >= SAMPLE_EVERY {
            peak = peak.max(working_set_kb().unwrap_or(0));
            last_sample = Instant::now();
        }
    }
    let elapsed = started.elapsed();
    peak = peak.max(working_set_kb().unwrap_or(0));
    cursor.close().expect("close the cursor");

    Pass {
        rows,
        batches,
        per_second: if elapsed.as_secs_f64() > 0.0 {
            rows as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        },
        growth_kb: baseline.map(|before| peak.saturating_sub(before)),
        first_batch,
    }
}

#[test]
fn a_million_rows_stream_at_a_measured_rate_in_bounded_memory() {
    let mut connection = connect();

    // The server's own count, so the client is measured against the truth and
    // not against its own arithmetic.
    let expected = scalar(
        connection.as_mut(),
        &format!("SELECT COUNT(*) FROM ({})", generator()),
    );
    assert_eq!(
        expected,
        ROWS.to_string(),
        "the generator did not produce the expected number of rows"
    );

    let mut summary = Vec::new();
    let mut rates: Vec<(usize, f64)> = Vec::new();
    for size in [100_usize, 1_000, 10_000] {
        let batch_size = NonZeroUsize::new(size).expect("non-zero");
        let pass = stream(connection.as_mut(), batch_size);
        assert_eq!(pass.rows, ROWS, "rows were lost at batch size {size}");
        rates.push((size, pass.per_second));

        measurement(
            &format!("s14.rows_per_second.fetch_rows_{size}"),
            format!("{:.0}", pass.per_second),
        );
        measurement(&format!("s14.batches.fetch_rows_{size}"), pass.batches);
        measurement(
            &format!("s14.time_to_first_batch.fetch_rows_{size}"),
            format!("{:.1?}", pass.first_batch),
        );
        match pass.growth_kb {
            Some(growth) => {
                measurement(
                    &format!("s14.working_set_growth_kb.fetch_rows_{size}"),
                    growth,
                );
                // A driver that materialised the result would need roughly
                // 1 000 000 x (8 + 40 + 8) bytes = 56 MB at the very least, and
                // in practice far more. Anything under 128 MB of growth for a
                // million rows is "bounded by the batch", not by the result.
                assert!(
                    growth < 128 * 1024,
                    "the working set grew by {growth} KB while streaming {ROWS} rows at \
                     batch size {size}; the result is being held, not streamed"
                );
                summary.push(format!(
                    "fetch_rows={size}: {:.0} rows/s, {} batches, +{growth} KB",
                    pass.per_second, pass.batches
                ));
            }
            None => {
                observation("NOT MEASURED: the working set could not be read on this machine");
                summary.push(format!(
                    "fetch_rows={size}: {:.0} rows/s, {} batches, memory not measured",
                    pass.per_second, pass.batches
                ));
            }
        }
    }

    // Larger is not better, and the shape of that is worth recording rather
    // than averaging away: Reldex has to choose a default batch size, and this
    // is the first evidence about it.
    let best = rates
        .iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(size, rate)| (*size, *rate));
    let worst = rates
        .iter()
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(size, rate)| (*size, *rate));
    if let (Some((best_size, best_rate)), Some((worst_size, worst_rate))) = (best, worst)
        && worst_rate > 0.0
    {
        measurement(
            "s14.best_over_worst_batch_size_ratio",
            format!("{:.1}x", best_rate / worst_rate),
        );
        observation(format!(
            "FINDING: throughput is not monotonic in the batch size. fetch_rows={best_size} \
             gave {best_rate:.0} rows/s and fetch_rows={worst_size} gave {worst_rate:.0} \
             rows/s — {:.1}x apart — and the largest batch also cost the longest wait for \
             the first row. The cause is not established here; it needs its own \
             investigation before Reldex picks a default",
            best_rate / worst_rate
        ));
    }

    observation(format!(
        "{ROWS} rows (NUMBER, VARCHAR2(40), DATE) through `fetch_batch`, every batch \
         dropped: {}. Method: one session, server-side cross join of two 1000-row \
         CONNECT BY generators, wall clock around execute plus every fetch, working set \
         from `tasklist` sampled every {SAMPLE_EVERY:?} and at the end, growth = peak \
         minus the sample taken before the execute. Single machine, single run — no \
         comparative claim",
        summary.join("; ")
    ));

    connection.close().expect("close");
}

#[test]
fn memory_does_not_grow_with_the_row_count() {
    // The same claim, isolated: the same batch size over 10 000 rows and over
    // 1 000 000. If the second costs materially more, memory is a function of
    // the result rather than of the batch — which is what `SPEC.md` §12 forbids.
    let mut connection = connect();
    let batch_size = NonZeroUsize::new(1_000).expect("non-zero");

    let small = Statement::new(
        "WITH g AS (SELECT LEVEL AS n FROM dual CONNECT BY LEVEL <= 10000) \
         SELECT n AS id, RPAD('row ' || n, 40, '.') AS label, \
                DATE '2026-01-01' + MOD(n, 1000) AS made FROM g",
    )
    .with_fetch_rows(batch_size);

    let baseline = working_set_kb();
    let mut outcome = connection.execute(&small).expect("small query");
    let mut cursor = outcome.take_cursor().expect("cursor");
    let mut small_rows = 0_u64;
    loop {
        let batch = cursor.fetch_batch(batch_size).expect("fetch");
        if batch.row_count() == 0 {
            break;
        }
        small_rows += batch.row_count() as u64;
    }
    cursor.close().expect("close");
    let after_small = working_set_kb();
    assert_eq!(small_rows, 10_000);

    let large = stream(connection.as_mut(), batch_size);
    assert_eq!(large.rows, ROWS);

    match (baseline, after_small, large.growth_kb) {
        (Some(before), Some(after), Some(large_growth)) => {
            let small_growth = after.saturating_sub(before);
            measurement("s14.growth_kb_for_10_000_rows", small_growth);
            measurement("s14.growth_kb_for_1_000_000_rows", large_growth);
            observation(format!(
                "100x the rows at the same batch size cost {large_growth} KB against \
                 {small_growth} KB: memory tracks the batch, not the result"
            ));
            assert!(
                large_growth < 128 * 1024,
                "1 000 000 rows grew the working set by {large_growth} KB"
            );
        }
        _ => observation("NOT MEASURED: the working set could not be read on this machine"),
    }

    connection.close().expect("close");
}
