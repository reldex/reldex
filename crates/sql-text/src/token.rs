//! Tokens, and the small state carried across lines for incremental
//! (`QSyntaxHighlighter`-shaped) lexing.

use core::ops::Range;

/// The category of one lexical token.
///
/// `#[non_exhaustive]`: a future dialect capability can add a variant
/// without breaking a caller that already matches with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TokenKind {
    /// A run of whitespace: spaces, tabs, line breaks, and a byte-order
    /// mark wherever one appears.
    Whitespace,
    /// An unquoted word found in [`SqlDialect::keywords`](crate::SqlDialect::keywords).
    Keyword,
    /// An unquoted word that is not a recognized keyword.
    Identifier,
    /// A double-quoted identifier, e.g. `"My Table"`.
    QuotedIdentifier,
    /// A string literal: plain `'…'`, national `N'…'`, or — when the dialect
    /// enables it — an alternative-quoted form (`q'[…]'`, `nq'{…}'`, …).
    String,
    /// A numeric literal.
    Number,
    /// A `--` line comment, a `/* … */` block comment, or — when the
    /// dialect enables it — a `REM`/`REMARK` line comment.
    Comment,
    /// Punctuation or an operator: `;`, `(`, `)`, `,`, `.`, `+`, `:=`, a bare
    /// `/`, and so on.
    Operator,
    /// A bind-variable placeholder: `:name`, `:1`, or `:"Quoted Name"`.
    BindVariable,
    /// A SQL\*Plus substitution variable: `&name`, `&&name`, or either form
    /// with a trailing `.` terminator.
    SubstitutionVariable,
    /// A conditional-compilation directive or inquiry identifier introduced
    /// by [`SqlDialect::directive_prefix`](crate::SqlDialect::directive_prefix)
    /// (Oracle: `$IF`, `$THEN`, `$ELSIF`, `$ELSE`, `$END`, `$$PLSQL_UNIT`, …),
    /// lexed as one token so the word after the prefix (which may collide
    /// with a real keyword, e.g. `$END`'s `END`) is never separately visible
    /// to [`crate::splitter`]'s block-depth tracking.
    Directive,
    /// A string, quoted identifier, or comment that was still open when the
    /// text truly ended (see [`crate::tokenize`]). Never produced by
    /// [`crate::tokenize_block`] on its own: a block that ends mid-construct
    /// simply carries an open [`LexState`] to the next block, which is the
    /// normal, expected case for an editor line, not an error.
    Error,
}

/// One lexical token: a [`TokenKind`] and the byte range it covers in the
/// text that was scanned.
///
/// `Token` is a plain `Copy` value — [`crate::tokenize`] and
/// [`crate::tokenize_block`] allocate only the `Vec` they return, never per
/// token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Token {
    /// What this token is.
    pub kind: TokenKind,
    /// The byte offset of the token's first byte.
    pub start: usize,
    /// The byte offset one past the token's last byte.
    pub end: usize,
}

impl Token {
    /// This token's byte range, as a [`Range`].
    #[must_use]
    pub const fn range(&self) -> Range<usize> {
        self.start..self.end
    }

    /// The number of bytes this token covers.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.end - self.start
    }

    /// Whether this token covers zero bytes.
    ///
    /// Only ever true for the boundary produced by two adjacent empty
    /// statements; [`crate::tokenize`]/[`crate::tokenize_block`] never
    /// produce an empty token.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// What was still open at the end of a [`crate::tokenize_block`] call,
/// carried forward as the next call's input state.
///
/// Encodes to and from a plain `i32` ([`LexState::to_raw`],
/// [`LexState::from_raw`]) because that is what
/// `QSyntaxHighlighter::setCurrentBlockState`/`previousBlockState` store —
/// exactly one `int` per text block, no room for anything richer. Qt's own
/// convention reserves `-1` for "no previous state" (the document's first
/// block, or a block whose state was never computed); [`LexState::from_raw`]
/// treats every negative value, and any positive value this crate did not
/// itself produce, as [`LexState::INITIAL`] — so the first call, a corrupted
/// state, and a state from an unrelated document all restart cleanly rather
/// than panicking or misreading the text that follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LexState(pub(crate) Mode);

impl LexState {
    /// The state at the very start of a document: not inside anything.
    pub const INITIAL: Self = Self(Mode::Normal);

    /// Encodes this state as the `int`
    /// `QSyntaxHighlighter::setCurrentBlockState` expects.
    #[must_use]
    pub fn to_raw(self) -> i32 {
        encode(self.0)
    }

    /// Decodes a state previously produced by [`LexState::to_raw`].
    ///
    /// Never panics: a negative value (Qt's own "no previous state" sentinel
    /// is `-1`) or a value this crate never produced decodes to
    /// [`LexState::INITIAL`].
    #[must_use]
    pub fn from_raw(raw: i32) -> Self {
        Self(decode(raw))
    }

    /// Whether this is the initial "not inside anything" state.
    #[must_use]
    pub const fn is_initial(self) -> bool {
        matches!(self.0, Mode::Normal)
    }
}

/// The internal scanning state a [`LexState`] wraps.
///
/// Not `SingleQuoted`/`DoubleQuoted`/`QString` distinguished by "national"
/// (`N'…'` vs `'…'`, `Nq'…'` vs `q'…'`): resuming an open construct on the
/// next block never needs to know that, because the token produced is the
/// same [`TokenKind`] either way ([`TokenKind::String`]) and the closing
/// rule is identical. The distinction only matters at the moment a literal
/// *starts*, which [`crate::lexer`] decides before any state needs carrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Mode {
    #[default]
    Normal,
    BlockComment,
    SingleQuoted,
    DoubleQuoted,
    /// Inside an alternative-quoted (`q'X…`) literal; `char` is the
    /// **closing** delimiter (already resolved from the opening one, e.g.
    /// `[` stores `]`) so resuming never has to re-derive it.
    QString(char),
}

const TAG_MASK: i32 = 0x7;
const TAG_BLOCK_COMMENT: i32 = 1;
const TAG_SINGLE_QUOTED: i32 = 2;
const TAG_DOUBLE_QUOTED: i32 = 3;
const TAG_QSTRING: i32 = 4;
const DELIMITER_SHIFT: u32 = 8;

fn encode(mode: Mode) -> i32 {
    match mode {
        Mode::Normal => 0,
        Mode::BlockComment => TAG_BLOCK_COMMENT,
        Mode::SingleQuoted => TAG_SINGLE_QUOTED,
        Mode::DoubleQuoted => TAG_DOUBLE_QUOTED,
        Mode::QString(closing) => TAG_QSTRING | ((closing as i32) << DELIMITER_SHIFT),
    }
}

fn decode(raw: i32) -> Mode {
    if raw < 0 {
        return Mode::Normal;
    }
    match raw & TAG_MASK {
        TAG_BLOCK_COMMENT => Mode::BlockComment,
        TAG_SINGLE_QUOTED => Mode::SingleQuoted,
        TAG_DOUBLE_QUOTED => Mode::DoubleQuoted,
        TAG_QSTRING => {
            let delimiter = (raw >> DELIMITER_SHIFT) as u32;
            char::from_u32(delimiter).map_or(Mode::Normal, Mode::QString)
        }
        _ => Mode::Normal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state_round_trips_through_zero() {
        assert_eq!(LexState::INITIAL.to_raw(), 0);
        assert!(LexState::from_raw(0).is_initial());
        assert!(LexState::INITIAL.is_initial());
    }

    #[test]
    fn every_mode_round_trips_through_raw() {
        for mode in [
            Mode::Normal,
            Mode::BlockComment,
            Mode::SingleQuoted,
            Mode::DoubleQuoted,
            Mode::QString(']'),
            Mode::QString('\''), // an unpaired delimiter closes on itself
            Mode::QString('ก'),  // non-ASCII delimiters are legal Oracle syntax
        ] {
            let state = LexState(mode);
            let raw = state.to_raw();
            assert!(raw >= 0, "encoded state must be non-negative: {raw}");
            assert_eq!(LexState::from_raw(raw), state, "mode {mode:?} raw={raw}");
        }
    }

    #[test]
    fn negative_one_sentinel_and_garbage_decode_to_initial() {
        assert!(LexState::from_raw(-1).is_initial(), "Qt's own sentinel");
        assert!(LexState::from_raw(i32::MIN).is_initial());
        assert!(LexState::from_raw(-42).is_initial());
    }
}
