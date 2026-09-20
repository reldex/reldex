//! [`SqlDialect`]: the data a driver supplies so this crate never hard-codes
//! a vendor's syntax.
//!
//! # Why `SqlDialect` lives in `reldex-sql-text`, not in `db-core` or
//! `db-driver-api`
//!
//! `docs/exec-plans/active/phase-1.md` §B4 sketched this descriptor as living
//! in `db-core`. Two constraints specific to how this task was carried out
//! made that impractical and, on reflection, not the best home anyway:
//!
//! 1. **Isolation.** `db-core` was being changed concurrently by another
//!    workstream (M2.5/M2.6, the event-queue and session-registry refactor)
//!    in a separate worktree; adding a type there would have collided with
//!    that work for no benefit to either.
//! 2. **Dependency direction.** [`SqlDialect`] exists to be the parameter of
//!    [`crate::tokenize`] and [`crate::split_statements`] — this crate's own
//!    public API. Putting the type in `db-core` would make `db-core` depend
//!    on `reldex-sql-text` (fine) while this crate's *signature* names a type
//!    it does not own (awkward, and it would force every caller of the
//!    lexer/splitter — including a future non-`db-core` consumer such as a
//!    standalone formatter or linter — to also depend on `db-core`, which
//!    pulls in sessions, transactions and the driver contract for no
//!    reason). Putting it in `db-driver-api` was the other candidate the
//!    task considered; rejected because the driver contract is deliberately
//!    small (ADR-0002 D8 lists "script/statement-boundary parsing" as
//!    explicitly out of scope for *the contract*) and `SqlDialect` is
//!    consumed by `sql-text`, not by anything crossing the
//!    `DatabaseConnection` boundary.
//!
//! The result keeps the dependency graph a straight line —
//! `reldex-sql-text` depends on nothing; `reldex-driver-oracle-thin` depends
//! on `reldex-sql-text` (as it already depends on `reldex-db-driver-api`) and
//! exposes `oracle_thin::sql_dialect()`; `db-core` and the Qt/FFI layer may
//! depend on `reldex-sql-text` directly for the editor's highlighter and
//! statement splitter, without needing a driver in scope at all. See the
//! ADR-0002 amendment "The `SqlDialect` descriptor" for the recorded
//! decision.
//!
//! # What is data here, and what stays fixed
//!
//! The *lexical shape* of a comment, a plain string, a quoted identifier and
//! an alternative-quoted (`q'...'`) literal is fixed SQL syntax that this
//! crate's scanner knows how to recognize once enabled. What varies by
//! dialect — and is therefore a field on [`SqlDialect`], never a literal in
//! `lexer.rs` or `splitter.rs` — is: whether each of those forms is enabled
//! at all, what the comment delimiters actually are, which keywords open a
//! block statement, which keyword closes one, and whether `/` is meaningful.
//! Nothing in this module or its siblings spells out an Oracle keyword;
//! Oracle's own descriptor is built by `reldex-driver-oracle-thin`.

/// A case-insensitive multi-word phrase matched in order, e.g. `&["OR",
/// "REPLACE"]` or the single-word `&["TRIGGER"]`.
///
/// Each element is compared with [`str::eq_ignore_ascii_case`], which is
/// correct for every keyword this crate matches against (SQL/PL-SQL keywords
/// are ASCII).
pub type Phrase = &'static [&'static str];

/// One matching step in a [`BlockStarter`] pattern.
#[derive(Debug, Clone, Copy)]
pub enum KeywordSlot {
    /// Exactly one of these phrases must match at this position, or the
    /// pattern fails.
    Required(&'static [Phrase]),
    /// Zero or one of these phrases may match at this position; if none
    /// does, no keywords are consumed and matching proceeds to the next
    /// slot. Used for optional modifiers such as `OR REPLACE` and
    /// `EDITIONABLE`/`NONEDITIONABLE`.
    Optional(&'static [Phrase]),
}

/// A pattern recognizing a statement that opens a "block": a statement whose
/// body may contain the dialect's own statement terminator(s) without
/// ending the statement, and whose real end is governed by
/// [`SqlDialect::slash_terminates_block`] /
/// [`SqlDialect::block_may_end_without_slash`] rather than by the first
/// terminator found.
///
/// Matching walks a statement's *leading* keywords against `slots` in order.
/// For Oracle, four shapes are registered by
/// `reldex_driver_oracle_thin::sql_dialect`: a bare `DECLARE`, a bare
/// `BEGIN`, `CREATE [OR REPLACE] [EDITIONABLE|NONEDITIONABLE]
/// {PROCEDURE|FUNCTION|TRIGGER}`, and `CREATE [OR REPLACE]
/// [EDITIONABLE|NONEDITIONABLE] {PACKAGE[ BODY]|TYPE[ BODY]}` — the last one
/// distinct from the third because of
/// [`absorbs_body_opener`](Self::absorbs_body_opener).
#[derive(Debug, Clone, Copy)]
pub struct BlockStarter {
    /// The slots tried in order, left to right.
    pub slots: &'static [KeywordSlot],
    /// Whether this statement's outermost closing
    /// [`SqlDialect::block_end_keyword`] pairs with a single
    /// [`SqlDialect::block_body_opener`] found somewhere in its body.
    ///
    /// `true` for a `DECLARE`/`BEGIN` block, a single `PROCEDURE`,
    /// `FUNCTION` or `TRIGGER`: exactly one `BEGIN` (however far into the
    /// statement it appears, past a declare section) opens the statement's
    /// own body, and the first `END` found once inside it — after every
    /// nested `BEGIN`/`CASE`/`IF`/`LOOP` it contains has been balanced back
    /// out — is that body's own close, which is the statement's close.
    ///
    /// `false` for `PACKAGE[ BODY]`/`TYPE[ BODY]`: these are a *sequence* of
    /// independent member declarations, each of which may be a complete
    /// subprogram body with its **own**, unrelated `BEGIN ... END` pair (so
    /// `CREATE PACKAGE BODY p AS PROCEDURE a IS BEGIN … END; FUNCTION b …
    /// BEGIN … END; END p;` contains *two* fully self-contained blocks
    /// before the package's own closing `END p;`). Absorbing "the first
    /// `BEGIN`" here would mistake the first member's `END` for the
    /// package's own; instead every `BEGIN` (including the first) counts as
    /// an ordinary nested opener, and the statement's own close is the first
    /// `END` seen while nothing is open at all (nesting depth `0`) — which
    /// is only reached once each member's internal block has already
    /// balanced back out.
    pub absorbs_body_opener: bool,
}

/// Quoting forms this dialect recognizes, beyond the SQL-standard plain
/// `'...'` string (with `''` escaping) and `"..."` quoted identifier, which
/// this crate's lexer always recognizes.
#[derive(Debug, Clone, Copy, Default)]
pub struct QuotingRules {
    /// Oracle's alternative quoting: `q'[...]'`, `q'{...}'`, `q'<...>'`,
    /// `q'(...)'`, or `q'X...X'` for any other delimiter character `X`. Also
    /// gates the `nq'...'` national form (see
    /// [`national_prefix`](Self::national_prefix)).
    pub alternative_quoting: bool,
    /// The national-character-set string prefix: `N'...'` (a plain string)
    /// and, when [`alternative_quoting`](Self::alternative_quoting) is also
    /// set, `Nq'...'`/`nq'...'`.
    pub national_prefix: bool,
}

/// Comment forms this dialect recognizes.
#[derive(Debug, Clone, Copy)]
pub struct CommentRules {
    /// The line-comment prefix, e.g. `Some("--")`. Runs to end of line.
    pub line_comment: Option<&'static str>,
    /// The block-comment delimiters, e.g. `Some(("/*", "*/"))`. Does not
    /// nest: the first closing delimiter found ends the comment, exactly as
    /// Oracle behaves.
    pub block_comment: Option<(&'static str, &'static str)>,
    /// Whether a line whose first word (case-insensitive) is `REM` or
    /// `REMARK` is a SQL\*Plus line comment. `SPEC.md` §15 defers SQL\*Plus
    /// client commands ("Future SQL\*Plus-like commands may be added
    /// progressively"), so Oracle's Phase 1 descriptor leaves this `false`
    /// even though the lexer implements it and is tested against it.
    pub sqlplus_rem: bool,
}

/// The vendor-specific facts this crate's lexer and splitter need, supplied
/// by the driver as plain data.
///
/// Every field is `'static` data (character/boolean/string-slice values and
/// slices of them), so `SqlDialect` is `Copy` and cheap to pass by value; no
/// heap allocation is involved in building or using one.
///
/// See the [module documentation](self) for why this type lives in
/// `reldex-sql-text` rather than in `db-core` or `db-driver-api`.
#[derive(Debug, Clone, Copy)]
pub struct SqlDialect {
    /// Characters that terminate a plain (non-block) statement. Oracle: `[';']`.
    pub statement_terminators: &'static [char],
    /// Whether a line containing only `/` (surrounding whitespace allowed)
    /// ends a block statement. Oracle: `true` (SQL\*Plus's own convention).
    pub slash_terminates_block: bool,
    /// Whether a block statement may end at the statement terminator that
    /// closes its outermost `END`, when no `/` line follows (only
    /// whitespace/comments may separate that terminator from either the next
    /// non-trivial text or end of input). When `false`, a block without a
    /// trailing `/` line is reported as **not terminated** — mirroring
    /// SQL\*Plus itself, which never sends a block to the server until `/`
    /// is typed.
    pub block_may_end_without_slash: bool,
    /// Patterns recognizing a statement that opens a block. See
    /// [`BlockStarter`].
    pub block_starters: &'static [BlockStarter],
    /// The keyword that opens a block's body, matched at most once without
    /// increasing nesting depth (it is the block's *own* opener, not a
    /// nested one) — Oracle: `"BEGIN"`. Every later occurrence nests.
    pub block_body_opener: &'static str,
    /// Keywords that always open a nested construct requiring its own
    /// matching [`block_end_keyword`](Self::block_end_keyword) — Oracle:
    /// `["CASE", "IF", "LOOP"]`. `LOOP` covers `FOR`/`WHILE` loops too, since
    /// both end in a bare `LOOP ... END LOOP`.
    pub block_nesting_openers: &'static [&'static str],
    /// The keyword that closes one level of nesting, and — at the outermost
    /// level — the block itself. Oracle: `"END"`.
    pub block_end_keyword: &'static str,
    /// Which quoting forms beyond plain `'...'`/`"..."` this dialect enables.
    pub quoting: QuotingRules,
    /// Which comment forms this dialect enables.
    pub comments: CommentRules,
    /// Whether `:name` / `:1` / `:"Quoted Name"` are lexed as
    /// [`crate::TokenKind::BindVariable`] rather than an operator followed by
    /// an identifier/number.
    pub bind_variables: bool,
    /// Whether `&name` / `&&name` (optionally followed by a single trailing
    /// `.`) are lexed as [`crate::TokenKind::SubstitutionVariable`].
    pub substitution_variables: bool,
    /// The dialect's keyword set, compared case-insensitively. Used only to
    /// decide [`crate::TokenKind::Keyword`] vs.
    /// [`crate::TokenKind::Identifier`] for an unquoted word — a cosmetic
    /// (highlighting) distinction. **Nothing in [`crate::splitter`] depends
    /// on this list being complete**: block-boundary detection matches
    /// specific words (`block_body_opener`, `block_nesting_openers`,
    /// `block_end_keyword`, and the words inside `block_starters`) directly
    /// against token text, regardless of whether they happen to appear here.
    pub keywords: &'static [&'static str],
}

impl SqlDialect {
    /// Whether `word` is one of this dialect's keywords, compared
    /// case-insensitively.
    #[must_use]
    pub fn is_keyword(&self, word: &str) -> bool {
        self.keywords.iter().any(|k| k.eq_ignore_ascii_case(word))
    }
}
