//! Splitting a script into statements ([`split_statements`]), and finding
//! the one at a cursor position ([`statement_at`]).
//!
//! `SPEC.md` §15: "Do not split scripts by every semicolon. The parser must
//! understand SQL and PL/SQL block boundaries and `/`." This module is built
//! on [`crate::tokenize`] rather than a second, separate scan of the text —
//! so "is this `;` inside a string/comment" is answered by the same code the
//! highlighter uses, not re-derived and possibly re-broken here.
//!
//! # The block-termination rule
//!
//! A statement is a **block** when its leading keywords match one of
//! [`SqlDialect::block_starters`] (Oracle: `DECLARE`, `BEGIN`, `CREATE
//! [OR REPLACE] [EDITIONABLE|NONEDITIONABLE] {PROCEDURE|FUNCTION|TRIGGER}`,
//! or `CREATE [OR REPLACE] [EDITIONABLE|NONEDITIONABLE]
//! {PACKAGE[ BODY]|TYPE[ BODY]}`). Its outermost `END` is found by
//! depth-counting [`SqlDialect::block_nesting_openers`] (`CASE`/`IF`/`LOOP`)
//! and [`SqlDialect::block_body_opener`] (`BEGIN`) against
//! [`SqlDialect::block_end_keyword`] (`END`), closing the statement the
//! moment an `END` is found with nothing else open (depth `0`).
//!
//! The one subtlety is *which* `BEGIN` counts as "nothing else open yet"
//! rather than a fresh level of nesting — see
//! [`BlockStarter::absorbs_body_opener`](crate::dialect::BlockStarter::absorbs_body_opener):
//! a `DECLARE`/`BEGIN` block, a lone `PROCEDURE`/`FUNCTION`/`TRIGGER` has
//! *one* `BEGIN` that is the statement's own body (so it does not count, the
//! first time); a `PACKAGE`/`TYPE` (spec or body) is a sequence of
//! independent members, each of which may be a *complete*, self-contained
//! `BEGIN ... END` block of its own — none of those is the package's own
//! body, so every one of them counts as nesting, and the package's own close
//! is only reached once every member's internal block has balanced back to
//! depth `0` on its own. One rule, two settings of one flag, rather than
//! three separate algorithms.
//!
//! Once that `;` is found, `SPEC.md` §15's own words — "block boundaries
//! **and** `/`" — are followed literally: if [`SqlDialect::slash_terminates_block`]
//! is set (Oracle: yes), the splitter looks past only whitespace and
//! comments for a line containing nothing but `/`, and the block's true end
//! (and `terminated: true`) is there, exactly matching SQL\*Plus, which does
//! not send a block to the server until `/` is typed. If no such line
//! appears before other content or end of input,
//! [`SqlDialect::block_may_end_without_slash`] decides whether the block is
//! still considered terminated at the `END`'s own `;` (`true` — Oracle's
//! descriptor sets this, since the common desktop-editor case of pasting a
//! script and running it without `/` between blocks should not be reported
//! as broken) or left `terminated: false` (`false` — the strict SQL\*Plus
//! reading).
//!
//! # Known limitations
//!
//! - **`CREATE TRIGGER … CALL proc(:NEW.x);` — a trigger body that is a bare
//!   `CALL`, with no `BEGIN`/`END` at all.** This is real, legal Oracle DDL
//!   (`oracle-thin::rewrite` handles exactly this shape for its own,
//!   different reason — stripping the SQL\*Plus terminator before deciding
//!   whether to rewrite it). It is detected as a block starter (it begins
//!   `CREATE TRIGGER`) but this splitter's depth scan looks for an `END`
//!   that the statement will never contain, so it does not stop where a
//!   human reading the script would. See the `#[ignore]`d
//!   `create_trigger_with_a_call_body_has_no_end_and_is_a_known_limitation`
//!   test in `tests/corpus.rs`.
//! - **`CREATE TYPE t AS OBJECT (...);` — an object type *spec* with no
//!   `BEGIN`/`END` at all**, for the same reason as the `CALL` trigger above:
//!   the whole declaration is one parenthesized attribute/method list ending
//!   in a plain `;`, never an `END`. ADR-0002 D8 lists object types as out of
//!   scope for the driver contract; this splitter inherits that scope limit
//!   rather than resolving it. `CREATE TYPE BODY`, which does end in `END;`,
//!   is unaffected. See the `#[ignore]`d
//!   `create_type_as_object_spec_has_no_end_and_is_a_known_limitation` test.
//! - **`WITH FUNCTION … SELECT …` (Oracle 12c inline PL/SQL in a query).**
//!   The `WITH` clause can declare a PL/SQL function whose body contains
//!   `;`, ahead of the query that uses it — a block-containing statement
//!   that does not *start* with a block-starter keyword. Recognizing it
//!   would mean scanning past an arbitrary amount of `WITH`-clause syntax
//!   before knowing whether a function is being declared, which is beyond
//!   what this task scoped. See the `#[ignore]`d
//!   `with_function_inline_plsql_is_a_known_limitation` test.
//! - **SQL\*Plus client commands** (`SET …`, `PROMPT`, `EXEC[UTE] …`) are not
//!   recognized as a distinct [`StatementKind`] variant. `SPEC.md` §15:
//!   "Future SQL\*Plus-like commands may be added progressively" — Phase 1
//!   does not require them, so none of Oracle's Phase-1 descriptor enables
//!   the machinery `sql-text` would need (see [`crate::dialect::CommentRules::sqlplus_rem`]
//!   for a case where the machinery exists but is deliberately left off).
//!   `StatementKind` is `#[non_exhaustive]` so a variant can be added later
//!   without a breaking change.

use crate::dialect::{KeywordSlot, Phrase, SqlDialect};
use crate::lexer::tokenize;
use crate::token::{Token, TokenKind};

/// How many of a statement's leading word-tokens are inspected against
/// [`SqlDialect::block_starters`]. Generous headroom over the longest
/// pattern this crate ships (`CREATE OR REPLACE EDITIONABLE PACKAGE BODY` —
/// 6 words).
const MAX_LEADING_WORDS: usize = 12;

/// What kind of statement a [`StatementSpan`] describes.
///
/// `#[non_exhaustive]`: see the "Known limitations" section on the [module
/// documentation](self) for the `SqlPlusCommand` variant this deliberately
/// does not yet have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StatementKind {
    /// An ordinary statement, ending at the dialect's statement terminator
    /// (Oracle: `;`).
    Plain,
    /// A block statement (an anonymous `DECLARE`/`BEGIN` block, or DDL that
    /// creates a stored PL/SQL unit or trigger) — see the [module
    /// documentation](self) for how its end is found.
    Block,
}

/// One statement found by [`split_statements`].
///
/// All positions are byte offsets into the text that was split. `start_line`
/// and `start_column` are 1-based; `start_column` counts **Unicode scalar
/// values** (`char`s), not UTF-16 code units — see the crate-level
/// documentation's "What lives elsewhere" section for why, and how to
/// convert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementSpan {
    /// Byte offset where the statement's own text begins (after any
    /// leading whitespace/comments, which belong to the gap before it).
    pub content_start: usize,
    /// Byte offset one past the statement's own text, **excluding** its
    /// terminator. For a block statement this includes the closing `END`
    /// and its `;` — PL/SQL requires that semicolon, so it is not a
    /// "terminator" in the sense this field excludes.
    pub content_end: usize,
    /// Byte offset one past the statement's terminator: for
    /// [`StatementKind::Plain`], one past the terminator character; for
    /// [`StatementKind::Block`] with a `/` line, one past that line
    /// (including its newline, when present).
    pub full_end: usize,
    /// The statement's first line, counting from `1`.
    pub start_line: u32,
    /// The statement's first character's column on that line, counting
    /// from `1` in Unicode scalar values.
    pub start_column: u32,
    /// Whether this is a plain or block statement.
    pub kind: StatementKind,
    /// Whether an explicit terminator was found. `false` means the
    /// statement ran to end of input (a truncated script, or — for a block,
    /// when [`SqlDialect::block_may_end_without_slash`] is unset — a block
    /// whose `/` never arrived).
    pub terminated: bool,
}

impl StatementSpan {
    /// This statement's text, excluding its terminator.
    #[must_use]
    pub fn content<'a>(&self, text: &'a str) -> &'a str {
        &text[self.content_start..self.content_end]
    }

    /// This statement's text, including its terminator.
    #[must_use]
    pub fn full<'a>(&self, text: &'a str) -> &'a str {
        &text[self.content_start..self.full_end]
    }
}

/// Splits `text` into statements.
///
/// Concatenating every span's [`StatementSpan::full`] range with the gaps
/// between them (leading/trailing whitespace and comments, which belong to
/// no statement) reproduces `text` exactly — `tests/corpus.rs` checks this
/// over the whole corpus.
#[must_use]
pub fn split_statements(text: &str, dialect: &SqlDialect) -> Vec<StatementSpan> {
    let tokens = tokenize(text, dialect);
    let mut spans = Vec::new();
    let mut cursor = PositionCursor::new();
    let mut i = 0usize;

    while i < tokens.len() {
        i = skip_trivial(&tokens, i);
        let Some(first) = tokens.get(i) else { break };

        let content_start = first.start;
        cursor.advance_to(text, content_start);
        let start_line = cursor.line;
        let start_column = cursor.column;

        let leading_words = collect_leading_words(&tokens, text, i);
        let matched_starter = dialect
            .block_starters
            .iter()
            .find(|starter| matches_block_starter(starter, &leading_words));

        let (content_end, full_end, terminated, next_i, kind) = match matched_starter {
            Some(starter) => scan_block(&tokens, text, i, dialect, starter.absorbs_body_opener),
            None => scan_plain(&tokens, text, i, dialect),
        };

        cursor.advance_to(text, full_end);
        spans.push(StatementSpan {
            content_start,
            content_end,
            full_end,
            start_line,
            start_column,
            kind,
            terminated,
        });
        i = next_i;
    }

    spans
}

/// Finds the statement at `offset` (a byte offset into `text`), for "run
/// statement at cursor".
///
/// The rule, since a cursor can sit in the gap between statements: the
/// statement that contains `offset` (from its first character through the
/// end of its terminator, inclusive of both ends), else the statement that
/// most closely precedes `offset` **if** no line break separates them, else
/// `None`. A cursor on a blank or comment-only line clearly separated from
/// the previous statement resolves to nothing to run, matching what a user
/// would expect from an editor rather than silently re-running whatever
/// came before.
#[must_use]
pub fn statement_at(text: &str, offset: usize, dialect: &SqlDialect) -> Option<StatementSpan> {
    let offset = offset.min(text.len());
    let spans = split_statements(text, dialect);

    if let Some(hit) = spans
        .iter()
        .find(|span| span.content_start <= offset && offset <= span.full_end)
    {
        return Some(hit.clone());
    }

    let preceding = spans
        .iter()
        .filter(|span| span.full_end <= offset)
        .max_by_key(|span| span.full_end)?;

    if text[preceding.full_end..offset].contains('\n') {
        None
    } else {
        Some(preceding.clone())
    }
}

// --------------------------------------------------------------- internals

fn is_word(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::Keyword | TokenKind::Identifier)
}

fn is_trivial(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::Whitespace | TokenKind::Comment)
}

fn skip_trivial(tokens: &[Token], mut i: usize) -> usize {
    while tokens.get(i).is_some_and(|t| is_trivial(t.kind)) {
        i += 1;
    }
    i
}

/// Whether `token` is a single-character operator matching one of the
/// dialect's statement terminators.
fn is_terminator(text: &str, token: Token, dialect: &SqlDialect) -> bool {
    token.kind == TokenKind::Operator
        && text[token.start..token.end]
            .chars()
            .next()
            .is_some_and(|c| {
                token.len() == c.len_utf8() && dialect.statement_terminators.contains(&c)
            })
}

/// Collects up to [`MAX_LEADING_WORDS`] leading word-tokens' text, skipping
/// whitespace/comments, stopping at the first token that is neither — a
/// short statement (fewer words than any block-starter pattern needs) simply
/// yields fewer words, which no pattern will match.
fn collect_leading_words<'t>(tokens: &[Token], text: &'t str, mut i: usize) -> Vec<&'t str> {
    let mut words = Vec::with_capacity(MAX_LEADING_WORDS);
    while words.len() < MAX_LEADING_WORDS {
        let Some(token) = tokens.get(i) else { break };
        if is_word(token.kind) {
            words.push(&text[token.start..token.end]);
        } else if !is_trivial(token.kind) {
            break;
        }
        i += 1;
    }
    words
}

fn match_phrase(options: &[Phrase], words: &[&str]) -> Option<usize> {
    options.iter().find_map(|phrase| {
        (phrase.len() <= words.len()
            && phrase
                .iter()
                .zip(words.iter())
                .all(|(p, w)| p.eq_ignore_ascii_case(w)))
        .then_some(phrase.len())
    })
}

fn matches_block_starter(starter: &crate::dialect::BlockStarter, words: &[&str]) -> bool {
    let mut idx = 0;
    for slot in starter.slots {
        match slot {
            KeywordSlot::Required(options) => match match_phrase(options, &words[idx..]) {
                Some(consumed) => idx += consumed,
                None => return false,
            },
            KeywordSlot::Optional(options) => {
                if let Some(consumed) = match_phrase(options, &words[idx..]) {
                    idx += consumed;
                }
            }
        }
    }
    true
}

type ScanResult = (usize, usize, bool, usize, StatementKind);

/// Scans a non-block statement from its first token (`i`) to its terminator.
fn scan_plain(tokens: &[Token], text: &str, start: usize, dialect: &SqlDialect) -> ScanResult {
    let mut i = start;
    while let Some(&token) = tokens.get(i) {
        if is_terminator(text, token, dialect) {
            return (token.start, token.end, true, i + 1, StatementKind::Plain);
        }
        i += 1;
    }
    let end = tokens.last().map_or(text.len(), |t| t.end);
    (end, end, false, tokens.len(), StatementKind::Plain)
}

/// Scans a block statement from its first token (`i`): finds the outermost
/// `END` by depth, then (per [`SqlDialect::slash_terminates_block`]) a
/// following `/` line. See the [module documentation](self) for the rule and
/// [`BlockStarter::absorbs_body_opener`](crate::dialect::BlockStarter::absorbs_body_opener)
/// for why `absorbs_body_opener` changes how depth `0` is reached.
///
/// Nesting depth starts at `0` either way. [`SqlDialect::block_nesting_openers`]
/// (`CASE`/`IF`/`LOOP`) always increase it. [`SqlDialect::block_body_opener`]
/// (`BEGIN`) increases it too, **except** the first occurrence when
/// `absorbs_body_opener` is set — that one *is* the statement's own body
/// opener, not a nested one, so it does not count. [`SqlDialect::block_end_keyword`]
/// (`END`) closes the statement the moment it is seen at depth `0`
/// (nothing left open); otherwise it decreases depth by one and — since the
/// word immediately after an `END` is always that construct's own optional
/// qualifier (`LOOP`, `IF`, `CASE`, or a name), never a fresh statement — that
/// one word is skipped too, so it cannot be misread as a new opener.
fn scan_block(
    tokens: &[Token],
    text: &str,
    start: usize,
    dialect: &SqlDialect,
    absorbs_body_opener: bool,
) -> ScanResult {
    let mut i = start;
    let mut depth: i32 = 0;
    let mut absorbed_body_opener = false;

    while let Some(&token) = tokens.get(i) {
        if !is_word(token.kind) {
            i += 1;
            continue;
        }
        let word = &text[token.start..token.end];

        if word.eq_ignore_ascii_case(dialect.block_end_keyword) {
            if depth == 0 {
                return close_block(tokens, text, i, dialect);
            }
            depth -= 1;
            i = skip_trivial(tokens, i + 1);
            if tokens.get(i).is_some_and(|t| is_word(t.kind)) {
                i += 1;
            }
            continue;
        }

        if word.eq_ignore_ascii_case(dialect.block_body_opener) {
            if absorbs_body_opener && !absorbed_body_opener {
                absorbed_body_opener = true;
            } else {
                depth += 1;
            }
        } else if dialect
            .block_nesting_openers
            .iter()
            .any(|k| k.eq_ignore_ascii_case(word))
        {
            depth += 1;
        }
        i += 1;
    }

    // No outermost `END` at all before end of input: truncated script, or
    // (documented limitation, see the module docs) a body-less trigger or
    // object-type spec.
    let end = tokens.last().map_or(text.len(), |t| t.end);
    (end, end, false, tokens.len(), StatementKind::Block)
}

/// `tokens[end_index]` is the outermost `END`. Consumes its optional
/// trailing qualifier/name and required terminator, then applies the `/`
/// rule.
fn close_block(tokens: &[Token], text: &str, end_index: usize, dialect: &SqlDialect) -> ScanResult {
    let mut j = skip_trivial(tokens, end_index + 1);
    if tokens.get(j).is_some_and(|t| is_word(t.kind)) {
        j = skip_trivial(tokens, j + 1);
    }
    let Some(&terminator) = tokens.get(j) else {
        let end = tokens.last().map_or(text.len(), |t| t.end);
        return (end, end, false, tokens.len(), StatementKind::Block);
    };
    if !is_terminator(text, terminator, dialect) {
        let end = tokens.last().map_or(text.len(), |t| t.end);
        return (end, end, false, tokens.len(), StatementKind::Block);
    }

    let content_end = terminator.start;
    let semi_end = terminator.end;
    let next_i = j + 1;

    if !dialect.slash_terminates_block {
        return (content_end, semi_end, true, next_i, StatementKind::Block);
    }

    let mut k = next_i;
    while let Some(&token) = tokens.get(k) {
        if is_trivial(token.kind) {
            k += 1;
            continue;
        }
        if token.kind == TokenKind::Operator && is_lone_slash_line(text, token) {
            let full_end = trailing_line_end(text, token.end);
            return (content_end, full_end, true, k + 1, StatementKind::Block);
        }
        break;
    }

    (
        content_end,
        semi_end,
        dialect.block_may_end_without_slash,
        next_i,
        StatementKind::Block,
    )
}

/// Whether `token` is a `/` that is the only non-whitespace content on its
/// line.
fn is_lone_slash_line(text: &str, token: Token) -> bool {
    if &text[token.start..token.end] != "/" {
        return false;
    }
    let line_start = text[..token.start].rfind('\n').map_or(0, |p| p + 1);
    if !text[line_start..token.start].trim().is_empty() {
        return false;
    }
    let line_end = text[token.end..]
        .find('\n')
        .map_or(text.len(), |p| token.end + p);
    text[token.end..line_end].trim().is_empty()
}

/// One past the end of the line containing byte offset `from` — including
/// the newline itself, when there is one, so a `/` terminator's span
/// consumes its own line break.
fn trailing_line_end(text: &str, from: usize) -> usize {
    match text[from..].find('\n') {
        Some(p) => from + p + 1,
        None => text.len(),
    }
}

/// Tracks (line, column) while moving forward through `text`, so
/// [`split_statements`] computes every span's position in one linear pass
/// rather than rescanning from the start for each one.
struct PositionCursor {
    pos: usize,
    line: u32,
    column: u32,
}

impl PositionCursor {
    const fn new() -> Self {
        Self {
            pos: 0,
            line: 1,
            column: 1,
        }
    }

    fn advance_to(&mut self, text: &str, target: usize) {
        debug_assert!(target >= self.pos, "position cursor must move forward");
        for ch in text[self.pos..target].chars() {
            if ch == '\n' {
                self.line += 1;
                self.column = 1;
            } else {
                self.column += 1;
            }
        }
        self.pos = target;
    }
}
