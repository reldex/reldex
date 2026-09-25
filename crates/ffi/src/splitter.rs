//! Statement splitting (M2.11 family 3): `reldex-sql-text`'s
//! `split_statements`, across the boundary.
//!
//! Vendor-neutral by construction — `reldex_sql_text::split_statements` takes
//! a [`reldex_sql_text::SqlDialect`] as data — but this crate is the
//! composition root (`ARCHITECTURE.md` §2), so it supplies Oracle's dialect
//! the same way it supplies Oracle's `DriverBinding` and `MetadataCatalog`
//! (`crates/ffi/src/workspace.rs`, `crates/ffi/src/metadata.rs`). A second
//! driver's dialect would be a second `reldex_split_statements_for` entry
//! point, additive, not a change to this one.
//!
//! # Zero-copy, deliberately
//!
//! A [`ReldexStatementSpan`] carries byte offsets into the caller's own text,
//! nothing more — no allocation, no arena, no ownership to release. The
//! caller already has the text (it is what it passed in); this only tells it
//! where the statements are.
//!
//! # `is_trivial_or_directive`, and why it is not here
//!
//! `docs/exec-plans/active/phase-1.md`'s M2.11 row and this task's brief both
//! name a "`is_trivial_or_directive` flag". `reldex_sql_text::splitter`'s
//! private `is_trivial_or_directive(TokenKind) -> bool` is a **token**
//! predicate used internally to skip whitespace, comments and SQL\*Plus-style
//! directives between statements — it is not `pub`, and there is nothing
//! per-*statement* it could flag: [`reldex_sql_text::split_statements`]
//! already never returns a span for text that is only trivia, so every
//! [`ReldexStatementSpan`] this function hands out already excludes it by
//! construction. Re-exposing the predicate would need a second, token-level
//! API this milestone's UI has no consumer for (the editor's syntax
//! highlighting, which does want per-token classification, is
//! `reldex_sql_text::tokenize`/`tokenize_block`'s job, and out of scope
//! here). Recorded rather than silently dropped.

use reldex_sql_text::{EndedBy, StatementKind, StatementSpan, split_statements};

use crate::strings::{CStruct, ReldexStr};

/// Whether a statement is plain or a block (`SPEC.md` §15).
///
/// `0` is reserved for a kind this header predates. Named `ReldexSplitKind`,
/// not `ReldexStatementKind`, because that name is already
/// [`crate::ReldexStatementKind`] — the driver's classification of an
/// *executed* statement (`Query`/`Dml`/`Ddl`/…), a different axis entirely: a
/// block can be DDL or PL/SQL, and a plain statement can be DML or a query.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexSplitKind {
    /// A kind this header does not know.
    Unknown = 0,
    /// An ordinary statement, ending at the dialect's terminator or a lone
    /// `/` line.
    Plain = 1,
    /// A block statement (an anonymous block, or DDL that creates a stored
    /// PL/SQL unit, trigger, type, or opaque source).
    Block = 2,
}

impl From<StatementKind> for ReldexSplitKind {
    fn from(kind: StatementKind) -> Self {
        match kind {
            StatementKind::Plain => Self::Plain,
            StatementKind::Block => Self::Block,
            // `StatementKind` is `#[non_exhaustive]`.
            _ => Self::Unknown,
        }
    }
}

/// Why a [`ReldexStatementSpan`] ended where it did.
///
/// `0` is reserved for a value this header predates.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexEndedBy {
    /// A value this header does not know.
    Unknown = 0,
    /// The dialect's own statement terminator.
    Terminator = 1,
    /// An authoritative lone `/` line (safety principle S1).
    SlashLine = 2,
    /// A block's own structural close, with no `/` line following. See
    /// [`ReldexStatementSpan::terminated`].
    InferredBlockEnd = 3,
    /// End of input was reached with nothing above having closed the
    /// statement — a truncated script.
    EndOfInput = 4,
}

impl From<EndedBy> for ReldexEndedBy {
    fn from(ended_by: EndedBy) -> Self {
        match ended_by {
            EndedBy::Terminator => Self::Terminator,
            EndedBy::SlashLine => Self::SlashLine,
            EndedBy::InferredBlockEnd => Self::InferredBlockEnd,
            EndedBy::EndOfInput => Self::EndOfInput,
            // `EndedBy` is `#[non_exhaustive]`.
            _ => Self::Unknown,
        }
    }
}

/// One statement [`reldex_split_statements`] found, as byte offsets into the
/// text the caller passed it — nothing here is owned or allocated.
///
/// All positions are byte offsets, matching [`reldex_sql_text::StatementSpan`]
/// exactly; `start_column` counts **Unicode scalar values**, not UTF-16 code
/// units (use [`crate::reldex_utf16_offset`] to convert for a `QTextCursor`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReldexStatementSpan {
    /// `sizeof(ReldexStatementSpan)` on the way in; how much is valid on the
    /// way out.
    pub struct_size: u32,
    /// A [`ReldexSplitKind`].
    pub kind: i32,
    /// A [`ReldexEndedBy`].
    pub ended_by: i32,
    /// Byte offset where the statement's own text begins.
    pub content_start: usize,
    /// Byte offset one past the statement's own text, excluding its
    /// terminator.
    pub content_end: usize,
    /// Byte offset one past the statement's terminator (or, for
    /// [`ReldexEndedBy::SlashLine`], past the `/` line's own trailing
    /// newline).
    pub full_end: usize,
    /// The statement's first line, counting from 1.
    pub start_line: u32,
    /// The statement's first character's column on that line, counting from
    /// 1 in Unicode scalar values.
    pub start_column: u32,
    /// Whether an explicit terminator was found; see
    /// [`reldex_sql_text::StatementSpan::terminated`].
    pub terminated: bool,
}

// SAFETY: `#[repr(C)]`, `struct_size` first, every other field an integer or
// a `bool` — all valid as zero.
unsafe impl CStruct for ReldexStatementSpan {
    const MIN_SIZE: usize = size_of::<Self>();
}

impl Default for ReldexStatementSpan {
    fn default() -> Self {
        Self {
            struct_size: u32::try_from(size_of::<Self>()).unwrap_or(u32::MAX),
            kind: ReldexSplitKind::Unknown as i32,
            ended_by: ReldexEndedBy::Unknown as i32,
            content_start: 0,
            content_end: 0,
            full_end: 0,
            start_line: 0,
            start_column: 0,
            terminated: false,
        }
    }
}

impl From<&StatementSpan> for ReldexStatementSpan {
    fn from(span: &StatementSpan) -> Self {
        Self {
            kind: ReldexSplitKind::from(span.kind) as i32,
            ended_by: ReldexEndedBy::from(span.ended_by) as i32,
            content_start: span.content_start,
            content_end: span.content_end,
            full_end: span.full_end,
            start_line: span.start_line,
            start_column: span.start_column,
            terminated: span.terminated,
            ..Self::default()
        }
    }
}

/// Splits `text` into statements using the Oracle dialect (the only one this
/// build knows — `reldex_driver_oracle_thin::sql_dialect()`), writing up to
/// `capacity` spans into `out` and returning the **total** number of
/// statements found, exactly like `snprintf`: compare the return value
/// against `capacity` to know whether `out` holds all of them, and call again
/// with a bigger buffer (or `out = NULL`, `capacity = 0`) to size it first.
///
/// Every byte of `text` belongs to exactly one span's `[content_start,
/// full_end)` range or to the gap before/after/between spans (leading and
/// trailing whitespace and comments, which belong to no statement); this
/// function does not report the gaps, only the statements.
///
/// Allocates nothing beyond the `Vec` this crate builds internally and frees
/// before returning; `out` is the caller's own memory throughout.
///
/// # Safety
///
/// `text` must point at `text.len` readable UTF-8 bytes. `out` must be null
/// (to ask only for the count) or point at `capacity` writable
/// [`ReldexStatementSpan`]s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_split_statements(
    text: ReldexStr,
    out: *mut ReldexStatementSpan,
    capacity: usize,
) -> usize {
    crate::status::entry_value(0, || {
        // SAFETY: delegated to this function's contract for `text`.
        let Some(text) = (unsafe { text.as_str() }) else {
            crate::error::set_last_argument_error(
                "reldex_split_statements: `text` is null or is not valid UTF-8",
            );
            return 0;
        };
        let dialect = reldex_driver_oracle_thin::sql_dialect();
        let spans = split_statements(text, &dialect);
        if !out.is_null() && out.is_aligned() {
            for (index, span) in spans.iter().take(capacity).enumerate() {
                // SAFETY: `index < capacity` and the caller promises `out` has
                // room for `capacity` elements.
                unsafe { out.add(index).write(ReldexStatementSpan::from(span)) };
            }
        }
        spans.len()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_two_statement_text_is_split_with_byte_ranges() {
        let text = "SELECT 1 FROM dual;\nSELECT 2 FROM dual;\n";
        let sql = ReldexStr {
            ptr: text.as_ptr(),
            len: text.len(),
        };
        let mut spans = [ReldexStatementSpan::default(); 4];
        // SAFETY: `sql` borrows `text`, which outlives the call; `spans` is a
        // real, writable array.
        let total = unsafe { reldex_split_statements(sql, spans.as_mut_ptr(), spans.len()) };
        assert_eq!(total, 2);
        assert_eq!(spans[0].kind, ReldexSplitKind::Plain as i32);
        assert_eq!(
            &text[spans[0].content_start..spans[0].content_end],
            "SELECT 1 FROM dual"
        );
        assert_eq!(
            &text[spans[1].content_start..spans[1].content_end],
            "SELECT 2 FROM dual"
        );
        assert!(spans[0].terminated);
        assert_eq!(spans[1].start_line, 2);

        // Asking for fewer than exist reports the true total, snprintf-style.
        let mut one = [ReldexStatementSpan::default(); 1];
        // SAFETY: as above.
        let total_again = unsafe { reldex_split_statements(sql, one.as_mut_ptr(), one.len()) };
        assert_eq!(total_again, 2, "the true count, even though only one fit");

        // A null `out` just sizes it.
        // SAFETY: `sql` is valid; a null `out` is explicitly allowed.
        let sized = unsafe { reldex_split_statements(sql, std::ptr::null_mut(), 0) };
        assert_eq!(sized, 2);
    }

    #[test]
    fn invalid_utf8_is_refused_rather_than_read() {
        let bytes: &[u8] = b"\xff\xfe";
        let sql = ReldexStr {
            ptr: bytes.as_ptr(),
            len: bytes.len(),
        };
        // SAFETY: `bytes` covers `len` readable bytes; they are not UTF-8,
        // which is the case under test.
        let total = unsafe { reldex_split_statements(sql, std::ptr::null_mut(), 0) };
        assert_eq!(total, 0);
    }
}
