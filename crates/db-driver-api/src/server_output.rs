//! Server output: text a session writes **out of band** while a statement
//! runs, which the server buffers until a client asks for it — Oracle's
//! `DBMS_OUTPUT` is the case this exists for (ADR-0002 amendment T,
//! `docs/exec-plans/active/phase-1.md` §B4 item 2).
//!
//! The contract is deliberately small and vendor-neutral. A driver that has
//! nothing like it inherits the defaulted [`crate::DatabaseConnection`]
//! methods, which answer [`crate::ErrorKind::Unsupported`], and leaves
//! [`crate::Capabilities::server_output`] off. A driver that has it owns every
//! vendor statement involved; nothing above the driver ever sees one.
//!
//! # Cost, and who pays it
//!
//! Reading server output is a round trip, so it must never happen for a
//! session that did not ask for it. The contract makes that the caller's
//! decision: a driver issues nothing on its own, and `db-core` calls
//! [`crate::DatabaseConnection::set_server_output`] and
//! [`crate::DatabaseConnection::take_server_output`] only for a session whose
//! user turned the output pane on.

use std::num::NonZeroU32;

/// How much output the server may buffer for one session.
///
/// The limit is the server's: a statement that writes more than it allows is
/// the *statement* failing (for `DBMS_OUTPUT`, `ORA-20000` / `ORU-10027`), and
/// is reported as that statement's error. Nothing is truncated silently on
/// either side.
///
/// Deliberately **not** `#[non_exhaustive]`: "limited to N bytes" or "not
/// limited" is the whole truth table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServerOutputBuffer {
    /// No limit other than the server's memory.
    Unlimited,
    /// At most this many bytes, in the server's character set.
    ///
    /// A driver may have to adjust the number to a range its server accepts;
    /// it reports the value it actually used — see
    /// [`crate::DatabaseConnection::set_server_output`].
    Bytes(NonZeroU32),
}

/// Whether a session collects server output, and with what buffer.
///
/// One enum rather than an `(enabled, buffer)` pair, so that "disabled, with a
/// 20,000-byte buffer" is not a value anyone has to interpret.
///
/// Deliberately **not** `#[non_exhaustive]`, for the same reason as
/// [`ServerOutputBuffer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ServerOutputSetting {
    /// The server does not buffer output for this session. The default: it is
    /// what a freshly opened session is assumed to be.
    #[default]
    Disabled,
    /// The server buffers output, up to this limit.
    Enabled(ServerOutputBuffer),
}

impl ServerOutputSetting {
    /// Whether output is being collected.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled(_))
    }

    /// The buffer, when output is being collected.
    #[must_use]
    pub const fn buffer(self) -> Option<ServerOutputBuffer> {
        match self {
            Self::Enabled(buffer) => Some(buffer),
            Self::Disabled => None,
        }
    }
}

/// One bounded batch of server output, in the order the server produced it.
///
/// Returned by [`crate::DatabaseConnection::take_server_output`]. The lines
/// are the lines the server holds, **exactly**: an empty line is an empty
/// string (never dropped, never merged into its neighbour), and each line is
/// decoded as UTF-8 **independently of the others** (ADR-0002 amendment T,
/// M2.12) — one line's bytes being invalid never costs the rest of the chunk.
/// A line whose bytes are not valid UTF-8 is still delivered, with the
/// invalid sequences replaced by U+FFFD, and counted in
/// [`ServerOutputChunk::invalid_utf8_lines`] rather than silently accepted or
/// dropped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerOutputChunk {
    lines: Vec<Box<str>>,
    drained: bool,
    invalid_utf8_lines: u32,
}

impl ServerOutputChunk {
    /// A chunk of `lines`. `drained` says whether the driver knows the server
    /// now holds nothing more; see [`ServerOutputChunk::is_drained`]. No line
    /// is reported invalid; see [`ServerOutputChunk::with_invalid_utf8_lines`]
    /// for a driver that must report some.
    #[must_use]
    pub const fn new(lines: Vec<Box<str>>, drained: bool) -> Self {
        Self {
            lines,
            drained,
            invalid_utf8_lines: 0,
        }
    }

    /// An empty chunk from a server that holds nothing.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            lines: Vec::new(),
            drained: true,
            invalid_utf8_lines: 0,
        }
    }

    /// Records that `invalid_utf8_lines` of this chunk's lines were not valid
    /// UTF-8 on the wire and were delivered with U+FFFD in place of the
    /// offending bytes — see [`ServerOutputChunk::invalid_utf8_lines`].
    /// Additive: a driver with nothing to report never calls this, and the
    /// count stays zero.
    #[must_use]
    pub const fn with_invalid_utf8_lines(mut self, invalid_utf8_lines: u32) -> Self {
        self.invalid_utf8_lines = invalid_utf8_lines;
        self
    }

    /// The lines, in order.
    #[must_use]
    pub fn lines(&self) -> &[Box<str>] {
        &self.lines
    }

    /// How many lines this chunk carries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether this chunk carries no lines.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Whether the driver **knows** the server's buffer is now empty.
    ///
    /// `false` means only "there may be more": the caller asks again. A driver
    /// that cannot tell reports `false` and lets the next call return an
    /// empty, drained chunk. An empty chunk must always be reported drained —
    /// a caller is entitled to stop on one rather than spin.
    #[must_use]
    pub const fn is_drained(&self) -> bool {
        self.drained
    }

    /// How many of [`ServerOutputChunk::lines`] were not valid UTF-8 on the
    /// wire. Such a line is still present in `lines`, with each invalid byte
    /// sequence replaced by U+FFFD, never dropped and never allowed to cost
    /// the other lines of the same chunk. Zero normally; the caller decides
    /// whether and how to flag the affected lines in the UI.
    #[must_use]
    pub const fn invalid_utf8_lines(&self) -> u32 {
        self.invalid_utf8_lines
    }

    /// Takes the lines out.
    #[must_use]
    pub fn into_lines(self) -> Vec<Box<str>> {
        self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_starts_with_output_disabled() {
        assert_eq!(
            ServerOutputSetting::default(),
            ServerOutputSetting::Disabled
        );
        assert!(!ServerOutputSetting::Disabled.is_enabled());
        assert_eq!(ServerOutputSetting::Disabled.buffer(), None);
    }

    #[test]
    fn an_enabled_setting_carries_its_buffer() {
        let sized = ServerOutputSetting::Enabled(ServerOutputBuffer::Bytes(
            NonZeroU32::new(20_000).expect("non-zero"),
        ));
        assert!(sized.is_enabled());
        assert_eq!(
            sized.buffer(),
            Some(ServerOutputBuffer::Bytes(
                NonZeroU32::new(20_000).expect("non-zero")
            ))
        );
        let unlimited = ServerOutputSetting::Enabled(ServerOutputBuffer::Unlimited);
        assert_eq!(unlimited.buffer(), Some(ServerOutputBuffer::Unlimited));
    }

    #[test]
    fn an_empty_line_is_a_line() {
        let chunk = ServerOutputChunk::new(vec!["".into(), "ทดสอบ".into()], false);
        assert_eq!(chunk.len(), 2, "an empty line must not vanish");
        assert_eq!(chunk.lines()[0].as_ref(), "");
        assert!(!chunk.is_drained());
        assert_eq!(
            chunk.invalid_utf8_lines(),
            0,
            "nothing was reported invalid"
        );
        assert_eq!(chunk.into_lines().len(), 2);
    }

    #[test]
    fn the_empty_chunk_is_drained() {
        let chunk = ServerOutputChunk::empty();
        assert!(chunk.is_empty());
        assert!(
            chunk.is_drained(),
            "an empty chunk must let the caller stop"
        );
        assert_eq!(chunk.invalid_utf8_lines(), 0);
    }

    #[test]
    fn invalid_utf8_lines_is_additive_and_defaults_to_zero() {
        let plain = ServerOutputChunk::new(vec!["a".into()], true);
        assert_eq!(plain.invalid_utf8_lines(), 0);

        let flagged = ServerOutputChunk::new(vec!["a".into(), "b\u{FFFD}c".into()], true)
            .with_invalid_utf8_lines(1);
        assert_eq!(flagged.invalid_utf8_lines(), 1);
        assert_eq!(flagged.len(), 2, "the invalid line is still delivered");
    }
}
