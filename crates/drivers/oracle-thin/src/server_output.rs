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
//! loop and packs the lines it gets into one `VARCHAR2(32767)`, each preceded
//! by a fixed-width, five-digit count of its characters (`LENGTH4`: Unicode
//! code points, which is what a Rust `char` is). Framing is by length, never
//! by a delimiter, because a line may contain any character at all — a
//! newline, a NUL, the digits of a length.
//!
//! A line is consumed from the server's buffer the moment `GET_LINE` returns
//! it, and there is no way to put it back. So the line that does not fit in
//! the packed buffer is returned **on its own**, unframed, in a second bind
//! (`:tail`), and the block stops there. Every round trip therefore returns at
//! least one line when there is one, a line of any legal length (up to
//! 32,767 bytes) always fits in the tail, and nothing is ever lost between
//! calls.
//!
//! Both text binds are declared `LONG`, not `VARCHAR`. A pure OUT `VARCHAR` bind
//! is sized from the type's 4,000-character default and the server refuses
//! more than 16,000 bytes into it (`ORA-06502`); a `LONG` OUT bind has no
//! client-side ceiling below PL/SQL's own 32,767 bytes. Measured on 19.3 with
//! Thai text: 32,767 bytes in, 32,767 bytes out, byte for byte.
//!
//! **Measured cost** (`tests/m2_7_server_output.rs`, `v$mystat` "SQL*Net
//! roundtrips to/from client"): 10,003 lines — 10,000 short ones, an empty
//! one, a Thai/emoji one and one of 32,767 bytes — in **6 round trips**, where
//! `GET_LINE` would take 10,004.
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

use oracledb::{Connection, DB_TYPE_LONG, DB_TYPE_NUMBER, DbType, ToDbValue};
use reldex_db_driver_api::{
    DbError, DbResult, ErrorKind, ServerOutputBuffer, ServerOutputChunk, ServerOutputSetting,
};

/// The smallest buffer `DBMS_OUTPUT.ENABLE` accepts; smaller requests are
/// raised to it by the server.
pub(crate) const MIN_BUFFER_BYTES: u32 = 2_000;

/// The largest buffer `DBMS_OUTPUT.ENABLE` accepts; larger requests are
/// lowered to it by the server. Only `NULL` (unlimited) goes beyond.
pub(crate) const MAX_BUFFER_BYTES: u32 = 1_000_000;

/// The most bytes one PL/SQL `VARCHAR2` can hold, and so the most the packed
/// buffer carries per round trip.
const MAX_PACKED_BYTES: usize = 32_767;

/// Width of the character-count prefix in front of each packed line. A
/// `DBMS_OUTPUT` line is at most 32,767 bytes, so at most 32,767 characters:
/// five digits always suffice.
const PREFIX_WIDTH: usize = 5;

/// The take block. Binds, in order of first appearance: `:1` the most lines to
/// take, `:2` the byte budget for the packed buffer, then the outputs `:3`
/// packed lines, `:4` the tail line, `:5` whether there is a tail line, `:6`
/// how many lines were taken, `:7` whether `GET_LINE` reported the buffer
/// empty. Each output is assigned exactly once, at the end.
const TAKE_BLOCK: &str = "DECLARE
  l_line     VARCHAR2(32767);
  l_status   INTEGER;
  l_buf      VARCHAR2(32767);
  l_used     PLS_INTEGER := 0;
  l_count    PLS_INTEGER := 0;
  l_len      PLS_INTEGER;
  l_max      PLS_INTEGER := :1;
  l_budget   PLS_INTEGER := :2;
  l_tail     VARCHAR2(32767);
  l_has_tail PLS_INTEGER := 0;
  l_drained  PLS_INTEGER := 0;
BEGIN
  WHILE l_count < l_max LOOP
    DBMS_OUTPUT.GET_LINE(l_line, l_status);
    IF l_status <> 0 THEN
      l_drained := 1;
      EXIT;
    END IF;
    l_count := l_count + 1;
    l_len := NVL(LENGTHB(l_line), 0);
    IF l_used + 5 + l_len <= l_budget THEN
      l_buf := l_buf || TO_CHAR(NVL(LENGTH4(l_line), 0), 'FM00000') || l_line;
      l_used := l_used + 5 + l_len;
    ELSE
      l_tail := l_line;
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
        ServerOutputSetting::Disabled => connection.execute("BEGIN DBMS_OUTPUT.DISABLE; END;", &[]),
        ServerOutputSetting::Enabled(ServerOutputBuffer::Unlimited) => {
            connection.execute("BEGIN DBMS_OUTPUT.ENABLE(NULL); END;", &[])
        }
        ServerOutputSetting::Enabled(ServerOutputBuffer::Bytes(bytes)) => {
            let size = i64::from(clamp_buffer(bytes).get());
            connection.execute("BEGIN DBMS_OUTPUT.ENABLE(:1); END;", &[&size])
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
    let long: &'static DbType = &DB_TYPE_LONG;
    let number: &'static DbType = &DB_TYPE_NUMBER;
    let params: [&dyn ToDbValue; 7] =
        [&max_lines, &budget, &long, &long, &number, &number, &number];
    let mut result = connection
        .execute(TAKE_BLOCK, &params)
        .map_err(|error| crate::error::map(&error))?;
    let mut row = result.out_bind_data();
    let take_err = |error: oracledb::Error| crate::error::map(&error);
    let packed: Option<String> = row.take(0).map_err(take_err)?;
    let tail: Option<String> = row.take(1).map_err(take_err)?;
    let has_tail: Option<i64> = row.take(2).map_err(take_err)?;
    let count: Option<i64> = row.take(3).map_err(take_err)?;
    let drained: Option<i64> = row.take(4).map_err(take_err)?;

    let count = usize::try_from(count.unwrap_or(0)).map_err(|_| framing_error("negative count"))?;
    let mut lines = unpack(packed.as_deref().unwrap_or(""), count)?;
    if has_tail == Some(1) {
        // NULL is an empty line, exactly as in the packed part.
        lines.push(tail.unwrap_or_default().into_boxed_str());
    }
    if lines.len() != count {
        return Err(framing_error(&format!(
            "the server reported {count} lines and {} were decoded",
            lines.len()
        )));
    }
    Ok(ServerOutputChunk::new(lines, drained == Some(1)))
}

/// Splits the packed buffer back into lines.
///
/// Each line is five ASCII digits giving its length in characters, then that
/// many characters. Anything else — a short buffer, a non-digit prefix, more
/// lines than the server said it sent — is reported as an error rather than
/// guessed at: a mis-split would hand the user lines that were never printed.
fn unpack(packed: &str, expected: usize) -> DbResult<Vec<Box<str>>> {
    let mut lines = Vec::with_capacity(expected);
    let mut rest = packed;
    while !rest.is_empty() {
        let prefix = rest
            .get(..PREFIX_WIDTH)
            .filter(|prefix| prefix.bytes().all(|byte| byte.is_ascii_digit()))
            .ok_or_else(|| framing_error("a line's length prefix is missing or malformed"))?;
        let chars: usize = prefix
            .parse()
            .map_err(|_| framing_error("a line's length prefix is not a number"))?;
        let body = &rest[PREFIX_WIDTH..];
        let end = byte_offset_after_chars(body, chars)
            .ok_or_else(|| framing_error("a line is shorter than its length prefix says"))?;
        lines.push(Box::from(&body[..end]));
        rest = &body[end..];
        if lines.len() > expected {
            return Err(framing_error(
                "more lines were decoded than the server sent",
            ));
        }
    }
    Ok(lines)
}

/// The byte offset just past the first `chars` characters of `text`, or
/// `None` when it has fewer.
fn byte_offset_after_chars(text: &str, chars: usize) -> Option<usize> {
    if chars == 0 {
        return Some(0);
    }
    let mut seen = 0_usize;
    for (offset, character) in text.char_indices() {
        seen += 1;
        if seen == chars {
            return Some(offset + character.len_utf8());
        }
    }
    None
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

    fn pack(lines: &[&str]) -> String {
        lines
            .iter()
            .map(|line| format!("{:05}{line}", line.chars().count()))
            .collect()
    }

    fn unpacked(lines: &[&str]) -> Vec<String> {
        unpack(&pack(lines), lines.len())
            .expect("well-formed")
            .into_iter()
            .map(String::from)
            .collect()
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
        assert_eq!(unpacked(&lines), lines);
    }

    #[test]
    fn a_line_may_contain_anything_a_delimiter_could_be() {
        // Length framing, so none of these can split or merge a line.
        let lines = ["a\nb", "\u{0}", "\r\n", "|;,\t"];
        assert_eq!(unpacked(&lines), lines);
    }

    #[test]
    fn an_empty_buffer_is_no_lines() {
        assert!(unpack("", 0).expect("empty").is_empty());
    }

    #[test]
    fn malformed_framing_is_an_error_not_a_guess() {
        for (packed, expected) in [
            ("0003ab", 1),       // four-digit prefix
            ("00005abc", 1),     // shorter than it says
            ("x0001a", 1),       // not a digit
            ("00001a00001b", 1), // more lines than reported
        ] {
            let error = unpack(packed, expected).expect_err(packed);
            assert_eq!(error.kind(), ErrorKind::DataConversion, "{packed}");
        }
    }

    #[test]
    fn a_character_count_is_not_a_byte_count() {
        // Five Thai characters are fifteen UTF-8 bytes; the prefix counts
        // characters, which is what `LENGTH4` reports on the server.
        let packed = "00005ทดสอบ00001x";
        let lines = unpack(packed, 2).expect("well-formed");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].as_ref(), "ทดสอบ");
        assert_eq!(lines[1].as_ref(), "x");
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
