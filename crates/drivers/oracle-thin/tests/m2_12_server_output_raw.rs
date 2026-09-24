//! M2.12 — server-output framing over `RAW`/`LENGTHB`, with per-line UTF-8
//! decoding in Rust, against a real database.
//!
//! Follow-up to M2.7 (`tests/m2_7_server_output.rs`), whose framing is now
//! bytes rather than characters and whose text OUT binds are now `LONG RAW`
//! rather than `LONG`. What the offline unit tests in `src/server_output.rs`
//! cannot show: that `SYS.UTL_I18N.STRING_TO_RAW` exists and is executable by
//! the test user, that the review's exact reproduction (one invalid-UTF-8
//! line between two good ones) costs only itself, that Thai/emoji/combining
//! marks and 32,767-byte content still come back byte-exact through the new
//! byte framing, that `max_bytes` is honoured in UTF-8 bytes, and that the
//! round-trip cost is unchanged from M2.7. Every M2.7 test in the sibling
//! file must still pass unmodified against the same database.

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "M2.12 results are measurements a human reads from the test output"
)]

mod common;

use std::num::NonZeroUsize;

use common::{connect, exec, measurement, observation, scalar};
use reldex_db_driver_api::{
    DatabaseConnection, ServerOutputBuffer, ServerOutputSetting, Statement,
};

/// db-core's defaults (`SessionLimits`), matching `tests/m2_7_server_output.rs`.
const CHUNK_LINES: usize = 4096;
const CHUNK_BYTES: usize = 32 * 1024;

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).expect("non-zero")
}

const UNLIMITED: ServerOutputSetting = ServerOutputSetting::Enabled(ServerOutputBuffer::Unlimited);

fn enable(connection: &mut dyn DatabaseConnection, setting: ServerOutputSetting) {
    let effective = connection
        .set_server_output(setting)
        .expect("DBMS_OUTPUT.ENABLE");
    assert_eq!(effective, setting, "no clamp expected for {setting:?}");
}

/// Reads until the server says the buffer is empty, as db-core's drain does.
/// Returns the lines, how many `take_server_output` calls it took, and the
/// total `invalid_utf8_lines()` reported across every chunk.
fn drain(
    connection: &mut dyn DatabaseConnection,
    max_lines: usize,
    max_bytes: usize,
) -> (Vec<String>, usize, u32) {
    let mut lines = Vec::new();
    let mut calls = 0_usize;
    let mut invalid = 0_u32;
    loop {
        let chunk = connection
            .take_server_output(nz(max_lines), nz(max_bytes))
            .expect("take_server_output");
        calls += 1;
        invalid += chunk.invalid_utf8_lines();
        let stop = chunk.is_drained() || chunk.is_empty();
        lines.extend(chunk.into_lines().into_iter().map(String::from));
        if stop {
            return (lines, calls, invalid);
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

/// Round trips `work` costs, net of the reading query's own.
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

#[test]
fn utl_i18n_string_to_raw_is_available_and_executable_here() {
    let mut connection = connect();
    let charset = scalar(
        connection.as_mut(),
        "SELECT value FROM nls_database_parameters WHERE parameter = 'NLS_CHARACTERSET'",
    );
    observation(format!("NLS_CHARACTERSET = {charset}"));
    assert_eq!(
        charset, "AL32UTF8",
        "this module's framing is tested and documented against AL32UTF8; a \
         different value here means the residual limit in src/server_output.rs \
         (a line's UTF-8 form exceeding 32,767 bytes) is now reachable, not just \
         theoretical, and the suite below should be re-read with that in mind"
    );

    let hex = scalar(
        connection.as_mut(),
        "SELECT SYS.UTL_I18N.STRING_TO_RAW('AB', 'AL32UTF8') FROM dual",
    );
    assert_eq!(
        hex, "4142",
        "SYS.UTL_I18N.STRING_TO_RAW('AB', 'AL32UTF8') must be plain ASCII"
    );
    observation("SYS.UTL_I18N.STRING_TO_RAW exists and is executable by RELDEX_TEST");

    // The exact malformed input the review used (UTL_RAW.CAST_TO_VARCHAR2
    // reinterprets raw bytes as VARCHAR2 without charset validation): does
    // STRING_TO_RAW raise on it, or pass it through unchanged. Measured, not
    // assumed, because the answer decides whether the block below needs the
    // UTL_RAW.CAST_TO_RAW fallback the task brief allows.
    let passthrough = scalar(
        connection.as_mut(),
        "SELECT SYS.UTL_I18N.STRING_TO_RAW(UTL_RAW.CAST_TO_VARCHAR2('41FF42'), 'AL32UTF8') \
         FROM dual",
    );
    assert_eq!(
        passthrough, "41FF42",
        "STRING_TO_RAW must pass an already-AL32UTF8 malformed byte sequence through \
         unchanged, not raise or repair it — TAKE_BLOCK relies on this so the invalid \
         line still reaches Rust for per-line decoding"
    );
    observation(format!(
        "STRING_TO_RAW(CAST_TO_VARCHAR2('41FF42'), 'AL32UTF8') = {passthrough}: malformed \
         input passes through unchanged, with no error — this is why decoding one line \
         at a time in Rust, not on the server, is where U+FFFD recovery has to happen"
    ));
    connection.close().expect("close");
}

#[test]
fn an_invalid_utf8_line_costs_only_itself_the_review_reproduction() {
    let mut connection = connect();
    enable(connection.as_mut(), UNLIMITED);
    exec(
        connection.as_mut(),
        "BEGIN \
         DBMS_OUTPUT.PUT_LINE('before'); \
         DBMS_OUTPUT.PUT_LINE(UTL_RAW.CAST_TO_VARCHAR2('41FF42')); \
         DBMS_OUTPUT.PUT_LINE('after'); \
         END;",
    );
    let chunk = connection
        .take_server_output(nz(CHUNK_LINES), nz(CHUNK_BYTES))
        .expect("take_server_output");
    assert_eq!(chunk.len(), 3, "all three lines must survive the bad one");
    assert_eq!(chunk.lines()[0].as_ref(), "before");
    assert_eq!(
        chunk.lines()[1].as_ref(),
        "A\u{FFFD}B",
        "0x41 0xFF 0x42 decodes to A, U+FFFD, B"
    );
    assert_eq!(chunk.lines()[2].as_ref(), "after");
    assert_eq!(
        chunk.invalid_utf8_lines(),
        1,
        "exactly the middle line is reported invalid"
    );
    assert!(chunk.is_drained());
    observation(
        "the review's reproduction (PUT_LINE('before'), PUT_LINE with \
         UTL_RAW.CAST_TO_VARCHAR2('41FF42'), PUT_LINE('after')): all three lines survive \
         one default take, the middle one decodes to A\\u{FFFD}B, invalid_utf8_lines() = 1 \
         — M2.7 lost every line of this read to a DataConversion error",
    );

    // The session is still usable after an invalid line.
    let mut outcome = connection
        .execute(&Statement::new("SELECT 1 FROM dual"))
        .expect("the session must still be usable after an invalid line");
    outcome
        .take_cursor()
        .expect("cursor")
        .close()
        .expect("close cursor");
    connection.close().expect("close");
}

#[test]
fn thai_emoji_and_32767_byte_lines_come_back_byte_for_byte_through_raw_framing() {
    let mut connection = connect();
    enable(connection.as_mut(), UNLIMITED);

    let short = [
        "สวัสดีครับ ทดสอบภาษาไทย ๑๒๓",
        "emoji 😀👍🏽 flag 🇹🇭 zwj 👩‍💻",
        "combining e\u{301} and ก\u{e34}\u{e4a}",
        "00012 looks like a length prefix",
    ];
    // 10,922 three-byte Thai characters and one ASCII byte; 8,191 four-byte
    // emoji and three ASCII bytes: both exactly 32,767 UTF-8 bytes, matching
    // PL/SQL's own limit and this module's packed-buffer ceiling.
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

    // Every M2.7 chunk size, plus (1, 1): the same content, the same claim
    // ("byte-exact"), now proven over the byte-framed wire format.
    for (max_lines, max_bytes) in [(CHUNK_LINES, CHUNK_BYTES), (3, 64), (1, 1)] {
        exec(connection.as_mut(), &block);
        let (lines, calls, invalid) = drain(connection.as_mut(), max_lines, max_bytes);
        assert_eq!(lines.len(), expected.len(), "({max_lines}, {max_bytes})");
        assert_eq!(invalid, 0, "nothing here is malformed UTF-8");
        for (index, (got, want)) in lines.iter().zip(&expected).enumerate() {
            assert!(
                got.as_bytes() == want.as_bytes(),
                "line {index} differs at ({max_lines}, {max_bytes}): {} bytes back, {} expected",
                got.len(),
                want.len()
            );
        }
        observation(format!(
            "7 lines incl. two of 32,767 UTF-8 bytes, byte-exact, in {calls} take(s) at \
             max_lines={max_lines} max_bytes={max_bytes}"
        ));
    }
    connection.close().expect("close");
}

#[test]
fn max_bytes_is_honoured_in_utf8_bytes_a_chunk_never_exceeds_it_by_more_than_one_line() {
    // `take` returns one round trip's worth of lines, which may be the
    // packed lines alone or the packed lines plus one unframed tail line
    // appended after them (module docs, ":tail"). The tail is the *only*
    // line allowed to push a chunk over `max_bytes`, and only by itself: the
    // packed lines ahead of it always sum to at most `max_bytes`, by
    // construction of the budget check inside TAKE_BLOCK. So the invariant
    // this test checks is: drop the chunk's last line, and what remains must
    // fit the budget.
    let mut connection = connect();
    enable(connection.as_mut(), UNLIMITED);

    // Every line is Thai: three UTF-8 bytes per character, so a chunk's
    // exact byte total (not its character count, not any database-charset
    // byte count) is what has to respect `max_bytes`.
    let thai_line = "ทดสอบ".repeat(50); // 250 characters, 750 UTF-8 bytes
    assert_eq!(thai_line.len(), 750);
    let block =
        format!("BEGIN FOR i IN 1..40 LOOP DBMS_OUTPUT.PUT_LINE('{thai_line}'); END LOOP; END;");
    exec(connection.as_mut(), &block);

    let max_bytes = 2_000_usize;
    let mut worst_packed_total = 0_usize;
    let mut tail_over_budget_lines = 0_usize;
    let mut chunks = 0_usize;
    loop {
        let chunk = connection
            .take_server_output(nz(CHUNK_LINES), nz(max_bytes))
            .expect("take_server_output");
        chunks += 1;
        let lines = chunk.lines();
        let last_line_bytes = lines.last().map_or(0, |line| line.len());
        let packed_total: usize = lines
            .iter()
            .rev()
            .skip(1) // the possible unframed tail line
            .map(|line| line.len())
            .sum();
        assert!(
            packed_total <= max_bytes,
            "the packed lines of a chunk (everything but its possible tail line) total \
             {packed_total} UTF-8 bytes, over the {max_bytes}-byte budget"
        );
        worst_packed_total = worst_packed_total.max(packed_total);
        if packed_total + last_line_bytes > max_bytes {
            // The chunk's own last line was the one line allowed to push the
            // total over budget on its own — never more than one line.
            tail_over_budget_lines += 1;
        }
        let done = chunk.is_drained() || chunk.is_empty();
        if done {
            break;
        }
        assert!(chunks < 1_000_000, "the drain did not terminate");
    }
    assert!(
        chunks > 1,
        "the budget must have forced more than one chunk"
    );
    measurement("m2_12.max_bytes_budget", max_bytes);
    measurement(
        "m2_12.max_bytes_worst_packed_chunk_bytes",
        worst_packed_total,
    );
    observation(format!(
        "40 Thai lines of 750 UTF-8 bytes each at max_bytes={max_bytes}: every chunk's \
         packed lines stayed within budget (worst {worst_packed_total} bytes); \
         {tail_over_budget_lines} chunk(s) exceeded the budget only because of their own \
         last (tail) line, never by more than that one line"
    ));
    connection.close().expect("close");
}

#[test]
fn ten_thousand_lines_still_drain_in_a_bounded_number_of_round_trips() {
    let mut connection = connect();
    enable(connection.as_mut(), UNLIMITED);
    exec(
        connection.as_mut(),
        "BEGIN FOR i IN 1..10000 LOOP DBMS_OUTPUT.PUT_LINE('line ' || i); END LOOP; END;",
    );
    let ((lines, calls, invalid), trips) = counted(connection.as_mut(), |connection| {
        drain(connection, CHUNK_LINES, CHUNK_BYTES)
    });
    assert_eq!(lines.len(), 10_000);
    assert_eq!(invalid, 0);
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(line, &format!("line {}", index + 1));
    }
    assert_eq!(
        trips,
        i64::try_from(calls).expect("small"),
        "every take_server_output call is exactly one round trip"
    );
    // ASCII content: UTF-8 byte length equals character length equals the
    // database-charset byte length, so the packed size and the round-trip
    // bound are the same shape M2.7 measured.
    let packed: usize = lines.iter().map(|line| 5 + line.len()).sum();
    let bound = packed.div_ceil(32_767 - 5 - 10) + 1;
    assert!(calls <= bound, "{calls} round trips, bound {bound}");
    assert!(
        trips <= 6,
        "M2.7 measured 10,003 similar lines in 6 round trips; this must not regress"
    );
    measurement("m2_12.drain_10000_lines.round_trips", trips);
    measurement("m2_12.drain_10000_lines.packed_bytes", packed);
    observation(format!(
        "10,000 lines drained in {trips} round trips through the byte-framed wire format \
         (M2.7 measured 5 at these chunk sizes for the same shape of content); \
         DBMS_OUTPUT.GET_LINE per line would be 10,001"
    ));
    connection.close().expect("close");
}

/// Guards against a decode-path regression in the OUT-bind read this module
/// depends on (module doc, "This is an OUT-bind read... never a cursor fetch
/// of a LONG RAW column"): confirms the process is still alive and the
/// connection still usable after several `LONG RAW` OUT-bind round trips,
/// unlike the quarantined `tests/s12_dev_features.rs` LONG RAW **column**
/// probes.
#[test]
fn repeated_long_raw_out_bind_reads_do_not_disturb_the_process_or_the_session() {
    let mut connection = connect();
    enable(connection.as_mut(), UNLIMITED);
    for round in 0..20 {
        exec(
            connection.as_mut(),
            &format!("BEGIN DBMS_OUTPUT.PUT_LINE('round {round}'); END;"),
        );
        let chunk = connection
            .take_server_output(nz(CHUNK_LINES), nz(CHUNK_BYTES))
            .expect("take_server_output");
        assert_eq!(chunk.lines()[0].as_ref(), format!("round {round}"));
    }
    let value = scalar(connection.as_mut(), "SELECT 'still alive' FROM dual");
    assert_eq!(value, "still alive");
    observation("20 LONG RAW OUT-bind reads: process alive, session usable throughout");
    connection.close().expect("close");
}
