//! Spikes S7 (large-object streaming) and S9 (concurrent sessions).
//!
//! S7's kill criterion is memory: a 100 MB `CLOB` must be readable without the
//! client holding 100 MB, because `SPEC.md` §12 forbids materialising a value
//! the user only wants to look at. S9 checks that eight sessions can work at
//! once, which is what ADR-0002's one-thread-per-session model assumes.

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "spike results are measurements a human reads from the test output"
)]

mod common;

use std::process::Command;
use std::thread;
use std::time::Instant;

use common::{connect, exec, exec_quietly, measurement, observation, params, try_connect, unique};
use reldex_db_driver_api::{LobKind, Statement, ValueRef};

/// Megabytes written into each large object.
const MEGABYTES: usize = 100;

/// The chunk each `read_chunk` is given. Bounded on purpose: this is the number
/// that decides the client's memory use, not the object's size.
const CHUNK: usize = 64 * 1024;

/// This process's working set in kilobytes, as Windows reports it.
///
/// Read from `tasklist` rather than through a crate: one process-spawn per
/// measurement is cheaper than a dependency that would then ship in every
/// build.
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

#[test]
fn a_hundred_megabyte_clob_and_blob_stream_in_bounded_memory() {
    let mut connection = connect();
    let table = unique("s7_lob");
    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(5), c CLOB, b BLOB)"),
    );

    // Built on the server so the size being measured is the *read* path, not a
    // 100 MB literal pushed over the wire.
    let built = Instant::now();
    exec(
        connection.as_mut(),
        &format!(
            "DECLARE \
               c CLOB; b BLOB; \
               one_kb VARCHAR2(1024) := RPAD('x', 1024, 'x'); \
             BEGIN \
               INSERT INTO {table} VALUES (1, EMPTY_CLOB(), EMPTY_BLOB()) \
                 RETURNING c, b INTO c, b; \
               FOR i IN 1 .. {} LOOP \
                 DBMS_LOB.WRITEAPPEND(c, 1024, one_kb); \
                 DBMS_LOB.WRITEAPPEND(b, 1024, UTL_RAW.CAST_TO_RAW(one_kb)); \
               END LOOP; \
               COMMIT; \
             END;",
            MEGABYTES * 1024
        ),
    );
    measurement("s7.build_time", format!("{:.1?}", built.elapsed()));

    let baseline = working_set_kb();
    let mut peak = baseline.unwrap_or(0);

    for (column, label, expected_kind) in [
        (1_usize, "CLOB", LobKind::Character),
        (2, "BLOB", LobKind::Binary),
    ] {
        let mut outcome = connection
            .execute(&Statement::new(format!(
                "SELECT id, c, b FROM {table} WHERE id = 1"
            )))
            .expect("select the large objects");
        let mut cursor = outcome.take_cursor().expect("cursor");
        let mut batch = cursor
            .fetch_batch(std::num::NonZeroUsize::MIN)
            .expect("fetch");

        // A large object arrives as a locator, not as data.
        assert!(
            matches!(batch.value(0, column), Some(ValueRef::Lob(_))),
            "{label} was not delivered as a locator"
        );
        let mut locator = batch
            .column_mut(column)
            .and_then(|c| c.take_lob(0))
            .unwrap_or_else(|| panic!("{label} locator"));
        assert_eq!(locator.kind(), expected_kind);
        let hint = locator.size_hint();

        let started = Instant::now();
        let mut buffer = vec![0_u8; CHUNK];
        let mut total = 0_u64;
        let mut chunks = 0_u64;
        loop {
            let read = locator.read_chunk(&mut buffer).expect("read a chunk");
            if read == 0 {
                break;
            }
            assert!(read <= CHUNK, "a chunk overflowed the caller's buffer");
            total += read as u64;
            chunks += 1;
            if chunks % 200 == 0 {
                peak = peak.max(working_set_kb().unwrap_or(0));
            }
        }
        let elapsed = started.elapsed();
        peak = peak.max(working_set_kb().unwrap_or(0));

        measurement(&format!("s7.{label}_bytes_read"), total);
        measurement(&format!("s7.{label}_chunks"), chunks);
        measurement(&format!("s7.{label}_read_time"), format!("{elapsed:.1?}"));
        observation(format!(
            "{label} size_hint = {hint:?}, bytes read = {total}"
        ));
        assert_eq!(
            total,
            (MEGABYTES * 1024 * 1024) as u64,
            "{label} did not stream its whole content"
        );

        // A finished stream keeps answering zero rather than failing.
        assert_eq!(locator.read_chunk(&mut buffer).expect("after the end"), 0);
        cursor.close().expect("close");
    }

    if let (Some(before), true) = (baseline, peak > 0) {
        let growth = peak.saturating_sub(before);
        measurement("s7.working_set_before_kb", before);
        measurement("s7.working_set_peak_kb", peak);
        measurement("s7.working_set_growth_kb", growth);
        // 200 MB of data passed through a 64 KiB window. Anything approaching
        // the object's own size would mean the driver materialised it.
        assert!(
            growth < 32 * 1024,
            "the working set grew by {growth} KB while streaming {} MB; the driver is \
             holding the object rather than streaming it",
            MEGABYTES * 2
        );
        observation(format!(
            "streaming {} MB grew this process's working set by {growth} KB",
            MEGABYTES * 2
        ));
    } else {
        observation("NOT MEASURED: the working set could not be read on this machine");
    }

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn a_lob_read_after_its_connection_closes_reports_rather_than_panics() {
    let mut connection = connect();
    let table = unique("s7_life");
    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (c CLOB)"),
    );
    exec(
        connection.as_mut(),
        &format!("INSERT INTO {table} VALUES ('a small value')"),
    );
    connection.commit().expect("commit");

    let mut outcome = connection
        .execute(&Statement::new(format!("SELECT c FROM {table}")))
        .expect("select");
    let mut cursor = outcome.take_cursor().expect("cursor");
    let mut batch = cursor
        .fetch_batch(std::num::NonZeroUsize::MIN)
        .expect("fetch");
    let mut locator = batch
        .column_mut(0)
        .and_then(|c| c.take_lob(0))
        .expect("locator");

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");

    let mut buffer = [0_u8; 64];
    let error = match locator.read_chunk(&mut buffer) {
        Ok(_) => panic!("reading over a closed connection must fail"),
        Err(error) => error,
    };
    observation(format!("LOB read after connection close -> {error}"));
}

#[test]
fn eight_sessions_can_work_at_the_same_time() {
    let table = unique("s9_conc");
    let mut setup = connect();
    exec(
        setup.as_mut(),
        &format!("CREATE TABLE {table} (worker NUMBER(5), n NUMBER(10))"),
    );

    let started = Instant::now();
    let workers: Vec<_> = (0..8_i64)
        .map(|worker| {
            let table = table.clone();
            thread::spawn(move || {
                let mut connection = match try_connect(&params()) {
                    Ok(connection) => connection,
                    Err(error) => panic!("worker {worker}: {error}"),
                };
                for n in 0..50_i64 {
                    exec(
                        connection.as_mut(),
                        &format!("INSERT INTO {table} VALUES ({worker}, {n})"),
                    );
                }
                connection.commit().expect("commit");
                let value = common::scalar(
                    connection.as_mut(),
                    &format!("SELECT COUNT(*) FROM {table} WHERE worker = {worker}"),
                );
                connection.close().expect("close");
                value
            })
        })
        .collect();

    for (worker, handle) in workers.into_iter().enumerate() {
        let count = handle
            .join()
            .unwrap_or_else(|_| panic!("worker {worker} panicked"));
        assert_eq!(count, "50", "worker {worker} lost rows");
    }
    let elapsed = started.elapsed();

    let total = common::scalar(setup.as_mut(), &format!("SELECT COUNT(*) FROM {table}"));
    assert_eq!(total, "400");
    measurement("s9.eight_sessions_400_inserts", format!("{elapsed:.1?}"));
    observation("eight concurrent sessions each inserted 50 rows and read their own back");

    exec_quietly(setup.as_mut(), &format!("DROP TABLE {table} PURGE"));
    setup.close().expect("close");
}
