//! Oracle's [`SqlDialect`] descriptor (M2.4), consumed by `reldex-sql-text`.
//!
//! Everything here is plain data: no logic, no I/O, nothing that could
//! itself be tested independently of `reldex-sql-text`'s lexer and splitter
//! (which the shared corpus in that crate's `tests/` exercises against a
//! dialect built the same way as this one — see that crate's `tests/support`
//! for why it is not literally *this* function, and the crate-level "Why
//! `SqlDialect` lives in `reldex-sql-text`" doc on `reldex_sql_text::dialect`
//! for the dependency-direction reasoning).
//!
//! # Overlap with [`crate::rewrite`]
//!
//! `rewrite.rs`'s `classify::Keywords` scanner and this descriptor answer
//! related but distinct questions and were kept separate rather than merged,
//! for now:
//!
//! - `Keywords` exists to reproduce **`oracledb`'s own** first-keyword rule
//!   byte-for-byte (`classify.rs`'s module doc: "the question ... must be
//!   answered by upstream's own rules, not by an approximation of them").
//!   `rewrite::Rewritable::detect` reuses it for exactly that reason: trigger
//!   detection has to agree with the classifier that decides whether a
//!   statement is routed down the row-returning path, and both ultimately
//!   answer "what would upstream's parser do", not "what does Oracle's SQL
//!   grammar say".
//! - This descriptor's `block_starters` recognizes the **same** `CREATE
//!   [OR REPLACE] [EDITIONABLE|NONEDITIONABLE] TRIGGER` shape (among others)
//!   for a different purpose: finding a statement's boundary in a script,
//!   which has nothing to do with `oracledb`'s bind-scan quirk.
//! - `rewrite::strip_sqlplus_terminator`/`strip_statement_terminator` also
//!   duplicate a piece of `reldex-sql-text`'s own job — recognizing a lone
//!   `/` line and recognizing where a PL/SQL block's terminator is real vs.
//!   client punctuation — because `rewrite.rs` needed exactly that logic
//!   before this crate existed, on a single statement already isolated by
//!   the caller.
//!
//! Once `db-core` drives script execution through `reldex-sql-text`
//! (`SPEC.md` §15, M4.3), `rewrite.rs` will receive statements whose
//! terminator has already been classified by the splitter, and at that point
//! `strip_sqlplus_terminator`/`strip_statement_terminator`/`closes_a_plsql_block`
//! become candidates to delete in favor of the splitter's own
//! `StatementSpan`. That refactor is deliberately **not** done in this task
//! (`rewrite.rs` is explicitly out of scope for M2.4): today `rewrite::plan`
//! is called with a single statement's text and no positional context, so it
//! still needs its own answer to "is there a trailing `/`" independent of
//! whatever produced that text.
//!
//! `Keywords` is not reused *by* this descriptor either, in the other
//! direction: it returns owned, upper-cased `String`s and stops at the first
//! non-matching statement shape it wasn't built to describe (`ALTER SESSION`,
//! `SET TRANSACTION`, …), because it is a bind-scan/classification helper,
//! not a general keyword tokenizer. `reldex_sql_text`'s own lexer is the
//! general one; nothing here needs `Keywords`' specific behavior.

use reldex_sql_text::{
    BlockKind, BlockStarter, CommentRules, KeywordSlot, Phrase, QuotingRules, SqlDialect,
};

const OR_REPLACE: Phrase = &["OR", "REPLACE"];
const EDITIONABLE: Phrase = &["EDITIONABLE"];
const NONEDITIONABLE: Phrase = &["NONEDITIONABLE"];
const PACKAGE_BODY: Phrase = &["PACKAGE", "BODY"];
const TYPE_BODY: Phrase = &["TYPE", "BODY"];
const AND_RESOLVE: Phrase = &["AND", "RESOLVE"];
const AND_COMPILE: Phrase = &["AND", "COMPILE"];
const NOFORCE: Phrase = &["NOFORCE"];
const JAVA_SOURCE: Phrase = &["JAVA", "SOURCE"];

/// `CREATE [OR REPLACE] [EDITIONABLE|NONEDITIONABLE] {PROCEDURE|FUNCTION|TRIGGER}`:
/// a single subprogram or trigger. [`BlockKind::Structured`]'s
/// `pending_bodies` model treats a lone body, a sequence of package/type
/// members, and any nested subprogram declarations inside a declare section
/// uniformly — see `reldex_sql_text::splitter`'s module docs.
const CREATE_SUBPROGRAM_STARTER: BlockStarter = BlockStarter {
    slots: &[
        KeywordSlot::Required(&[&["CREATE"]]),
        KeywordSlot::Optional(&[OR_REPLACE]),
        KeywordSlot::Optional(&[EDITIONABLE, NONEDITIONABLE]),
        KeywordSlot::Required(&[&["PROCEDURE"], &["FUNCTION"], &["TRIGGER"]]),
    ],
    kind: BlockKind::Structured,
    // The subprogram's/trigger's own body still lies ahead (its `BEGIN`, or
    // `CALL` for a body-less trigger) — this starter consumes only the
    // introducing DDL keywords.
    opens_body: false,
};

/// `CREATE [OR REPLACE] [EDITIONABLE|NONEDITIONABLE] {PACKAGE[ BODY]|TYPE BODY}`:
/// a sequence of independent members, each of which may carry its own
/// complete `BEGIN ... END`, plus an optional initialization section.
/// `CREATE TYPE` *without* `BODY` is a separate, [`BlockKind::ParenDelimited`]
/// starter below — an object/varray/table type spec has no `BEGIN`/`END` at
/// all.
const CREATE_COMPOUND_STARTER: BlockStarter = BlockStarter {
    slots: &[
        KeywordSlot::Required(&[&["CREATE"]]),
        KeywordSlot::Optional(&[OR_REPLACE]),
        KeywordSlot::Optional(&[EDITIONABLE, NONEDITIONABLE]),
        KeywordSlot::Required(&[&["PACKAGE"], PACKAGE_BODY, TYPE_BODY]),
    ],
    kind: BlockKind::Structured,
    // The container's optional initialization section (its own `BEGIN`, if
    // any) still lies ahead, after any number of members — this starter
    // consumes only the introducing DDL keywords.
    opens_body: false,
};

/// `CREATE [OR REPLACE] [EDITIONABLE|NONEDITIONABLE] TYPE` (without `BODY`):
/// an object/varray/table-of spec, or the simple `IS <type>` synonym form —
/// neither has `BEGIN`/`END`. See
/// [`BlockKind::ParenDelimited`](reldex_sql_text::dialect::BlockKind::ParenDelimited).
const CREATE_TYPE_SPEC_STARTER: BlockStarter = BlockStarter {
    slots: &[
        KeywordSlot::Required(&[&["CREATE"]]),
        KeywordSlot::Optional(&[OR_REPLACE]),
        KeywordSlot::Optional(&[EDITIONABLE, NONEDITIONABLE]),
        KeywordSlot::Required(&[&["TYPE"]]),
    ],
    kind: BlockKind::ParenDelimited,
    // `opens_body` is meaningless for `BlockKind::ParenDelimited` (there is
    // no `BEGIN`/`END` body-tracking scan to seed at all); `false` is inert.
    opens_body: false,
};

/// `CREATE [OR REPLACE] [AND RESOLVE|AND COMPILE] [NOFORCE] JAVA SOURCE ...`:
/// the body is Java source text, not PL/SQL, and may itself contain `;` —
/// only a lone `/` line or end of input ends it. `CREATE LIBRARY` is **not**
/// included: a `LIBRARY` declaration names an OS shared object and has no
/// body at all, so it never needs block-terminator handling in the first
/// place (it is an ordinary plain statement, correctly handled without being
/// a block starter). `JAVA SOURCE` is the one that *does* need it, and is
/// the reason this starter exists.
const CREATE_JAVA_SOURCE_STARTER: BlockStarter = BlockStarter {
    slots: &[
        KeywordSlot::Required(&[&["CREATE"]]),
        KeywordSlot::Optional(&[OR_REPLACE]),
        KeywordSlot::Optional(&[AND_RESOLVE, AND_COMPILE]),
        KeywordSlot::Optional(&[NOFORCE]),
        KeywordSlot::Required(&[JAVA_SOURCE]),
    ],
    kind: BlockKind::OpaqueSource,
    // `opens_body` is meaningless for `BlockKind::OpaqueSource` (no
    // `BEGIN`/`END` tracking at all); `false` is inert.
    opens_body: false,
};

const DECLARE_STARTER: BlockStarter = BlockStarter {
    slots: &[KeywordSlot::Required(&[&["DECLARE"]])],
    kind: BlockKind::Structured,
    // A `DECLARE` block's own `BEGIN` still lies ahead, after the declare
    // section — this starter consumes only the `DECLARE` keyword itself.
    opens_body: false,
};

const BEGIN_STARTER: BlockStarter = BlockStarter {
    slots: &[KeywordSlot::Required(&[&["BEGIN"]])],
    kind: BlockKind::Structured,
    // Matching this starter already consumed the block's own `BEGIN`, so
    // the *next* `BEGIN` the scan finds is unambiguously a nested block —
    // see `BlockStarter::opens_body`'s own documentation for the
    // adversarial-review defect this field fixes.
    opens_body: true,
};

const BLOCK_STARTERS: &[BlockStarter] = &[
    DECLARE_STARTER,
    BEGIN_STARTER,
    CREATE_JAVA_SOURCE_STARTER,
    CREATE_SUBPROGRAM_STARTER,
    CREATE_COMPOUND_STARTER,
    CREATE_TYPE_SPEC_STARTER,
];

/// Not exhaustive of Oracle's reserved-word list — only
/// [`TokenKind::Keyword`](reldex_sql_text::TokenKind::Keyword) vs.
/// [`TokenKind::Identifier`](reldex_sql_text::TokenKind::Identifier)
/// (highlighting) depends on it. Nothing in `reldex_sql_text::splitter`
/// consults this list: block-boundary detection matches specific words
/// (`BEGIN`, `END`, `CASE`, `IF`, `LOOP`, and the words inside
/// [`BLOCK_STARTERS`]) directly, so an omission here cannot mis-split a
/// script — at most it colors one more word as a plain identifier.
#[rustfmt::skip]
const KEYWORDS: &[&str] = &[
    // DML / query
    "SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "FROM", "WHERE", "INTO",
    "VALUES", "SET", "GROUP", "BY", "HAVING", "ORDER", "UNION", "INTERSECT",
    "MINUS", "ALL", "DISTINCT", "AS", "JOIN", "INNER", "OUTER", "LEFT",
    "RIGHT", "FULL", "CROSS", "ON", "USING", "WITH", "CONNECT", "START",
    "PRIOR", "RETURNING", "FOR", "UPDATE", "LOCK",
    // predicates / operators
    "AND", "OR", "NOT", "IS", "IN", "LIKE", "BETWEEN", "EXISTS", "NULL",
    "ANY", "SOME",
    // transaction control
    "COMMIT", "ROLLBACK", "SAVEPOINT", "WORK", "TO",
    // DDL
    "CREATE", "ALTER", "DROP", "TRUNCATE", "COMMENT", "RENAME", "GRANT",
    "REVOKE", "ANALYZE", "AUDIT", "NOAUDIT", "FLASHBACK", "PURGE",
    "ASSOCIATE", "DISASSOCIATE", "TABLE", "VIEW", "INDEX", "SEQUENCE",
    "SYNONYM", "TRIGGER", "PROCEDURE", "FUNCTION", "PACKAGE", "BODY", "TYPE",
    "TABLESPACE", "MATERIALIZED", "CONSTRAINT", "PRIMARY", "KEY", "FOREIGN",
    "REFERENCES", "CHECK", "DEFAULT", "UNIQUE", "REPLACE", "EDITIONABLE",
    "NONEDITIONABLE", "OR", "REFERENCING", "NEW", "OLD", "EACH", "ROW",
    "COMPOUND", "INSTEAD", "OF", "BEFORE", "AFTER", "CALL", "RESOLVE",
    "COMPILE", "NOFORCE", "MEMBER", "STATIC", "CONSTRUCTOR", "UNDER",
    // PL/SQL
    "DECLARE", "BEGIN", "END", "IF", "THEN", "ELSE", "ELSIF", "LOOP",
    "WHILE", "EXIT", "CONTINUE", "RETURN", "EXCEPTION", "WHEN", "CASE",
    "RAISE", "GOTO", "CURSOR", "FETCH", "OPEN", "CLOSE", "BULK", "COLLECT",
    "LIMIT", "PRAGMA", "AUTONOMOUS_TRANSACTION", "EXECUTE", "IMMEDIATE",
    "USING", "OUT", "IN", "NOCOPY", "IS", "AS", "CONSTANT", "OTHERS",
    "RAISE_APPLICATION_ERROR", "RESULT_CACHE", "DETERMINISTIC", "PIPELINED",
    "AUTHID", "DEFINER", "CURRENT_USER", "LANGUAGE", "JAVA", "LIBRARY",
    "EXTERNAL", "ROWTYPE", "SUBTYPE", "REF", "SELF", "RESULT",
    // types
    "NUMBER", "VARCHAR2", "CHAR", "NCHAR", "NVARCHAR2", "DATE", "TIMESTAMP",
    "INTERVAL", "CLOB", "NCLOB", "BLOB", "RAW", "LONG", "ROWID", "UROWID",
    "BOOLEAN", "PLS_INTEGER", "BINARY_INTEGER", "VARRAY", "OBJECT", "RECORD",
    "JSON", "XMLTYPE",
    // session / system
    "SESSION", "SYSTEM", "ROLE", "TRANSACTION", "CONSTRAINTS", "PARTITION",
    "SUBPARTITION", "STORAGE",
];

/// Oracle's [`SqlDialect`] descriptor for `reldex-sql-text` (M2.4).
///
/// `block_may_end_without_slash` is `true`: Reldex is a desktop editor, not
/// only a SQL\*Plus terminal, and a user who selects a whole script and runs
/// it (`SPEC.md` §15's "run script") reasonably expects a block ending in a
/// bare `END;` with no `/` to still count as complete, not as an incomplete
/// buffer waiting for one more line. The strict "no `/`, not terminated"
/// reading SQL\*Plus itself uses is available (flip the field) if that
/// forgiveness turns out to be wrong once M4.3 wires this up to real
/// execution.
#[must_use]
pub const fn sql_dialect() -> SqlDialect {
    SqlDialect {
        statement_terminators: &[';'],
        slash_terminates_block: true,
        slash_terminates_plain: true,
        block_may_end_without_slash: true,
        block_starters: BLOCK_STARTERS,
        label_delimiters: Some(("<<", ">>")),
        block_body_opener: "BEGIN",
        block_nesting_openers: &["CASE", "IF", "LOOP"],
        block_end_keyword: "END",
        subprogram_header_keywords: &["PROCEDURE", "FUNCTION"],
        body_intro_keywords: &["IS", "AS"],
        // A constructor method's `RETURN SELF AS RESULT IS ...`: the `AS` in
        // `SELF AS RESULT` is not a body intro (the trailing `IS` is).
        body_intro_exceptions: &[&["SELF", "AS"]],
        // Full phrases, not single words — see `call_spec_phrases`'s own
        // documentation for why a lone `LANGUAGE`/`EXTERNAL` word must never
        // be enough (a variable, constant, or cursor may legally be named
        // either). The legacy `EXTERNAL` call-spec is always written with a
        // following `LIBRARY` or `NAME`.
        call_spec_phrases: &[
            &["LANGUAGE", "JAVA"],
            &["LANGUAGE", "C"],
            &["LANGUAGE", "JAVASCRIPT"],
            &["EXTERNAL", "LIBRARY"],
            &["EXTERNAL", "NAME"],
        ],
        body_less_markers: &["CALL"],
        sectioned_body_marker: Some(&["COMPOUND", "TRIGGER"]),
        section_header_starters: &["BEFORE", "AFTER", "INSTEAD"],
        directive_prefix: Some('$'),
        quoting: QuotingRules {
            alternative_quote_prefixes: &["Q", "NQ"],
            national_string_prefixes: &["N"],
        },
        comments: CommentRules {
            line_comment: Some("--"),
            block_comment: Some(("/*", "*/")),
            // SQL*Plus client commands are out of scope for Phase 1
            // (`SPEC.md` §15: "Future SQL*Plus-like commands may be added
            // progressively"); leaving this empty means `REM ...` lines are
            // lexed as ordinary statement text today rather than silently
            // misread. `reldex_sql_text::lexer` implements and tests the
            // non-empty behavior for when that work is scheduled.
            line_comment_words: &[],
        },
        bind_variables: true,
        substitution_variables: true,
        keywords: KEYWORDS,
    }
}

#[cfg(test)]
mod tests {
    use super::sql_dialect;
    use reldex_sql_text::{StatementKind, split_statements};

    #[test]
    fn a_simple_script_splits_into_the_expected_statement_kinds() {
        let dialect = sql_dialect();
        let script = "SELECT 1 FROM dual;\n\
                       CREATE OR REPLACE PROCEDURE p AS BEGIN NULL; END;\n\
                       /\n\
                       COMMIT;\n";
        let spans = split_statements(script, &dialect);
        let kinds: Vec<_> = spans.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            [
                StatementKind::Plain,
                StatementKind::Block,
                StatementKind::Plain
            ]
        );
        assert!(spans.iter().all(|s| s.terminated), "{spans:?}");
        assert_eq!(
            spans[1].full(script).trim_end(),
            "CREATE OR REPLACE PROCEDURE p AS BEGIN NULL; END;\n/"
        );
    }

    #[test]
    fn the_trigger_rewrite_shape_is_recognized_as_a_block_starter() {
        let dialect = sql_dialect();
        let script =
            "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN :NEW.a := 1; END;\n/\n";
        let spans = split_statements(script, &dialect);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].kind, StatementKind::Block);
        assert!(spans[0].terminated);
    }
}
