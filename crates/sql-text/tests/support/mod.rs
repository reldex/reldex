//! A test-only Oracle-*shaped* dialect for `sql-text`'s own tests.
//!
//! This crate must not depend on any driver crate (`reldex-sql-text`'s job is
//! to stay usable without one), so this is a deliberate, independent copy of
//! the same descriptor `reldex_driver_oracle_thin::sql_dialect()` builds —
//! not a shortcut around the "no driver dependency" rule, but the
//! documented cost of it. If the two ever drift, `crates/drivers/oracle-thin`'s
//! own `sql_dialect::tests` module is what would catch it for that crate; this
//! copy exists so `sql-text`'s corpus can be exercised without a driver in
//! scope at all.

use reldex_sql_text::{BlockStarter, CommentRules, KeywordSlot, Phrase, QuotingRules, SqlDialect};

const OR_REPLACE: Phrase = &["OR", "REPLACE"];
const EDITIONABLE: Phrase = &["EDITIONABLE"];
const NONEDITIONABLE: Phrase = &["NONEDITIONABLE"];
const PACKAGE_BODY: Phrase = &["PACKAGE", "BODY"];
const TYPE_BODY: Phrase = &["TYPE", "BODY"];

const CREATE_SUBPROGRAM_STARTER: BlockStarter = BlockStarter {
    slots: &[
        KeywordSlot::Required(&[&["CREATE"]]),
        KeywordSlot::Optional(&[OR_REPLACE]),
        KeywordSlot::Optional(&[EDITIONABLE, NONEDITIONABLE]),
        KeywordSlot::Required(&[&["PROCEDURE"], &["FUNCTION"], &["TRIGGER"]]),
    ],
    absorbs_body_opener: true,
};

const CREATE_COMPOUND_STARTER: BlockStarter = BlockStarter {
    slots: &[
        KeywordSlot::Required(&[&["CREATE"]]),
        KeywordSlot::Optional(&[OR_REPLACE]),
        KeywordSlot::Optional(&[EDITIONABLE, NONEDITIONABLE]),
        KeywordSlot::Required(&[&["PACKAGE"], PACKAGE_BODY, &["TYPE"], TYPE_BODY]),
    ],
    absorbs_body_opener: false,
};

const DECLARE_STARTER: BlockStarter = BlockStarter {
    slots: &[KeywordSlot::Required(&[&["DECLARE"]])],
    absorbs_body_opener: true,
};

const BEGIN_STARTER: BlockStarter = BlockStarter {
    slots: &[KeywordSlot::Required(&[&["BEGIN"]])],
    absorbs_body_opener: true,
};

const BLOCK_STARTERS: &[BlockStarter] = &[
    DECLARE_STARTER,
    BEGIN_STARTER,
    CREATE_SUBPROGRAM_STARTER,
    CREATE_COMPOUND_STARTER,
];

const KEYWORDS: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "FROM",
    "WHERE",
    "INTO",
    "VALUES",
    "SET",
    "GROUP",
    "BY",
    "HAVING",
    "ORDER",
    "UNION",
    "ALL",
    "DISTINCT",
    "AS",
    "JOIN",
    "ON",
    "USING",
    "WITH",
    "AND",
    "OR",
    "NOT",
    "IS",
    "IN",
    "LIKE",
    "BETWEEN",
    "EXISTS",
    "NULL",
    "COMMIT",
    "ROLLBACK",
    "SAVEPOINT",
    "CREATE",
    "ALTER",
    "DROP",
    "TRUNCATE",
    "TABLE",
    "VIEW",
    "INDEX",
    "TRIGGER",
    "PROCEDURE",
    "FUNCTION",
    "PACKAGE",
    "BODY",
    "TYPE",
    "REPLACE",
    "EDITIONABLE",
    "NONEDITIONABLE",
    "REFERENCING",
    "NEW",
    "OLD",
    "EACH",
    "ROW",
    "BEFORE",
    "AFTER",
    "CALL",
    "DECLARE",
    "BEGIN",
    "END",
    "IF",
    "THEN",
    "ELSE",
    "ELSIF",
    "LOOP",
    "WHILE",
    "FOR",
    "EXIT",
    "RETURN",
    "EXCEPTION",
    "WHEN",
    "CASE",
    "RAISE",
    "CURSOR",
    "FETCH",
    "OPEN",
    "CLOSE",
    "EXECUTE",
    "IMMEDIATE",
    "OUT",
    "IS",
    "CONSTANT",
    "OTHERS",
    "NUMBER",
    "VARCHAR2",
    "DATE",
];

/// Oracle-*shaped* dialect: `/` terminates a block, and a block may also end
/// at its own closing `;` when no `/` follows.
pub(crate) fn oracle_like() -> SqlDialect {
    SqlDialect {
        statement_terminators: &[';'],
        slash_terminates_block: true,
        block_may_end_without_slash: true,
        block_starters: BLOCK_STARTERS,
        block_body_opener: "BEGIN",
        block_nesting_openers: &["CASE", "IF", "LOOP"],
        block_end_keyword: "END",
        quoting: QuotingRules {
            alternative_quoting: true,
            national_prefix: true,
        },
        comments: CommentRules {
            line_comment: Some("--"),
            block_comment: Some(("/*", "*/")),
            sqlplus_rem: false,
        },
        bind_variables: true,
        substitution_variables: true,
        keywords: KEYWORDS,
    }
}

/// The same dialect, but strict about `/`: a block without a trailing `/`
/// line is reported as not terminated. Used by the tests that specifically
/// exercise [`SqlDialect::block_may_end_without_slash`].
pub(crate) fn oracle_like_strict_slash() -> SqlDialect {
    SqlDialect {
        block_may_end_without_slash: false,
        ..oracle_like()
    }
}

/// The same dialect, with the SQL\*Plus `REM`/`REMARK` line comment enabled —
/// used by the one test that exercises
/// [`reldex_sql_text::CommentRules::sqlplus_rem`], which Oracle's own Phase 1
/// descriptor leaves off (out of scope per `SPEC.md` §15).
pub(crate) fn oracle_like_with_rem_comments() -> SqlDialect {
    SqlDialect {
        comments: CommentRules {
            sqlplus_rem: true,
            ..oracle_like().comments
        },
        ..oracle_like()
    }
}
