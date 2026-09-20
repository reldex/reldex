//! Lazy, bounded-memory access to large objects (ADR-0002 D5).
//!
//! A LOB never becomes a materialized value. The driver hands the core a
//! [`LobLocator`], and the core reads it in chunks it sizes itself, so peak
//! memory is the caller's buffer rather than the object
//! (`SPEC.md` §12 "lazy large-value access", §21 "large export must stream").

use std::fmt;

use crate::error::DbResult;
use crate::ids::ConnectionId;

/// What a large object contains.
///
/// `#[non_exhaustive]`: `BFILE` and temporary-LOB distinctions are plausible
/// additions that must not break the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
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
///
/// # Thread affinity
///
/// A `LobStream` is `Send`, so the type system will happily let it travel to
/// another thread — and it must not. Like a [`crate::Cursor`], it is a *derived
/// handle*: it reads over the connection that produced it, which lives on one
/// `db-core` worker thread. Using it anywhere else would issue protocol traffic
/// on a connection another thread believes it owns.
///
/// **Dropping counts as using it.** A stream's `Drop` releases a driver-side
/// locator, which is driver work like any other, so a stream — and a
/// [`crate::RowBatch`] still holding one, and a [`LobLocator`] inside a
/// [`crate::Value`] — must be dropped on the owning worker thread too. With a
/// driver that serialises its calls through an internal mutex, getting this
/// wrong is a deadlock rather than a diagnosable error.
///
/// Only plain data crosses threads: a [`crate::RowBatch`] whose locators have
/// all been taken out. Nothing in the type system enforces this, which is
/// precisely why [`LobStream::connection_id`] exists: `db-core` asserts against
/// it, and takes every locator out of a batch before the batch goes anywhere.
///
/// # Lifecycle
///
/// - **After any error** from [`LobStream::read_chunk`] the stream is finished.
///   The only legal action is to drop it. A further `read_chunk` must return a
///   [`crate::DbError`] — it must never panic, block, or resume reading as if
///   nothing happened.
/// - **After the owning connection is closed**, every remaining stream returns
///   [`crate::DbError::connection_closed`] from `read_chunk`. It must never
///   panic and never block on a connection that no longer exists.
/// - **A commit or rollback may invalidate the locator.** Most servers scope a
///   LOB locator to the transaction that produced it, so the driver must expect
///   reads after a commit or rollback to fail and must report that as an
///   ordinary [`crate::DbError`] (`ErrorKind::Transaction` when the server says
///   so). `db-core` therefore treats an open locator as transaction-scoped and
///   must not promise the user otherwise.
///
/// `Sync` as well as `Send`: every method that changes a stream takes
/// `&mut self`, so the bound costs an implementor nothing it was not already
/// doing (all four implementors in this workspace satisfied it unchanged), and
/// it is what lets a `RowBatch` — which may hold parked locators — be shared
/// between threads at all. The FFI boundary depends on that: it documents
/// concurrent read-only access to a fetched batch as sound (ADR-0003 D4,
/// ADR-0002 amendment I3), and without `Sync` here that promise would be a
/// false one about `&RowBatch`.
///
/// This does **not** weaken the single-thread rule above: `db-core` still
/// touches a stream only on the worker thread that owns its connection.
/// `Sync` says a shared reference may cross a thread boundary, not that a read
/// may.
pub trait LobStream: Send + Sync {
    /// The connection this stream reads over.
    ///
    /// `db-core` asserts that a stream is only touched on the worker thread that
    /// owns this connection. Must be cheap: it is an accessor on a field the
    /// driver already holds, never a round trip.
    fn connection_id(&self) -> ConnectionId;

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
    /// chunk independently. "Multi-byte sequence" includes a surrogate pair in
    /// the driver's own encoding: a driver whose native read can cut a non-BMP
    /// character in half must re-join the halves before returning UTF-8 here.
    ///
    /// # Errors
    ///
    /// Any [`crate::DbError`] the underlying read produced. After an error the
    /// stream is finished; see the trait's lifecycle rules.
    fn read_chunk(&mut self, buf: &mut [u8]) -> DbResult<usize>;
}

/// A handle to a large object that has not been read.
///
/// Held inside a [`crate::Value`] or a result [`crate::Column`], it costs one
/// pointer and no data. Reading requires ownership, which is why a locator is
/// taken out of a batch rather than borrowed from it.
///
/// It carries the thread-affinity and lifecycle rules of the [`LobStream`] it
/// wraps — including that **dropping** it is driver work and belongs on the
/// owning worker thread; read those before implementing or consuming one.
pub struct LobLocator {
    stream: Box<dyn LobStream>,
}

impl LobLocator {
    /// Wraps a driver-provided stream.
    #[must_use]
    pub fn new(stream: Box<dyn LobStream>) -> Self {
        Self { stream }
    }

    /// The connection this locator reads over. See [`LobStream::connection_id`].
    #[must_use]
    pub fn connection_id(&self) -> ConnectionId {
        self.stream.connection_id()
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
            .field("connection", &format_args!("{}", self.connection_id()))
            .field("kind", &self.kind())
            .field("size_hint", &self.size_hint())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::error::ErrorKind;

    struct SliceLob {
        connection: ConnectionId,
        data: &'static [u8],
        position: usize,
        kind: LobKind,
        /// Set once an error has been reported; a conforming stream must then
        /// refuse further reads instead of resuming.
        finished: bool,
        closed: bool,
    }

    impl LobStream for SliceLob {
        fn connection_id(&self) -> ConnectionId {
            self.connection
        }

        fn kind(&self) -> LobKind {
            self.kind
        }

        fn size_hint(&self) -> Option<u64> {
            Some(self.data.len() as u64)
        }

        fn read_chunk(&mut self, buf: &mut [u8]) -> DbResult<usize> {
            if self.closed {
                return Err(crate::error::DbError::connection_closed("LOB stream"));
            }
            if self.finished {
                return Err(crate::error::DbError::internal(
                    "LOB stream read after a failure; the only legal action was to drop it",
                ));
            }
            let remaining = &self.data[self.position..];
            let take = remaining.len().min(buf.len());
            buf[..take].copy_from_slice(&remaining[..take]);
            self.position += take;
            Ok(take)
        }
    }

    fn lob(data: &'static [u8], kind: LobKind) -> SliceLob {
        SliceLob {
            connection: ConnectionId::from_raw(11),
            data,
            position: 0,
            kind,
            finished: false,
            closed: false,
        }
    }

    fn locator(data: &'static [u8], kind: LobKind) -> LobLocator {
        LobLocator::new(Box::new(lob(data, kind)))
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

    #[test]
    fn a_locator_reports_the_connection_that_owns_it() {
        // `db-core` asserts on this before touching a derived handle: nothing in
        // the type system stops a `Send` stream from reaching the wrong thread.
        let lob = locator(b"abc", LobKind::Binary);
        assert_eq!(lob.connection_id(), ConnectionId::from_raw(11));
        assert!(format!("{lob:?}").contains("ConnectionId#11"));
    }

    #[test]
    fn a_stream_reports_rather_than_panics_after_failure_or_close() {
        let mut stream = lob(b"abcdefgh", LobKind::Binary);
        stream.finished = true;
        let error = stream
            .read_chunk(&mut [0_u8; 4])
            .expect_err("a finished stream must refuse further reads");
        assert_eq!(error.kind(), ErrorKind::DriverInternal);

        let mut stream = lob(b"abcdefgh", LobKind::Binary);
        stream.closed = true;
        let error = stream
            .read_chunk(&mut [0_u8; 4])
            .expect_err("a stream whose connection closed must refuse reads");
        assert_eq!(error.kind(), ErrorKind::DriverInternal);
        assert_eq!(error.session_state(), crate::error::SessionState::Lost);
    }
}
