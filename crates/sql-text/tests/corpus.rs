//! Corpus-driven tests: real-shaped Oracle scripts (`tests/corpus/*.sql`)
//! checked against the crate's two public invariants —
//!
//! 1. tokenizing a document one `QSyntaxHighlighter`-block at a time, with
//!    the returned [`LexState`] carried forward, produces exactly the same
//!    tokens as [`tokenize`]ing the whole document at once;
//! 2. a statement span's [`StatementSpan::full`] ranges, concatenated with
//!    the gaps between them, reproduce the input exactly —
//!
//! plus the specific scenarios `docs/exec-plans/active/phase-1.md` M2.4
//! calls for by name (nested blocks, every `q'...'` delimiter form, `/`,
//! comments containing terminators, Thai text) and the edge cases named in
//! this task's brief (empty statements, no terminator, CRLF, a UTF-8 BOM, a
//! 5&nbsp;MB script).

use reldex_sql_text::{
    LexState, SqlDialect, StatementKind, StatementSpan, Token, TokenKind, split_statements,
    statement_at, tokenize, tokenize_block,
};

#[path = "support/mod.rs"]
mod support;

const CORPUS_FILES: &[(&str, &str)] = &[
    ("package", include_str!("corpus/package.sql")),
    ("trigger", include_str!("corpus/trigger.sql")),
    ("thai", include_str!("corpus/thai.sql")),
    ("qquotes", include_str!("corpus/qquotes.sql")),
    ("no_slash", include_str!("corpus/no_slash.sql")),
];

// --------------------------------------------------------------- helpers

/// Tokenizes `text` one `\n`-separated block at a time, carrying
/// [`LexState`] the way a `QSyntaxHighlighter` would, and applies the same
/// "still open at true end of input becomes `Error`" rule [`tokenize`]
/// documents. This exercises the *public* `tokenize_block` contract
/// end to end, independently of how [`tokenize`] happens to be implemented.
fn reconstruct_line_by_line(text: &str, dialect: &SqlDialect) -> Vec<Token> {
    let mut state = LexState::INITIAL;
    let mut pos = 0usize;
    let mut tokens = Vec::new();
    let mut lines = text.split('\n').peekable();

    while let Some(line) = lines.next() {
        let (mut line_tokens, next_state) = tokenize_block(line, state, dialect);
        for token in &mut line_tokens {
            token.start += pos;
            token.end += pos;
        }
        tokens.append(&mut line_tokens);
        pos += line.len();
        state = next_state;
        if lines.peek().is_some() {
            tokens.push(Token {
                kind: TokenKind::Whitespace,
                start: pos,
                end: pos + 1,
            });
            pos += 1;
        }
    }
    if !state.is_initial()
        && let Some(last) = tokens.last_mut()
    {
        last.kind = TokenKind::Error;
    }
    tokens
}

fn assert_full_coverage(text: &str, tokens: &[Token]) {
    let mut pos = 0usize;
    for token in tokens {
        assert_eq!(
            token.start, pos,
            "gap or overlap before {token:?} in {text:?}"
        );
        assert!(
            token.end <= text.len(),
            "{token:?} runs past end of {text:?}"
        );
        pos = token.end;
    }
    assert_eq!(pos, text.len(), "tokens do not cover all of {text:?}");
}

fn assert_line_by_line_matches_whole_document(text: &str, dialect: &SqlDialect) {
    let whole = tokenize(text, dialect);
    assert_full_coverage(text, &whole);
    let piecewise = reconstruct_line_by_line(text, dialect);
    assert_eq!(
        whole, piecewise,
        "line-by-line tokenization (carried state) must equal whole-document tokenization for {text:?}"
    );
}

fn assert_spans_reproduce_text(text: &str, spans: &[StatementSpan]) {
    let mut pos = 0usize;
    let mut rebuilt = String::with_capacity(text.len());
    for span in spans {
        assert!(
            span.content_start >= pos,
            "statement span went backwards: {span:?}"
        );
        assert!(span.content_end <= span.full_end);
        assert!(span.full_end <= text.len());
        rebuilt.push_str(&text[pos..span.full_end]);
        pos = span.full_end;
    }
    rebuilt.push_str(&text[pos..]);
    assert_eq!(
        rebuilt, text,
        "spans + gaps must reproduce the input exactly"
    );
}

fn kinds(spans: &[StatementSpan]) -> Vec<StatementKind> {
    spans.iter().map(|s| s.kind).collect()
}

// ---------------------------------------------------------- whole corpus

#[test]
fn every_corpus_file_satisfies_both_invariants_with_lf_endings() {
    let dialect = support::oracle_like();
    for (name, text) in CORPUS_FILES {
        assert_line_by_line_matches_whole_document(text, &dialect);
        let spans = split_statements(text, &dialect);
        assert!(!spans.is_empty(), "{name} produced no statements");
        assert_spans_reproduce_text(text, &spans);
    }
}

#[test]
fn every_corpus_file_satisfies_both_invariants_with_crlf_endings() {
    let dialect = support::oracle_like();
    for (name, text) in CORPUS_FILES {
        let crlf = text.replace('\n', "\r\n");
        assert_line_by_line_matches_whole_document(&crlf, &dialect);
        let spans = split_statements(&crlf, &dialect);
        assert!(!spans.is_empty(), "{name} (CRLF) produced no statements");
        assert_spans_reproduce_text(&crlf, &spans);

        // CRLF must not change *what* was found, only where the bytes are.
        let lf_spans = split_statements(text, &dialect);
        assert_eq!(
            kinds(&spans),
            kinds(&lf_spans),
            "{name}: CRLF changed the statement kinds found"
        );
        assert_eq!(
            spans.iter().map(|s| s.terminated).collect::<Vec<_>>(),
            lf_spans.iter().map(|s| s.terminated).collect::<Vec<_>>(),
            "{name}: CRLF changed termination"
        );
    }
}

// --------------------------------------------------------- named corpora

#[test]
fn package_spec_and_body_are_two_block_statements() {
    let dialect = support::oracle_like();
    let spans = split_statements(include_str!("corpus/package.sql"), &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Block]);
    assert!(spans.iter().all(|s| s.terminated), "{spans:?}");
}

#[test]
fn nested_begin_case_if_loop_end_do_not_end_the_block_early() {
    // A regression guard for the specific nesting shapes `package.sql`
    // exercises: `FOR ... LOOP`, `IF ... END IF`, `CASE ... END CASE`, and a
    // nested anonymous `BEGIN ... END;`, all inside one outer block. If any
    // of those matched the outer block's own `END`, this would split into
    // far more than two statements.
    let dialect = support::oracle_like();
    let text = include_str!("corpus/package.sql");
    let spans = split_statements(text, &dialect);
    assert_eq!(spans.len(), 2, "{spans:?}");
    let body = spans[1].full(text);
    assert!(body.trim_end().ends_with("END pkg_demo;\n/"), "{body}");
}

#[test]
fn a_comment_containing_a_terminator_does_not_split_the_statement() {
    let dialect = support::oracle_like();
    let text = include_str!("corpus/package.sql");
    // The line comment "-- a comment that mentions a terminator: END; ..."
    // and the block comment "/* ... a semicolon; right here */" must not be
    // read as real terminators.
    assert!(text.contains("-- a comment that mentions a terminator"));
    assert!(text.contains("/* block comment with a semicolon; right here */"));
    let spans = split_statements(text, &dialect);
    assert_eq!(spans.len(), 2, "{spans:?}");
}

#[test]
fn the_trigger_rewrite_shape_is_one_terminated_block() {
    let dialect = support::oracle_like();
    let text = include_str!("corpus/trigger.sql");
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block]);
    assert!(spans[0].terminated);
    assert!(spans[0].full(text).contains(":NEW.amount"));
}

#[test]
fn thai_text_in_strings_comments_and_quoted_identifiers_keeps_byte_offsets_correct() {
    let dialect = support::oracle_like();
    let text = include_str!("corpus/thai.sql");
    let spans = split_statements(text, &dialect);
    assert_eq!(
        kinds(&spans),
        [
            StatementKind::Plain,
            StatementKind::Plain,
            StatementKind::Block
        ]
    );
    assert!(spans.iter().all(|s| s.terminated), "{spans:?}");

    // The `;` inside the Thai string literal must not have split statement 2.
    let insert = spans[1].full(text);
    assert!(insert.contains("สวัสดีครับ; นี่คือข้อความ"), "{insert}");

    // Every span's content must be valid UTF-8 (guaranteed by slicing at the
    // token boundaries this crate itself produced) and start/end on a char
    // boundary — `&text[range]` above would already have panicked otherwise,
    // so reaching this point is itself the assertion.
    for span in &spans {
        let _ = span.content(text);
    }
}

#[test]
fn every_q_quote_delimiter_form_lexes_as_one_string_token() {
    let dialect = support::oracle_like();
    let text = include_str!("corpus/qquotes.sql");
    let tokens = tokenize(text, &dialect);
    assert_full_coverage(text, &tokens);

    let strings: Vec<&str> = tokens
        .iter()
        .filter(|t| t.kind == TokenKind::String)
        .map(|t| &text[t.start..t.end])
        .collect();
    assert_eq!(
        strings,
        [
            "q'[bracket ] form]'",
            "q'{brace } form}'",
            "q'<angle > form>'",
            "q'(paren ) form)'",
            "q'!bang ! form!'",
            "nq'[national bracket]'",
            "Q'#hash # form#'",
        ],
        "every alternative-quote delimiter form must lex as exactly one String token"
    );

    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block]);
    assert!(spans[0].terminated);
}

#[test]
fn a_block_without_a_trailing_slash_still_terminates_when_the_dialect_allows_it() {
    let dialect = support::oracle_like();
    let text = include_str!("corpus/no_slash.sql");
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans.iter().all(|s| s.terminated), "{spans:?}");
    assert!(spans[0].full(text).trim_end().ends_with("END;"));
}

#[test]
fn the_strict_dialect_reports_the_same_slash_less_block_as_not_terminated() {
    let dialect = support::oracle_like_strict_slash();
    let text = include_str!("corpus/no_slash.sql");
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(
        !spans[0].terminated,
        "strict dialect: no `/` means not sent"
    );
    assert!(
        spans[1].terminated,
        "the following plain statement is unaffected"
    );
}

// -------------------------------------------------------------- both / forms

#[test]
fn both_the_slash_and_no_slash_forms_of_the_same_block_are_recognized() {
    let dialect = support::oracle_like();
    let with_slash = "BEGIN\n  NULL;\nEND;\n/\n";
    let without_slash = "BEGIN\n  NULL;\nEND;\n";

    let a = split_statements(with_slash, &dialect);
    assert_eq!(a.len(), 1);
    assert!(a[0].terminated);
    // The terminator span includes the `/` line's own trailing newline.
    assert_eq!(a[0].full(with_slash), "BEGIN\n  NULL;\nEND;\n/\n");

    let b = split_statements(without_slash, &dialect);
    assert_eq!(b.len(), 1);
    assert!(b[0].terminated);
    assert_eq!(b[0].full(without_slash), "BEGIN\n  NULL;\nEND;");
}

// ------------------------------------------------------------- edge cases

#[test]
fn empty_statements_between_two_terminators_are_reported_as_zero_length() {
    let dialect = support::oracle_like();
    let text = "SELECT 1 FROM dual;;SELECT 2 FROM dual;";
    let spans = split_statements(text, &dialect);
    assert_eq!(spans.len(), 3, "{spans:?}");
    assert_eq!(
        spans[1].content_start, spans[1].content_end,
        "the middle statement is empty"
    );
    assert!(spans.iter().all(|s| s.terminated));
    assert_spans_reproduce_text(text, &spans);
}

#[test]
fn trailing_text_without_a_terminator_is_reported_as_not_terminated() {
    let dialect = support::oracle_like();
    let text = "SELECT 1 FROM dual";
    let spans = split_statements(text, &dialect);
    assert_eq!(spans.len(), 1);
    assert!(!spans[0].terminated);
    assert_eq!(spans[0].full_end, text.len());
    assert_eq!(spans[0].content(text), text);
}

#[test]
fn a_truncated_block_with_no_end_at_all_runs_to_end_of_input() {
    let dialect = support::oracle_like();
    let text = "BEGIN\n  NULL;\n-- the script was cut off before END";
    let spans = split_statements(text, &dialect);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].kind, StatementKind::Block);
    assert!(!spans[0].terminated);
    assert_eq!(spans[0].full_end, text.len());
}

#[test]
fn a_utf8_bom_at_the_start_of_the_script_does_not_defeat_splitting() {
    let dialect = support::oracle_like();
    let text = "\u{feff}SELECT 1 FROM dual;\n";
    let tokens = tokenize(text, &dialect);
    assert_full_coverage(text, &tokens);
    assert_eq!(tokens[0].kind, TokenKind::Whitespace);
    assert_eq!(&text[tokens[0].start..tokens[0].end], "\u{feff}");

    let spans = split_statements(text, &dialect);
    assert_eq!(spans.len(), 1);
    assert!(spans[0].terminated);
    assert_eq!(spans[0].content(text), "SELECT 1 FROM dual");
}

#[test]
fn a_5mb_script_of_many_statements_is_split_and_tokenized_correctly() {
    let dialect = support::oracle_like();
    const STATEMENT: &str = "INSERT INTO t (a, b) VALUES (1, 'hello there ๑๒๓ text'); -- row\n";
    let repeats = 5 * 1024 * 1024 / STATEMENT.len();
    let text = STATEMENT.repeat(repeats);
    assert!(
        text.len() > 4 * 1024 * 1024,
        "sanity: the script should be multiple MB"
    );

    let tokens = tokenize(&text, &dialect);
    assert_full_coverage(&text, &tokens);

    let spans = split_statements(&text, &dialect);
    assert_eq!(
        spans.len(),
        repeats,
        "expected exactly one statement per repeat"
    );
    assert!(spans.iter().all(|s| s.kind == StatementKind::Plain));
    assert!(spans.iter().all(|s| s.terminated));
    assert_spans_reproduce_text(&text, &spans);
}

#[test]
fn rem_comments_are_available_but_off_by_default_in_oracles_phase_1_descriptor() {
    let text = "REM this line is a comment when enabled\nSELECT 1 FROM dual;\n";

    let off = support::oracle_like();
    let spans_off = split_statements(text, &off);
    // With REM disabled, "REM this line ..." is ordinary (nonsensical, but
    // harmless) statement text: nothing ends it until the `;` after "dual",
    // so it and the real `SELECT` are read as *one* plain statement.
    assert_eq!(spans_off.len(), 1, "{spans_off:?}");
    assert_eq!(spans_off[0].kind, StatementKind::Plain);
    assert!(spans_off[0].terminated);
    assert_eq!(
        spans_off[0].content(text),
        "REM this line is a comment when enabled\nSELECT 1 FROM dual"
    );

    let on = support::oracle_like_with_rem_comments();
    let tokens_on = tokenize(text, &on);
    assert_eq!(tokens_on[0].kind, TokenKind::Comment);
    let spans_on = split_statements(text, &on);
    assert_eq!(spans_on.len(), 1, "{spans_on:?}");
    assert_eq!(spans_on[0].content(text), "SELECT 1 FROM dual");
}

// ----------------------------------------------------- statement_at rules

#[test]
fn statement_at_finds_the_statement_the_cursor_touches() {
    let dialect = support::oracle_like();
    let text = "SELECT 1 FROM dual;\nSELECT 2 FROM dual;\n";
    let second_start = text
        .find("SELECT 2")
        .expect("test text contains \"SELECT 2\"");

    // Inside the first statement.
    let hit = statement_at(text, 3, &dialect).expect("inside statement 1");
    assert_eq!(hit.content(text), "SELECT 1 FROM dual");

    // Exactly on the cursor touching the second statement's first character.
    let hit = statement_at(text, second_start, &dialect).expect("start of statement 2");
    assert_eq!(hit.content(text), "SELECT 2 FROM dual");

    // Right after a statement's terminator still counts as touching it.
    let after_first_semicolon = text.find(';').expect("test text contains a semicolon") + 1;
    let hit = statement_at(text, after_first_semicolon, &dialect).expect("just after terminator");
    assert_eq!(hit.content(text), "SELECT 1 FROM dual");
}

#[test]
fn statement_at_falls_back_to_the_preceding_statement_on_the_same_line() {
    let dialect = support::oracle_like();
    let text = "SELECT 1 FROM dual;   \nSELECT 2 FROM dual;\n";
    // In the trailing spaces after the first statement's `;`, same line.
    let offset = text.find("   ").expect("test text contains three spaces") + 1;
    let hit = statement_at(text, offset, &dialect).expect("same line as statement 1");
    assert_eq!(hit.content(text), "SELECT 1 FROM dual");
}

#[test]
fn statement_at_returns_none_on_a_blank_line_separated_from_any_statement() {
    let dialect = support::oracle_like();
    let text = "SELECT 1 FROM dual;\n\n\nSELECT 2 FROM dual;\n";
    let blank_line_offset = text
        .find("\n\n\n")
        .expect("test text contains a blank line")
        + 2;
    assert!(statement_at(text, blank_line_offset, &dialect).is_none());
}

#[test]
fn statement_at_returns_none_before_the_first_statement() {
    let dialect = support::oracle_like();
    let text = "\n\nSELECT 1 FROM dual;\n";
    assert!(statement_at(text, 0, &dialect).is_none());
}

// ------------------------------------------------------- known limitations

#[test]
#[ignore = "known limitation (M2.4): a CALL-form trigger body has no BEGIN/END \
            at all, so the depth-based block scan never finds a matching END \
            and mis-splits the rest of the script. See splitter.rs module docs."]
fn create_trigger_with_a_call_body_has_no_end_and_is_a_known_limitation() {
    let dialect = support::oracle_like();
    let text = "CREATE TRIGGER t AFTER INSERT ON x FOR EACH ROW CALL p(:NEW.id);\n\
                SELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    // What a correct implementation would report: two statements, the
    // trigger recognized as ending at its own `;` (no BEGIN/END body).
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated);
}

#[test]
#[ignore = "known limitation (M2.4): a `CREATE TYPE ... AS OBJECT (...);` spec \
            has no BEGIN/END at all (ADR-0002 D8 scopes object types out), so \
            the depth-based block scan never finds a matching END. See \
            splitter.rs module docs."]
fn create_type_as_object_spec_has_no_end_and_is_a_known_limitation() {
    let dialect = support::oracle_like();
    let text = "CREATE TYPE point_t AS OBJECT (x NUMBER, y NUMBER);\n\
                SELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated);
}

#[test]
#[ignore = "known limitation (M2.4): `WITH FUNCTION ... SELECT ...` (Oracle 12c \
            inline PL/SQL) is a block-containing statement that does not start \
            with a block-starter keyword; recognizing it needs scanning past \
            arbitrary WITH-clause syntax, out of scope for this task. See \
            splitter.rs module docs."]
fn with_function_inline_plsql_is_a_known_limitation() {
    let dialect = support::oracle_like();
    let text = "WITH FUNCTION f(x NUMBER) RETURN NUMBER IS BEGIN RETURN x * 2; END;\n\
                SELECT f(1) FROM dual;\n";
    let spans = split_statements(text, &dialect);
    // A correct implementation would report exactly one statement (the
    // semicolons inside the inline function body must not split it); today
    // this splitter, not recognizing `WITH FUNCTION` as a block starter,
    // reports several.
    assert_eq!(spans.len(), 1, "{spans:?}");
}
