//! Lazy, bounded-memory access to large objects (ADR-0002 D5).
//!
//! A LOB never becomes a materialized value. The driver hands the core a
//! [`LobLocator`], and the core reads it in chunks it sizes itself, so peak
//! memory is the caller's buffer rather than the object
//! (`SPEC.md` §12 "lazy large-value access", §21 "large export must stream").

use std::fmt;

use crate::error::DbResult;

/// What a large object contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LobKind {
    /// Binary data (`BLOB`). Chunks are raw bytes.
    Binary,
    /// Character data (`CLOB`). Chunks are UTF-8.
    Character,
    /// National character data (`NCLOB`). Chunks are UTF-8.
    NationalCharacter,
}

impl LobKind {
    /// Whether chunks read from this kind of object are UTF-8 text.
    #[must_use]
    pub const fn is_character(self) -> bool {
        matches!(self, Self::Character | Self::NationalCharacter)
    }
}

/// A driver-owned, forward-only stream over one large object.
///
/// Implementations live in driver crates and hold whatever native locator the
/// vendor protocol needs; nothing about that leaks through this trait.
pub trait LobStream: Send {
    /// What the object contains.
    fn kind(&self) -> LobKind;

    /// Total size in bytes, if the driver can report it cheaply.
    ///
    /// This is a hint for buffer sizing and progress reporting only. Callers
    /// must still read until [`LobStream::read_chunk`] returns `0`.
    fn size_hint(&self) -> Option<u64>;

    /// Reads the next chunk into `buf`, returning the number of bytes written.
    ///
    /// A return value of `0` means the object is exhausted. For character kinds
    /// the bytes are UTF-8 and an implementation must not split a multi-byte
    /// sequence across chunks when `buf.len() >= 4`, so a caller can decode each
    /// chunk independently.
    ///
    /// # Errors
    ///
    /// Any [`crate::DbError`] the underlying read produced.
    fn read_chunk(&mut self, buf: &mut [u8]) -> DbResult<usize>;
}

/// A handle to a large object that has not been read.
///
/// Held inside a [`crate::Value`] or a result [`crate::Column`], it costs one
/// pointer and no data. Reading requires ownership, which is why a locator is
/// taken out of a batch rather than borrowed from it.
pub struct LobLocator {
    stream: Box<dyn LobStream>,
}

impl LobLocator {
    /// Wraps a driver-provided stream.
    #[must_use]
    pub fn new(stream: Box<dyn LobStream>) -> Self {
        Self { stream }
    }

    /// What the object contains.
    #[must_use]
    pub fn kind(&self) -> LobKind {
        self.stream.kind()
    }

    /// Total size in bytes, if the driver can report it cheaply.
    #[must_use]
    pub fn size_hint(&self) -> Option<u64> {
        self.stream.size_hint()
    }

    /// Reads the next chunk. See [`LobStream::read_chunk`].
    ///
    /// # Errors
    ///
    /// Any [`crate::DbError`] the underlying read produced.
    pub fn read_chunk(&mut self, buf: &mut [u8]) -> DbResult<usize> {
        self.stream.read_chunk(buf)
    }
}

impl fmt::Debug for LobLocator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LobLocator")
            .field("kind", &self.kind())
            .field("size_hint", &self.size_hint())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SliceLob {
        data: &'static [u8],
        position: usize,
        kind: LobKind,
    }

    impl LobStream for SliceLob {
        fn kind(&self) -> LobKind {
            self.kind
        }

        fn size_hint(&self) -> Option<u64> {
            Some(self.data.len() as u64)
        }

        fn read_chunk(&mut self, buf: &mut [u8]) -> DbResult<usize> {
            let remaining = &self.data[self.position..];
            let take = remaining.len().min(buf.len());
            buf[..take].copy_from_slice(&remaining[..take]);
            self.position += take;
            Ok(take)
        }
    }

    fn locator(data: &'static [u8], kind: LobKind) -> LobLocator {
        LobLocator::new(Box::new(SliceLob {
            data,
            position: 0,
            kind,
        }))
    }

    #[test]
    fn reads_in_bounded_chunks_until_exhausted() {
        let mut lob = locator(b"abcdefgh", LobKind::Binary);
        assert_eq!(lob.size_hint(), Some(8));
        let mut buf = [0_u8; 3];
        let mut collected = Vec::new();
        loop {
            let read = lob.read_chunk(&mut buf).expect("read");
            if read == 0 {
                break;
            }
            collected.extend_from_slice(&buf[..read]);
        }
        assert_eq!(collected, b"abcdefgh");
    }

    #[test]
    fn character_kinds_are_flagged() {
        assert!(LobKind::Character.is_character());
        assert!(LobKind::NationalCharacter.is_character());
        assert!(!LobKind::Binary.is_character());
    }

    #[test]
    fn debug_shows_shape_not_content() {
        let lob = locator("ข้อมูล".as_bytes(), LobKind::NationalCharacter);
        let rendered = format!("{lob:?}");
        assert!(rendered.contains("NationalCharacter"), "{rendered}");
        assert!(!rendered.contains("ข้อมูล"), "{rendered}");
    }
}
