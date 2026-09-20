//! Splitting a script into statements ([`split_statements`]), and finding
//! the one at a cursor position ([`statement_at`]).
//!
//! `SPEC.md` §15: "Do not split scripts by every semicolon. The parser must
//! understand SQL and PL/SQL block boundaries and `/`." This module is built
//! on [`crate::tokenize`] rather than a second, separate scan of the text —
//! so "is this `;` inside a string/comment" is answered by the same code the
//! highlighter uses, not re-derived and possibly re-broken here.
//!
//! # Safety first: spans are executed against real databases
//!
//! Every rule below is secondary to three principles, because a
//! [`StatementSpan`] is not just a highlighting range — it is what gets sent
//! to a database:
//!
//! - **S1 — a lone terminator line is authoritative.** If
//!   [`SqlDialect::slash_terminates_block`]/[`SqlDialect::slash_terminates_plain`]
//!   is set, a line containing only `/` ends whatever statement is currently
//!   open **the moment it is seen**, regardless of nesting depth or any
//!   member body still owed. This is checked on every token of every scan
//!   in this module, not only after a structurally-recognized close. It
//!   bounds the damage of any block-structure miscount — known or
//!   undiscovered — to at most one over-large statement, never a
//!   fragment carved out of the middle of one: SQL\*Plus itself never sends
//!   a block to the server until `/` is typed, so a script that relies on
//!   correct execution already contains this same safety net.
//! - **S2 — inside a recognized block, a statement terminator never ends it
//!   on its own.** Only the structure tracker (this module) or S1 can close
//!   a [`StatementKind::Block`] span; an ordinary `;` inside one, however
//!   deeply nested, is content.
//! - **S3 — when structure is ambiguous, prefer one larger span.** Every
//!   documented limitation in this module fails by over-including text into
//!   a span (which a database rejects as a syntax error) rather than by
//!   under-including — producing a smaller, independently executable
//!   statement out of what a human would read as the middle of a block. A
//!   span that is too small and still parses on its own is the dangerous
//!   failure mode this module is designed never to produce.
//!
//! # The block-termination rule
//!
//! A statement is a **block** ([`BlockKind::Structured`]) when, after
//! skipping any [`SqlDialect::label_delimiters`], its leading keywords match
//! one of [`SqlDialect::block_starters`] (Oracle: `DECLARE`, `BEGIN`, `CREATE
//! [OR REPLACE] [EDITIONABLE|NONEDITIONABLE] {PROCEDURE|FUNCTION|TRIGGER}`,
//! or `{PACKAGE[ BODY]|TYPE BODY}`). Its outermost `END` is found by
//! depth-counting [`SqlDialect::block_nesting_openers`] (`CASE`/`IF`/`LOOP`)
//! and [`SqlDialect::block_body_opener`] (`BEGIN`) against
//! [`SqlDialect::block_end_keyword`] (`END`), closing the statement the
//! moment an `END` is found with nothing else open (depth `0`).
//!
//! The one subtlety is *which* `BEGIN` counts as "nothing else open yet"
//! rather than a fresh level of nesting, and this module tracks it with one
//! extra counter, `pending_bodies`, rather than a fixed "single body vs.
//! sequence of members" flag per starter:
//!
//! - A word matching [`SqlDialect::subprogram_header_keywords`]
//!   (`PROCEDURE`/`FUNCTION`) — or, once
//!   [`SqlDialect::sectioned_body_marker`] has been seen,
//!   [`SqlDialect::section_header_starters`] (`BEFORE`/`AFTER`/
//!   `INSTEAD`) — starts a header scan: skip forward, tracking `(`/`)` depth
//!   so a parameter default's own `IS`/`AS`/terminator (`CAST(x AS t)`,
//!   `CASE WHEN x IS NULL THEN … END`) is never mistaken for the header's
//!   own, to the first depth-`0` [`SqlDialect::body_intro_keywords`] word
//!   (`IS`/`AS`) or statement terminator, whichever comes first — except a
//!   word that only completes a [`SqlDialect::body_intro_exceptions`] phrase
//!   (Oracle: `SELF AS`, inside a constructor's `RETURN SELF AS RESULT IS`),
//!   which is skipped over as ordinary content since it does not introduce a
//!   body itself. A terminator first means a *forward declaration* (a
//!   package/type spec's `PROCEDURE go;`, owing nothing). `IS`/`AS`
//!   immediately followed by a [`SqlDialect::call_spec_phrases`] phrase
//!   (`LANGUAGE JAVA …`/`EXTERNAL LIBRARY …`) means a call-spec (also owing
//!   nothing, ending at its own next terminator) — matched as a full phrase,
//!   never a single word, so that a variable, constant, or cursor legally
//!   named `language` or `external` is never mistaken for one (see
//!   [`SqlDialect::call_spec_phrases`]'s own documentation for why this
//!   fail-safe direction was chosen). Otherwise one body is now *owed*
//!   (`pending_bodies += 1`).
//! - A `BEGIN`: if a body is owed, this is it — fulfill one
//!   (`pending_bodies -= 1`) and open a nesting level for its matching
//!   `END`. Otherwise, if nothing has been absorbed yet at the *current*
//!   nesting level (depth `0` relative to this statement, or a member's own
//!   depth `0` once its header's `BEGIN` has been fulfilled), this `BEGIN`
//!   *is* that frame's own body opener and does not nest — this is a
//!   package/type body's optional initialization section, or a lone
//!   `PROCEDURE`/`DECLARE`/`BEGIN` statement's one required body, handled by
//!   the same rule. Any other `BEGIN` nests.
//!
//!   Whether the *matched starter's own keywords* already consumed the
//!   frame's body opener is [`BlockStarter::opens_body`] — data, not an
//!   assumption this scan makes: true only for a bare `BEGIN` starter (whose
//!   match already consumed the one `BEGIN` this frame will ever be owed
//!   unconditionally, so the scan starts as though that absorption already
//!   happened), false for every other starter (`DECLARE`, `CREATE
//!   PROCEDURE`/`FUNCTION`/`TRIGGER`, `PACKAGE`/`TYPE BODY`), whose own
//!   `BEGIN` still lies ahead and is absorbed by this same rule the normal
//!   way. An adversarial review (ADR-0002 amendment J, round 2) found that
//!   an earlier revision always started this scan as though nothing had been
//!   absorbed, so a `BEGIN` starter's first *nested* `BEGIN…END` was
//!   misappropriated as the frame's own body — splitting `BEGIN BEGIN NULL;
//!   END; END;` into two spans (an orphaned `END`) and, with sibling inner
//!   blocks, carving a complete runnable statement out of the middle of one
//!   (`BEGIN BEGIN NULL; END; BEGIN NULL; END; END;` → 3 spans instead of 1).
//! - [`SqlDialect::body_less_markers`] (`CALL`), seen before any body has
//!   been owed or absorbed at the outermost level, means the whole statement
//!   is body-less DDL ending at its own next terminator (a trigger whose
//!   body is a bare `CALL`, never `BEGIN`/`END`).
//!
//! One counter and one rule replace what would otherwise be a different
//! algorithm for "one body", "a sequence of members", and "a sequence of
//! members that may themselves declare nested subprograms in their own
//! declare sections" — all three are the same shape once headers are allowed
//! to owe a body independently of the statement's own.
//!
//! All of the word-dispatch above — `body_less_markers`, the outer
//! statement's own call-spec check, `subprogram_header_keywords`/
//! `section_header_starters`, `block_end_keyword`, `block_body_opener`, and
//! `block_nesting_openers` — is additionally gated on this scan's own `(`/`)`
//! depth being `0`: a parameter default's `CASE … END` or `CAST(x AS t)`
//! sitting in the *outer* statement's own signature (not only a nested
//! member's, which the internal `scan_header` helper handles separately)
//! must not be read as structural. This closes the other half of the same
//! round-2 finding as `scan_header`'s own paren tracking.
//!
//! Once the outermost `END` (or a body-less marker's own terminator) is
//! found, `SPEC.md` §15's own words — "block boundaries **and** `/`" — are
//! followed literally: if [`SqlDialect::slash_terminates_block`] is set
//! (Oracle: yes), the splitter looks past only whitespace and comments for a
//! line containing nothing but `/`, and the block's true end
//! ([`EndedBy::SlashLine`]) is there, exactly matching SQL\*Plus, which does
//! not send a block to the server until `/` is typed. If no such line
//! appears before other content or end of input,
//! [`SqlDialect::block_may_end_without_slash`] decides whether the block is
//! still considered terminated at the `END`'s own `;`
//! ([`EndedBy::InferredBlockEnd`], `terminated: true` — Oracle's descriptor
//! sets this, since the common desktop-editor case of pasting a script and
//! running it without `/` between blocks should not be reported as broken)
//! or left `terminated: false` (Oracle's strict variant — the SQL\*Plus
//! reading).
//!
//! A [`BlockKind::ParenDelimited`] starter (Oracle: `CREATE [OR REPLACE]
//! TYPE`, without `BODY`) has no `BEGIN`/`END` at all: it scans forward
//! tracking `(`/`)` depth and ends at the first statement terminator found
//! at depth `0` — which handles both the parenthesized object/varray/table
//! forms and the simple `CREATE TYPE t IS BOOLEAN;` synonym form (no parens
//! at all) with the same rule, since depth simply never leaves `0` in the
//! second case.
//!
//! A [`BlockKind::OpaqueSource`] starter (Oracle: `CREATE ... JAVA SOURCE
//! ...`) is not PL/SQL at all: its body is source text in another language
//! that may itself contain `;`, so nothing but S1 (a lone `/` line) or end of
//! input ever closes it.
//!
//! # A lone `/` line with nothing pending produces no span at all
//!
//! [`split_statements`] checks, before it decides whether the next
//! statement is plain or a block — before it even records a
//! [`StatementSpan::content_start`] — whether the very next non-trivial
//! token is itself an authoritative lone `/` line. If it is, nothing was
//! pending: the previous statement (if any) already closed cleanly, only
//! whitespace/comments separate it from this `/`, and there is no text for
//! this `/` to submit. Such a line is skipped and produces **no span**: not
//! an empty statement, and not a re-submission of whatever preceded it.
//!
//! This is a deliberate product decision (lead decision, ADR-0002 amendment
//! J round 2), not the only defensible reading of SQL\*Plus's own semantics
//! — SQL\*Plus re-runs its statement buffer on a bare `/`, which for a
//! desktop editor's purposes is ambiguous (re-run the *previous* statement?
//! treat it as inert?) and unverified against real SQL\*Plus/SQLcl behavior.
//! Producing no span at all is the one reading that cannot surprise a caller
//! by re-executing something the user did not select, and it is what keeps
//! [`StatementSpan::content_start`] `<=` [`StatementSpan::content_end`] in
//! every case: before this rule existed, a lone `/` immediately following an
//! already-terminated statement could compute a `content_end` for a *new*,
//! near-empty span that landed **before** that span's own `content_start`
//! (the new span's only "content" was trivia the previous statement had
//! already consumed), which made [`StatementSpan::content`] panic — a
//! confirmed adversarial-review defect (ADR-0002 amendment J, round 2).
//! The internal `end_at_slash_line` helper additionally clamps its own
//! `content_end` to never go below the `content_start` its caller is
//! building, as defense in depth for any scan this top-level check does not
//! cover.
//!
//! # Known limitations
//!
//! - **`WITH FUNCTION … SELECT …` (Oracle 12c inline PL/SQL in a query).**
//!   The `WITH` clause can declare a PL/SQL function whose body contains
//!   `;`, ahead of the query that uses it — a block-containing statement
//!   that does not *start* with a block-starter keyword. Recognizing it
//!   would mean scanning past an arbitrary amount of `WITH`-clause syntax
//!   before knowing whether a function is being declared, which is beyond
//!   what this task scoped. **Failure mode (S3-safe):** the inline
//!   function's own `;` terminators are read as ordinary plain-statement
//!   terminators, so the script is over-split into several statements, each
//!   of which fails to parse on its own — never an executable fragment. See
//!   the `#[ignore]`d `with_function_inline_plsql_is_a_known_limitation`
//!   test in `tests/corpus.rs`.
//! - **SQL\*Plus client commands** (`SET …`, `PROMPT`, `EXEC[UTE] …`) are not
//!   recognized as a distinct [`StatementKind`] variant. `SPEC.md` §15:
//!   "Future SQL\*Plus-like commands may be added progressively" — Phase 1
//!   does not require them, so none of Oracle's Phase-1 descriptor enables
//!   the machinery `sql-text` would need (see
//!   [`crate::dialect::CommentRules::line_comment_words`] for a case
//!   where the machinery exists but is deliberately left off).
//!   `StatementKind` is `#[non_exhaustive]` so a variant can be added later
//!   without a breaking change.

use crate::dialect::{BlockKind, BlockStarter, KeywordSlot, Phrase, SqlDialect};
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
    /// (Oracle: `;`) or a lone `/` line (S1).
    Plain,
    /// A block statement (an anonymous `DECLARE`/`BEGIN` block, or DDL that
    /// creates a stored PL/SQL unit, trigger, type, or opaque source) — see
    /// the [module documentation](self) for how its end is found.
    Block,
}

/// Why a [`StatementSpan`] ended where it did.
///
/// `#[non_exhaustive]`: a future refinement (e.g. distinguishing a
/// body-less-marker close from a structural `END` close) can add a variant
/// without breaking a caller that already matches with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EndedBy {
    /// The dialect's own statement terminator (Oracle: `;`), for a
    /// [`StatementKind::Plain`] statement. Every [`StatementKind::Block`]
    /// form — including [`crate::dialect::BlockKind::ParenDelimited`], a
    /// forward declaration, a call-spec, and a body-less marker like `CALL`
    /// — closes through the same slash-or-inferred decision as an ordinary
    /// `BEGIN…END` block ([`EndedBy::SlashLine`] or
    /// [`EndedBy::InferredBlockEnd`]), on the theory that a script author's
    /// habit of following any multi-line `CREATE …` with `/` is the same
    /// habit whether or not that particular statement happens to contain
    /// `BEGIN`/`END`.
    Terminator,
    /// An authoritative lone `/` line (safety principle S1) — the statement
    /// was submitted, complete or not.
    SlashLine,
    /// A block's own structural close (its outermost `END` and terminator)
    /// with no `/` line following. [`StatementSpan::terminated`] reflects
    /// [`SqlDialect::block_may_end_without_slash`] for this case: the
    /// boundary itself is not in doubt (it was found by the same depth
    /// tracking as every other block close), only whether SQL\*Plus
    /// semantics would consider it *sent*.
    InferredBlockEnd,
    /// End of input was reached with nothing above having closed the
    /// statement — a truncated script, or (see "Known limitations" on the
    /// [module documentation](self)) a documented scanning gap.
    EndOfInput,
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
    /// terminator. For a block statement this is right after the closing
    /// `END`, excluding its own required `;` (which [`full_end`](Self::full_end)
    /// includes, same as a plain statement's terminator).
    pub content_end: usize,
    /// Byte offset one past the statement's terminator: for
    /// [`StatementKind::Plain`], one past the terminator character (or past
    /// a `/` line's own trailing newline, when [`ended_by`](Self::ended_by)
    /// is [`EndedBy::SlashLine`]); for [`StatementKind::Block`], one past
    /// the closing `END`'s own `;`, or past a following `/` line's trailing
    /// newline when one is present.
    pub full_end: usize,
    /// The statement's first line, counting from `1`.
    pub start_line: u32,
    /// The statement's first character's column on that line, counting
    /// from `1` in Unicode scalar values.
    pub start_column: u32,
    /// Whether this is a plain or block statement.
    pub kind: StatementKind,
    /// Why this statement ended where it did.
    pub ended_by: EndedBy,
    /// Whether an explicit terminator was found. Always `false` when
    /// [`ended_by`](Self::ended_by) is [`EndedBy::EndOfInput`], always `true`
    /// for [`EndedBy::Terminator`]/[`EndedBy::SlashLine`], and equal to
    /// [`SqlDialect::block_may_end_without_slash`] for
    /// [`EndedBy::InferredBlockEnd`].
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

        // A lone `/` line with nothing pending: no statement text separates
        // it from whatever (if anything) came before, so it terminates
        // nothing and opens nothing. Produce no span at all — see the module
        // docs' "A lone `/` line with nothing pending produces no span at
        // all" section. This must be checked before `content_start` is even
        // computed, since there is no statement here to have one.
        if first.kind == TokenKind::Operator
            && (dialect.slash_terminates_block || dialect.slash_terminates_plain)
            && is_lone_slash_line(text, *first)
        {
            i += 1;
            continue;
        }

        let content_start = first.start;
        cursor.advance_to(text, content_start);
        let start_line = cursor.line;
        let start_column = cursor.column;

        let label_skipped = skip_labels(&tokens, text, i, dialect);
        let leading_words = collect_leading_words(&tokens, text, label_skipped);
        let matched_starter = dialect.block_starters.iter().find_map(|starter| {
            matches_block_starter(starter, &leading_words, label_skipped)
                .map(|resume| (starter, resume))
        });

        let outcome = match matched_starter {
            Some((starter, resume)) => match starter.kind {
                BlockKind::Structured => {
                    scan_structured(&tokens, text, resume, dialect, starter, content_start)
                }
                BlockKind::OpaqueSource => {
                    scan_opaque_source(&tokens, text, resume, dialect, content_start)
                }
                BlockKind::ParenDelimited => {
                    scan_paren_delimited(&tokens, text, resume, dialect, content_start)
                }
            },
            None => scan_plain(&tokens, text, i, dialect, content_start),
        };

        cursor.advance_to(text, outcome.full_end);
        spans.push(StatementSpan {
            content_start,
            content_end: outcome.content_end,
            full_end: outcome.full_end,
            start_line,
            start_column,
            kind: outcome.kind,
            ended_by: outcome.ended_by,
            terminated: outcome.terminated,
        });
        i = outcome.next_i;
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
///
/// Never panics regardless of `offset`: an out-of-range offset is clamped to
/// `text.len()`, and — since neither a caller-supplied offset nor `text.len()`
/// itself is guaranteed to land on a UTF-8 character boundary (`text` may be
/// sliced from a larger buffer, or `offset` may come from an editor cursor
/// mid-selection over multi-byte text) — the clamped value is then floored to
/// the nearest character boundary at or before it before anything is sliced.
#[must_use]
pub fn statement_at(text: &str, offset: usize, dialect: &SqlDialect) -> Option<StatementSpan> {
    let offset = floor_char_boundary(text, offset.min(text.len()));
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

/// The largest byte index `<= index` that is a UTF-8 character boundary in
/// `text` (`0` and `text.len()` always qualify). Equivalent to the standard
/// library's `str::floor_char_boundary`, which is not yet stable; `text` is
/// never more than a few characters scanned backwards in practice, since
/// UTF-8 continuation bytes run at most three deep.
fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

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

/// Advances past any number of leading `<<label>>` sequences (per
/// [`SqlDialect::label_delimiters`]), so a labelled block
/// (`<<outer>> BEGIN ... END outer;`) is still recognized as one by
/// [`matches_block_starter`]. Malformed input (an opening delimiter with no
/// matching close) stops without consuming anything past what was already
/// confirmed — matching then simply fails on whatever follows, which is
/// safe (see S3 in the [module documentation](self)).
fn skip_labels(tokens: &[Token], text: &str, start: usize, dialect: &SqlDialect) -> usize {
    let Some((open, close)) = dialect.label_delimiters else {
        return start;
    };
    let mut i = start;
    loop {
        let probe = skip_trivial(tokens, i);
        let Some(&open_token) = tokens.get(probe) else {
            return i;
        };
        if open_token.kind != TokenKind::Operator || &text[open_token.start..open_token.end] != open
        {
            return i;
        }
        let mut j = skip_trivial(tokens, probe + 1);
        if tokens.get(j).is_some_and(|t| is_word(t.kind)) {
            j = skip_trivial(tokens, j + 1);
        }
        let Some(&close_token) = tokens.get(j) else {
            return i;
        };
        if close_token.kind != TokenKind::Operator
            || &text[close_token.start..close_token.end] != close
        {
            return i;
        }
        i = j + 1;
    }
}

/// Collects up to [`MAX_LEADING_WORDS`] leading word-tokens' text and the
/// token index one past each, skipping whitespace/comments, stopping at the
/// first token that is neither — a short statement (fewer words than any
/// block-starter pattern needs) simply yields fewer words, which no pattern
/// will match.
fn collect_leading_words<'t>(
    tokens: &[Token],
    text: &'t str,
    mut i: usize,
) -> Vec<(&'t str, usize)> {
    let mut words = Vec::with_capacity(MAX_LEADING_WORDS);
    while words.len() < MAX_LEADING_WORDS {
        let Some(token) = tokens.get(i) else { break };
        if is_word(token.kind) {
            words.push((&text[token.start..token.end], i + 1));
        } else if !is_trivial(token.kind) {
            break;
        }
        i += 1;
    }
    words
}

fn match_phrase(options: &[Phrase], words: &[(&str, usize)]) -> Option<usize> {
    options.iter().find_map(|phrase| {
        (phrase.len() <= words.len()
            && phrase
                .iter()
                .zip(words.iter())
                .all(|(p, (w, _))| p.eq_ignore_ascii_case(w)))
        .then_some(phrase.len())
    })
}

/// Whether `starter` matches the words collected starting at `start_index`
/// (the position [`collect_leading_words`] began from, i.e. right after any
/// skipped label). Returns the token index to resume scanning the
/// statement's body from — right after the last keyword this pattern
/// consumed, or `start_index` itself if the pattern matched zero words (only
/// possible for a starter made entirely of [`KeywordSlot::Optional`] slots,
/// which none of this crate's own dialects use).
fn matches_block_starter(
    starter: &crate::dialect::BlockStarter,
    words: &[(&str, usize)],
    start_index: usize,
) -> Option<usize> {
    let mut idx = 0;
    for slot in starter.slots {
        match slot {
            KeywordSlot::Required(options) => {
                idx += match_phrase(options, &words[idx..])?;
            }
            KeywordSlot::Optional(options) => {
                if let Some(consumed) = match_phrase(options, &words[idx..]) {
                    idx += consumed;
                }
            }
        }
    }
    Some(if idx == 0 {
        start_index
    } else {
        words[idx - 1].1
    })
}

/// Whether `phrase`'s words match consecutively starting at token index `i`
/// (skipping trivia between them), without consuming anything — used to spot
/// [`SqlDialect::sectioned_body_marker`] appearing anywhere in a
/// [`BlockKind::Structured`] scan, and a [`SqlDialect::call_spec_phrases`]
/// phrase starting right after a `body_intro_keywords` word.
fn peek_phrase_matches(tokens: &[Token], text: &str, start: usize, phrase: Phrase) -> bool {
    let mut i = start;
    for (n, expected) in phrase.iter().enumerate() {
        if n > 0 {
            i = skip_trivial(tokens, i);
        }
        let Some(&token) = tokens.get(i) else {
            return false;
        };
        if !is_word(token.kind) || !expected.eq_ignore_ascii_case(&text[token.start..token.end]) {
            return false;
        }
        i += 1;
    }
    true
}

/// Whether `phrase`'s words match, in order, ending at and including token
/// index `end` (skipping trivia between them going backward) — the mirror
/// image of [`peek_phrase_matches`], used to check
/// [`SqlDialect::body_intro_exceptions`]: does the word *before* a candidate
/// `body_intro_keywords` match (Oracle: `AS`) complete an excepted phrase
/// (Oracle: `SELF AS`) rather than a real body intro?
fn ends_with_phrase(tokens: &[Token], text: &str, end: usize, phrase: Phrase) -> bool {
    let mut cursor = Some(end);
    for expected in phrase.iter().rev() {
        loop {
            let Some(idx) = cursor else { return false };
            let Some(&token) = tokens.get(idx) else {
                return false;
            };
            if is_trivial(token.kind) {
                cursor = idx.checked_sub(1);
                continue;
            }
            if !is_word(token.kind) || !expected.eq_ignore_ascii_case(&text[token.start..token.end])
            {
                return false;
            }
            cursor = idx.checked_sub(1);
            break;
        }
    }
    true
}

/// The result of scanning one statement's body, from whichever `scan_*`
/// function handled it.
struct ScanOutcome {
    content_end: usize,
    full_end: usize,
    terminated: bool,
    ended_by: EndedBy,
    next_i: usize,
    kind: StatementKind,
}

fn ran_to_eof(tokens: &[Token], text: &str, kind: StatementKind) -> ScanOutcome {
    let end = tokens.last().map_or(text.len(), |t| t.end);
    ScanOutcome {
        content_end: end,
        full_end: end,
        terminated: false,
        ended_by: EndedBy::EndOfInput,
        next_i: tokens.len(),
        kind,
    }
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

/// Builds the [`ScanOutcome`] for safety principle S1: `slash_token` is an
/// authoritative lone `/` line ending the statement right now, regardless of
/// any other state the caller was tracking.
///
/// `content_start` is the byte offset of the span's own
/// [`StatementSpan::content_start`] (not necessarily this scan's own `start`
/// parameter, which for a block statement is already past the matched
/// starter's keywords). `content_end` is clamped to never fall below it: by
/// construction (see the module docs' "lone `/` line with nothing pending"
/// section) it never should, since a real, already-known-non-trivial token
/// always lies between `content_start` and `slash_token`, but this is
/// defense in depth for any scan path that does not itself guarantee that —
/// an unclamped `content_end` computed purely from `slash_token`'s position
/// in the whole document was a confirmed adversarial-review defect
/// (ADR-0002 amendment J, round 2): `text[..slash_token.start].trim_end()`
/// can strip back past a span's own `content_start` when only trivia
/// separates them, producing `content_start > content_end` and a panic in
/// [`StatementSpan::content`].
fn end_at_slash_line(
    text: &str,
    slash_token: Token,
    kind: StatementKind,
    next_i: usize,
    content_start: usize,
) -> ScanOutcome {
    let content_end = text[..slash_token.start]
        .trim_end()
        .len()
        .max(content_start);
    let full_end = trailing_line_end(text, slash_token.end);
    ScanOutcome {
        content_end,
        full_end,
        terminated: true,
        ended_by: EndedBy::SlashLine,
        next_i,
        kind,
    }
}

/// After a terminator has been found at `terminator`, looks past only
/// whitespace/comments for a following lone `/` line ([`EndedBy::SlashLine`]
/// wins when [`SqlDialect::slash_terminates_block`] is set); otherwise
/// reports the terminator itself as the end, with
/// [`terminated`](StatementSpan::terminated) following
/// [`SqlDialect::block_may_end_without_slash`].
fn finish_with_maybe_slash(
    tokens: &[Token],
    text: &str,
    dialect: &SqlDialect,
    content_end: usize,
    semi_end: usize,
    next_i: usize,
    kind: StatementKind,
) -> ScanOutcome {
    if dialect.slash_terminates_block {
        let mut k = next_i;
        while let Some(&token) = tokens.get(k) {
            if is_trivial(token.kind) {
                k += 1;
                continue;
            }
            if token.kind == TokenKind::Operator && is_lone_slash_line(text, token) {
                let full_end = trailing_line_end(text, token.end);
                return ScanOutcome {
                    content_end,
                    full_end,
                    terminated: true,
                    ended_by: EndedBy::SlashLine,
                    next_i: k + 1,
                    kind,
                };
            }
            break;
        }
    }
    ScanOutcome {
        content_end,
        full_end: semi_end,
        terminated: dialect.block_may_end_without_slash,
        ended_by: EndedBy::InferredBlockEnd,
        next_i,
        kind,
    }
}

/// Scans a non-block statement from its first token (`i`): ends at the
/// dialect's statement terminator, or — safety principle S1 — an
/// authoritative lone `/` line, whichever comes first. `/` wins even over an
/// obviously-incomplete buffer (`SELECT 10\n/\n2 FROM dual;` runs only
/// `SELECT 10`, exactly matching SQL\*Plus/SQLcl): the splitter's job is to
/// find where the *client* would draw the boundary, not to validate SQL
/// grammar.
fn scan_plain(
    tokens: &[Token],
    text: &str,
    start: usize,
    dialect: &SqlDialect,
    content_start: usize,
) -> ScanOutcome {
    let mut i = start;
    while let Some(&token) = tokens.get(i) {
        if dialect.slash_terminates_plain
            && token.kind == TokenKind::Operator
            && is_lone_slash_line(text, token)
        {
            return end_at_slash_line(text, token, StatementKind::Plain, i + 1, content_start);
        }
        if is_terminator(text, token, dialect) {
            return ScanOutcome {
                content_end: token.start,
                full_end: token.end,
                terminated: true,
                ended_by: EndedBy::Terminator,
                next_i: i + 1,
                kind: StatementKind::Plain,
            };
        }
        i += 1;
    }
    ran_to_eof(tokens, text, StatementKind::Plain)
}

/// Scans a statement that is not PL/SQL at all
/// ([`crate::dialect::BlockKind::OpaqueSource`], Oracle: `CREATE ... JAVA
/// SOURCE ...`): its terminator character(s) are ordinary body content, so
/// only a lone `/` line or end of input can close it.
fn scan_opaque_source(
    tokens: &[Token],
    text: &str,
    start: usize,
    _dialect: &SqlDialect,
    content_start: usize,
) -> ScanOutcome {
    let mut i = start;
    while let Some(&token) = tokens.get(i) {
        if token.kind == TokenKind::Operator && is_lone_slash_line(text, token) {
            return end_at_slash_line(text, token, StatementKind::Block, i + 1, content_start);
        }
        i += 1;
    }
    ran_to_eof(tokens, text, StatementKind::Block)
}

/// Scans a [`crate::dialect::BlockKind::ParenDelimited`] statement (Oracle:
/// `CREATE [OR REPLACE] TYPE`, without `BODY`): no `BEGIN`/`END`, just
/// balanced `(`/`)` and a statement terminator at depth `0`. Also honors S1
/// (a lone `/` line always wins) so an unexpectedly unbalanced paren in
/// unusual input still cannot run past a `/` line.
fn scan_paren_delimited(
    tokens: &[Token],
    text: &str,
    start: usize,
    dialect: &SqlDialect,
    content_start: usize,
) -> ScanOutcome {
    let mut i = start;
    let mut paren_depth: i32 = 0;
    while let Some(&token) = tokens.get(i) {
        if dialect.slash_terminates_block
            && token.kind == TokenKind::Operator
            && is_lone_slash_line(text, token)
        {
            return end_at_slash_line(text, token, StatementKind::Block, i + 1, content_start);
        }
        if token.kind == TokenKind::Operator {
            match &text[token.start..token.end] {
                "(" => {
                    paren_depth += 1;
                    i += 1;
                    continue;
                }
                ")" => {
                    paren_depth -= 1;
                    i += 1;
                    continue;
                }
                _ => {}
            }
        }
        if paren_depth <= 0 && is_terminator(text, token, dialect) {
            return finish_with_maybe_slash(
                tokens,
                text,
                dialect,
                token.start,
                token.end,
                i + 1,
                StatementKind::Block,
            );
        }
        i += 1;
    }
    ran_to_eof(tokens, text, StatementKind::Block)
}

/// Scans forward from right after a [`SqlDialect::subprogram_header_keywords`]/
/// [`SqlDialect::section_header_starters`] word (`i` is the index of the
/// token right after it) to decide whether this header owes a body.
///
/// Tracks `(`/`)` depth: a statement terminator or
/// [`SqlDialect::body_intro_keywords`] word only counts at depth `0`, so a
/// parameter default's own `CAST(x AS t)`/`CASE WHEN x IS NULL THEN … END`
/// cannot be mistaken for the header's own `AS`/`IS` or terminator (a
/// confirmed adversarial-review defect — ADR-0002 amendment J, round 2 —
/// this scan was previously blind to parens entirely). A depth-`0`
/// `body_intro_keywords` word that only completes a
/// [`SqlDialect::body_intro_exceptions`] phrase (Oracle: a constructor's
/// `RETURN SELF AS RESULT IS`, where `AS` is not the real body intro) is
/// skipped over rather than treated as one.
///
/// Returns `(resume_index, owes_body)`.
fn scan_header(tokens: &[Token], text: &str, start: usize, dialect: &SqlDialect) -> (usize, bool) {
    let mut i = start;
    let mut paren_depth: i32 = 0;
    while let Some(&token) = tokens.get(i) {
        if token.kind == TokenKind::Operator {
            match &text[token.start..token.end] {
                "(" => {
                    paren_depth += 1;
                    i += 1;
                    continue;
                }
                ")" => {
                    paren_depth -= 1;
                    i += 1;
                    continue;
                }
                _ => {}
            }
            if paren_depth <= 0 && is_terminator(text, token, dialect) {
                return (i + 1, false);
            }
            i += 1;
            continue;
        }
        if paren_depth <= 0 && is_word(token.kind) {
            let word = &text[token.start..token.end];
            if dialect
                .body_intro_keywords
                .iter()
                .any(|k| k.eq_ignore_ascii_case(word))
                && !dialect
                    .body_intro_exceptions
                    .iter()
                    .any(|phrase| ends_with_phrase(tokens, text, i, phrase))
            {
                let after = skip_trivial(tokens, i + 1);
                let is_call_spec = dialect
                    .call_spec_phrases
                    .iter()
                    .any(|phrase| peek_phrase_matches(tokens, text, after, phrase));
                if is_call_spec {
                    let mut j = after;
                    while let Some(&t2) = tokens.get(j) {
                        if is_terminator(text, t2, dialect) {
                            return (j + 1, false);
                        }
                        j += 1;
                    }
                    return (tokens.len(), false);
                }
                return (i + 1, true);
            }
        }
        i += 1;
    }
    (tokens.len(), false)
}

/// Scans forward to the next statement terminator (honoring S1's `/` line
/// authority along the way) and closes the statement there — used for
/// [`SqlDialect::body_less_markers`] (`CALL`): the rest of the statement is
/// ordinary content with no PL/SQL structure at all.
fn scan_to_terminator(
    tokens: &[Token],
    text: &str,
    start: usize,
    dialect: &SqlDialect,
    content_start: usize,
) -> ScanOutcome {
    let mut i = start;
    while let Some(&token) = tokens.get(i) {
        if dialect.slash_terminates_block
            && token.kind == TokenKind::Operator
            && is_lone_slash_line(text, token)
        {
            return end_at_slash_line(text, token, StatementKind::Block, i + 1, content_start);
        }
        if is_terminator(text, token, dialect) {
            return finish_with_maybe_slash(
                tokens,
                text,
                dialect,
                token.start,
                token.end,
                i + 1,
                StatementKind::Block,
            );
        }
        i += 1;
    }
    ran_to_eof(tokens, text, StatementKind::Block)
}

/// Consumes an `END`'s optional qualifier (any number of words — a name,
/// `LOOP`, `IF`, `CASE`, a compound trigger's timing-point words) up to and
/// including the following statement terminator. Stops (without consuming)
/// at the first token that is neither a word nor the terminator, so
/// malformed/truncated input cannot make this swallow unrelated content —
/// consistent with S3 (prefer a larger span, but do not invent structure
/// that is not there).
fn skip_end_qualifier(tokens: &[Token], text: &str, start: usize, dialect: &SqlDialect) -> usize {
    let mut i = start;
    loop {
        i = skip_trivial(tokens, i);
        let Some(&token) = tokens.get(i) else {
            return i;
        };
        if is_terminator(text, token, dialect) {
            return i + 1;
        }
        if !is_word(token.kind) {
            return i;
        }
        i += 1;
    }
}

/// `tokens[end_index]` is the outermost `END` (nesting depth `0`).
/// Consumes its qualifier and required terminator, then applies the `/`
/// rule via [`finish_with_maybe_slash`].
fn finish_structured(
    tokens: &[Token],
    text: &str,
    end_index: usize,
    dialect: &SqlDialect,
) -> ScanOutcome {
    let mut j = skip_trivial(tokens, end_index + 1);
    while tokens.get(j).is_some_and(|t| is_word(t.kind)) {
        j = skip_trivial(tokens, j + 1);
    }
    let Some(&terminator) = tokens.get(j) else {
        return ran_to_eof(tokens, text, StatementKind::Block);
    };
    if !is_terminator(text, terminator, dialect) {
        return ran_to_eof(tokens, text, StatementKind::Block);
    }
    finish_with_maybe_slash(
        tokens,
        text,
        dialect,
        terminator.start,
        terminator.end,
        j + 1,
        StatementKind::Block,
    )
}

/// Scans a [`crate::dialect::BlockKind::Structured`] statement from its
/// first token (`start`, right after the matched `starter`'s own leading
/// keywords). See the [module documentation](self)'s "block-termination
/// rule" section for the full algorithm; safety principle S1 (an
/// authoritative `/` line) is checked before anything else on every token,
/// so it always wins regardless of `depth`/`pending_bodies`/`paren_depth`.
///
/// `starter.opens_body` seeds `absorbed_body`: see [`BlockStarter::opens_body`]
/// and the module docs for why a bare `BEGIN` starter must start this scan
/// as though its one body has already been absorbed, while every other
/// starter shape starts fresh. All structural word-dispatch below is also
/// gated on this scan's own `(`/`)` depth being `0`, so the *outer*
/// statement's own parameter list (as opposed to a nested member's, which
/// [`scan_header`] guards separately) cannot corrupt `depth`/`pending_bodies`
/// via a parameter default's `CASE … END`/`CAST(x AS t)`.
#[allow(clippy::too_many_lines)]
fn scan_structured(
    tokens: &[Token],
    text: &str,
    start: usize,
    dialect: &SqlDialect,
    starter: &BlockStarter,
    content_start: usize,
) -> ScanOutcome {
    let mut i = start;
    let mut depth: i32 = 0;
    let mut pending_bodies: u32 = 0;
    let mut absorbed_body = starter.opens_body;
    let mut is_compound = false;
    let mut paren_depth: i32 = 0;

    while let Some(&token) = tokens.get(i) {
        if dialect.slash_terminates_block
            && token.kind == TokenKind::Operator
            && is_lone_slash_line(text, token)
        {
            return end_at_slash_line(text, token, StatementKind::Block, i + 1, content_start);
        }

        if token.kind == TokenKind::Operator {
            match &text[token.start..token.end] {
                "(" => paren_depth += 1,
                ")" => paren_depth -= 1,
                _ => {}
            }
            i += 1;
            continue;
        }

        if !is_word(token.kind) || paren_depth > 0 {
            i += 1;
            continue;
        }
        let word = &text[token.start..token.end];

        if !is_compound
            && let Some(marker) = dialect.sectioned_body_marker
            && peek_phrase_matches(tokens, text, i, marker)
        {
            is_compound = true;
        }

        if depth == 0
            && pending_bodies == 0
            && !absorbed_body
            && dialect
                .body_less_markers
                .iter()
                .any(|k| k.eq_ignore_ascii_case(word))
        {
            return scan_to_terminator(tokens, text, i + 1, dialect, content_start);
        }

        // The *outer* statement's own header is already consumed by the
        // matched `BlockStarter` pattern (that is what `start` resumes
        // after), so it never reaches `subprogram_header_keywords` detection
        // below the way a nested member's header does. A single subprogram
        // can still be a call-spec in its own right (`CREATE FUNCTION f(...)
        // RETURN t IS LANGUAGE JAVA ...;`), so its `body_intro_keywords`
        // word is checked here, the same way `scan_header` checks a nested
        // one — only while still awaiting *this* frame's own body.
        if depth == 0
            && pending_bodies == 0
            && !absorbed_body
            && dialect
                .body_intro_keywords
                .iter()
                .any(|k| k.eq_ignore_ascii_case(word))
            && !dialect
                .body_intro_exceptions
                .iter()
                .any(|phrase| ends_with_phrase(tokens, text, i, phrase))
        {
            let after = skip_trivial(tokens, i + 1);
            let is_call_spec = dialect
                .call_spec_phrases
                .iter()
                .any(|phrase| peek_phrase_matches(tokens, text, after, phrase));
            if is_call_spec {
                return scan_to_terminator(tokens, text, after, dialect, content_start);
            }
        }

        let is_header_word = dialect
            .subprogram_header_keywords
            .iter()
            .any(|k| k.eq_ignore_ascii_case(word))
            || (is_compound
                && dialect
                    .section_header_starters
                    .iter()
                    .any(|k| k.eq_ignore_ascii_case(word)));

        if is_header_word {
            let (resume, owes_body) = scan_header(tokens, text, i + 1, dialect);
            if owes_body {
                pending_bodies += 1;
            }
            i = resume;
            continue;
        }

        if word.eq_ignore_ascii_case(dialect.block_end_keyword) {
            if depth == 0 {
                return finish_structured(tokens, text, i, dialect);
            }
            depth -= 1;
            i = skip_end_qualifier(tokens, text, i + 1, dialect);
            continue;
        }

        if word.eq_ignore_ascii_case(dialect.block_body_opener) {
            // `pending_bodies > 0` alone is not enough: it only says *some*
            // header somewhere up the stack still owes a body, not that
            // *this* `BEGIN` is the one that fulfills it. Once `depth > 0` —
            // already inside a body that a previous `BEGIN` opened, whether
            // by fulfilling a debt or by absorption — any further `BEGIN`
            // is unambiguously a nested block, never a fulfillment, no
            // matter how many debts are still outstanding further up.
            // Requiring `depth == 0` too was a confirmed defect the
            // grammar-based differential test found (ADR-0002 amendment J,
            // round 2): a two-level-deep nested subprogram whose *inner*
            // member's own body itself contained a nested `BEGIN … END` had
            // that inner `BEGIN` wrongly consumed as fulfilling the
            // *outer* member's still-pending debt, corrupting depth
            // tracking so the outermost statement's real closing `END`
            // never matched at depth `0`.
            if pending_bodies > 0 && depth == 0 {
                pending_bodies -= 1;
                depth += 1;
            } else if depth == 0 && !absorbed_body {
                absorbed_body = true;
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

    // No outermost `END` (or body-less-marker terminator) at all before end
    // of input: a truncated script, or a documented scanning gap — see the
    // module docs' "Known limitations".
    ran_to_eof(tokens, text, StatementKind::Block)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_char_boundary_never_lands_inside_a_multibyte_character() {
        let text = ";\u{feff}"; // ';' (1 byte) + BOM (3 bytes) = 4 bytes total
        for probe in 0..=(text.len() + 2) {
            let floored = floor_char_boundary(text, probe.min(text.len()));
            assert!(
                text.is_char_boundary(floored),
                "probe={probe} floored={floored}"
            );
        }
    }
}
