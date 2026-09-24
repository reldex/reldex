//! M2.7 — server output (`DBMS_OUTPUT`) against a real database.
//!
//! What the offline tests cannot show: that the packed read in
//! `src/server_output.rs` returns what the server buffered **byte for byte**
//! (Thai, emoji, combining marks, 32,767-byte lines, empty lines), that it is
//! bounded in round trips, and what `DBMS_OUTPUT` itself does at the edges —
//! clamping, overflow, a failing block, a `PUT` with no line end. Round trips
//! are counted by the server (`v$mystat`, "SQL*Net roundtrips to/from
//! client"), not inferred.

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "M2.7 results are measurements a human reads from the test output"
)]

mod common;

use std::num::{NonZeroU32, NonZeroUsize};

use common::{connect, exec, exec_quietly, measurement, observation, scalar, unique};
use reldex_db_driver_api::{
    DatabaseConnection, DbError, ServerOutputBuffer, ServerOutputSetting, SessionState, Statement,
};

/// db-core's defaults (`SessionLimits`), so what is measured here is what a
/// worksheet pays.
const CHUNK_LINES: usize = 4096;
const CHUNK_BYTES: usize = 32 * 1024;

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).expect("non-zero")
}

fn bytes(n: u32) -> ServerOutputSetting {
    ServerOutputSetting::Enabled(ServerOutputBuffer::Bytes(
        NonZeroU32::new(n).expect("non-zero"),
    ))
}

const UNLIMITED: ServerOutputSetting = ServerOutputSetting::Enabled(ServerOutputBuffer::Unlimited);

fn enable(connection: &mut dyn DatabaseConnection, setting: ServerOutputSetting) {
    let effective = connection
        .set_server_output(setting)
        .expect("DBMS_OUTPUT.ENABLE");
    assert_eq!(effective, setting, "no clamp expected for {setting:?}");
}

/// Reads until the server says the buffer is empty, as db-core's drain does.
/// Returns the lines and how many `take_server_output` calls it took.
fn drain(
    connection: &mut dyn DatabaseConnection,
    max_lines: usize,
    max_bytes: usize,
) -> (Vec<String>, usize) {
    let mut lines = Vec::new();
    let mut calls = 0_usize;
    loop {
        let chunk = connection
            .take_server_output(nz(max_lines), nz(max_bytes))
            .expect("take_server_output");
        calls += 1;
        let stop = chunk.is_drained() || chunk.is_empty();
        lines.extend(chunk.into_lines().into_iter().map(String::from));
        if stop {
            return (lines, calls);
        }
        assert!(calls < 1_000_000, "the drain did not terminate");
    }
}

/// The server's own count of round trips on this session.
fn round_trips(connection: &mut dyn DatabaseConnection) -> i64 {
    scalar(
        connection,
        "SELECT TO_CHAR(m.value) FROM v$mystat m JOIN v$statname n \
         ON m.statistic# = n.statistic# \
         WHERE n.name = 'SQL*Net roundtrips to/from client'",
    )
    .parse()
    .expect("a round-trip count")
}

/// Round trips `work` cost, net of the reading query's own.
fn counted<T>(
    connection: &mut dyn DatabaseConnection,
    work: impl FnOnce(&mut dyn DatabaseConnection) -> T,
) -> (T, i64) {
    let a = round_trips(connection);
    let b = round_trips(connection);
    let overhead = b - a;
    let result = work(connection);
    let c = round_trips(connection);
    (result, c - b - overhead)
}

fn failing(connection: &mut dyn DatabaseConnection, sql: &str) -> DbError {
    match connection.execute(&Statement::new(sql)) {
        Ok(_) => panic!("{sql}\n  was expected to fail"),
        Err(error) => error,
    }
}

fn native_code(error: &DbError) -> i32 {
    error
        .native()
        .map(reldex_db_driver_api::NativeError::code)
        .unwrap_or_else(|| panic!("no native code on {error}"))
}

#[test]
fn thai_emoji_and_32767_byte_lines_come_back_byte_for_byte() {
    let mut connection = connect();
    enable(connection.as_mut(), UNLIMITED);

    let short = [
        "สวัสดีครับ ทดสอบภาษาไทย ๑๒๓",
        "emoji 😀👍🏽 flag 🇹🇭 zwj 👩‍💻",
        "combining e\u{301} and ก\u{e34}\u{e4a}",
        "00012 looks like a length prefix",
    ];
    // 10,922 three-byte Thai characters and one ASCII byte; 8,191 four-byte
    // emoji and three ASCII bytes: both exactly 32,767 bytes, PL/SQL's limit.
    let long_thai = format!("{}x", "ก".repeat(10_922));
    let long_emoji = format!("{}abc", "😀".repeat(8_191));
    assert_eq!(long_thai.len(), 32_767);
    assert_eq!(long_emoji.len(), 32_767);

    let block = format!(
        "DECLARE l VARCHAR2(32767); BEGIN \
         DBMS_OUTPUT.PUT_LINE('{0}'); \
         DBMS_OUTPUT.PUT_LINE('{1}'); \
         FOR i IN 1..10922 LOOP l := l || 'ก'; END LOOP; \
         DBMS_OUTPUT.PUT_LINE(l || 'x'); \
         DBMS_OUTPUT.PUT_LINE('{2}'); \
         l := NULL; FOR i IN 1..8191 LOOP l := l || '😀'; END LOOP; \
         DBMS_OUTPUT.PUT_LINE(l || 'abc'); \
         DBMS_OUTPUT.PUT_LINE('a' || CHR(10) || 'b'); \
         DBMS_OUTPUT.PUT_LINE('{3}'); \
         END;",
        short[0], short[1], short[2], short[3]
    );
    let expected: Vec<String> = vec![
        short[0].to_owned(),
        short[1].to_owned(),
        long_thai.clone(),
        short[2].to_owned(),
        long_emoji.clone(),
        "a\nb".to_owned(),
        short[3].to_owned(),
    ];

    // The same output read three ways: db-core's chunk sizes, a budget small
    // enough that most lines travel as the unframed tail, and one line per call.
    for (max_lines, max_bytes) in [(CHUNK_LINES, CHUNK_BYTES), (3, 64), (1, 1)] {
        exec(connection.as_mut(), &block);
        let (lines, calls) = drain(connection.as_mut(), max_lines, max_bytes);
        assert_eq!(lines.len(), expected.len(), "({max_lines}, {max_bytes})");
        for (index, (got, want)) in lines.iter().zip(&expected).enumerate() {
            assert!(
                got.as_bytes() == want.as_bytes(),
                "line {index} differs at ({max_lines}, {max_bytes}): {} bytes back, {} expected",
                got.len(),
                want.len()
            );
        }
        observation(format!(
            "7 lines incl. two of 32,767 bytes, byte-exact, in {calls} take(s) at \
             max_lines={max_lines} max_bytes={max_bytes}"
        ));
    }
    connection.close().expect("close");
}

#[test]
fn empty_lines_are_kept_and_a_put_without_a_line_end_is_not_invented() {
    let mut connection = connect();
    enable(connection.as_mut(), UNLIMITED);
    exec(
        connection.as_mut(),
        "BEGIN \
         DBMS_OUTPUT.PUT_LINE(''); \
         DBMS_OUTPUT.PUT_LINE(NULL); \
         DBMS_OUTPUT.NEW_LINE; \
         DBMS_OUTPUT.PUT_LINE('  padded  '); \
         DBMS_OUTPUT.PUT_LINE(''); \
         END;",
    );
    let (lines, _) = drain(connection.as_mut(), CHUNK_LINES, CHUNK_BYTES);
    assert_eq!(lines, ["", "", "", "  padded  ", ""]);
    observation("PUT_LINE(''), PUT_LINE(NULL) and NEW_LINE each come back as one empty line");

    // A PUT with no line end is not a line: GET_LINE does not return it...
    exec(
        connection.as_mut(),
        "BEGIN DBMS_OUTPUT.PUT('partial'); END;",
    );
    let (lines, _) = drain(connection.as_mut(), CHUNK_LINES, CHUNK_BYTES);
    assert!(
        lines.is_empty(),
        "a PUT without a line end was returned: {lines:?}"
    );
    // ...and once a read has happened, DBMS_OUTPUT discards the unfinished
    // part at the next PUT. db-core reads after every statement while output
    // is on, so a PUT left unfinished at the end of a statement is lost — the
    // same as in SQL*Plus. Reported, not invented.
    exec(
        connection.as_mut(),
        "BEGIN DBMS_OUTPUT.PUT_LINE(' completed'); END;",
    );
    let (lines, _) = drain(connection.as_mut(), CHUNK_LINES, CHUNK_BYTES);
    assert_eq!(lines, [" completed"]);
    // Within one statement a PUT does join the line that ends it.
    exec(
        connection.as_mut(),
        "BEGIN DBMS_OUTPUT.PUT('joined'); DBMS_OUTPUT.PUT_LINE(' line'); END;",
    );
    let (lines, _) = drain(connection.as_mut(), CHUNK_LINES, CHUNK_BYTES);
    assert_eq!(lines, ["joined line"]);
    observation(
        "PUT without a line end is never returned; after a read it is discarded at the \
         next PUT (as in SQL*Plus); within one statement PUT + PUT_LINE is one line",
    );
    connection.close().expect("close");
}

#[test]
fn ten_thousand_lines_take_a_bounded_number_of_round_trips() {
    let mut connection = connect();
    enable(connection.as_mut(), UNLIMITED);
    exec(
        connection.as_mut(),
        "BEGIN FOR i IN 1..10000 LOOP DBMS_OUTPUT.PUT_LINE('line ' || i); END LOOP; END;",
    );
    let ((lines, calls), trips) = counted(connection.as_mut(), |connection| {
        drain(connection, CHUNK_LINES, CHUNK_BYTES)
    });
    assert_eq!(lines.len(), 10_000);
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(line, &format!("line {}", index + 1));
    }
    assert_eq!(
        trips,
        i64::try_from(calls).expect("small"),
        "every take_server_output call is exactly one round trip"
    );
    // The packed size is five prefix bytes plus the line; the bound is one
    // round trip per full budget plus the one that finds the buffer empty.
    let packed: usize = lines.iter().map(|line| 5 + line.len()).sum();
    let bound = packed.div_ceil(32_767 - 5 - 10) + 1;
    assert!(calls <= bound, "{calls} round trips, bound {bound}");
    measurement("m2_7.drain_10000_lines.round_trips", trips);
    measurement("m2_7.drain_10000_lines.packed_bytes", packed);
    observation(format!(
        "10,000 lines drained in {trips} round trips at db-core's chunk sizes \
         ({CHUNK_LINES} lines / {CHUNK_BYTES} bytes); DBMS_OUTPUT.GET_LINE per \
         line would be 10,001"
    ));
    connection.close().expect("close");
}

#[test]
fn output_written_while_rows_are_fetched_waits_for_the_next_read() {
    let mut connection = connect();
    let function = unique("m27_f");
    exec(
        connection.as_mut(),
        &format!(
            "CREATE OR REPLACE FUNCTION {function}(n IN NUMBER) RETURN NUMBER AS \
             BEGIN DBMS_OUTPUT.PUT_LINE('row ' || n); RETURN n; END;"
        ),
    );
    enable(connection.as_mut(), UNLIMITED);

    // The driver executes a query without prefetching, so the function runs
    // during the fetch, not the execute: the read db-core makes after the
    // execute finds nothing...
    let mut outcome = exec(
        connection.as_mut(),
        &format!("SELECT {function}(level) FROM dual CONNECT BY level <= 3"),
    );
    let (lines, _) = drain(connection.as_mut(), CHUNK_LINES, CHUNK_BYTES);
    assert!(lines.is_empty(), "{lines:?}");
    let mut cursor = outcome.take_cursor().expect("a cursor");
    let batch = cursor.fetch_batch(nz(100)).expect("fetch");
    assert_eq!(batch.row_count(), 3);
    cursor.close().expect("close the cursor");

    // ...db-core does not read after a fetch, and the lines are not lost: the
    // next statement's read returns them ahead of that statement's own.
    exec(
        connection.as_mut(),
        "BEGIN DBMS_OUTPUT.PUT_LINE('next statement'); END;",
    );
    let (lines, _) = drain(connection.as_mut(), CHUNK_LINES, CHUNK_BYTES);
    assert_eq!(lines, ["row 1", "row 2", "row 3", "next statement"]);
    observation(
        "output written during a fetch survives the next statement's PUT_LINE and \
         arrives with that statement's read, ahead of its own lines",
    );
    exec_quietly(connection.as_mut(), &format!("DROP FUNCTION {function}"));
    connection.close().expect("close");
}

#[test]
fn a_block_that_raises_still_leaves_what_it_printed() {
    let mut connection = connect();
    enable(connection.as_mut(), UNLIMITED);
    let error = failing(
        connection.as_mut(),
        "BEGIN DBMS_OUTPUT.PUT_LINE('before the failure'); \
         RAISE_APPLICATION_ERROR(-20001, 'deliberate'); END;",
    );
    assert_eq!(native_code(&error), 20_001, "{error}");
    let (lines, _) = drain(connection.as_mut(), CHUNK_LINES, CHUNK_BYTES);
    assert_eq!(lines, ["before the failure"]);
    observation("a block that raises keeps the lines it printed; they drain after its error");
    connection.close().expect("close");
}

#[test]
fn overflow_is_the_statements_error_with_its_native_code_and_the_lines_before_it_remain() {
    let mut connection = connect();
    enable(connection.as_mut(), bytes(2_000));
    let error = failing(
        connection.as_mut(),
        "BEGIN FOR i IN 1..100 LOOP DBMS_OUTPUT.PUT_LINE(RPAD('x', 100, 'x')); END LOOP; END;",
    );
    assert_eq!(native_code(&error), 20_000, "{error}");
    assert!(error.message().contains("ORU-10027"), "{error}");
    assert_eq!(error.session_state(), SessionState::Usable, "{error}");
    let (lines, _) = drain(connection.as_mut(), CHUNK_LINES, CHUNK_BYTES);
    assert!(
        !lines.is_empty() && lines.len() < 100,
        "{} lines survived",
        lines.len()
    );
    assert!(lines.iter().all(|line| line == &"x".repeat(100)));
    observation(format!(
        "ENABLE(2000) overflows with ORA-20000/ORU-10027 as the statement's error, \
         and the {} lines written before it still drain",
        lines.len()
    ));
    connection.close().expect("close");
}

#[test]
fn lines_a_read_leaves_behind_are_purged_by_the_next_put_not_overflowed() {
    // Why db-core reads to the end (ADR-0002 T6). A read that stops early
    // does not make a sized buffer overflow later. Instead, the next PUT after
    // a read purges what was left, with nothing counted, and a statement that
    // prints nothing has the leftovers read after it, in its own window.
    let mut connection = connect();
    enable(connection.as_mut(), bytes(2_000));
    let nineteen =
        "BEGIN FOR i IN 1..19 LOOP DBMS_OUTPUT.PUT_LINE(RPAD('p', 100, 'p')); END LOOP; END;";
    exec(connection.as_mut(), nineteen);
    let chunk = connection
        .take_server_output(nz(1), nz(CHUNK_BYTES))
        .expect("take one line");
    assert_eq!(chunk.len(), 1);
    assert!(!chunk.is_drained());
    // 18 lines are still buffered; 19 more would overflow 2,000 bytes if
    // they were kept. They are not: the first PUT purges them.
    exec(connection.as_mut(), nineteen);
    let (lines, _) = drain(connection.as_mut(), CHUNK_LINES, CHUNK_BYTES);
    assert_eq!(lines.len(), 19, "only the second statement's lines remain");

    // A statement that prints nothing does not purge: the leftovers are read
    // after it.
    exec(
        connection.as_mut(),
        "BEGIN FOR i IN 1..5 LOOP DBMS_OUTPUT.PUT_LINE('left ' || i); END LOOP; END;",
    );
    let chunk = connection
        .take_server_output(nz(1), nz(CHUNK_BYTES))
        .expect("take one line");
    assert_eq!(chunk.lines().len(), 1);
    exec(connection.as_mut(), "BEGIN NULL; END;");
    let (lines, _) = drain(connection.as_mut(), CHUNK_LINES, CHUNK_BYTES);
    assert_eq!(lines, ["left 2", "left 3", "left 4", "left 5"]);
    observation(
        "a partial read's leftovers are purged by the next PUT (no overflow, nothing \
         counted), and read after a statement that prints nothing",
    );
    connection.close().expect("close");
}

#[test]
fn a_session_that_never_enabled_output_pays_nothing_for_it() {
    let mut connection = connect();
    // The driver issues DBMS_OUTPUT calls only from `set_server_output` and
    // `take_server_output`, never from `execute`: a statement that prints
    // costs exactly what one that does not print costs.
    let (_, quiet) = counted(connection.as_mut(), |connection| {
        exec(connection, "BEGIN NULL; END;");
    });
    let (_, printing) = counted(connection.as_mut(), |connection| {
        exec(
            connection,
            "BEGIN DBMS_OUTPUT.PUT_LINE('nobody is listening'); END;",
        );
    });
    assert_eq!(
        quiet, printing,
        "a printing statement cost extra round trips"
    );
    assert_eq!(printing, 1, "one statement, one round trip");
    measurement("m2_7.execute_without_output.round_trips", quiet);
    measurement("m2_7.execute_printing_while_off.round_trips", printing);

    // And nothing piles up on the server: output written while off is
    // discarded there, not buffered for later.
    enable(connection.as_mut(), UNLIMITED);
    let (lines, calls) = drain(connection.as_mut(), CHUNK_LINES, CHUNK_BYTES);
    assert!(lines.is_empty(), "{lines:?}");
    assert_eq!(calls, 1);
    observation(
        "never enabled: a printing statement costs 1 round trip like BEGIN NULL; END; \
         and its output is discarded server-side",
    );
    connection.close().expect("close");
}

#[test]
fn unlimited_and_sized_buffers_behave_as_reported() {
    let mut connection = connect();
    let transaction = connection.transaction_state();

    // 2,000 bytes is the smallest buffer the server accepts: 100 is raised.
    let effective = connection
        .set_server_output(bytes(100))
        .expect("ENABLE(100)");
    assert_eq!(effective, bytes(2_000));
    // Neither call is a statement the user wrote: the transaction tracking a
    // fresh connection starts with is left exactly as it was.
    let (lines, _) = drain(connection.as_mut(), CHUNK_LINES, CHUNK_BYTES);
    assert!(lines.is_empty());
    assert_eq!(
        connection.transaction_state(),
        transaction,
        "server output is session state and never touches the transaction"
    );
    exec(
        connection.as_mut(),
        "BEGIN FOR i IN 1..10 LOOP DBMS_OUTPUT.PUT_LINE(RPAD('y', 100, 'y')); END LOOP; END;",
    );
    let (lines, _) = drain(connection.as_mut(), CHUNK_LINES, CHUNK_BYTES);
    assert_eq!(lines.len(), 10, "1,000 bytes fit in the clamped 2,000");
    let error = failing(
        connection.as_mut(),
        "BEGIN FOR i IN 1..30 LOOP DBMS_OUTPUT.PUT_LINE(RPAD('y', 100, 'y')); END LOOP; END;",
    );
    assert_eq!(native_code(&error), 20_000, "{error}");
    connection
        .set_server_output(ServerOutputSetting::Disabled)
        .expect("DISABLE");

    // 1,000,000 is the largest: 2,000,000 is lowered, so 1.1 MB overflows...
    let effective = connection
        .set_server_output(bytes(2_000_000))
        .expect("ENABLE(2000000)");
    assert_eq!(effective, bytes(1_000_000));
    let megabyte_and_a_bit =
        "BEGIN FOR i IN 1..11000 LOOP DBMS_OUTPUT.PUT_LINE(RPAD('z', 100, 'z')); END LOOP; END;";
    let error = failing(connection.as_mut(), megabyte_and_a_bit);
    assert_eq!(native_code(&error), 20_000, "{error}");
    // DISABLE purges, so the megabyte is not read back.
    connection
        .set_server_output(ServerOutputSetting::Disabled)
        .expect("DISABLE");

    // ...and unlimited takes it.
    let effective = connection
        .set_server_output(UNLIMITED)
        .expect("ENABLE(NULL)");
    assert_eq!(effective, UNLIMITED);
    exec(connection.as_mut(), megabyte_and_a_bit);
    let (_, trips) = counted(connection.as_mut(), |connection| {
        connection
            .set_server_output(ServerOutputSetting::Disabled)
            .expect("DISABLE")
    });
    assert_eq!(trips, 1, "disabling is one round trip");
    // Nothing is left after DISABLE, even once re-enabled.
    let (_, trips) = counted(connection.as_mut(), |connection| {
        enable(connection, UNLIMITED);
    });
    assert_eq!(trips, 1, "enabling is one round trip");
    let (lines, _) = drain(connection.as_mut(), CHUNK_LINES, CHUNK_BYTES);
    assert!(lines.is_empty(), "DISABLE left {} lines", lines.len());

    observation(
        "ENABLE(100) reports and behaves as 2,000; ENABLE(2,000,000) as 1,000,000 \
         (1.1 MB overflows); ENABLE(NULL) takes 1.1 MB; DISABLE is one round trip \
         and purges",
    );
    connection.close().expect("close");
}
