//! M5.2 (ADR-0004): the Result Store against the live Oracle 19c test
//! database, through `db-core` sessions and the Oracle thin driver.
//!
//! Behind the `oracle-it` feature, so `cargo test --workspace` stays green
//! with no database; run it with
//!
//! ```text
//! RELDEX_IT_PACKAGE=reldex-core-poc bash tools/oracle-test-db/run-it.sh m5_2_result_store_live
//! ```
//!
//! which loads `tools/oracle-test-db/.env` into the environment. Each test
//! skips itself, and says so, when `RELDEX_TEST_ORACLE_DSN` is not set. No
//! credential appears here or in the output.
//!
//! It lives in this crate because it needs both `db-core` and a concrete
//! driver, and a driver crate may not depend on `db-core` (ADR-0002).
#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "the measurements are read by a human from the test output"
)]

use std::env;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Instant;

use reldex_db_core::{
    Cap, CapSource, CellValue, CloseDisposition, ConnectionParams, DatabaseSession, EndCause,
    FetchTicket, Fetched, LimitKind, LobCell, LobUnavailable, MoreRows, ResultCaps, ResultPhase,
    ResultPolicy, ResultStore, SegmentData, SegmentReply, SessionManager, Sourced, Statement,
    StatementKind, StoreAction,
};
use reldex_db_driver_api::{Credentials, DbResult, Endpoint, Number, Secret};
use reldex_driver_oracle_thin::OracleThinDriver;

/// Opens a session on the test database, or `None` — and the test skips —
/// when the environment names none.
fn session(test: &str) -> Option<DatabaseSession> {
    let variable = |name: &str| env::var(name).ok().filter(|value| !value.is_empty());
    let (Some(dsn), Some(user), Some(password)) = (
        variable("RELDEX_TEST_ORACLE_DSN"),
        variable("RELDEX_TEST_ORACLE_USER"),
        variable("RELDEX_TEST_ORACLE_PASSWORD"),
    ) else {
        println!("{test}: skipped, RELDEX_TEST_ORACLE_DSN/_USER/_PASSWORD are not set");
        return None;
    };
    let params = ConnectionParams::new(
        Endpoint::ConnectString(dsn),
        Credentials::UserPassword {
            username: user,
            password: Secret::new(password),
        },
    );
    Some(
        SessionManager::new()
            .open_session(Arc::new(OracleThinDriver::new()), params)
            .expect("the test database accepts the session"),
    )
}

fn close(session: DatabaseSession) {
    session
        .close(Some(CloseDisposition::Rollback))
        .expect("the session closes");
}

fn nz(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("non-zero")
}

fn caps(rows: Cap, bytes: Cap) -> ResultCaps {
    ResultCaps::new(
        Sourced::new(rows, CapSource::Application),
        Sourced::new(bytes, CapSource::Application),
    )
}

fn open(session: &DatabaseSession, sql: &str, policy: ResultPolicy) -> ResultStore {
    let outcome = session
        .execute(Statement::new(sql))
        .wait()
        .expect("the query runs");
    ResultStore::new(outcome.result.expect("a cursor"), &outcome.columns, policy)
}

/// Runs every action the store makes due until none is, on the completion
/// path. Returns every request's size.
fn drive(session: &DatabaseSession, store: &mut ResultStore) -> Vec<usize> {
    let mut sizes = Vec::new();
    loop {
        let mut answered: Vec<(FetchTicket, DbResult<SegmentReply>)> = Vec::new();
        store
            .pump(|action| match action {
                StoreAction::Fetch(fetch) => {
                    sizes.push(fetch.max_rows().get());
                    answered.push((fetch.ticket(), session.fetch_segment(fetch).wait()));
                    Ok(())
                }
                StoreAction::CloseResult(result) => session.close_result(result).wait(),
                other => panic!("an action this test does not know: {other:?}"),
            })
            .expect("the session accepts every action");
        if answered.is_empty() {
            return sizes;
        }
        for (ticket, reply) in answered {
            let fetched = store.on_fetched(ticket, reply);
            assert!(
                matches!(fetched, Fetched::Appended { .. }),
                "{fetched:?}: {:?}",
                store.state().phase()
            );
        }
    }
}

fn number(store: &ResultStore, row: usize, column: usize) -> Option<Number> {
    match store.value(row, column).expect("in range") {
        CellValue::Number(value) => {
            let exact = value.to_number();
            assert_eq!(
                value.to_string(),
                exact.to_string(),
                "row {row}: a scaled NUMBER formats as the Number it holds"
            );
            Some(exact)
        }
        CellValue::Null => None,
        other => panic!("row {row} column {column}: {other:?}"),
    }
}

fn decimal(text: &str) -> Number {
    text.parse().expect("a valid decimal")
}

fn report(test: &str, store: &ResultStore, started: Instant, sizes: &[usize]) {
    let state = store.state();
    println!(
        "{test}: {} rows, {} bytes accounted ({:.1} B/row), {} segments, constant-time lookup \
         {}, {} requests (first {:?}, largest {:?}), {:.0} ms",
        state.rows(),
        state.retained_bytes(),
        state.retained_bytes() as f64 / state.retained_rows().max(1) as f64,
        store.segments().count(),
        store.lookup_is_constant_time(),
        sizes.len(),
        sizes.first(),
        sizes.iter().max(),
        started.elapsed().as_secs_f64() * 1e3,
    );
}

/// 100,000 rows whose every column varies per row, fetched to the end, then
/// checked cell by cell.
#[test]
fn a_hundred_thousand_varying_rows_arrive_intact() {
    const TEST: &str = "a_hundred_thousand_varying_rows_arrive_intact";
    let Some(session) = session(TEST) else {
        return;
    };
    let started = Instant::now();
    let mut store = open(
        &session,
        "SELECT LEVEL AS id, \
                CASE WHEN MOD(LEVEL, 100) = 0 THEN NULL \
                     ELSE 'row ' || LEVEL || RPAD('.', MOD(LEVEL, 37), '.') END AS name, \
                DATE '2026-01-01' + MOD(LEVEL, 3000) AS created, \
                LEVEL / 4 AS quarter \
           FROM dual CONNECT BY LEVEL <= 100000",
        ResultPolicy::new(caps(Cap::At(nz(1_000_000)), Cap::At(nz(512 << 20)))),
    );
    store.fetch_all();
    let sizes = drive(&session, &mut store);
    report(TEST, &store, started, &sizes);

    assert!(matches!(store.state().phase(), ResultPhase::Complete));
    assert_eq!(store.row_count(), 100_000);
    for row in 0..store.row_count() {
        let n = row + 1;
        assert_eq!(number(&store, row, 0), Some(decimal(&n.to_string())));
        let name = match store.value(row, 1).expect("in range") {
            CellValue::Text(text) => Some(text),
            CellValue::Null => None,
            other => panic!("row {row}: {other:?}"),
        };
        let expected = (n % 100 != 0).then(|| format!("row {n}{}", ".".repeat(n % 37)));
        assert_eq!(name, expected.as_deref(), "row {row}");
        assert!(matches!(store.value(row, 2), Some(CellValue::Timestamp(_))));
        let quarter = format!("{}.{:02}", n / 4, (n % 4) * 25);
        assert_eq!(number(&store, row, 3), Some(decimal(&quarter)), "row {row}");
    }
    close(session);
}

/// Four `VARCHAR2(4000)` columns: the declared width keeps the first request
/// small, and a byte cap stops the result with its overshoot bounded.
#[test]
fn wide_rows_are_sized_by_declared_width_and_stop_at_the_byte_cap() {
    const TEST: &str = "wide_rows_are_sized_by_declared_width_and_stop_at_the_byte_cap";
    let Some(session) = session(TEST) else {
        return;
    };
    let cap = 16 << 20;
    let started = Instant::now();
    let mut store = open(
        &session,
        "SELECT RPAD('a', 4000, 'a') AS c1, RPAD('b', 4000, 'b') AS c2, \
                RPAD('c', 3000 + MOD(LEVEL, 1000), 'c') AS c3, \
                RPAD(TO_CHAR(LEVEL), 4000, 'd') AS c4 \
           FROM dual CONNECT BY LEVEL <= 20000",
        ResultPolicy::new(caps(Cap::Unlimited, Cap::At(nz(cap)))),
    );
    // The first page and its read-ahead. The store asks for a few dozen
    // rows; the driver's wire array was fixed at execute, before the
    // describe, from the statement's fetch-size hint (ADR-0004 accepted
    // limitation 11). This statement carries none, as the adapter's execute
    // does today, so the array is `oracledb`'s default of 100 rows. Two of
    // the four columns repeat on every row, which TTC compresses, so a wire
    // round trip here is far cheaper than Table 3a's all-distinct rows.
    let mut sizes = drive(&session, &mut store);
    println!(
        "{TEST}: first page {} rows in {:.0} ms",
        store.row_count(),
        started.elapsed().as_secs_f64() * 1e3
    );
    store.fetch_all();
    sizes.extend(drive(&session, &mut store));
    report(TEST, &store, started, &sizes);

    let state = store.state();
    assert!(
        matches!(
            state.phase(),
            ResultPhase::AtLimit {
                limit: LimitKind::Bytes,
                more: MoreRows::Unknown,
                cursor_open: true
            }
        ),
        "{:?}",
        state.phase()
    );
    // 192 KiB over four declared 4000-byte columns: about a dozen rows a
    // round trip, not `results.fetch_rows`' thousand.
    assert!(sizes[0] <= 64, "first request {}", sizes[0]);
    // The overshoot is what the fetches in flight carried: at most two
    // requests of full-width rows (no cell exceeds its declared 4000 bytes),
    // plus the segments' headers and the store's index.
    let largest = sizes.iter().copied().max().unwrap_or(0);
    assert!(
        state.retained_bytes() <= cap + 2 * largest * 16_033 + (64 << 10),
        "{} bytes against a {cap}-byte cap",
        state.retained_bytes()
    );
    assert!(
        state.retained_bytes() + 16_100 > cap,
        "stopped short of the cap"
    );
    let last = store.row_count() - 1;
    match store.value(last, 3) {
        Some(CellValue::Text(text)) => {
            assert_eq!(text.len(), 4000);
            assert!(text.starts_with(&(last + 1).to_string()));
        }
        other => panic!("{other:?}"),
    }
    close(session);
}

/// `LEVEL / 7` needs 40 digits and stays a `Number`; `LEVEL / 2` and
/// `LEVEL / 4` become scaled integers at different scales in different
/// segments, and read back, and format, exactly as the `Number`s they were.
#[test]
fn exact_and_inexact_numbers_read_back_exactly_across_segments() {
    const TEST: &str = "exact_and_inexact_numbers_read_back_exactly_across_segments";
    let Some(session) = session(TEST) else {
        return;
    };
    let started = Instant::now();
    let mut store = open(
        &session,
        "SELECT CASE WHEN LEVEL <= 1000 THEN LEVEL / 2 ELSE LEVEL / 4 END AS exact, \
                LEVEL / 7 AS inexact \
           FROM dual CONNECT BY LEVEL <= 5000",
        ResultPolicy::new(caps(Cap::Unlimited, Cap::Unlimited)).with_fetch_rows(nz(1_000)),
    );
    store.fetch_all();
    let sizes = drive(&session, &mut store);
    report(TEST, &store, started, &sizes);
    assert!(matches!(store.state().phase(), ResultPhase::Complete));
    assert_eq!(store.row_count(), 5_000);

    let mut scales = Vec::new();
    for (segment, _) in store.segments() {
        match segment.column(0).expect("column").data() {
            SegmentData::ScaledNumber { scale, .. } => scales.push(scale),
            other => panic!("LEVEL / 2 and LEVEL / 4 fit an i64: {other:?}"),
        }
        assert!(
            matches!(
                segment.column(1).expect("column").data(),
                SegmentData::Number(_)
            ),
            "LEVEL / 7 does not"
        );
    }
    println!("{TEST}: scales per segment {scales:?}");
    assert!(scales.contains(&1) && scales.contains(&2), "{scales:?}");

    for row in 0..store.row_count() {
        let n = row + 1;
        let expected = if n <= 1000 {
            format!("{}.{}", n / 2, (n % 2) * 5)
        } else {
            format!("{}.{:02}", n / 4, (n % 4) * 25)
        };
        assert_eq!(
            number(&store, row, 0),
            Some(decimal(&expected)),
            "row {row}"
        );
        let inexact = number(&store, row, 1).expect("not NULL");
        if n % 7 == 0 {
            assert_eq!(inexact, decimal(&(n / 7).to_string()), "row {row}");
        }
    }
    close(session);
}

/// A CLOB column: every cell is a handle read on the worker, and SQL NULL
/// stays NULL.
#[test]
fn a_lob_column_is_read_through_parked_handles() {
    const TEST: &str = "a_lob_column_is_read_through_parked_handles";
    let Some(session) = session(TEST) else {
        return;
    };
    let mut store = open(
        &session,
        "SELECT LEVEL AS id, \
                CASE WHEN MOD(LEVEL, 10) = 0 THEN NULL \
                     ELSE TO_CLOB(RPAD('x', 100 + LEVEL, 'x')) END AS doc \
           FROM dual CONNECT BY LEVEL <= 50",
        ResultPolicy::new(caps(Cap::Unlimited, Cap::Unlimited)),
    );
    store.fetch_all();
    drive(&session, &mut store);
    assert!(matches!(store.state().phase(), ResultPhase::Complete));
    assert_eq!(store.row_count(), 50);
    for row in 0..50 {
        let n = row + 1;
        if n % 10 == 0 {
            assert_eq!(store.value(row, 1), Some(CellValue::Null), "row {row}");
            assert_eq!(store.lob(row, 1), None, "row {row}");
            continue;
        }
        let Some(LobCell::Readable(lob)) = store.lob(row, 1) else {
            panic!("row {row}: {:?}", store.lob(row, 1));
        };
        let mut text = Vec::new();
        loop {
            let chunk = session.read_lob_chunk(lob, nz(64)).wait().expect("a chunk");
            if chunk.is_empty() {
                break;
            }
            text.extend_from_slice(&chunk);
        }
        assert_eq!(text, vec![b'x'; 100 + n], "row {row}");
    }
    close(session);
}

/// A `COMMIT` typed as text ends the transaction: the worker releases the
/// cursor and every parked LOB (ADR-0002 X1), and the store says so —
/// `Ended{TransactionEnded}`, its prefix readable, its LOB cells unavailable
/// rather than NULL.
#[test]
fn a_typed_commit_ends_an_open_result_and_its_lob_cells() {
    const TEST: &str = "a_typed_commit_ends_an_open_result_and_its_lob_cells";
    let Some(session) = session(TEST) else {
        return;
    };
    let mut store = open(
        &session,
        "SELECT LEVEL AS id, TO_CLOB('doc ' || LEVEL) AS doc \
           FROM dual CONNECT BY LEVEL <= 10000",
        ResultPolicy::new(caps(Cap::Unlimited, Cap::Unlimited)).with_fetch_rows(nz(100)),
    );
    drive(&session, &mut store);
    assert!(matches!(store.state().phase(), ResultPhase::Open));
    let shown = store.row_count();
    assert!(shown > 0 && shown < 10_000, "{shown}");
    let Some(LobCell::Readable(lob)) = store.lob(4, 1) else {
        panic!("row 4 holds a readable LOB before the commit");
    };
    let before = session
        .read_lob_chunk(lob, nz(64))
        .wait()
        .expect("readable before the commit");
    assert_eq!(before, b"doc 5");

    let commit = session
        .execute(Statement::new("COMMIT"))
        .wait()
        .expect("COMMIT runs");
    assert_eq!(commit.statement_kind, StatementKind::TransactionControl);
    store.statement_executed(&commit);

    let state = store.state();
    assert!(matches!(
        state.phase(),
        ResultPhase::Ended {
            cause: EndCause::TransactionEnded
        }
    ));
    assert_eq!(state.rows(), shown, "the prefix stays");
    assert_eq!(
        number(&store, shown - 1, 0),
        Some(decimal(&shown.to_string()))
    );
    assert_eq!(
        store.lob(4, 1),
        Some(LobCell::Unavailable(LobUnavailable::TransactionEnded))
    );
    assert_eq!(
        store.value(4, 1),
        Some(CellValue::LobUnavailable(LobUnavailable::TransactionEnded))
    );
    store.set_demand(10_000);
    assert!(
        drive(&session, &mut store).is_empty(),
        "nothing is fetched after the end"
    );

    // The worker agrees: the cursor and the LOB were released with the
    // transaction.
    session
        .read_lob_chunk(lob, nz(64))
        .wait()
        .expect_err("the COMMIT released the parked LOB");
    session
        .fetch_batch(store.result(), nz(10))
        .wait()
        .expect_err("the COMMIT released the cursor");
    println!("{TEST}: {shown} rows kept after the typed COMMIT");
    close(session);
}
