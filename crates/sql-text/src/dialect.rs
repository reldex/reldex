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
//! at all, what the comment delimiters (and SQL\*Plus-style line-comment
//! *words*, e.g. Oracle's `REM`/`REMARK`, and alternative-quote/national
//! string *prefix words*, e.g. Oracle's `Q`/`NQ`/`N`) actually are, which
//! keywords open a block statement or a nested member header, which keyword
//! closes one, and whether `/` is meaningful. Nothing in this module or its
//! siblings spells out an Oracle keyword; Oracle's own descriptor is built by
//! `reldex-driver-oracle-thin`.

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

/// How [`crate::splitter`] scans a statement recognized by a [`BlockStarter`]
/// once its leading keywords have matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BlockKind {
    /// PL/SQL block structure: `BEGIN`/`CASE`/`IF`/`LOOP` vs. `END`, plus
    /// member/subprogram headers that may each owe a body of their own
    /// (`PROCEDURE`/`FUNCTION … IS|AS`, and — once
    /// [`SqlDialect::compound_trigger_marker`] has been seen — a compound
    /// trigger's timing-point sections). One depth-tracking algorithm
    /// handles a lone `BEGIN…END`, a single subprogram, and a
    /// `PACKAGE`/`TYPE`/compound-`TRIGGER` body with any number of
    /// independent members: see the [module documentation](crate::splitter)
    /// on `splitter.rs` for the full rule.
    Structured,
    /// Not PL/SQL at all: the statement's terminator character(s) are
    /// ordinary content, not a boundary. Ends only at a lone `/` line or end
    /// of input (Oracle: `CREATE ... JAVA SOURCE ...`, whose body is Java
    /// source text that may itself contain `;`).
    OpaqueSource,
    /// A single balanced-parenthesis declaration with no `BEGIN`/`END` at
    /// all: scans forward tracking paren depth and ends at the first
    /// statement terminator found at depth `0` (Oracle: `CREATE [OR REPLACE]
    /// TYPE name ...` — the object/varray/table-of forms end in a
    /// parenthesized attribute/method list, and the simple `IS <type>`
    /// synonym form has no parens at all, which this scan handles the same
    /// way: the terminator is found immediately since depth never leaves
    /// `0`).
    ParenDelimited,
}

/// A pattern recognizing a statement that opens a block (in the sense of
/// [`BlockKind`]) — a statement whose real end is governed by
/// [`kind`](Self::kind) rather than by the first statement terminator found.
///
/// Matching walks a statement's *leading* keywords against `slots` in order,
/// after skipping any [`SqlDialect::label_delimiters`]. For Oracle, six
/// shapes are registered by `reldex_driver_oracle_thin::sql_dialect`: a bare
/// `DECLARE`, a bare `BEGIN`, `CREATE [OR REPLACE] [EDITIONABLE|NONEDITIONABLE]
/// {PROCEDURE|FUNCTION|TRIGGER}`, `CREATE [OR REPLACE]
/// [EDITIONABLE|NONEDITIONABLE] {PACKAGE[ BODY]|TYPE BODY}`, `CREATE [OR
/// REPLACE] [EDITIONABLE|NONEDITIONABLE] TYPE` (without `BODY`, since it
/// needs [`BlockKind::ParenDelimited`] rather than [`BlockKind::Structured`]),
/// and `CREATE [OR REPLACE] [AND RESOLVE|AND COMPILE] [NOFORCE] JAVA SOURCE`.
#[derive(Debug, Clone, Copy)]
pub struct BlockStarter {
    /// The slots tried in order, left to right.
    pub slots: &'static [KeywordSlot],
    /// How the statement's body is scanned once these keywords have matched.
    pub kind: BlockKind,
}

/// Quoting forms this dialect recognizes, beyond the SQL-standard plain
/// `'...'` string (with `''` escaping) and `"..."` quoted identifier, which
/// this crate's lexer always recognizes.
#[derive(Debug, Clone, Copy, Default)]
pub struct QuotingRules {
    /// Word prefixes (compared case-insensitively) that introduce an
    /// alternative-quoted string — `<prefix>'[...]'`, `<prefix>'{...}'`,
    /// `<prefix>'<...>'`, `<prefix>'(...)'`, or `<prefix>'X...X'` for any
    /// other delimiter character `X` — when the word is immediately followed
    /// by `'`. Oracle: `["Q", "NQ"]` (the second doubling as the "national"
    /// alternative-quoted form). Empty disables the alternative-quoted form
    /// entirely; nothing in this crate's lexer hard-codes `Q`/`NQ`.
    pub alternative_quote_prefixes: &'static [&'static str],
    /// Word prefixes that introduce a plain (non-alternative) national
    /// string — `<prefix>'...'` — when the word is immediately followed by
    /// `'`. Oracle: `["N"]`. Empty disables the national-string form.
    pub national_string_prefixes: &'static [&'static str],
}

/// Comment forms this dialect recognizes.
#[derive(Debug, Clone, Copy, Default)]
pub struct CommentRules {
    /// The line-comment prefix, e.g. `Some("--")`. Runs to end of line.
    pub line_comment: Option<&'static str>,
    /// The block-comment delimiters, e.g. `Some(("/*", "*/"))`. Does not
    /// nest: the first closing delimiter found ends the comment, exactly as
    /// Oracle behaves.
    pub block_comment: Option<(&'static str, &'static str)>,
    /// Words (compared case-insensitively) that, as a line's very first
    /// word, start a SQL\*Plus-style line comment running to end of line —
    /// Oracle: `["REM", "REMARK"]`. Empty disables this form. `SPEC.md` §15
    /// defers SQL\*Plus client commands ("Future SQL\*Plus-like commands may
    /// be added progressively"), so Oracle's Phase 1 descriptor leaves this
    /// empty even though the lexer implements it and is tested against it.
    pub sqlplus_line_comment_words: &'static [&'static str],
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
    /// authoritatively ends a **block** statement, wherever it appears —
    /// regardless of nesting depth or any pending member body — the moment
    /// it is seen. Oracle: `true` (SQL\*Plus's own convention). This is the
    /// splitter's safety net: even a block-structure miscount can carve out
    /// at most one over-large statement, never an executable fragment from
    /// the middle of one, because a lone `/` line always closes whatever is
    /// currently open. See the [`crate::splitter`] module docs' "Safety"
    /// section.
    pub slash_terminates_block: bool,
    /// The same authority, for a **plain** statement: a lone `/` line ends
    /// it even though no statement terminator was seen (SQL\*Plus itself
    /// submits whatever is in its buffer when `/` is typed, complete or
    /// not). Oracle: `true`.
    pub slash_terminates_plain: bool,
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
    /// The opening/closing delimiters of a statement label, e.g. Oracle's
    /// `Some(("<<", ">>"))` for `<<my_label>>`. A label may precede a block
    /// statement's own leading keywords (`<<outer>> BEGIN ... END outer;`)
    /// or a `LOOP`; [`crate::splitter`] skips any number of them before
    /// matching [`block_starters`](Self::block_starters). `None` disables
    /// label recognition (a leading `<<` is then ordinary content, most
    /// likely an operator token that simply fails every block-starter
    /// pattern).
    pub label_delimiters: Option<(&'static str, &'static str)>,
    /// The keyword that opens a block's body, matched at most once per
    /// nesting frame without increasing depth (it is that frame's *own*
    /// opener, not a nested one) — Oracle: `"BEGIN"`. Every other occurrence
    /// nests, or — while a member header's [`subprogram_header_keywords`](Self::subprogram_header_keywords)
    /// scan has left one body owed — fulfills that debt instead. See the
    /// [`crate::splitter`] module docs for the full rule.
    pub block_body_opener: &'static str,
    /// Keywords that always open a nested construct requiring its own
    /// matching [`block_end_keyword`](Self::block_end_keyword) — Oracle:
    /// `["CASE", "IF", "LOOP"]`. `LOOP` covers `FOR`/`WHILE` loops too, since
    /// both end in a bare `LOOP ... END LOOP`.
    pub block_nesting_openers: &'static [&'static str],
    /// The keyword that closes one level of nesting, and — at the outermost
    /// level — the block itself. Oracle: `"END"`. Everything between an
    /// `END` and the next statement terminator is that construct's
    /// qualifier (a name, `LOOP`, `IF`, `CASE`, or a compound trigger's
    /// timing-point words) and is skipped as a unit, however many words long.
    pub block_end_keyword: &'static str,
    /// Keywords that introduce a member/subprogram header which may — but,
    /// if it turns out to be a forward declaration ending directly at a
    /// terminator, or a call-spec (see
    /// [`call_spec_keywords`](Self::call_spec_keywords)), need not — owe a
    /// body of its own. Oracle: `["PROCEDURE", "FUNCTION"]` (this covers
    /// `MEMBER FUNCTION`, `STATIC PROCEDURE`, `CONSTRUCTOR FUNCTION`, … too,
    /// since only the trigger word itself is matched).
    pub subprogram_header_keywords: &'static [&'static str],
    /// Keywords that, once a [`subprogram_header_keywords`](Self::subprogram_header_keywords)
    /// header reaches one of them, mean "the body follows here" (unless
    /// immediately followed by a [`call_spec_keywords`](Self::call_spec_keywords)
    /// word). Oracle: `["IS", "AS"]`.
    pub body_intro_keywords: &'static [&'static str],
    /// Keywords that, immediately after a
    /// [`body_intro_keywords`](Self::body_intro_keywords) word, mean the
    /// header is a call-spec (`LANGUAGE JAVA ...` / `EXTERNAL ...`) with no
    /// PL/SQL body — it ends at its own next statement terminator instead.
    /// Oracle: `["LANGUAGE", "EXTERNAL"]`.
    pub call_spec_keywords: &'static [&'static str],
    /// Keywords that, seen before any body has been owed or absorbed at the
    /// outermost level of a [`BlockKind::Structured`] statement, mean the
    /// whole statement is body-less DDL ending at its own next statement
    /// terminator (Oracle: `["CALL"]`, for `CREATE TRIGGER t ... CALL
    /// proc(:NEW.x);` — a trigger whose body is a single `CALL`, never a
    /// `BEGIN`/`END` block).
    pub body_less_markers: &'static [&'static str],
    /// The phrase that marks a compound trigger, e.g. Oracle's
    /// `Some(&["COMPOUND", "TRIGGER"])`. Once seen anywhere during a
    /// [`BlockKind::Structured`] scan, [`compound_trigger_timing_starters`](Self::compound_trigger_timing_starters)
    /// words are treated like [`subprogram_header_keywords`](Self::subprogram_header_keywords)
    /// for the rest of that statement. Before the marker is seen (or when
    /// this field is `None`), those words are ordinary content — an
    /// ordinary (non-compound) trigger's own `BEFORE INSERT ON t` timing
    /// clause must not be mistaken for a compound trigger's timing-point
    /// section header.
    pub compound_trigger_marker: Option<Phrase>,
    /// Timing-point words that introduce a compound trigger's own
    /// `<timing-point> IS ... BEGIN ... END <timing-point>;` section, once
    /// [`compound_trigger_marker`](Self::compound_trigger_marker) has been
    /// seen. Oracle: `["BEFORE", "AFTER", "INSTEAD"]` (covering `BEFORE
    /// STATEMENT`, `AFTER STATEMENT`, `BEFORE EACH ROW`, `AFTER EACH ROW`,
    /// and `INSTEAD OF EACH ROW` — only the first word is matched, the rest
    /// is skipped the same way a subprogram's name and parameter list are).
    pub compound_trigger_timing_starters: &'static [&'static str],
    /// The conditional-compilation directive prefix character, e.g. Oracle's
    /// `Some('$')` for `$IF`/`$THEN`/`$ELSIF`/`$ELSE`/`$END` and the inquiry
    /// form `$$name` (`$$PLSQL_UNIT`, …). When set, the lexer reads
    /// `<prefix>[<prefix>]<identifier>` as one [`crate::TokenKind::Directive`]
    /// token rather than an operator followed by a keyword — so, critically,
    /// `$END` never presents a bare `END` keyword token to
    /// [`crate::splitter`]'s depth tracking. `None` disables directive
    /// lexing (a lone prefix character is then an ordinary operator).
    pub directive_prefix: Option<char>,
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
    /// specific words directly against token text, regardless of whether
    /// they happen to appear here.
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
