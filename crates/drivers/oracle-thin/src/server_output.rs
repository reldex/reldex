//! `DBMS_OUTPUT`, behind the vendor-neutral server-output contract
//! (ADR-0002 amendment T). Every Oracle statement involved lives here and
//! nowhere else.
//!
//! # Why not `DBMS_OUTPUT.GET_LINES`
//!
//! `GET_LINES` returns a `DBMS_OUTPUT.CHARARR`, a PL/SQL index-by table, and
//! the only way to get one out of the server is an **array OUT bind**.
//! `oracledb` 26.0.0-beta.3 has no PL/SQL collection binds: `Metadata::is_array`
//! is set only for DML `RETURNING` binds (`statement/bind_info.rs`), and its
//! `BindParameters::Slice` is the `executemany` row array, not a collection.
//! The one-line-per-round-trip `GET_LINE` loop spike S5 used
//! (`tests/s5_plsql.rs`) is what production must not do.
//!
//! # What this does instead: one packed block per round trip
//!
//! [`TAKE_BLOCK`] is an anonymous block that calls `GET_LINE` in a **server-side**
//! loop and packs the lines it gets into one `RAW`, each preceded by a
//! fixed-width, five-digit count of its **UTF-8 bytes**. Every line is
//! converted to UTF-8 on the server, with `SYS.UTL_I18N.STRING_TO_RAW(line,
//! 'AL32UTF8')`, before it is measured or packed — independent of the
//! database's own character set (M2.12; see "Framing is bytes, not
//! characters" below). Framing is by length, never by a delimiter, because a
//! line may contain any character at all — a newline, a NUL, the digits of a
//! length.
//!
//! A line is consumed from the server's buffer the moment `GET_LINE` returns
//! it, and there is no way to put it back. So the line that does not fit in
//! the packed buffer is returned **on its own**, unframed, in a second bind
//! (`:tail`), and the block stops there. Every round trip therefore returns at
//! least one line when there is one, a line of any legal length (up to
//! 32,767 bytes) always fits in the tail, and nothing is ever lost between
//! calls.
//!
//! Both binds are declared `LONG RAW`, not `RAW`. A pure OUT `RAW` bind is
//! sized from the type's 2,000-byte default and the server refuses more than
//! that into it (`ORA-06502`); a `LONG RAW` OUT bind has no client-side
//! ceiling below PL/SQL's own 32,767 bytes — the same trick M2.7 used for the
//! `LONG` text binds this replaces (`DB_TYPE_LONG_RAW`'s `buffer_size_factor`
//! is the same `2147483647` as `DB_TYPE_LONG`'s). Measured on 19.3 with Thai
//! text: 32,767 bytes in, 32,767 bytes out, byte for byte.
//!
//! This is an OUT-bind read (`out_bind_data()`/`Row::take`), never a cursor
//! fetch of a `LONG RAW` **column** — the path `tests/s12_dev_features.rs`
//! quarantines behind `#[ignore]` because that decode is unproven and may
//! abort the process (upstream gap U-4). `Row::take::<Vec<u8>>` on a bound
//! `DB_TYPE_RAW`/`DB_TYPE_LONG_RAW` value goes through a plain byte copy in
//! `oracledb::db_value` (`ORA_TYPE_NUM_RAW | ORA_TYPE_NUM_LONG_RAW`), with no
//! decode step at all — unlike the text path (`ORA_TYPE_NUM_LONG` /
//! `ORA_TYPE_NUM_VARCHAR`) it replaced, which ran every byte through a strict
//! `std::str::from_utf8` before this module ever saw it. That strict decode,
//! not any panic, is the M2.7 defect this module now avoids: an invalid line
//! used to fail the whole bind (`Err`, not a panic) and lose every line read
//! with it. `LONG RAW` sidesteps it entirely by never asking `oracledb` to
//! decode text; decoding one line at a time, with recovery, is this module's
//! own job now (see [`unpack`]).
//!
//! # Framing is bytes, not characters
//!
//! M2.7's framing measured each line's length with `LENGTH4` (Unicode code
//! points) and assumed that equalled the Rust `char` count of the decoded
//! text — true only for a database whose character set is itself Unicode. On
//! a single-byte database character set the assumption silently broke
//! `max_bytes`: a "kilobyte" of single-byte text could become several
//! kilobytes of UTF-8 after decoding, so a caller's byte budget was not
//! honoured. This module has no such assumption left: `SYS.UTL_RAW.LENGTH`
//! measures the already-UTF-8 `RAW` line, the prefix is that exact byte
//! count, and `max_bytes` bounds it exactly, on any database character set.
//!
//! **Residual limit, accepted rather than solved.** A PL/SQL `RAW` local
//! variable and a bind's own PL/SQL-side buffer are both capped at 32,767
//! bytes — the same ceiling `DBMS_OUTPUT` itself imposes on one line. On
//! `AL32UTF8` (this module's tested and supported configuration) that is a
//! non-issue: the database already stores UTF-8, so `STRING_TO_RAW(line,
//! 'AL32UTF8')` is a byte-identical no-op and a line already ≤ 32,767 bytes
//! stays ≤ 32,767 bytes. On a single-byte database character set, a line
//! whose non-ASCII repertoire fills that 32,767-byte limit can expand past it
//! once converted to UTF-8; the assignment inside [`TAKE_BLOCK`] then fails
//! with a PL/SQL numeric/value error, which surfaces as this call's `DbError`
//! — reported, not a silent truncation and not a process abort. Solving it
//! would mean streaming the conversion through a LOB rather than a scalar
//! `RAW`, which is out of scope here.
//!
//! **Measured cost** (`tests/m2_7_server_output.rs`, `v$mystat` "SQL*Net
//! roundtrips to/from client"): 10,003 lines — 10,000 short ones, an empty
//! one, a Thai/emoji one and one of 32,767 bytes — in **6 round trips**, where
//! `GET_LINE` would take 10,004. M2.12 changes the wire format, not the round
//! trip count; `tests/m2_12_server_output_raw.rs` re-measures it.
//!
//! # What `DBMS_OUTPUT` does that callers must know
//!
//! * A `PUT` with no `NEW_LINE` after it is not a line yet, and `GET_LINE`
//!   does not return it; it stays in the server's buffer until a line end
//!   arrives. Reported as-is, not invented.
//! * `PUT_LINE('')`, `PUT_LINE(NULL)` and `NEW_LINE` each produce an empty
//!   line — `GET_LINE` returns NULL for it — which this module returns as an
//!   empty string.
//! * `ENABLE(n)` silently clamps `n` into 2,000 ..= 1,000,000 bytes (measured
//!   on 19.3: `ENABLE(100)` overflows at 2,000, `ENABLE(2_000_000)` at
//!   1,000,000). [`set`] applies the same clamp and reports the size in force.
//! * `DISABLE` purges whatever the buffer still held.
//! * A block that writes past the limit fails with `ORA-20000` /
//!   `ORU-10027: buffer overflow` — the user's statement failing, reported as
//!   its error. The lines buffered before the overflow stay readable.

use std::num::{NonZeroU32, NonZeroUsize};

use oracledb::{Connection, DB_TYPE_LONG_RAW, DB_TYPE_NUMBER, DbType, ToDbValue};
use reldex_db_driver_api::{
    DbError, DbResult, ErrorKind, ServerOutputBuffer, ServerOutputChunk, ServerOutputSetting,
};

/// The smallest buffer `DBMS_OUTPUT.ENABLE` accepts; smaller requests are
/// raised to it by the server.
pub(crate) const MIN_BUFFER_BYTES: u32 = 2_000;

/// The largest buffer `DBMS_OUTPUT.ENABLE` accepts; larger requests are
/// lowered to it by the server. Only `NULL` (unlimited) goes beyond.
pub(crate) const MAX_BUFFER_BYTES: u32 = 1_000_000;

/// The most bytes one PL/SQL `RAW` local variable can hold, and so the most
/// the packed buffer carries per round trip.
const MAX_PACKED_BYTES: usize = 32_767;

/// Width of the byte-count prefix in front of each packed line. A
/// `DBMS_OUTPUT` line is at most 32,767 bytes; its UTF-8 form on a
/// single-byte database character set can in principle exceed that (see the
/// module documentation's "Residual limit"), but never past 99,999: five
/// digits always suffice.
const PREFIX_WIDTH: usize = 5;

/// The take block. Binds, in order of first appearance: `:1` the most lines to
/// take, `:2` the byte budget for the packed buffer, then the outputs `:3`
/// packed lines, `:4` the tail line, `:5` whether there is a tail line, `:6`
/// how many lines were taken, `:7` whether `GET_LINE` reported the buffer
/// empty. Each output is assigned exactly once, at the end.
///
/// Every call here names a package as `SYS.DBMS_OUTPUT`, `SYS.UTL_I18N` or
/// `SYS.UTL_RAW`. An unqualified name resolves through the user's own schema
/// first, so a table, package or synonym with one of those names there would
/// capture the call. The qualified name always reaches the real package.
///
/// Each line is converted to UTF-8 bytes with `UTL_I18N.STRING_TO_RAW`
/// **before** it is measured or packed, so the five-digit prefix that follows
/// is always a byte count in UTF-8, never a character count and never a byte
/// count in the database's own character set (M2.12; see the module
/// documentation). `UTL_RAW.CAST_TO_RAW` on the prefix's own decimal digits is
/// safe on any database character set: `'0'`..`'9'` are the ASCII-invariant
/// subset every Oracle character set shares.
const TAKE_BLOCK: &str = "DECLARE
  l_line     VARCHAR2(32767);
  l_status   INTEGER;
  l_bytes    RAW(32767);
  l_buf      RAW(32767);
  l_used     PLS_INTEGER := 0;
  l_count    PLS_INTEGER := 0;
  l_len      PLS_INTEGER;
  l_max      PLS_INTEGER := :1;
  l_budget   PLS_INTEGER := :2;
  l_tail     RAW(32767);
  l_has_tail PLS_INTEGER := 0;
  l_drained  PLS_INTEGER := 0;
BEGIN
  WHILE l_count < l_max LOOP
    SYS.DBMS_OUTPUT.GET_LINE(l_line, l_status);
    IF l_status <> 0 THEN
      l_drained := 1;
      EXIT;
    END IF;
    l_count := l_count + 1;
    l_bytes := SYS.UTL_I18N.STRING_TO_RAW(l_line, 'AL32UTF8');
    l_len := NVL(SYS.UTL_RAW.LENGTH(l_bytes), 0);
    IF l_used + 5 + l_len <= l_budget THEN
      l_buf := SYS.UTL_RAW.CONCAT(
        l_buf, SYS.UTL_RAW.CAST_TO_RAW(TO_CHAR(l_len, 'FM00000')), l_bytes);
      l_used := l_used + 5 + l_len;
    ELSE
      l_tail := l_bytes;
      l_has_tail := 1;
      EXIT;
    END IF;
  END LOOP;
  :3 := l_buf;
  :4 := l_tail;
  :5 := l_has_tail;
  :6 := l_count;
  :7 := l_drained;
END;";

/// Turns `DBMS_OUTPUT` on or off. One round trip.
pub(crate) fn set(
    connection: &Connection,
    setting: ServerOutputSetting,
) -> DbResult<ServerOutputSetting> {
    let result = match setting {
        ServerOutputSetting::Disabled => {
            connection.execute("BEGIN SYS.DBMS_OUTPUT.DISABLE; END;", &[])
        }
        ServerOutputSetting::Enabled(ServerOutputBuffer::Unlimited) => {
            connection.execute("BEGIN SYS.DBMS_OUTPUT.ENABLE(NULL); END;", &[])
        }
        ServerOutputSetting::Enabled(ServerOutputBuffer::Bytes(bytes)) => {
            let size = i64::from(clamp_buffer(bytes).get());
            connection.execute("BEGIN SYS.DBMS_OUTPUT.ENABLE(:1); END;", &[&size])
        }
    };
    result.map_err(|error| crate::error::map(&error))?;
    Ok(effective(setting))
}

/// The setting `DBMS_OUTPUT` actually applies for `requested`.
pub(crate) fn effective(requested: ServerOutputSetting) -> ServerOutputSetting {
    match requested {
        ServerOutputSetting::Enabled(ServerOutputBuffer::Bytes(bytes)) => {
            ServerOutputSetting::Enabled(ServerOutputBuffer::Bytes(clamp_buffer(bytes)))
        }
        other => other,
    }
}

fn clamp_buffer(bytes: NonZeroU32) -> NonZeroU32 {
    let clamped = bytes.get().clamp(MIN_BUFFER_BYTES, MAX_BUFFER_BYTES);
    NonZeroU32::new(clamped).unwrap_or(bytes)
}

/// Takes up to `max_lines` lines in **one** round trip; see the module
/// documentation for the framing.
pub(crate) fn take(
    connection: &Connection,
    max_lines: NonZeroUsize,
    max_bytes: NonZeroUsize,
) -> DbResult<ServerOutputChunk> {
    let max_lines = i64::try_from(max_lines.get())
        .unwrap_or(i64::MAX)
        .min(i64::from(i32::MAX));
    let budget = i64::try_from(max_bytes.get().min(MAX_PACKED_BYTES)).unwrap_or(0);
    let long_raw: &'static DbType = &DB_TYPE_LONG_RAW;
    let number: &'static DbType = &DB_TYPE_NUMBER;
    let params: [&dyn ToDbValue; 7] = [
        &max_lines, &budget, &long_raw, &long_raw, &number, &number, &number,
    ];
    let mut result = connection
        .execute(TAKE_BLOCK, &params)
        .map_err(|error| crate::error::map(&error))?;
    let mut row = result.out_bind_data();
    let take_err = |error: oracledb::Error| crate::error::map(&error);
    let packed: Option<Vec<u8>> = row.take(0).map_err(take_err)?;
    let tail: Option<Vec<u8>> = row.take(1).map_err(take_err)?;
    let has_tail: Option<i64> = row.take(2).map_err(take_err)?;
    let count: Option<i64> = row.take(3).map_err(take_err)?;
    let drained: Option<i64> = row.take(4).map_err(take_err)?;

    let count = usize::try_from(count.unwrap_or(0)).map_err(|_| framing_error("negative count"))?;
    let (mut lines, mut invalid) = unpack(packed.as_deref().unwrap_or(&[]), count)?;
    if has_tail == Some(1) {
        // A missing tail RAW is an empty line, exactly as in the packed part.
        let (line, bad) = decode_line(tail.as_deref().unwrap_or(&[]));
        if bad {
            invalid += 1;
        }
        lines.push(line);
    }
    if lines.len() != count {
        return Err(framing_error(&format!(
            "the server reported {count} lines and {} were decoded",
            lines.len()
        )));
    }
    Ok(ServerOutputChunk::new(lines, drained == Some(1)).with_invalid_utf8_lines(invalid))
}

/// Decodes one line's UTF-8 bytes. A line whose bytes are not valid UTF-8 is
/// still delivered — never dropped, and never allowed to cost any other line
/// of the same read — with each invalid byte sequence replaced by U+FFFD
/// (`String::from_utf8_lossy`); the returned `bool` says whether that
/// happened, so the caller can count it rather than lose the information
/// silently.
fn decode_line(bytes: &[u8]) -> (Box<str>, bool) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (Box::from(text), false),
        Err(_) => (Box::from(String::from_utf8_lossy(bytes).into_owned()), true),
    }
}

/// Splits the packed byte buffer back into lines, decoding each one
/// independently, and returns them alongside how many were not valid UTF-8.
///
/// Each frame is five ASCII digits giving the line's length **in UTF-8
/// bytes** (never a character count — see the module documentation), then
/// that many bytes. A frame's *framing* being wrong — a short buffer, a
/// non-digit prefix, more lines than the server said it sent — is reported as
/// an error rather than guessed at: a mis-split would hand the user lines
/// that were never printed. A frame's *content* not being valid UTF-8 is not
/// a framing error: see [`decode_line`].
fn unpack(packed: &[u8], expected: usize) -> DbResult<(Vec<Box<str>>, u32)> {
    let mut lines = Vec::with_capacity(expected.min(packed.len() / PREFIX_WIDTH + 1));
    let mut invalid = 0_u32;
    let mut rest = packed;
    while !rest.is_empty() {
        let prefix = rest
            .get(..PREFIX_WIDTH)
            .filter(|prefix| prefix.iter().all(u8::is_ascii_digit))
            .ok_or_else(|| framing_error("a line's length prefix is missing or malformed"))?;
        // `prefix` is ASCII digits, so this is always valid UTF-8.
        let frame_bytes: usize = std::str::from_utf8(prefix)
            .ok()
            .and_then(|digits| digits.parse().ok())
            .ok_or_else(|| framing_error("a line's length prefix is not a number"))?;
        let body = &rest[PREFIX_WIDTH..];
        let frame = body
            .get(..frame_bytes)
            .ok_or_else(|| framing_error("a line is shorter than its length prefix says"))?;
        let (line, bad) = decode_line(frame);
        if bad {
            invalid += 1;
        }
        lines.push(line);
        rest = &body[frame_bytes..];
        if lines.len() > expected {
            return Err(framing_error(
                "more lines were decoded than the server sent",
            ));
        }
    }
    Ok((lines, invalid))
}

fn framing_error(detail: &str) -> DbError {
    DbError::new(
        ErrorKind::DataConversion,
        format!(
            "reldex-driver-oracle-thin: DBMS_OUTPUT lines could not be decoded ({detail}); \
             the lines this read took from the server are lost"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Packs `frames` — already-encoded byte lines, matching what
    /// `UTL_I18N.STRING_TO_RAW` would hand back on the server — the way
    /// [`TAKE_BLOCK`] does: a five-digit ASCII byte-count prefix, then the
    /// bytes themselves.
    fn pack_bytes(frames: &[&[u8]]) -> Vec<u8> {
        let mut packed = Vec::new();
        for frame in frames {
            packed.extend(format!("{:05}", frame.len()).into_bytes());
            packed.extend_from_slice(frame);
        }
        packed
    }

    fn pack(lines: &[&str]) -> Vec<u8> {
        pack_bytes(&lines.iter().map(|line| line.as_bytes()).collect::<Vec<_>>())
    }

    fn unpacked(lines: &[&str]) -> (Vec<String>, u32) {
        let (decoded, invalid) = unpack(&pack(lines), lines.len()).expect("well-formed");
        (decoded.into_iter().map(String::from).collect(), invalid)
    }

    #[test]
    fn lines_round_trip_through_the_framing() {
        let lines = [
            "first",
            "",
            "ทดสอบภาษาไทย",
            "emoji 😀 and e\u{301}",
            "00012 digits",
        ];
        let (decoded, invalid) = unpacked(&lines);
        assert_eq!(decoded, lines);
        assert_eq!(invalid, 0);
    }

    #[test]
    fn a_line_may_contain_anything_a_delimiter_could_be() {
        // Length framing, so none of these can split or merge a line.
        let lines = ["a\nb", "\u{0}", "\r\n", "|;,\t"];
        let (decoded, invalid) = unpacked(&lines);
        assert_eq!(decoded, lines);
        assert_eq!(invalid, 0);
    }

    #[test]
    fn an_empty_buffer_is_no_lines() {
        let (lines, invalid) = unpack(&[], 0).expect("empty");
        assert!(lines.is_empty());
        assert_eq!(invalid, 0);
    }

    #[test]
    fn a_zero_length_line_is_an_empty_string_not_a_dropped_line() {
        let (decoded, invalid) = unpacked(&["before", "", "after"]);
        assert_eq!(decoded, ["before", "", "after"]);
        assert_eq!(invalid, 0);
    }

    #[test]
    fn a_32767_byte_line_round_trips_whole() {
        let line = "x".repeat(32_767);
        let (decoded, invalid) = unpacked(&[line.as_str()]);
        assert_eq!(decoded, [line]);
        assert_eq!(invalid, 0);
    }

    #[test]
    fn malformed_framing_is_an_error_not_a_guess() {
        for (packed, expected) in [
            (&b"0003ab"[..], 1),       // four-digit prefix
            (&b"00005abc"[..], 1),     // shorter than it says
            (&b"x0001a"[..], 1),       // not a digit
            (&b"00001a00001b"[..], 1), // more lines than reported
        ] {
            let error = unpack(packed, expected).expect_err(&format!("{packed:?}"));
            assert_eq!(error.kind(), ErrorKind::DataConversion, "{packed:?}");
        }
    }

    #[test]
    fn a_truncated_frame_is_an_error() {
        // The prefix promises 2 bytes ("00002"); only 1 is present.
        let packed = pack_bytes(&[b"ab"]);
        let mut short = packed[..5].to_vec(); // just the prefix "00002"
        short.extend_from_slice(b"a"); // one byte short of the two promised
        let error = unpack(&short, 1).expect_err("truncated frame");
        assert_eq!(error.kind(), ErrorKind::DataConversion);
    }

    #[test]
    fn a_line_count_mismatch_is_an_error() {
        // Well-formed framing, but more lines are packed than the server's
        // own `l_count` (passed in here as `expected`) said it sent.
        // `unpack` catches this bound directly; `take` layers the symmetric
        // "fewer than declared" check on top (`lines.len() != count`), which
        // needs a live connection to exercise and is covered on the real
        // database instead.
        let packed = pack(&["one", "two"]);
        let error = unpack(&packed, 1).expect_err("count mismatch");
        assert_eq!(error.kind(), ErrorKind::DataConversion);
    }

    #[test]
    fn invalid_utf8_bytes_are_replaced_and_counted_not_dropped() {
        // 0x41 'A', 0xFF (never valid in UTF-8, any position), 0x42 'B' —
        // exactly `UTL_RAW.CAST_TO_RAW(UTL_RAW.CAST_TO_VARCHAR2('41FF42'))`
        // would decode to on the server, the review's reproduction.
        let bad_frame: &[u8] = &[0x41, 0xFF, 0x42];
        let good_before: &[u8] = b"before";
        let good_after: &[u8] = b"after";
        let packed = pack_bytes(&[good_before, bad_frame, good_after]);
        let (lines, invalid) = unpack(&packed, 3).expect("framing is well-formed");
        assert_eq!(invalid, 1, "exactly the one bad line is counted");
        assert_eq!(lines.len(), 3, "the good lines are not lost with it");
        assert_eq!(lines[0].as_ref(), "before");
        assert_eq!(lines[1].as_ref(), "A\u{FFFD}B");
        assert_eq!(lines[2].as_ref(), "after");
    }

    #[test]
    fn decode_line_reports_which_lines_were_replaced() {
        let (line, bad) = decode_line(b"clean");
        assert_eq!(line.as_ref(), "clean");
        assert!(!bad);

        let (line, bad) = decode_line(&[0xFF]);
        assert_eq!(line.as_ref(), "\u{FFFD}");
        assert!(bad);
    }

    #[test]
    fn a_byte_count_is_not_a_character_count() {
        // Five Thai characters are fifteen UTF-8 bytes; the prefix counts
        // bytes (M2.12), never characters.
        let packed = pack(&["ทดสอบ", "x"]);
        let (lines, invalid) = unpack(&packed, 2).expect("well-formed");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].as_ref(), "ทดสอบ");
        assert_eq!(lines[1].as_ref(), "x");
        assert_eq!(invalid, 0);
    }

    #[test]
    fn a_requested_buffer_is_clamped_the_way_the_server_clamps_it() {
        let bytes = |n: u32| {
            ServerOutputSetting::Enabled(ServerOutputBuffer::Bytes(
                NonZeroU32::new(n).expect("non-zero"),
            ))
        };
        assert_eq!(effective(bytes(100)), bytes(MIN_BUFFER_BYTES));
        assert_eq!(effective(bytes(20_000)), bytes(20_000));
        assert_eq!(effective(bytes(2_000_000)), bytes(MAX_BUFFER_BYTES));
        let unlimited = ServerOutputSetting::Enabled(ServerOutputBuffer::Unlimited);
        assert_eq!(effective(unlimited), unlimited);
        assert_eq!(
            effective(ServerOutputSetting::Disabled),
            ServerOutputSetting::Disabled
        );
    }
}
