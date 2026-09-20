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

const CREATE_SUBPROGRAM_STARTER: BlockStarter = BlockStarter {
    slots: &[
        KeywordSlot::Required(&[&["CREATE"]]),
        KeywordSlot::Optional(&[OR_REPLACE]),
        KeywordSlot::Optional(&[EDITIONABLE, NONEDITIONABLE]),
        KeywordSlot::Required(&[&["PROCEDURE"], &["FUNCTION"], &["TRIGGER"]]),
    ],
    kind: BlockKind::Structured,
    opens_body: false,
};

const CREATE_COMPOUND_STARTER: BlockStarter = BlockStarter {
    slots: &[
        KeywordSlot::Required(&[&["CREATE"]]),
        KeywordSlot::Optional(&[OR_REPLACE]),
        KeywordSlot::Optional(&[EDITIONABLE, NONEDITIONABLE]),
        KeywordSlot::Required(&[&["PACKAGE"], PACKAGE_BODY, TYPE_BODY]),
    ],
    kind: BlockKind::Structured,
    opens_body: false,
};

const CREATE_TYPE_SPEC_STARTER: BlockStarter = BlockStarter {
    slots: &[
        KeywordSlot::Required(&[&["CREATE"]]),
        KeywordSlot::Optional(&[OR_REPLACE]),
        KeywordSlot::Optional(&[EDITIONABLE, NONEDITIONABLE]),
        KeywordSlot::Required(&[&["TYPE"]]),
    ],
    kind: BlockKind::ParenDelimited,
    opens_body: false,
};

const CREATE_JAVA_SOURCE_STARTER: BlockStarter = BlockStarter {
    slots: &[
        KeywordSlot::Required(&[&["CREATE"]]),
        KeywordSlot::Optional(&[OR_REPLACE]),
        KeywordSlot::Optional(&[AND_RESOLVE, AND_COMPILE]),
        KeywordSlot::Optional(&[NOFORCE]),
        KeywordSlot::Required(&[JAVA_SOURCE]),
    ],
    kind: BlockKind::OpaqueSource,
    opens_body: false,
};

const DECLARE_STARTER: BlockStarter = BlockStarter {
    slots: &[KeywordSlot::Required(&[&["DECLARE"]])],
    kind: BlockKind::Structured,
    opens_body: false,
};

const BEGIN_STARTER: BlockStarter = BlockStarter {
    slots: &[KeywordSlot::Required(&[&["BEGIN"]])],
    kind: BlockKind::Structured,
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
    "OBJECT",
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
    "INSTEAD",
    "OF",
    "COMPOUND",
    "CALL",
    "RESOLVE",
    "COMPILE",
    "NOFORCE",
    "JAVA",
    "LANGUAGE",
    "EXTERNAL",
    "MEMBER",
    "STATIC",
    "CONSTRUCTOR",
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
    "ROWTYPE",
    "SUBTYPE",
    "REF",
    "SELF",
    "RESULT",
    "RECORD",
    "UNDER",
];

/// Oracle-*shaped* dialect: `/` terminates a block or plain statement, and a
/// block may also end at its own closing `;` when no `/` follows.
pub(crate) fn oracle_like() -> SqlDialect {
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
        body_intro_exceptions: &[&["SELF", "AS"]],
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
            line_comment_words: &[],
        },
        bind_variables: true,
        substitution_variables: true,
        keywords: KEYWORDS,
    }
}

/// The same dialect, but strict about `/`: a block without a trailing `/`
/// line is reported as not terminated. Used by the tests that specifically
/// exercise [`SqlDialect::block_may_end_without_slash`].
///
/// `#[allow(dead_code)]`: this file is compiled once per test binary (each
/// of `corpus.rs`/`differential.rs` declares its own `#[path] mod support`),
/// and not every binary exercises every dialect variant this module offers.
#[allow(dead_code)]
pub(crate) fn oracle_like_strict_slash() -> SqlDialect {
    SqlDialect {
        block_may_end_without_slash: false,
        ..oracle_like()
    }
}

/// The same dialect, with the SQL\*Plus `REM`/`REMARK` line comment enabled —
/// used by the one test that exercises
/// [`reldex_sql_text::CommentRules::line_comment_words`], which Oracle's own
/// Phase 1 descriptor leaves empty (out of scope per `SPEC.md` §15).
///
/// `#[allow(dead_code)]`: see [`oracle_like_strict_slash`]'s doc comment.
#[allow(dead_code)]
pub(crate) fn oracle_like_with_rem_comments() -> SqlDialect {
    SqlDialect {
        comments: CommentRules {
            line_comment_words: &["REM", "REMARK"],
            ..oracle_like().comments
        },
        ..oracle_like()
    }
}
