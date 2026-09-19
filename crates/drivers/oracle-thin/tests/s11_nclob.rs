//! Spike S11 — `NCLOB`.
//!
//! `SPEC.md` §8 lists `CLOB/NCLOB/BLOB` together; Phase 0 tested `CLOB` and
//! `BLOB` streaming (S7) and Thai/non-BMP fidelity in `NVARCHAR2` (S2), and
//! inferred `NCLOB` from the two. This file stops inferring.
//!
//! What is actually different about an `NCLOB` is the character set: the
//! server stores it in the **national** character set (`AL16UTF16` on this
//! database), not in the database character set, and `oracledb` reads a
//! character LOB as UCS-2 and re-encodes it. That is one more conversion than a
//! `CLOB` pays, and it is the one that a surrogate pair can fall through.

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "spike results are measurements a human reads from the test output"
)]

mod common;

use std::num::NonZeroUsize;

use common::{connect, exec, exec_quietly, measurement, observation, render, scalar, unique};
use reldex_db_driver_api::{
    DatabaseConnection, LobKind, LobLocator, RowBatch, Statement, ValueRef,
};

/// Thai, a non-BMP emoji, and both mixed with ASCII — the S2 corpus, so the
/// two spikes are comparable.
const THAI: &str = "ทดสอบภาษาไทย";
/// U+1F418, a surrogate pair in UTF-16.
const ELEPHANT: &str = "🐘";

/// Reads a whole LOB through `read_chunk`, returning the bytes and the number
/// of round trips it took.
fn drain(locator: &mut LobLocator, chunk: usize) -> (Vec<u8>, usize) {
    let mut buffer = vec![0_u8; chunk];
    let mut out = Vec::new();
    let mut chunks = 0;
    loop {
        let read = locator.read_chunk(&mut buffer).expect("read a chunk");
        if read == 0 {
            break;
        }
        assert!(read <= chunk, "a chunk overflowed the caller's buffer");
        out.extend_from_slice(&buffer[..read]);
        chunks += 1;
    }
    (out, chunks)
}

/// Selects one row and hands back the batch, so a caller can take its locator.
fn fetch_one(connection: &mut dyn DatabaseConnection, sql: &str) -> RowBatch {
    let mut outcome = connection
        .execute(&Statement::new(sql))
        .unwrap_or_else(|error| panic!("{sql}\n  failed: {error}"));
    let mut cursor = outcome.take_cursor().expect("cursor");
    let batch = cursor.fetch_batch(NonZeroUsize::MIN).expect("fetch");
    cursor.close().expect("close");
    batch
}

#[test]
fn an_nclob_round_trips_thai_and_non_bmp_text_exactly() {
    let mut connection = connect();
    let table = unique("s11_nc");
    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(5), n NCLOB, c CLOB)"),
    );

    // Written from the server side, so the comparison is against the server's
    // own bytes rather than against a bind path this spike is not testing.
    let text = format!("{THAI}{ELEPHANT}a{THAI}b{ELEPHANT}");
    exec(
        connection.as_mut(),
        &format!("INSERT INTO {table} VALUES (1, TO_NCLOB(N'{text}'), TO_CLOB('{text}'))"),
    );
    connection.commit().expect("commit");

    // The server's own measurements first: this is the yardstick.
    let server_length = scalar(
        connection.as_mut(),
        &format!("SELECT DBMS_LOB.GETLENGTH(n) FROM {table} WHERE id = 1"),
    );
    let server_dump = scalar(
        connection.as_mut(),
        &format!("SELECT DUMP(TO_CHAR(SUBSTR(n, 1, 6)), 1016) FROM {table} WHERE id = 1"),
    );
    observation(format!(
        "server: NCLOB length = {server_length} (national-character-set units), first six \
         characters {server_dump}"
    ));

    let mut batch = fetch_one(
        connection.as_mut(),
        &format!("SELECT n, c FROM {table} WHERE id = 1"),
    );
    assert!(
        matches!(batch.value(0, 0), Some(ValueRef::Lob(_))),
        "an NCLOB must arrive as a locator, not as a materialised value"
    );

    let mut national = batch
        .column_mut(0)
        .and_then(|column| column.take_lob(0))
        .expect("an NCLOB locator");
    // The contract distinguishes the two character kinds, and the driver
    // reports the national one rather than collapsing it into `Character`: a
    // UI that wants to say "NCLOB" can, and the S7 rule that a character LOB
    // publishes no byte size hint still applies to both.
    assert_eq!(
        national.kind(),
        LobKind::NationalCharacter,
        "an NCLOB must be reported as a national character LOB, not as a CLOB"
    );
    assert!(national.kind().is_character());
    let (bytes, chunks) = drain(&mut national, 64 * 1024);
    let read = String::from_utf8(bytes).expect("an NCLOB must arrive as valid UTF-8");
    assert_eq!(read, text, "the NCLOB did not round-trip exactly");
    observation(format!(
        "NCLOB read back byte-exact: {} UTF-8 bytes in {chunks} chunk(s), Thai and a \
         surrogate pair included",
        read.len()
    ));

    // And the same content in a CLOB, so any difference is attributable.
    let mut database = batch
        .column_mut(1)
        .and_then(|column| column.take_lob(0))
        .expect("a CLOB locator");
    let (clob_bytes, _) = drain(&mut database, 64 * 1024);
    assert_eq!(
        String::from_utf8(clob_bytes).expect("UTF-8"),
        text,
        "the CLOB and the NCLOB disagree about the same text"
    );

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn an_nclob_survives_a_surrogate_pair_landing_on_a_chunk_boundary() {
    let mut connection = connect();
    let table = unique("s11_edge");
    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(5), n NCLOB)"),
    );

    // A long run of ASCII followed by the emoji, repeated: with a caller buffer
    // chosen to land inside the surrogate pair, a naive implementation either
    // splits the character or returns invalid UTF-8.
    exec(
        connection.as_mut(),
        &format!(
            "DECLARE n NCLOB; BEGIN \
               INSERT INTO {table} VALUES (1, EMPTY_CLOB()) RETURNING n INTO n; \
               FOR i IN 1 .. 400 LOOP \
                 DBMS_LOB.WRITEAPPEND(n, 21, TO_NCLOB(N'{}' || N'{ELEPHANT}')); \
               END LOOP; \
               COMMIT; END;",
            "x".repeat(19)
        ),
    );

    let expected = format!("{}{ELEPHANT}", "x".repeat(19)).repeat(400);
    // Several buffer sizes, including ones that cannot hold a whole character
    // run: the boundary has to move through the emoji.
    for chunk in [16_usize, 17, 23, 64, 100, 4096] {
        let mut batch = fetch_one(
            connection.as_mut(),
            &format!("SELECT n FROM {table} WHERE id = 1"),
        );
        let mut locator = batch
            .column_mut(0)
            .and_then(|column| column.take_lob(0))
            .expect("an NCLOB locator");
        let (bytes, chunks) = drain(&mut locator, chunk);
        let read = String::from_utf8(bytes)
            .unwrap_or_else(|error| panic!("chunk {chunk}: not valid UTF-8 — {error}"));
        assert_eq!(
            read, expected,
            "chunk {chunk}: the NCLOB came back different"
        );
        measurement(&format!("s11.nclob_chunks_for_{chunk}_byte_buffer"), chunks);
    }
    observation(
        "an NCLOB carrying a surrogate pair every 20 characters read back exactly through \
         buffers of 16, 17, 23, 64, 100 and 4096 bytes; no boundary split a character and \
         no read produced invalid UTF-8",
    );

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn a_null_nclob_and_an_empty_one_are_different_things() {
    let mut connection = connect();
    let table = unique("s11_null");
    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(5), n NCLOB)"),
    );
    exec(
        connection.as_mut(),
        &format!("INSERT INTO {table} VALUES (1, NULL)"),
    );
    exec(
        connection.as_mut(),
        &format!("INSERT INTO {table} VALUES (2, EMPTY_CLOB())"),
    );
    exec(
        connection.as_mut(),
        &format!("INSERT INTO {table} VALUES (3, TO_NCLOB(N'{THAI}'))"),
    );
    connection.commit().expect("commit");

    // The server's own view, for comparison.
    let server = scalar(
        connection.as_mut(),
        &format!(
            "SELECT LISTAGG(id || '=' || NVL(TO_CHAR(DBMS_LOB.GETLENGTH(n)), 'null'), ' ') \
             WITHIN GROUP (ORDER BY id) FROM {table}"
        ),
    );
    observation(format!("server: DBMS_LOB.GETLENGTH per row -> {server}"));

    // Three rows in one batch, so the NULL, the empty locator and the filled
    // one are compared side by side inside a single fetch.
    let mut outcome = connection
        .execute(&Statement::new(format!(
            "SELECT id, n FROM {table} ORDER BY id"
        )))
        .expect("select");
    let mut cursor = outcome.take_cursor().expect("cursor");
    let mut batch = cursor
        .fetch_batch(NonZeroUsize::new(3).expect("non-zero"))
        .expect("fetch");
    cursor.close().expect("close");
    assert_eq!(batch.row_count(), 3);

    // Row 1: SQL NULL. Not an empty locator, not an empty string.
    assert!(
        matches!(batch.value(0, 1), Some(ValueRef::Null)),
        "a NULL NCLOB must read as NULL, not as an empty stream; got {}",
        render(batch.value(0, 1).unwrap_or(ValueRef::Null))
    );

    // Row 2: EMPTY_CLOB() — a locator whose stream is empty from the first read.
    assert!(
        matches!(batch.value(1, 1), Some(ValueRef::Lob(_))),
        "EMPTY_CLOB() must arrive as a locator, not as NULL"
    );
    let mut empty = batch
        .column_mut(1)
        .and_then(|column| column.take_lob(1))
        .expect("an empty NCLOB locator");
    let hint = empty.size_hint();
    let (bytes, chunks) = drain(&mut empty, 64);
    assert!(bytes.is_empty(), "EMPTY_CLOB() produced bytes");
    assert_eq!(chunks, 0, "an empty stream should need no chunk at all");
    observation(format!(
        "NULL NCLOB -> ValueRef::Null; EMPTY_CLOB() -> a locator whose first read returns \
         0 with size_hint {hint:?}"
    ));

    // Row 3: content, and the size hint's documented meaning.
    let mut filled = batch
        .column_mut(1)
        .and_then(|column| column.take_lob(2))
        .expect("a filled NCLOB locator");
    let hint = filled.size_hint();
    let (bytes, _) = drain(&mut filled, 64);
    let text = String::from_utf8(bytes).expect("UTF-8");
    assert_eq!(text, THAI);
    assert_eq!(
        hint, None,
        "a character LOB's server-side length is counted in UCS-2 units, so the contract's \
         byte hint must stay None (the CLOB rule from S7 applies to NCLOB too)"
    );
    measurement("s11.thai_nclob_utf8_bytes", text.len());
    observation(format!(
        "12 Thai characters occupy {} UTF-8 bytes on this side and are reported with no \
         byte size hint, exactly as a CLOB is",
        text.len()
    ));

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn a_large_nclob_streams_in_bounded_chunks_like_a_clob() {
    let mut connection = connect();
    let table = unique("s11_big");
    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(5), n NCLOB)"),
    );
    // 4 MB of Thai: enough round trips for the per-chunk arithmetic to show,
    // small enough not to add a minute to the suite.
    exec(
        connection.as_mut(),
        &format!(
            "DECLARE n NCLOB; k NVARCHAR2(600) := TO_NCLOB(N'{}'); BEGIN \
               INSERT INTO {table} VALUES (1, EMPTY_CLOB()) RETURNING n INTO n; \
               FOR i IN 1 .. 4000 LOOP DBMS_LOB.WRITEAPPEND(n, 300, k); END LOOP; \
               COMMIT; END;",
            THAI.repeat(25)
        ),
    );

    let characters = scalar(
        connection.as_mut(),
        &format!("SELECT DBMS_LOB.GETLENGTH(n) FROM {table} WHERE id = 1"),
    );
    let mut batch = fetch_one(
        connection.as_mut(),
        &format!("SELECT n FROM {table} WHERE id = 1"),
    );
    let mut locator = batch
        .column_mut(0)
        .and_then(|column| column.take_lob(0))
        .expect("locator");
    let started = std::time::Instant::now();
    let (bytes, chunks) = drain(&mut locator, 64 * 1024);
    let elapsed = started.elapsed();
    let text = String::from_utf8(bytes).expect("UTF-8");

    assert_eq!(
        text.chars().count(),
        1_200_000,
        "the NCLOB lost or gained characters"
    );
    assert!(
        text.chars().all(|c| THAI.contains(c)),
        "the NCLOB came back with characters that were never written"
    );
    measurement("s11.large_nclob_server_length", &characters);
    measurement("s11.large_nclob_utf8_bytes", text.len());
    measurement("s11.large_nclob_chunks", chunks);
    measurement("s11.large_nclob_read_time", format!("{elapsed:.1?}"));
    observation(format!(
        "a {characters}-character NCLOB streamed as {} UTF-8 bytes in {chunks} chunks of \
         at most 64 KiB in {elapsed:.1?}; the caller's buffer bounds the memory, as S7 \
         found for CLOB",
        text.len()
    ));

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}
