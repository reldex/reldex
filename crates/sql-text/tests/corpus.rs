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
    EndedBy, LexState, SqlDialect, StatementKind, StatementSpan, Token, TokenKind,
    split_statements, statement_at, tokenize, tokenize_block,
};

#[path = "support/mod.rs"]
mod support;

const CORPUS_FILES: &[(&str, &str)] = &[
    ("package", include_str!("corpus/package.sql")),
    ("trigger", include_str!("corpus/trigger.sql")),
    ("thai", include_str!("corpus/thai.sql")),
    ("qquotes", include_str!("corpus/qquotes.sql")),
    ("no_slash", include_str!("corpus/no_slash.sql")),
    ("edge_cases", include_str!("corpus/edge_cases.sql")),
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

/// Checks the invariants every [`StatementSpan`] must satisfy, on its own,
/// regardless of its neighbors: `content_start <= content_end <= full_end`,
/// every one of those three offsets sits on a UTF-8 character boundary of
/// `text` (a span built from a byte offset that is not one would slice a
/// multi-byte character in half), and [`StatementSpan::content`]/
/// [`StatementSpan::full`] do not panic. A round-2 adversarial review found a
/// span whose `content_start` could exceed its `content_end` (a lone `/`
/// line following an already-terminated statement, with nothing of its own
/// pending — see the [`crate` root splitter module docs' "lone `/` line with
/// nothing pending"] section), which made `.content()` panic; every test
/// helper and the fuzz test now calls this on every span they touch instead
/// of trusting the weaker `content_end <= full_end` check alone.
fn assert_span_invariants(text: &str, span: &StatementSpan) {
    assert!(
        text.is_char_boundary(span.content_start),
        "content_start {} is not a char boundary: {span:?}",
        span.content_start
    );
    assert!(
        text.is_char_boundary(span.content_end),
        "content_end {} is not a char boundary: {span:?}",
        span.content_end
    );
    assert!(
        text.is_char_boundary(span.full_end),
        "full_end {} is not a char boundary: {span:?}",
        span.full_end
    );
    assert!(
        span.content_start <= span.content_end,
        "content_start > content_end: {span:?}"
    );
    assert!(
        span.content_end <= span.full_end,
        "content_end > full_end: {span:?}"
    );
    assert!(span.full_end <= text.len(), "full_end past end: {span:?}");
    // Must not panic.
    let _ = span.content(text);
    let _ = span.full(text);
}

fn assert_spans_reproduce_text(text: &str, spans: &[StatementSpan]) {
    let mut pos = 0usize;
    let mut rebuilt = String::with_capacity(text.len());
    for span in spans {
        assert_span_invariants(text, span);
        assert!(
            span.content_start >= pos,
            "statement span went backwards: {span:?}"
        );
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
fn create_trigger_with_a_call_body_ends_at_its_own_terminator() {
    // Formerly a known limitation (M2.4 adversarial review MUST-FIX 3): the
    // `pending_bodies` model's `body_less_markers` now recognizes `CALL` as
    // a marker meaning "this statement owes no body at all".
    let dialect = support::oracle_like();
    let text = "CREATE TRIGGER t AFTER INSERT ON x FOR EACH ROW CALL p(:NEW.id);\n\
                SELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated);
    assert_eq!(spans[0].ended_by, EndedBy::InferredBlockEnd);
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn create_type_as_object_spec_ends_at_its_own_terminator() {
    // Formerly a known limitation: `BlockKind::ParenDelimited` now scans
    // balanced parens to the first depth-0 terminator instead of ever
    // looking for a `BEGIN`/`END` that does not exist.
    let dialect = support::oracle_like();
    let text = "CREATE TYPE point_t AS OBJECT (x NUMBER, y NUMBER);\n\
                SELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated);
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn create_type_simple_synonym_form_with_no_parens_also_ends_at_its_terminator() {
    let dialect = support::oracle_like();
    let text = "CREATE TYPE happy_day IS BOOLEAN;\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated);
}

#[test]
#[ignore = "known limitation (M2.4): `WITH FUNCTION ... SELECT ...` (Oracle 12c \
            inline PL/SQL) is a block-containing statement that does not start \
            with a block-starter keyword; recognizing it needs scanning past \
            arbitrary WITH-clause syntax, out of scope for this task. \
            Failure mode is S3-safe: the inline function's own `;`s are read \
            as ordinary plain-statement terminators, over-splitting into \
            several statements that each fail to parse alone — never an \
            executable fragment carved from the middle of one. See \
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

// ------------------------------------------- adversarial review MUST-FIXes

#[test]
fn statement_at_never_panics_on_a_non_char_boundary_offset() {
    // The adversarial review's exact reproducer: byte 2 of `";\u{feff}"`
    // falls inside the 3-byte BOM starting at byte 1.
    let dialect = support::oracle_like();
    let text = ";\u{feff}";
    let result = statement_at(text, 2, &dialect);
    // The only requirement is "does not panic"; whatever it resolves to
    // must itself be a valid, in-bounds, char-boundary span.
    if let Some(span) = result {
        assert!(text.is_char_boundary(span.content_start));
        assert!(text.is_char_boundary(span.content_end));
        assert!(text.is_char_boundary(span.full_end));
    }
}

#[test]
fn statement_at_never_panics_at_any_byte_offset_of_multibyte_text() {
    let dialect = support::oracle_like();
    for text in [
        ";\u{feff}",
        "SELECT '\u{1F600}' FROM dual;\nสวัสดี /* ครับ */ 'ข้อความ';\n",
        include_str!("corpus/thai.sql"),
    ] {
        for offset in 0..=(text.len() + 2) {
            if let Some(span) = statement_at(text, offset, &dialect) {
                assert!(
                    text.is_char_boundary(span.content_start),
                    "text={text:?} offset={offset} span={span:?}"
                );
                assert!(
                    text.is_char_boundary(span.content_end),
                    "text={text:?} offset={offset} span={span:?}"
                );
                assert!(
                    text.is_char_boundary(span.full_end),
                    "text={text:?} offset={offset} span={span:?}"
                );
            }
        }
    }
}

#[test]
fn a_lone_slash_line_authoritatively_ends_a_plain_statement_mid_buffer() {
    // Safety principle S1, exactly as SQL*Plus/SQLcl behave: `/` submits
    // whatever is in the buffer, complete or not. `SELECT 10` runs alone;
    // `2 FROM dual` is a separate (syntactically invalid on its own, but
    // that is not this crate's concern) following statement.
    let dialect = support::oracle_like();
    let text = "SELECT 10\n/\n2 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Plain, StatementKind::Plain]);
    assert_eq!(spans[0].content(text), "SELECT 10");
    assert_eq!(spans[0].ended_by, EndedBy::SlashLine);
    assert!(spans[0].terminated);
    assert_eq!(spans[1].content(text), "2 FROM dual");
    assert_eq!(spans[1].ended_by, EndedBy::Terminator);
}

#[test]
fn a_labelled_block_is_recognized_as_a_block_not_split_mid_structure() {
    let dialect = support::oracle_like();
    let text = "<<outer>>\nBEGIN\n  NULL;\nEND outer;\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated);
    assert!(spans[0].content(text).starts_with("<<outer>>"));
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn nested_labelled_blocks_inside_an_outer_block_do_not_confuse_depth() {
    let dialect = support::oracle_like();
    let text = "<<outer_block>>\nDECLARE\n  v NUMBER;\nBEGIN\n  <<inner_block>>\n  BEGIN\n    v := 1;\n  END inner_block;\nEND outer_block;\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated);
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn a_nested_subprogram_in_a_declare_section_does_not_confuse_the_outer_close() {
    let dialect = support::oracle_like();
    let text = "CREATE PROCEDURE outer_p AS\n\
                PROCEDURE inner_p IS\n\
                BEGIN\n\
                  NULL;\n\
                END inner_p;\n\
                BEGIN\n\
                  DELETE FROM important_table;\n\
                END outer_p;\n\
                /\n\
                SELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated);
    assert!(spans[0].content(text).trim_end().ends_with("END outer_p"));
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn a_compound_trigger_with_two_timing_points_does_not_swallow_the_next_statement() {
    let dialect = support::oracle_like();
    let text = "CREATE OR REPLACE TRIGGER compound_trg\n\
                FOR INSERT ON some_table\n\
                COMPOUND TRIGGER\n\
                  BEFORE STATEMENT IS\n\
                  BEGIN\n\
                    NULL;\n\
                  END BEFORE STATEMENT;\n\
                  AFTER STATEMENT IS\n\
                  BEGIN\n\
                    NULL;\n\
                  END AFTER STATEMENT;\n\
                END compound_trg;\n\
                /\n\
                DROP TABLE should_still_run;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated);
    assert_eq!(spans[1].content(text), "DROP TABLE should_still_run");
}

#[test]
fn an_ordinary_non_compound_trigger_is_unaffected_by_timing_point_detection() {
    // Guards against over-eagerly treating BEFORE/AFTER as a header
    // whenever seen: without a `COMPOUND TRIGGER` marker, this trigger's own
    // `BEFORE INSERT ON x` timing/event clause is ordinary content.
    let dialect = support::oracle_like();
    let text = "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN :NEW.a := 1; END;\n/\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].kind, StatementKind::Block);
    assert!(spans[0].terminated);
}

#[test]
fn a_package_body_with_members_and_an_init_section_closes_at_its_own_end() {
    let dialect = support::oracle_like();
    let text = include_str!("corpus/package.sql");
    let spans = split_statements(text, &dialect);
    assert_eq!(spans.len(), 2, "{spans:?}");
    assert!(spans.iter().all(|s| s.terminated), "{spans:?}");
}

#[test]
fn java_source_runs_to_the_slash_line_ignoring_embedded_semicolons() {
    let dialect = support::oracle_like();
    let text = "CREATE OR REPLACE AND COMPILE JAVA SOURCE NAMED \"Adder\" AS\n\
                public class Adder {\n\
                  public static int add(int a, int b) { return a + b; }\n\
                }\n\
                /\n\
                SELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated);
    assert_eq!(spans[0].ended_by, EndedBy::SlashLine);
    assert!(spans[0].content(text).contains("public class Adder"));
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn dollar_if_directives_inside_a_block_do_not_confuse_depth_tracking() {
    // The `$END` reproducer: without directive lexing, its `END` keyword
    // would close the block early and misread the rest of the script.
    let dialect = support::oracle_like();
    let text = "BEGIN\n\
                  $IF $$my_flag $THEN\n\
                    NULL;\n\
                  $ELSE\n\
                    NULL;\n\
                  $END;\n\
                  COMMIT;\n\
                END;\n\
                /\n\
                DROP TABLE should_still_run;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated);
    assert_eq!(spans[1].content(text), "DROP TABLE should_still_run");
}

#[test]
fn additional_bug_a_leading_directive_before_the_blocks_own_begin_still_recognizes_the_block() {
    // Found while extending the round-3 grammar with directives around "only
    // the outer BEGIN": `is_trivial` (used by `collect_leading_words` to
    // decide what may precede a block-starter's own keywords without
    // defeating the match) covered only whitespace/comments, not
    // `TokenKind::Directive`. A `$IF ... $THEN` sitting before this
    // anonymous block's own `BEGIN` stopped leading-keyword collection
    // before `BEGIN` was ever reached, so no `BlockStarter` matched at all
    // and the whole script was misread as 3 unrelated `Plain` statements
    // (split at the semicolons inside what should have been one block).
    let dialect = support::oracle_like();
    let text = "$IF $$my_flag $THEN\nBEGIN\n$END\n  NULL;\nEND;\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated, "{spans:?}");
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn dollar_if_directive_around_only_the_blocks_closing_end_is_still_recognized() {
    let dialect = support::oracle_like();
    let text = "BEGIN\n  NULL;\n$IF $$my_flag $THEN\nEND;\n$END\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated, "{spans:?}");
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn dollar_if_directive_around_a_whole_nested_block_does_not_confuse_depth_tracking() {
    let dialect = support::oracle_like();
    let text = "BEGIN\n  $IF $$flag $THEN\n    BEGIN\n      NULL;\n    END;\n  $ELSE\n    NULL;\n  $END\n  COMMIT;\nEND;\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated, "{spans:?}");
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn forward_declarations_in_a_package_spec_own_no_body() {
    let dialect = support::oracle_like();
    let text = "CREATE OR REPLACE PACKAGE forward_decls AS\n\
                  PROCEDURE go;\n\
                  FUNCTION calc_it(p_x NUMBER) RETURN NUMBER;\n\
                END forward_decls;\n\
                /\n\
                SELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated);
}

#[test]
fn a_call_spec_owns_no_plsql_body() {
    let dialect = support::oracle_like();
    let text = "CREATE OR REPLACE FUNCTION native_add(a NUMBER, b NUMBER) RETURN NUMBER\n\
                  IS LANGUAGE JAVA\n\
                  NAME 'Adder.add(int, int) return int';\n\
                SELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated);
    assert_eq!(spans[0].ended_by, EndedBy::InferredBlockEnd);
}

#[test]
fn ended_by_distinguishes_terminator_slash_line_and_inferred_block_end() {
    let dialect = support::oracle_like();

    let plain = split_statements("SELECT 1 FROM dual;", &dialect);
    assert_eq!(plain[0].ended_by, EndedBy::Terminator);

    let with_slash = split_statements("BEGIN\n  NULL;\nEND;\n/\n", &dialect);
    assert_eq!(with_slash[0].ended_by, EndedBy::SlashLine);

    let without_slash = split_statements("BEGIN\n  NULL;\nEND;\n", &dialect);
    assert_eq!(without_slash[0].ended_by, EndedBy::InferredBlockEnd);
    assert!(without_slash[0].terminated);

    let strict = support::oracle_like_strict_slash();
    let strict_without_slash = split_statements("BEGIN\n  NULL;\nEND;\n", &strict);
    assert_eq!(strict_without_slash[0].ended_by, EndedBy::InferredBlockEnd);
    assert!(!strict_without_slash[0].terminated);

    let truncated = split_statements("BEGIN\n  NULL;\n-- cut off", &dialect);
    assert_eq!(truncated[0].ended_by, EndedBy::EndOfInput);
    assert!(!truncated[0].terminated);
}

#[test]
fn consecutive_lone_slash_lines_with_nothing_pending_produce_no_spans_of_their_own() {
    // Lead decision (ADR-0002 amendment J, round 2): a lone `/` line with no
    // statement text pending produces no span at all — the first `/` closes
    // nothing new (statement 1 already ended at its own `;`), so it is
    // skipped rather than read as its own zero-content statement, and the
    // same for the second `/`. Before this rule, the plain-statement scan
    // that started at such a `/` computed a `content_end` that landed before
    // its own `content_start`, panicking in `StatementSpan::content`.
    let dialect = support::oracle_like();
    let text = "SELECT 1 FROM dual;\n/\n/\nSELECT 2 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_spans_reproduce_text(text, &spans);
    assert_eq!(spans.len(), 2, "{spans:?}");
    assert_eq!(spans[0].content(text), "SELECT 1 FROM dual");
    assert_eq!(spans[1].content(text), "SELECT 2 FROM dual");
}

#[test]
fn a_lone_slash_line_at_the_very_start_of_a_script_produces_no_span() {
    // Isolated reproducer for the same rule: nothing precedes the `/` at
    // all, so `content_start` was never even computed for a would-be span —
    // trim_end() on an all-whitespace prefix could otherwise underflow past
    // byte 0's neighborhood into a negative-looking (but merely very small)
    // `content_end` relative to whatever came after.
    let dialect = support::oracle_like();
    let text = "\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_spans_reproduce_text(text, &spans);
    assert_eq!(spans.len(), 1, "{spans:?}");
    assert_eq!(spans[0].content(text), "SELECT 1 FROM dual");
}

#[test]
fn a_lone_slash_line_immediately_after_a_terminated_plain_statement_produces_no_second_span() {
    // The exact reproducer cited by the round-2 review: `content_end` for
    // the second (would-be) span landed at byte 20 while `content_start` was
    // 21 — a `content_start > content_end` panic in `StatementSpan::content`.
    let dialect = support::oracle_like();
    let text = "SELECT 1 FROM dual;\n/\n";
    let spans = split_statements(text, &dialect);
    assert_spans_reproduce_text(text, &spans);
    assert_eq!(spans.len(), 1, "{spans:?}");
    assert_eq!(spans[0].content(text), "SELECT 1 FROM dual");
    assert_eq!(spans[0].ended_by, EndedBy::Terminator);
}

#[test]
fn a_lone_slash_line_with_nothing_before_it_at_all_produces_no_span() {
    // The other exact reproducer cited by the round-2 review: `"\n/"` alone
    // — `content_end` computed as `1` while `content_start` (had a span been
    // produced at all) would have been `1` as well, but the true bug was
    // upstream of that arithmetic: this text has no statement anywhere in
    // it, so the correct answer is zero spans, not a lucky-looking `1 <= 1`.
    let dialect = support::oracle_like();
    let text = "\n/";
    let spans = split_statements(text, &dialect);
    assert_spans_reproduce_text(text, &spans);
    assert_eq!(spans.len(), 0, "{spans:?}");
}

#[test]
fn a_quoted_end_identifier_is_never_read_as_the_block_end_keyword() {
    let dialect = support::oracle_like();
    let text = "BEGIN\n  SELECT x AS \"END\" INTO v_x FROM t;\nEND;\n/\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].kind, StatementKind::Block);
    assert!(spans[0].terminated);
}

#[test]
fn a_sql_case_expression_with_a_bare_end_inside_a_block_closes_correctly() {
    let dialect = support::oracle_like();
    let text = "BEGIN\n  SELECT CASE WHEN 1 = 1 THEN 'a' ELSE 'b' END INTO v FROM dual;\nEND;\n/\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(spans.len(), 1);
    assert!(spans[0].terminated);
}

// ------------------------------------------------- round-2 regression tests

#[test]
fn must_fix_1_bare_begin_starter_with_one_nested_inner_block_is_one_span() {
    // The round-2 reproducer: a bare `BEGIN` starter's match already
    // consumed the frame's own body-opening `BEGIN`, so the next `BEGIN`
    // must be read as a nested block, never as "the frame's body" again.
    // Before `BlockStarter::opens_body` existed, this split into 2 spans
    // (`"BEGIN\n  BEGIN\n    NULL;\n  END;"` and an orphaned `"END"`).
    let dialect = support::oracle_like();
    let text = "BEGIN\n  BEGIN\n    NULL;\n  END;\nEND;\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated, "{spans:?}");
    assert_eq!(
        spans[0].content(text),
        "BEGIN\n  BEGIN\n    NULL;\n  END;\nEND"
    );
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn must_fix_1_bare_begin_starter_with_sibling_inner_blocks_is_one_span() {
    // The "class A" variant: with N sibling inner blocks, the pre-fix defect
    // produced N+1 spans, with the middle one a complete, independently
    // runnable anonymous block carved out of the middle of the real one.
    let dialect = support::oracle_like();
    let text = "BEGIN\n  BEGIN NULL; END;\n  BEGIN NULL; END;\nEND;\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated, "{spans:?}");
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn must_fix_1_bare_begin_starter_with_an_exception_handler_containing_a_nested_block_is_one_span() {
    let dialect = support::oracle_like();
    let text = "BEGIN\n  NULL;\nEXCEPTION\n  WHEN OTHERS THEN\n    BEGIN\n      NULL;\n    END;\nEND;\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated, "{spans:?}");
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn must_fix_1_declare_starter_with_a_nested_inner_block_was_already_correct() {
    // Control: `DECLARE`'s `opens_body` is `false` (its own `BEGIN` still
    // lies ahead), so this shape never depended on the round-2 fix — it
    // stays correct before and after.
    let dialect = support::oracle_like();
    let text =
        "DECLARE\n  v NUMBER;\nBEGIN\n  BEGIN\n    NULL;\n  END;\nEND;\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated, "{spans:?}");
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn must_fix_1_create_procedure_with_a_nested_subprogram_was_already_correct() {
    // Control: a `CREATE PROCEDURE` starter's `opens_body` is `false` too —
    // its own body's `BEGIN` still lies ahead of a nested subprogram's own
    // complete `BEGIN ... END`.
    let dialect = support::oracle_like();
    let text = "CREATE OR REPLACE PROCEDURE p AS\n  PROCEDURE h IS\n  BEGIN\n    NULL;\n  END h;\nBEGIN\n  h;\nEND p;\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated, "{spans:?}");
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn must_fix_1_a_labelled_nested_block_inside_a_bare_begin_starter_is_one_span() {
    let dialect = support::oracle_like();
    let text =
        "BEGIN\n  <<inner>>\n  BEGIN\n    NULL;\n  END inner;\nEND;\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated, "{spans:?}");
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn must_fix_1_a_labelled_outer_and_inner_bare_begin_starter_is_one_span() {
    let dialect = support::oracle_like();
    let text = "<<outer>>\nBEGIN\n  <<inner>>\n  BEGIN\n    NULL;\n  END inner;\nEND outer;\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated, "{spans:?}");
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn must_fix_3_a_variable_named_language_right_after_is_does_not_trigger_a_false_call_spec() {
    // Fail-safe direction: a missed call spec merely swallows to the next
    // `/`/EOF; a false one carves the real body out from under the
    // statement. `language`/`external` must only ever match as full phrases.
    let dialect = support::oracle_like();
    let text = "CREATE OR REPLACE PROCEDURE p IS\n  language NUMBER := 1;\nBEGIN\n  NULL;\nEND p;\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated, "{spans:?}");
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn must_fix_3_a_constant_named_external_in_mixed_case_does_not_trigger_a_false_call_spec() {
    let dialect = support::oracle_like();
    let text = "CREATE OR REPLACE PROCEDURE p IS\n  External CONSTANT VARCHAR2(1) := 'x';\nBEGIN\n  NULL;\nEND p;\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated, "{spans:?}");
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn must_fix_3_a_cursor_named_language_does_not_trigger_a_false_call_spec() {
    let dialect = support::oracle_like();
    let text = "CREATE OR REPLACE PROCEDURE p IS\n  CURSOR Language IS SELECT 1 FROM dual;\nBEGIN\n  NULL;\nEND p;\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated, "{spans:?}");
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn must_fix_3_a_quoted_identifier_named_language_does_not_trigger_a_false_call_spec() {
    // A quoted identifier is a distinct `TokenKind` from a plain word, so it
    // was never a `is_word` candidate in the first place — this is a
    // regression net for the call-spec path specifically, alongside the
    // pre-existing `a_quoted_end_identifier_...` test for `END`.
    let dialect = support::oracle_like();
    let text = "CREATE OR REPLACE PROCEDURE p IS\n  \"language\" NUMBER := 1;\nBEGIN\n  NULL;\nEND p;\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated, "{spans:?}");
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn must_fix_4_a_forward_declarations_cast_default_does_not_corrupt_the_containers_init_section() {
    // Concrete pre-fix failure: without paren-depth tracking in
    // `scan_header`, the `AS` inside `CAST(1 AS NUMBER)` was read as this
    // forward declaration's own body intro, leaving a phantom
    // `pending_bodies` debt that the container's own init `BEGIN` then
    // wrongly "fulfilled" (nesting instead of being absorbed as the
    // container's own body) — corrupting depth tracking so the container's
    // real closing `END` never matched at depth `0`.
    //
    // Deliberately no `/` between `END pk7;` and the next statement: safety
    // principle S1 would otherwise rescue the miscounted scan at that `/`
    // line regardless of the bug (S1 is checked unconditionally, ahead of
    // depth/`pending_bodies`), masking the very difference this test exists
    // to catch — pre-fix, the corrupted depth never returns to `0` at `END
    // pk7`, so the scan runs past it looking for one more `END` that never
    // comes, swallowing `SELECT 1 FROM dual;` into one unterminated span
    // instead of two.
    let dialect = support::oracle_like();
    let text = "CREATE OR REPLACE PACKAGE BODY pk7 AS\n  PROCEDURE p2(a NUMBER DEFAULT CAST(1 AS NUMBER));\nBEGIN\n  NULL;\nEND pk7;\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert_eq!(spans[0].ended_by, EndedBy::InferredBlockEnd, "{spans:?}");
    assert!(spans[0].terminated, "{spans:?}");
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn must_fix_4_a_parameter_defaults_case_expression_does_not_corrupt_header_scanning() {
    let dialect = support::oracle_like();
    let text = "CREATE OR REPLACE PACKAGE BODY pk8 AS\n  FUNCTION f2(b NUMBER DEFAULT CASE WHEN 1 = 1 THEN 1 ELSE 2 END) RETURN NUMBER IS\n  BEGIN\n    RETURN b;\n  END f2;\nEND pk8;\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated, "{spans:?}");
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn must_fix_4_a_constructors_return_self_as_result_is_correctly_owes_a_body() {
    let dialect = support::oracle_like();
    let text = "CREATE OR REPLACE TYPE BODY obj_t AS\n  CONSTRUCTOR FUNCTION obj_t(x NUMBER) RETURN SELF AS RESULT IS\n  BEGIN\n    RETURN;\n  END;\nEND;\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated, "{spans:?}");
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn must_fix_4_a_forward_declared_constructors_return_self_as_result_owns_no_body() {
    // Concrete pre-fix failure: without `body_intro_exceptions`, the `AS` in
    // `SELF AS RESULT` was read as this forward declaration's own body
    // intro, leaving the same kind of phantom `pending_bodies` debt as the
    // `CAST` case above — wrongly consumed by the container's own init
    // `BEGIN`, corrupting depth tracking so the real closing `END` never
    // matched at depth `0`. No `/` after `END pkg6;`, for the same reason as
    // the `CAST` test above: S1 would otherwise rescue the miscounted scan
    // and mask the difference.
    let dialect = support::oracle_like();
    let text = "CREATE OR REPLACE PACKAGE BODY pkg6 AS\n  FUNCTION make_it(x NUMBER) RETURN SELF AS RESULT;\nBEGIN\n  NULL;\nEND pkg6;\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert_eq!(spans[0].ended_by, EndedBy::InferredBlockEnd, "{spans:?}");
    assert!(spans[0].terminated, "{spans:?}");
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn additional_bug_a_nested_begin_inside_an_inner_members_own_body_does_not_fulfill_an_outer_pending_debt()
 {
    // Found by the grammar-based differential test (`tests/differential.rs`),
    // not by any of the round-2 review's 4 named MUST-FIX items: a two-level
    // -deep nested subprogram (`mid_p` declared inside `outer_p`'s declare
    // section, and `inner_p` declared inside *`mid_p`'s own* declare
    // section) where `inner_p`'s own body itself contains a nested `BEGIN …
    // END`. Root cause: `scan_structured`'s `BEGIN` dispatch treated
    // `pending_bodies > 0` alone as "this `BEGIN` fulfills a header's debt",
    // without also requiring `depth == 0` — so once already inside
    // `inner_p`'s own body (`depth == 1`), the *nested* `BEGIN` there was
    // wrongly consumed as fulfilling `mid_p`'s still-outstanding debt
    // instead of simply nesting, corrupting depth tracking so `outer_p`'s
    // real closing `END` never matched at depth `0` — splitting this single
    // statement into two (one ending at `mid_p`'s own `END`, one starting
    // fresh at `outer_p`'s body).
    let dialect = support::oracle_like();
    let text = "CREATE OR REPLACE PROCEDURE outer_p AS\n\
                  PROCEDURE mid_p IS\n\
                    PROCEDURE inner_p IS\n\
                    BEGIN\n\
                      BEGIN\n\
                        NULL;\n\
                      END;\n\
                    END inner_p;\n\
                  BEGIN\n\
                    inner_p;\n\
                  END mid_p;\n\
                BEGIN\n\
                  mid_p;\n\
                END outer_p;\n\
                /\n\
                SELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated, "{spans:?}");
    assert!(
        spans[0].content(text).trim_end().ends_with("END outer_p"),
        "{spans:?}"
    );
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

#[test]
fn must_fix_4_the_outer_statements_own_cast_default_does_not_corrupt_the_top_level_scan() {
    // The other half of the same finding: the *outer* statement's own
    // signature (not only a nested member's) is scanned by `scan_structured`
    // itself, not `scan_header` — its word-dispatch is paren-gated too.
    let dialect = support::oracle_like();
    let text = "CREATE OR REPLACE FUNCTION f3(a NUMBER DEFAULT CAST(1 AS NUMBER)) RETURN NUMBER IS\nBEGIN\n  RETURN a;\nEND f3;\n/\nSELECT 1 FROM dual;\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(kinds(&spans), [StatementKind::Block, StatementKind::Plain]);
    assert!(spans[0].terminated, "{spans:?}");
    assert_eq!(spans[1].content(text), "SELECT 1 FROM dual");
}

// -------------------------------------------------------------- fuzzing

/// A tiny, dependency-free xorshift32 PRNG — this crate stays at zero
/// dependencies (`lib.rs`'s "Dependencies: None"), so a fuzz test cannot
/// reach for the `rand` crate. Deterministic and fast; good enough for
/// generating structurally-varied but grammar-aware SQL/PL-SQL fragments.
struct Xorshift32(u32);

impl Xorshift32 {
    fn new(seed: u32) -> Self {
        Self(if seed == 0 { 0xDEAD_BEEF } else { seed })
    }

    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    fn choose<'a, T>(&mut self, options: &'a [T]) -> &'a T {
        &options[(self.next_u32() as usize) % options.len()]
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u32() as usize) % bound
        }
    }
}

/// Grammar-aware fragments a fuzz case is assembled from — deliberately
/// including the exact shapes the adversarial review's MUST-FIX items named,
/// so a regression in any of them is caught by both the named test above
/// *and* by fuzzing combining it with everything else.
const FRAGMENTS: &[&str] = &[
    "SELECT 1 FROM dual;\n",
    "SELECT 'it''s ก' FROM dual;\n",
    "INSERT INTO t (a) VALUES (1);\n",
    "COMMIT;\n",
    "-- a line comment with a ; in it\n",
    "/* a block comment with a ; and END in it */\n",
    "BEGIN\n  NULL;\nEND;\n/\n",
    "<<lbl>>\nBEGIN\n  NULL;\nEND lbl;\n/\n",
    "DECLARE\n  v NUMBER;\nBEGIN\n  v := 1;\nEND;\n/\n",
    "CREATE OR REPLACE PROCEDURE p AS\n  PROCEDURE inner_p IS\n  BEGIN\n    NULL;\n  END inner_p;\nBEGIN\n  NULL;\nEND p;\n/\n",
    "CREATE OR REPLACE PACKAGE BODY pkg AS\n  PROCEDURE a IS BEGIN NULL; END a;\n  FUNCTION b RETURN NUMBER IS BEGIN RETURN 1; END b;\nBEGIN\n  NULL;\nEND pkg;\n/\n",
    "CREATE OR REPLACE TRIGGER trg FOR INSERT ON t COMPOUND TRIGGER\n  BEFORE STATEMENT IS BEGIN NULL; END BEFORE STATEMENT;\nEND trg;\n/\n",
    "CREATE TRIGGER t2 AFTER INSERT ON x FOR EACH ROW CALL p(:NEW.id);\n",
    "CREATE TYPE point_t AS OBJECT (x NUMBER, y NUMBER);\n",
    "CREATE TYPE happy_day IS BOOLEAN;\n",
    "CREATE OR REPLACE PACKAGE fwd AS\n  PROCEDURE go;\nEND fwd;\n/\n",
    "CREATE OR REPLACE AND COMPILE JAVA SOURCE NAMED \"X\" AS\npublic class X { void m() {} }\n/\n",
    "BEGIN\n  $IF $$flag $THEN NULL; $ELSE NULL; $END;\nEND;\n/\n",
    "SELECT q'[a;b]' FROM dual;\n",
    "SELECT N'nat' FROM dual;\n",
    "\n",
    "   \n",
    "/\n",
    // Round-2 vocabulary: a bare-`BEGIN` starter with nested/sibling inner
    // blocks (MUST-FIX #1) and an exception handler containing one.
    "BEGIN\n  BEGIN\n    NULL;\n  END;\nEND;\n/\n",
    "BEGIN\n  BEGIN NULL; END;\n  BEGIN NULL; END;\nEND;\n/\n",
    "BEGIN\n  NULL;\nEXCEPTION\n  WHEN OTHERS THEN\n    BEGIN\n      NULL;\n    END;\nEND;\n/\n",
    // Identifiers legally named `language`/`external`/`is`/`as`/`before`/
    // `after` right after a header's own `IS` (MUST-FIX #3's fail-safe
    // direction).
    "CREATE OR REPLACE PROCEDURE p3 IS\n  language NUMBER := 1;\n  external VARCHAR2(1) := 'x';\nBEGIN\n  NULL;\nEND p3;\n/\n",
    // A forward declaration whose parameter default hides `AS`/`IS`/`CASE …
    // END` behind parens (MUST-FIX #4).
    "CREATE OR REPLACE PACKAGE pk4 AS\n  PROCEDURE p2(a NUMBER DEFAULT CAST(1 AS NUMBER));\n  FUNCTION f2(b NUMBER DEFAULT CASE WHEN 1 = 1 THEN 1 ELSE 2 END) RETURN NUMBER;\nEND pk4;\n/\n",
    // A constructor method's `RETURN SELF AS RESULT IS` (MUST-FIX #4's
    // `body_intro_exceptions`).
    "CREATE OR REPLACE TYPE BODY obj_t AS\n  CONSTRUCTOR FUNCTION obj_t(x NUMBER) RETURN SELF AS RESULT IS\n  BEGIN\n    RETURN;\n  END;\nEND;\n/\n",
    // A lone `/` line immediately after an already-terminated statement, with
    // nothing pending (MUST-FIX #2's lead decision).
    "SELECT 1 FROM dual;\n/\n",
];

fn assert_split_invariants(text: &str, dialect: &SqlDialect) {
    let tokens = tokenize(text, dialect);
    assert_full_coverage(text, &tokens);
    assert_line_by_line_matches_whole_document(text, dialect);

    let spans = split_statements(text, dialect);
    // `assert_spans_reproduce_text` already calls `assert_span_invariants` on
    // every span (content_start <= content_end <= full_end, char-boundary
    // checks, and a non-panicking `.content()`/`.full()` call) — see that
    // helper's own docs for the round-2 defect (a lone `/` line with nothing
    // pending) this specifically guards against.
    assert_spans_reproduce_text(text, &spans);
    for pair in spans.windows(2) {
        assert!(pair[0].full_end <= pair[1].content_start, "{text:?}");
    }

    // `statement_at` must never panic, at any offset (including "just past
    // the end", which callers can legitimately pass).
    for offset in [
        0,
        text.len() / 3,
        text.len() / 2,
        text.len(),
        text.len() + 1,
    ] {
        let _ = statement_at(text, offset, dialect);
    }
}

#[test]
fn deterministic_fuzz_split_and_tokenize_invariants_hold_across_many_generated_scripts() {
    let dialect = support::oracle_like();
    let cases = if cfg!(debug_assertions) {
        2_000
    } else {
        200_000
    };
    let seeds: [u32; 4] = [1, 42, 0xC0FF_EE01, 0x5EED_5EED];

    for seed in seeds {
        let mut rng = Xorshift32::new(seed);
        for case in 0..cases {
            let fragment_count = 1 + rng.below(8);
            let mut text = String::new();
            for _ in 0..fragment_count {
                text.push_str(rng.choose(FRAGMENTS));
            }
            assert_split_invariants(&text, &dialect);

            // Also exercise the CRLF form periodically, since CRLF handling
            // has its own (already-tested) subtleties around line-based
            // scans like `is_lone_slash_line`.
            if case % 16 == 0 {
                let crlf = text.replace('\n', "\r\n");
                assert_split_invariants(&crlf, &dialect);
            }
        }
    }
}

#[test]
fn known_good_blocks_joined_by_slash_round_trip_through_split_and_rejoin() {
    // A property the adversarial review specifically asked for: take several
    // independently-valid `/`-terminated blocks/statements, concatenate
    // them, split the result, and check that re-joining every span's
    // `full()` text (each of which already ends at its own `/` line)
    // reproduces the same statements in the same order with nothing lost or
    // merged across a boundary.
    let dialect = support::oracle_like();
    let blocks: &[&str] = &[
        "BEGIN\n  NULL;\nEND;\n/\n",
        "SELECT 1 FROM dual;\n",
        "<<lbl>>\nBEGIN\n  NULL;\nEND lbl;\n/\n",
        "CREATE OR REPLACE PROCEDURE p AS BEGIN NULL; END p;\n/\n",
        "COMMIT;\n",
    ];
    let mut rng = Xorshift32::new(7);
    for _ in 0..500 {
        let chosen: Vec<&str> = (0..(1 + rng.below(6)))
            .map(|_| *rng.choose(blocks))
            .collect();
        let text = chosen.concat();
        let spans = split_statements(&text, &dialect);
        assert_spans_reproduce_text(&text, &spans);
        assert_eq!(
            spans.len(),
            chosen.len(),
            "expected exactly one span per joined block: {text:?} -> {spans:?}"
        );
        assert!(spans.iter().all(|s| s.terminated), "{text:?} -> {spans:?}");
    }
}
