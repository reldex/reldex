//! Bounded-memory streaming over large objects (ADR-0002 D5; `SPEC.md` §12).
//!
//! The wrapper always asks `oracledb` for LOB *locators*
//! (`Statement::fetch_lobs`) rather than letting it materialize CLOB/BLOB
//! values into the row, so peak memory is this module's staging buffer plus
//! whatever the caller passes to `read_chunk`, not the size of the object.
//!
//! # Why there is a staging buffer
//!
//! Two properties of `oracledb`'s `io::Read for Lob` make a direct pass-through
//! wrong, and both are about character LOBs:
//!
//! - It sizes its server-side request as `buf.len() / 3` **UCS-2 units**, so a
//!   caller buffer smaller than 3 bytes would ask for one unit and a caller
//!   buffer of any size can land the request boundary in the middle of a
//!   surrogate pair. When that happens the UTF-16 decode fails
//!   (`InvalidEncodedString`) and *the whole read fails* — a non-BMP character
//!   such as an emoji in a CLOB is enough. This is the upstream issue-#18 class
//!   of problem that ADR-0002 tells the wrapper to absorb. The fix is to retry
//!   with one fewer unit: the unit that was cut is a high surrogate, so the
//!   shorter request ends on a complete character.
//! - It returns `InvalidInput` instead of a short read when the decoded UTF-8 is
//!   larger than the caller's buffer.
//!
//! Reading into a staging buffer of a fixed size and serving `read_chunk` out
//! of it makes both problems disappear, keeps memory bounded, and lets
//! `read_chunk` honour its contract for any caller buffer of four bytes or more
//! — including never splitting a UTF-8 sequence across two chunks.

use std::io::Read as _;

use reldex_db_driver_api::{ConnectionId, DbError, DbResult, ErrorKind, LobKind, LobStream};

use oracledb::Lob;

use crate::conn::Closed;

/// How much is read from the server at a time.
///
/// Also the upper bound on this stream's own memory: one such buffer per open
/// LOB, independent of the object's size.
const STAGING_BYTES: usize = 64 * 1024;

/// The smallest staging buffer worth retrying with after a split surrogate.
const MIN_STAGING_BYTES: usize = 64;

/// A forward-only stream over one Oracle LOB.
pub(crate) struct OracleLobStream {
    lob: Lob,
    kind: LobKind,
    connection: ConnectionId,
    closed: Closed,
    size_hint: Option<u64>,
    staging: Vec<u8>,
    filled: usize,
    position: usize,
    exhausted: bool,
    failed: bool,
}

impl OracleLobStream {
    /// Wraps a locator the fetch produced.
    pub(crate) fn new(
        mut lob: Lob,
        kind: LobKind,
        connection: ConnectionId,
        closed: Closed,
    ) -> Self {
        // `get_size` is answered from the locator the fetch already returned,
        // so this is not an extra round trip in the normal case.
        let size_hint = lob.get_size().ok().map(|size| size as u64);
        Self {
            lob,
            kind,
            connection,
            closed,
            size_hint,
            staging: vec![0; STAGING_BYTES],
            filled: 0,
            position: 0,
            exhausted: false,
            failed: false,
        }
    }

    /// Refills the staging buffer, absorbing a request boundary that fell
    /// inside a surrogate pair by retrying with a smaller request.
    fn refill(&mut self) -> DbResult<()> {
        self.position = 0;
        self.filled = 0;
        let mut limit = self.staging.len();
        loop {
            match self.lob.read(&mut self.staging[..limit]) {
                Ok(0) => {
                    self.exhausted = true;
                    return Ok(());
                }
                Ok(read) => {
                    self.filled = read;
                    return Ok(());
                }
                Err(error) if is_split_character(&error) && limit > MIN_STAGING_BYTES => {
                    // One fewer UCS-2 unit: `oracledb` derives its request size
                    // as `len / 3`, so three bytes less asks for one unit less,
                    // which cannot end on the high half of a surrogate pair
                    // twice in a row.
                    limit -= 3;
                }
                Err(error) => {
                    self.failed = true;
                    return Err(DbError::new(
                        ErrorKind::DataConversion,
                        format!("reading a large object failed: {error}"),
                    ));
                }
            }
        }
    }

    /// The largest prefix of `available` that can be copied into `max` bytes
    /// without splitting a UTF-8 sequence.
    fn copyable(&self, available: &[u8], max: usize) -> usize {
        let mut take = available.len().min(max);
        if !self.kind.is_character() || take == available.len() {
            return take;
        }
        // Back off to a UTF-8 boundary. A sequence is at most four bytes, so
        // this loop runs at most three times and cannot reach zero for a caller
        // buffer of four bytes or more.
        while take > 0 && (available[take] & 0xC0) == 0x80 {
            take -= 1;
        }
        take
    }
}

impl LobStream for OracleLobStream {
    fn connection_id(&self) -> ConnectionId {
        self.connection
    }

    fn kind(&self) -> LobKind {
        self.kind
    }

    fn size_hint(&self) -> Option<u64> {
        self.size_hint
    }

    fn read_chunk(&mut self, buf: &mut [u8]) -> DbResult<usize> {
        if self.closed.is_closed() {
            return Err(DbError::connection_closed("LOB stream"));
        }
        if self.failed {
            return Err(DbError::internal(
                "LOB stream read after a failure; the only legal action was to drop it",
            ));
        }
        if buf.is_empty() {
            return Ok(0);
        }
        if self.position == self.filled {
            if self.exhausted {
                return Ok(0);
            }
            self.refill()?;
            if self.filled == 0 {
                return Ok(0);
            }
        }
        let available = &self.staging[self.position..self.filled];
        let take = self.copyable(available, buf.len());
        if take == 0 {
            // A caller buffer smaller than one character. The contract only
            // promises whole sequences from four bytes up.
            return Err(DbError::new(
                ErrorKind::DataConversion,
                "chunk buffer is too small to hold one character",
            ));
        }
        buf[..take].copy_from_slice(&available[..take]);
        self.position += take;
        Ok(take)
    }
}

/// Whether an `io::Error` from `Lob::read` is the split-character failure.
///
/// `oracledb` collapses its own error into `io::Error::other(error.to_string())`
/// for this case, so the only available signal is the text. That is fragile
/// across upstream versions, which is why the wrapper pins an exact one and
/// re-verifies on upgrade.
fn is_split_character(error: &std::io::Error) -> bool {
    let text = error.to_string();
    text.contains("invalid encoded string") || text.contains("invalid utf-16")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for the real stream that exercises the boundary arithmetic
    /// without a database.
    struct Splitter {
        data: Vec<u8>,
        position: usize,
        kind: LobKind,
    }

    impl Splitter {
        fn copyable(&self, max: usize) -> usize {
            let available = &self.data[self.position..];
            let mut take = available.len().min(max);
            if !self.kind.is_character() || take == available.len() {
                return take;
            }
            while take > 0 && (available[take] & 0xC0) == 0x80 {
                take -= 1;
            }
            take
        }
    }

    #[test]
    fn a_chunk_boundary_never_splits_a_utf8_sequence() {
        // Thai is three bytes per character, an emoji four: a naive byte split
        // would hand the caller invalid UTF-8.
        let text = "ทดสอบภาษาไทย😀ok";
        let mut splitter = Splitter {
            data: text.as_bytes().to_vec(),
            position: 0,
            kind: LobKind::Character,
        };
        let mut collected = Vec::new();
        for max in [4_usize, 5, 7, 11, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4] {
            if splitter.position == splitter.data.len() {
                break;
            }
            let take = splitter.copyable(max);
            assert!(take > 0, "a four-byte buffer must always make progress");
            let chunk = &splitter.data[splitter.position..splitter.position + take];
            assert!(
                std::str::from_utf8(chunk).is_ok(),
                "chunk {chunk:?} is not valid UTF-8 on its own"
            );
            collected.extend_from_slice(chunk);
            splitter.position += take;
        }
        assert_eq!(String::from_utf8(collected).expect("valid"), text);
    }

    #[test]
    fn binary_chunks_are_not_backed_off() {
        let splitter = Splitter {
            data: vec![0xFF; 10],
            position: 0,
            kind: LobKind::Binary,
        };
        assert_eq!(splitter.copyable(4), 4);
    }

    #[test]
    fn the_upstream_split_surrogate_failure_is_recognized() {
        let error = std::io::Error::other("invalid encoded string: invalid utf-16: lone surrogate");
        assert!(is_split_character(&error));
        let other = std::io::Error::other("connection reset");
        assert!(!is_split_character(&other));
    }

    #[test]
    fn character_kinds_are_reported_from_the_column_type() {
        assert!(LobKind::Character.is_character());
        assert!(LobKind::NationalCharacter.is_character());
        assert!(!LobKind::Binary.is_character());
    }
}
