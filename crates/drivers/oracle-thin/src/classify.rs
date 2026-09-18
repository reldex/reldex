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
//! keywords after skipping leading whitespace, `--` line comments, `/* … */`
//! block comments and optimizer hints (`/*+ … */`). It never tries to
//! understand the statement. Anything it does not recognize is
//! [`StatementKind::Other`], which routes to the non-query path and makes
//! `db-core` treat the transaction state as unknown — the conservative answer.
//!
//! It agrees with `oracledb`'s own private parser on the keywords that drive
//! bind handling (`DECLARE`/`BEGIN`/`CALL` are PL/SQL, `SELECT`/`WITH` are
//! queries), which matters because the upstream parser decides whether binds
//! are deduplicated by name.

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

/// Yields the leading identifier-like words of a statement, skipping
/// whitespace, `--` comments and `/* … */` comments (including `/*+ hints */`).
struct Keywords<'a> {
    rest: &'a str,
}

impl<'a> Keywords<'a> {
    const fn new(sql: &'a str) -> Self {
        Self { rest: sql }
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
}

impl Iterator for Keywords<'_> {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        if !self.skip_noise() {
            return None;
        }
        let end = self
            .rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$' || c == '#'))
            .unwrap_or(self.rest.len());
        if end == 0 {
            // A statement starting with punctuation (`(SELECT …)`, a label) is
            // not something this classifier claims to understand.
            return None;
        }
        let word = self.rest[..end].to_uppercase();
        self.rest = &self.rest[end..];
        Some(word)
    }
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
            "(SELECT 1 FROM DUAL)",
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
