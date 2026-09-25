//! Manual benchmark for the review must-fix on `crates/workspace/src/store/history.rs`
//! (M4.10/M6.2 fix round, 2026-09-25): the FIFO trim must be O(1) amortized
//! per insert, independent of both `history.max_entries_per_profile` and the
//! table's row count, not the O(min(rows, limit)) the original design was.
//!
//! Not run by `cargo test` (ignored) and not part of any gate: it measures
//! wall-clock time, which is noisy under `cargo test`'s default debug
//! profile and parallel test execution. Run explicitly, alone, in release:
//!
//! ```text
//! cargo test --release -p reldex-workspace --test history_trim_benchmark \
//!     -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Compares two trim strategies side by side, both implemented here with the
//! exact SQL `crates/workspace/src/store/history.rs` uses (or used):
//!
//! - **old** — the original design: `DELETE … WHERE id NOT IN (SELECT id …
//!   ORDER BY id DESC LIMIT ?limit)`, re-deriving "keep the newest `limit`"
//!   on every insert.
//! - **new** — the fix: a maintained `history_meta(profile_id, count)`
//!   counter (schema 4); a steady-state insert deletes exactly the rows
//!   pushed past the bound (1, once caught up) by an index-bound `ORDER BY
//!   id ASC LIMIT k`.
//!
//! Against both an in-memory connection and a real file with
//! `synchronous = FULL` (`Store::open`'s own setting), at several row counts
//! already in the table and several `history.max_entries_per_profile`
//! values, including "no limit". Results (with the exact numbers this
//! benchmark produced on the development machine) are recorded in the
//! ADR-0006 amendment and the PR description — this file is the harness
//! that produced them, kept so the comparison can be reproduced or re-run
//! after a further change.

// A manual, human-read benchmark report: printing its table to stdout is the
// point, not a leftover debug trace this lint is meant to catch.
#![allow(clippy::print_stdout)]

mod support;

use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use support::TempDir;

/// Schema fragment shared by both strategies: enough of `profile` to satisfy
/// the foreign keys `history`/`history_meta` declare, and `history` itself,
/// byte-for-byte the same DDL as schema 2
/// (`crates/workspace/src/store/schema.rs`'s `V2`), so the insert this
/// benchmark times pays exactly the indexing cost production pays.
const SCHEMA: &str = "
CREATE TABLE profile (
    id TEXT NOT NULL COLLATE NOCASE PRIMARY KEY
) STRICT;

CREATE TABLE history (
    id          INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
    profile_id  TEXT    NOT NULL COLLATE NOCASE REFERENCES profile (id) ON DELETE CASCADE,
    executed_at INTEGER NOT NULL,
    statement   TEXT    NOT NULL,
    outcome     TEXT    NOT NULL CHECK (outcome IN ('succeeded', 'failed', 'cancelled', 'timed_out')),
    native_code INTEGER,
    elapsed_ms  INTEGER NOT NULL,
    row_count   INTEGER,
    CHECK ((outcome = 'failed') OR (native_code IS NULL))
) STRICT;

CREATE INDEX history_profile_id_idx ON history (profile_id, id);

CREATE TABLE history_meta (
    profile_id TEXT    NOT NULL COLLATE NOCASE PRIMARY KEY
        REFERENCES profile (id) ON DELETE CASCADE,
    count      INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
";

const PROFILE_ID: &str = "11111111-1111-1111-1111-111111111111";

fn schema(connection: &Connection) {
    connection.execute_batch(SCHEMA).expect("schema");
    connection
        .execute("INSERT INTO profile (id) VALUES (?1)", params![PROFILE_ID])
        .expect("seed profile");
}

fn open_memory() -> Connection {
    let connection = Connection::open_in_memory().expect("open :memory:");
    connection
        .pragma_update(None, "synchronous", "FULL")
        .expect("synchronous");
    connection
        .pragma_update(None, "foreign_keys", true)
        .expect("foreign_keys");
    schema(&connection);
    connection
}

fn open_file(dir: &TempDir) -> Connection {
    let connection = Connection::open(dir.store_path()).expect("open file");
    connection
        .busy_timeout(Duration::from_secs(5))
        .expect("busy_timeout");
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .expect("journal_mode");
    connection
        .pragma_update(None, "synchronous", "FULL")
        .expect("synchronous");
    connection
        .pragma_update(None, "foreign_keys", true)
        .expect("foreign_keys");
    schema(&connection);
    connection
}

/// Inserts `count` history rows directly, in one transaction, with **no**
/// trim — fixture setup, not the operation under measurement. Real (`bulk`
/// == fast) inserts, so populating a large starting table does not itself
/// dominate the benchmark's running time.
fn bulk_populate(connection: &mut Connection, count: u32) {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("begin");
    for n in 0..count {
        transaction
            .execute(
                "INSERT INTO history (profile_id, executed_at, statement, outcome, elapsed_ms) \
                 VALUES (?1, 0, ?2, 'succeeded', 0)",
                params![PROFILE_ID, format!("select {n}")],
            )
            .expect("bulk insert");
    }
    transaction.commit().expect("commit");
}

/// Seeds `history_meta` to match a `bulk_populate`d table — what the real
/// schema-4 migration's backfill does, done here by hand since this harness
/// never calls it.
fn seed_meta(connection: &Connection, count: u32) {
    if count > 0 {
        connection
            .execute(
                "INSERT INTO history_meta (profile_id, count) VALUES (?1, ?2)",
                params![PROFILE_ID, i64::from(count)],
            )
            .expect("seed meta");
    }
}

/// The **old** strategy: `crates/workspace/src/store/history.rs` before this
/// fix. One insert, then (if bounded) one `DELETE … NOT IN (SELECT … ORDER
/// BY id DESC LIMIT ?)` re-deriving "keep the newest `limit`" from scratch.
fn old_insert(connection: &mut Connection, limit: Option<u64>) -> Duration {
    let start = Instant::now();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("begin");
    transaction
        .execute(
            "INSERT INTO history (profile_id, executed_at, statement, outcome, elapsed_ms) \
             VALUES (?1, 0, 'select 1', 'succeeded', 0)",
            params![PROFILE_ID],
        )
        .expect("insert");
    if let Some(limit) = limit {
        transaction
            .execute(
                "DELETE FROM history WHERE profile_id = ?1 AND id NOT IN ( \
                     SELECT id FROM history WHERE profile_id = ?1 ORDER BY id DESC LIMIT ?2)",
                params![PROFILE_ID, i64::try_from(limit).unwrap_or(i64::MAX)],
            )
            .expect("trim");
    }
    transaction.commit().expect("commit");
    start.elapsed()
}

/// The **new** strategy: the fix. One insert, one upsert of the maintained
/// counter (`RETURNING` its new value), then — only when over the bound — an
/// index-bound delete of exactly the rows pushed past it.
fn new_insert(connection: &mut Connection, limit: Option<u64>) -> Duration {
    let start = Instant::now();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("begin");
    transaction
        .execute(
            "INSERT INTO history (profile_id, executed_at, statement, outcome, elapsed_ms) \
             VALUES (?1, 0, 'select 1', 'succeeded', 0)",
            params![PROFILE_ID],
        )
        .expect("insert");
    let count: i64 = transaction
        .query_row(
            "INSERT INTO history_meta (profile_id, count) VALUES (?1, 1) \
             ON CONFLICT (profile_id) DO UPDATE SET count = count + 1 \
             RETURNING count",
            params![PROFILE_ID],
            |row| row.get(0),
        )
        .expect("upsert meta");
    if let Some(limit) = limit {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        if count > limit {
            let excess = count - limit;
            transaction
                .execute(
                    "DELETE FROM history WHERE id IN ( \
                         SELECT id FROM history WHERE profile_id = ?1 ORDER BY id ASC LIMIT ?2)",
                    params![PROFILE_ID, excess],
                )
                .expect("trim");
            transaction
                .execute(
                    "UPDATE history_meta SET count = count - ?2 WHERE profile_id = ?1",
                    params![PROFILE_ID, excess],
                )
                .expect("update meta");
        }
    }
    transaction.commit().expect("commit");
    start.elapsed()
}

const WARMUP: u32 = 3;
const MEASURED: usize = 25;

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn fmt(duration: Duration) -> String {
    let micros = duration.as_secs_f64() * 1_000_000.0;
    if micros >= 1_000.0 {
        format!("{:.2} ms", micros / 1_000.0)
    } else {
        format!("{micros:.1} \u{b5}s")
    }
}

#[derive(Clone, Copy)]
enum Strategy {
    Old,
    New,
}

impl Strategy {
    const fn label(self) -> &'static str {
        match self {
            Self::Old => "old (pre-fix)",
            Self::New => "new (fix)",
        }
    }

    fn run(self, connection: &mut Connection, limit: Option<u64>) -> Duration {
        match self {
            Self::Old => old_insert(connection, limit),
            Self::New => new_insert(connection, limit),
        }
    }
}

fn limit_label(limit: Option<u64>) -> String {
    match limit {
        Some(value) => value.to_string(),
        None => "unlimited".to_owned(),
    }
}

/// One (backend, strategy, checkpoint, limit) cell: a fresh schema, bulk
/// population to `rows_before` (untimed), then `WARMUP` discarded inserts
/// followed by `MEASURED` timed ones, reporting the median.
fn measure_cell(
    connection: &mut Connection,
    strategy: Strategy,
    rows_before: u32,
    limit: Option<u64>,
) -> Duration {
    bulk_populate(connection, rows_before);
    if matches!(strategy, Strategy::New) {
        seed_meta(connection, rows_before);
    }
    for _ in 0..WARMUP {
        strategy.run(connection, limit);
    }
    let samples = (0..MEASURED)
        .map(|_| strategy.run(connection, limit))
        .collect();
    median(samples)
}

const CHECKPOINTS: [u32; 4] = [0, 10_000, 25_000, 50_000];
const LIMITS: [Option<u64>; 4] = [Some(1_000), Some(100_000), Some(1_000_000), None];

#[test]
#[ignore = "manual benchmark; see this file's module documentation"]
fn history_trim_in_memory_old_vs_new() {
    println!(
        "\n| limit | rows before insert | {} | {} |\n\
           |---|---|---|---|",
        Strategy::Old.label(),
        Strategy::New.label()
    );
    for limit in LIMITS {
        for rows_before in CHECKPOINTS {
            let mut old = open_memory();
            let old_median = measure_cell(&mut old, Strategy::Old, rows_before, limit);
            let mut new = open_memory();
            let new_median = measure_cell(&mut new, Strategy::New, rows_before, limit);
            println!(
                "| {} | {rows_before} | {} | {} |",
                limit_label(limit),
                fmt(old_median),
                fmt(new_median)
            );
        }
    }
}

/// A reduced spot check on a real file (`synchronous = FULL`, WAL): only the
/// smallest and largest checkpoints — fsync-bound commit overhead is roughly
/// constant per transaction and orthogonal to the O(1)-vs-O(n) difference
/// the in-memory grid above already covers exhaustively; this confirms the
/// same shape holds with a real file's extra commit cost, not a second full
/// sweep of it.
#[test]
#[ignore = "manual benchmark; see this file's module documentation"]
fn history_trim_real_file_old_vs_new() {
    println!(
        "\n| limit | rows before insert | {}, real file | {}, real file |\n\
           |---|---|---|---|",
        Strategy::Old.label(),
        Strategy::New.label()
    );
    for limit in LIMITS {
        for rows_before in [0, 50_000] {
            let old_dir = TempDir::new("history-bench-old");
            let mut old = open_file(&old_dir);
            let old_median = measure_cell(&mut old, Strategy::Old, rows_before, limit);
            let new_dir = TempDir::new("history-bench-new");
            let mut new = open_file(&new_dir);
            let new_median = measure_cell(&mut new, Strategy::New, rows_before, limit);
            println!(
                "| {} | {rows_before} | {} | {} |",
                limit_label(limit),
                fmt(old_median),
                fmt(new_median)
            );
        }
    }
}

/// Not a benchmark: proves the two strategies agree on *behavior*, not just
/// that one is faster — same final row count kept, same newest rows
/// survive. A benchmark that quietly measured a strategy with a bug would be
/// worse than no benchmark.
#[test]
fn old_and_new_strategies_keep_the_same_rows() {
    for limit in [Some(3), Some(1_000), None] {
        let mut old = open_memory();
        let mut new = open_memory();
        for _ in 0..10 {
            old_insert(&mut old, limit);
            new_insert(&mut new, limit);
        }
        let old_ids: Vec<i64> = old
            .prepare("SELECT id FROM history ORDER BY id")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        let new_ids: Vec<i64> = new
            .prepare("SELECT id FROM history ORDER BY id")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(old_ids, new_ids, "limit {limit:?}: kept different rows");

        if limit.is_some() {
            let new_count: i64 = new
                .query_row(
                    "SELECT count FROM history_meta WHERE profile_id = ?1",
                    params![PROFILE_ID],
                    |row| row.get(0),
                )
                .optional()
                .expect("query")
                .unwrap_or(0);
            assert_eq!(
                new_count,
                i64::try_from(new_ids.len()).expect("row count fits in i64"),
                "counter drifted from the real row count"
            );
        }
    }
}
