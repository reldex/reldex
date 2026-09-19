//! Statement classification.
//!
//! The wrapper has to classify statements itself, for two reasons:
//!
//! 1. `oracledb` routes queries and non-queries through different calls
//!    (`Statement::query` vs `Statement::execute`), and its own SQL parser is
//!    private. Sending a `SELECT` down the `execute` path does not error — it
//!    silently discards the rows — so getting this wrong loses data.
//! 2. `ExecutionOutcome::statement_kind` is a contract obligation
//!    (ADR-0002 D4/S9): `db-core` never parses SQL, so the driver is the only
//!    layer that can tell it a DDL statement committed the open transaction.
//!
//! The classifier is deliberately small: it looks at the first one or two
//! keywords. It never tries to understand the statement. Anything it does not
//! recognize is [`StatementKind::Other`], which routes to the non-query path
//! and makes `db-core` treat the transaction state as unknown — the
//! conservative answer.
//!
//! # Finding the first keyword the way `oracledb` does
//!
//! Getting this wrong loses data in one direction only — a query sent down the
//! `execute` path has its rows discarded — so the rule for the **first**
//! keyword is taken from `oracledb`'s own parser rather than invented here.
//! `statement/sql_parser.rs` hands `determine_statement_type` the first maximal
//! run of **ASCII alphabetic** characters in the text, having consumed `--`
//! line comments, `/* … */` block comments (optimizer hints included) and
//! `'…'` / `"…"` quoted strings on the way. Crucially it skips **any** other
//! leading character, so `(SELECT 1 FROM dual)` — a legal top-level
//! parenthesised subquery — is a query upstream, and treating it as anything
//! else means silently dropping its rows. [`Keywords`] follows the same rule.
//!
//! Because that single decision is the one that loses data,
//! [`upstream_would_return_rows`] transcribes the upstream loop **literally**,
//! separately from the tokenizer below, and `conn::execute` refuses to run a
//! statement down the non-query path when the two disagree. Neither function is
//! derived from the other; `tests::the_classifier_and_the_upstream_rule_agree`
//! checks them against each other.

use reldex_db_driver_api::StatementKind;

/// What the classifier worked out about one statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Classification {
    /// The vendor-neutral kind reported through `ExecutionOutcome`.
    pub(crate) kind: StatementKind,
    /// Whether the statement must be sent down the row-returning path.
    pub(crate) returns_rows: bool,
    /// Whether the statement is PL/SQL, which changes how OUT binds behave.
    pub(crate) is_plsql: bool,
}

impl Classification {
    const fn new(kind: StatementKind, returns_rows: bool, is_plsql: bool) -> Self {
        Self {
            kind,
            returns_rows,
            is_plsql,
        }
    }
}

/// Classifies one statement from its leading keywords.
pub(crate) fn classify(sql: &str) -> Classification {
    let mut words = Keywords::new(sql);
    let Some(first) = words.next() else {
        return Classification::new(StatementKind::Other, false, false);
    };

    match first.as_str() {
        "SELECT" | "WITH" => Classification::new(StatementKind::Query, true, false),
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" => {
            Classification::new(StatementKind::Dml, false, false)
        }
        "BEGIN" | "DECLARE" | "CALL" => Classification::new(StatementKind::PlSqlBlock, false, true),
        "COMMIT" | "ROLLBACK" | "SAVEPOINT" => {
            Classification::new(StatementKind::TransactionControl, false, false)
        }
        // `SET TRANSACTION` and `SET CONSTRAINT(S)` are transaction control;
        // `SET ROLE` changes session state. Nothing else starts with SET.
        "SET" => match words.next().as_deref() {
            Some("TRANSACTION" | "CONSTRAINT" | "CONSTRAINTS") => {
                Classification::new(StatementKind::TransactionControl, false, false)
            }
            Some("ROLE") => Classification::new(StatementKind::SessionControl, false, false),
            _ => Classification::new(StatementKind::Other, false, false),
        },
        // `ALTER SESSION`/`ALTER SYSTEM` do **not** commit, so they must not be
        // reported as DDL: `StatementKind::Ddl` tells `db-core` the transaction
        // was resolved, and saying that about an `ALTER SESSION SET
        // NLS_DATE_FORMAT` would lose a live transaction from the user's view.
        "ALTER" => match words.next().as_deref() {
            Some("SESSION" | "SYSTEM") => {
                Classification::new(StatementKind::SessionControl, false, false)
            }
            _ => Classification::new(StatementKind::Ddl, false, false),
        },
        "CREATE" | "DROP" | "GRANT" | "REVOKE" | "TRUNCATE" | "COMMENT" | "ANALYZE" | "AUDIT"
        | "NOAUDIT" | "RENAME" | "FLASHBACK" | "PURGE" | "ASSOCIATE" | "DISASSOCIATE" => {
            Classification::new(StatementKind::Ddl, false, false)
        }
        // `EXPLAIN PLAN FOR …` writes rows into PLAN_TABLE inside the current
        // transaction and returns none. It is deliberately `Other` rather than
        // `Dml`: the driver is sure it returns no rows, but not sure enough
        // about its transaction effect to claim a category.
        _ => Classification::new(StatementKind::Other, false, false),
    }
}

/// Yields the leading keywords of a statement.
///
/// The **first** word follows `oracledb`'s own rule (see the module
/// documentation): comments and quoted strings are consumed, every other
/// non-alphabetic character is skipped, and the word is the first maximal run
/// of ASCII letters. Subsequent words are ordinary SQL identifiers, which is
/// what `ALTER SESSION` and `SET TRANSACTION` need.
struct Keywords<'a> {
    rest: &'a str,
    at_first: bool,
}

impl<'a> Keywords<'a> {
    const fn new(sql: &'a str) -> Self {
        Self {
            rest: sql,
            at_first: true,
        }
    }

    /// Advances past whitespace and comments, returning false at end of input.
    fn skip_noise(&mut self) -> bool {
        loop {
            // A byte-order mark is not Unicode whitespace, so `trim_start`
            // leaves it in place; a statement pasted from a UTF-8-with-BOM file
            // would otherwise be classified as unrecognized.
            self.rest = self.rest.trim_start_matches('\u{feff}').trim_start();
            if let Some(after) = self.rest.strip_prefix("--") {
                self.rest = after.find('\n').map_or("", |end| &after[end + 1..]);
            } else if let Some(after) = self.rest.strip_prefix("/*") {
                // An unterminated comment consumes the rest of the text; the
                // server would reject the statement anyway.
                self.rest = after.find("*/").map_or("", |end| &after[end + 2..]);
            } else {
                return !self.rest.is_empty();
            }
        }
    }

    /// The first ASCII-letter run, skipping everything `oracledb` skips.
    fn first_word(&mut self) -> Option<String> {
        loop {
            if !self.skip_noise() {
                return None;
            }
            let mut chars = self.rest.chars();
            let head = chars.next()?;
            if head.is_ascii_alphabetic() {
                let end = self
                    .rest
                    .find(|c: char| !c.is_ascii_alphabetic())
                    .unwrap_or(self.rest.len());
                let word = self.rest[..end].to_uppercase();
                self.rest = &self.rest[end..];
                return Some(word);
            }
            if head == '\'' || head == '"' {
                // A quoted string or quoted identifier before the first
                // keyword; upstream consumes it and carries on.
                let after = &self.rest[head.len_utf8()..];
                self.rest = after
                    .find(head)
                    .map_or("", |end| &after[end + head.len_utf8()..]);
            } else {
                // Anything else — `(`, a label's `<`, punctuation — is skipped
                // one character at a time, exactly as upstream does.
                self.rest = chars.as_str();
            }
        }
    }
}

impl Iterator for Keywords<'_> {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        if std::mem::take(&mut self.at_first) {
            return self.first_word();
        }
        if !self.skip_noise() {
            return None;
        }
        let end = self
            .rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$' || c == '#'))
            .unwrap_or(self.rest.len());
        if end == 0 {
            return None;
        }
        let word = self.rest[..end].to_uppercase();
        self.rest = &self.rest[end..];
        Some(word)
    }
}

/// Whether `oracledb` itself will treat this statement as row-returning.
///
/// A **literal** transcription of `statement/sql_parser.rs`'s `parse` loop and
/// `statement/mod.rs`'s `determine_statement_type`, written independently of
/// [`Keywords`] so that the two can be checked against each other and so that
/// `conn::execute` has something to assert against before it sends a statement
/// down a path that would discard its rows. Nothing else should depend on it.
pub(crate) fn upstream_would_return_rows(sql: &str) -> bool {
    let text: Vec<char> = sql.chars().collect();
    let mut position = 0_usize;
    let mut keyword_start: Option<usize> = None;
    let mut last_was_alpha = false;
    let mut last_ch = ' ';
    while position < text.len() {
        let ch = text[position];
        let is_alpha = ch.is_ascii_alphabetic();
        if is_alpha && !last_was_alpha {
            keyword_start = Some(position);
        } else if !is_alpha && last_was_alpha {
            // The first keyword is the only one `determine_statement_type` sees.
            let start = keyword_start.unwrap_or(position);
            let keyword: String = text[start..position].iter().collect();
            return matches!(keyword.to_uppercase().as_str(), "SELECT" | "WITH");
        }

        if ch == '\'' {
            // A quoted string, or a q-string when the previous character was
            // `q`/`Q`. Both end at the next occurrence of their terminator; for
            // the purpose of finding the *first* keyword the difference cannot
            // matter, because a leading `q` is itself the first keyword.
            position += 1;
            while position < text.len() && text[position] != '\'' {
                position += 1;
            }
        } else if !ch.is_whitespace() {
            if ch == '-' && last_ch == '-' {
                while position < text.len() && text[position] != '\n' {
                    position += 1;
                }
            } else if ch == '*' && last_ch == '/' {
                position += 1;
                while position + 1 < text.len()
                    && !(text[position] == '*' && text[position + 1] == '/')
                {
                    position += 1;
                }
                position = (position + 1).min(text.len());
            } else if ch == '"' {
                position += 1;
                while position < text.len() && text[position] != '"' {
                    position += 1;
                }
            }
        }

        last_was_alpha = is_alpha;
        last_ch = ch;
        position += 1;
    }
    if let Some(start) = keyword_start
        && last_was_alpha
    {
        let keyword: String = text[start..].iter().collect();
        return matches!(keyword.to_uppercase().as_str(), "SELECT" | "WITH");
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(sql: &str) -> StatementKind {
        classify(sql).kind
    }

    #[test]
    fn queries_are_routed_to_the_row_returning_path() {
        for sql in [
            "SELECT 1 FROM DUAL",
            "select * from t",
            "  \n\t SELECT 1 FROM DUAL",
            "WITH x AS (SELECT 1 c FROM DUAL) SELECT c FROM x",
            "with x as (select 1 from dual) select * from x",
        ] {
            let classification = classify(sql);
            assert_eq!(classification.kind, StatementKind::Query, "{sql}");
            assert!(classification.returns_rows, "{sql}");
            assert!(!classification.is_plsql, "{sql}");
        }
    }

    #[test]
    fn comments_and_hints_before_the_first_keyword_are_skipped() {
        for sql in [
            "-- fetch everything\nSELECT * FROM t",
            "--select is a word in this comment\nSELECT * FROM t",
            "/* block comment with the word insert */ SELECT * FROM t",
            "/*+ FIRST_ROWS(10) */ SELECT * FROM t",
            "SELECT /*+ FULL(t) */ * FROM t",
            "/* one */ -- two\n /* three */ SELECT 1 FROM DUAL",
            "\u{feff}SELECT 1 FROM DUAL",
        ] {
            assert_eq!(kind(sql), StatementKind::Query, "{sql}");
        }
        // A hint attached to the DML keyword must not change the verdict.
        assert_eq!(
            kind("/*+ APPEND */ INSERT INTO t VALUES (1)"),
            StatementKind::Dml
        );
    }

    #[test]
    fn plsql_blocks_are_recognized() {
        for sql in [
            "BEGIN NULL; END;",
            "begin p(1); end;",
            "DECLARE v NUMBER; BEGIN v := 1; END;",
            "  /* run it */ CALL p(1)",
            "-- comment\ndeclare\n  v number;\nbegin\n  null;\nend;",
        ] {
            let classification = classify(sql);
            assert_eq!(classification.kind, StatementKind::PlSqlBlock, "{sql}");
            assert!(classification.is_plsql, "{sql}");
            assert!(!classification.returns_rows, "{sql}");
        }
    }

    #[test]
    fn dml_and_ddl_are_separated_because_only_ddl_commits() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1",
            "DELETE FROM t",
            "MERGE INTO t USING s ON (t.id = s.id) WHEN MATCHED THEN UPDATE SET t.a = s.a",
        ] {
            assert_eq!(kind(sql), StatementKind::Dml, "{sql}");
            assert!(!kind(sql).commits_implicitly(), "{sql}");
        }

        for sql in [
            "CREATE TABLE t (a NUMBER)",
            "create or replace procedure p as begin null; end;",
            "DROP TABLE t",
            "TRUNCATE TABLE t",
            "GRANT SELECT ON t TO other",
            "REVOKE SELECT ON t FROM other",
            "COMMENT ON TABLE t IS 'x'",
            "ALTER TABLE t ADD (b NUMBER)",
            "RENAME t TO u",
            "PURGE RECYCLEBIN",
        ] {
            assert_eq!(kind(sql), StatementKind::Ddl, "{sql}");
            assert!(
                kind(sql).commits_implicitly(),
                "{sql} must be reported as an implicit commit"
            );
        }
    }

    #[test]
    fn alter_session_is_session_control_not_ddl() {
        // Reporting `Ddl` here would tell `db-core` the open transaction was
        // committed, which `ALTER SESSION` does not do. Losing a live
        // transaction from the user's view is exactly what `SPEC.md` §10
        // forbids.
        for sql in [
            "ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY-MM-DD'",
            "alter session set current_schema = other",
            "ALTER SYSTEM CANCEL SQL '1, 2'",
            "SET ROLE ALL",
        ] {
            assert_eq!(kind(sql), StatementKind::SessionControl, "{sql}");
            assert!(!kind(sql).commits_implicitly(), "{sql}");
        }
    }

    #[test]
    fn transaction_control_submitted_as_text_is_recognized() {
        for sql in [
            "COMMIT",
            "commit work",
            "ROLLBACK",
            "ROLLBACK TO SAVEPOINT sp1",
            "SAVEPOINT sp1",
            "SET TRANSACTION READ ONLY",
            "SET CONSTRAINTS ALL DEFERRED",
        ] {
            assert_eq!(kind(sql), StatementKind::TransactionControl, "{sql}");
        }
    }

    #[test]
    fn anything_unrecognized_is_other_and_stays_conservative() {
        for sql in [
            "",
            "   ",
            "-- only a comment\n",
            "/* only a comment */",
            "EXPLAIN PLAN FOR SELECT 1 FROM DUAL",
            "LOCK TABLE t IN EXCLUSIVE MODE",
            "๑๒๓",
        ] {
            let classification = classify(sql);
            assert_eq!(classification.kind, StatementKind::Other, "{sql:?}");
            assert!(!classification.returns_rows, "{sql:?}");
            assert!(
                classification.kind.transaction_state_is_unpredictable(),
                "{sql:?}"
            );
        }
    }

    #[test]
    fn a_parenthesised_query_is_a_query_and_keeps_its_rows() {
        // `(SELECT 1 FROM dual)` is legal top-level SQL and `oracledb` treats
        // it as a query. Classifying it as anything else sends it down
        // `Statement::execute`, which does not reject a query — it **discards
        // its rows**. The first keyword is therefore found the way upstream
        // finds it: the first run of ASCII letters, whatever precedes it.
        for sql in [
            "(SELECT 1 FROM dual)",
            "((SELECT 1 FROM dual))",
            "  (  SELECT 1 FROM dual )",
            "(select a from t) union (select b from u)",
            "/* hint */ (SELECT 1 FROM dual)",
            "(WITH x AS (SELECT 1 c FROM dual) SELECT c FROM x)",
        ] {
            let classification = classify(sql);
            assert_eq!(classification.kind, StatementKind::Query, "{sql}");
            assert!(classification.returns_rows, "{sql}");
        }
    }

    /// Every statement shape the other tests use, plus the awkward ones.
    fn corpus() -> Vec<&'static str> {
        vec![
            "",
            "   ",
            "-- only a comment\n",
            "/* only a comment */",
            "/* unterminated",
            "SELECT 1 FROM DUAL",
            "select * from t",
            "  \n\t SELECT 1 FROM DUAL",
            "\u{feff}SELECT 1 FROM DUAL",
            "WITH x AS (SELECT 1 c FROM DUAL) SELECT c FROM x",
            "(SELECT 1 FROM dual)",
            "((SELECT 1 FROM dual))",
            "(select a from t) union (select b from u)",
            "-- fetch everything\nSELECT * FROM t",
            "--select is a word in this comment\nSELECT * FROM t",
            "/* block comment with the word insert */ SELECT * FROM t",
            "/*+ FIRST_ROWS(10) */ SELECT * FROM t",
            "/* one */ -- two\n /* three */ SELECT 1 FROM DUAL",
            "/*+ APPEND */ INSERT INTO t VALUES (1)",
            "INSERT INTO t (a) SELECT a FROM s",
            "UPDATE t SET a = 1",
            "DELETE FROM t",
            "MERGE INTO t USING s ON (t.id = s.id) WHEN MATCHED THEN UPDATE SET t.a = s.a",
            "BEGIN NULL; END;",
            "DECLARE v NUMBER; BEGIN v := 1; END;",
            "  /* run it */ CALL p(1)",
            "COMMIT",
            "ROLLBACK TO SAVEPOINT sp1",
            "SAVEPOINT sp1",
            "SET TRANSACTION READ ONLY",
            "SET ROLE ALL",
            "ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY-MM-DD'",
            "ALTER TABLE t ADD (b NUMBER)",
            "CREATE TABLE t (a NUMBER)",
            "create or replace procedure p as begin null; end;",
            "DROP TABLE t",
            "TRUNCATE TABLE t",
            "EXPLAIN PLAN FOR SELECT 1 FROM DUAL",
            "LOCK TABLE t IN EXCLUSIVE MODE",
            "๑๒๓",
            "'a string' SELECT",
            "\"quoted identifier\" SELECT 1 FROM dual",
            "INSERT INTO t VALUES (q'[it's fine]')",
            "SELECT 'don''t' FROM dual",
            "SELECT",
        ]
    }

    #[test]
    fn the_classifier_and_the_upstream_rule_agree_on_every_statement() {
        // The invariant `conn::execute` asserts before it runs anything down
        // the non-query path. `upstream_would_return_rows` is a literal
        // transcription of `oracledb`'s parser; `classify` is this crate's own
        // keyword table. If they ever disagree, a result set is being dropped.
        for sql in corpus() {
            assert_eq!(
                classify(sql).returns_rows,
                upstream_would_return_rows(sql),
                "{sql:?}"
            );
        }
    }

    #[test]
    fn the_upstream_rule_finds_the_keyword_past_comments_and_punctuation() {
        assert!(upstream_would_return_rows("(SELECT 1 FROM dual)"));
        assert!(upstream_would_return_rows("/* x */ select 1 from dual"));
        assert!(upstream_would_return_rows(
            "-- x\nWITH a AS (SELECT 1 FROM dual) SELECT * FROM a"
        ));
        assert!(!upstream_would_return_rows(
            "INSERT INTO t (a) SELECT a FROM s"
        ));
        assert!(!upstream_would_return_rows(
            "-- select 1\nINSERT INTO t VALUES (1)"
        ));
        assert!(!upstream_would_return_rows("/* select */ DELETE FROM t"));
        assert!(!upstream_would_return_rows("'select' FROM t"));
        assert!(!upstream_would_return_rows(""));
        assert!(!upstream_would_return_rows("๑๒๓"));
        // A statement that is nothing but the keyword still classifies.
        assert!(upstream_would_return_rows("SELECT"));
    }

    #[test]
    fn the_first_keyword_is_not_confused_by_a_later_one() {
        // `SELECT` appearing inside a DML statement must not make it a query.
        assert_eq!(
            kind("INSERT INTO t (a) SELECT a FROM s"),
            StatementKind::Dml
        );
        assert_eq!(
            kind("CREATE TABLE t AS SELECT * FROM s"),
            StatementKind::Ddl
        );
    }
}
