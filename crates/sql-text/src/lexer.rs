//! The lexer: [`tokenize`] (a whole document) and [`tokenize_block`] (one
//! `QSyntaxHighlighter` text block, carrying a [`LexState`] across calls).
//!
//! Both are built on the same one-token-at-a-time scanner (`next_token`,
//! private), so "what counts as a string/comment/identifier" is
//! answered in exactly one place. [`tokenize`] is itself implemented as
//! repeated [`tokenize_block`] calls, one per `\n`-delimited line, which is
//! what makes the crate's key invariant — *highlighting a document line by
//! line, carrying state, must produce exactly the tokens that tokenizing the
//! whole document at once would* — true by construction rather than by two
//! implementations happening to agree. `tests/corpus.rs` still checks it
//! independently, because the public contract is what callers rely on, not
//! how this module happens to be written.

use crate::dialect::SqlDialect;
use crate::token::{LexState, Mode, Token, TokenKind};

/// Tokenizes an entire document.
///
/// Implemented as [`tokenize_block`] applied to each `\n`-separated line in
/// turn, carrying the [`LexState`] forward and inserting a one-byte
/// [`TokenKind::Whitespace`] token for each `\n` consumed. The result covers
/// every byte of `text` exactly once — concatenating every token's range
/// reproduces `text` — for both `\n` and `\r\n` line endings (a `\r`
/// immediately before `\n` is ordinary whitespace, absorbed into whichever
/// token already covers it) and for text carrying a leading UTF-8
/// byte-order mark (treated as whitespace).
///
/// If something is still open when the document truly ends — an
/// unterminated string, quoted identifier, block comment, or
/// alternative-quoted literal — the final token is reported as
/// [`TokenKind::Error`] rather than its usual kind. [`tokenize_block`] never
/// does this on its own, because a block ending mid-construct is the normal
/// case for an editor (more text is coming on the next line); only this
/// whole-document view can tell the difference between "not yet closed" and
/// "never going to be".
#[must_use]
pub fn tokenize(text: &str, dialect: &SqlDialect) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut state = LexState::INITIAL;
    let mut pos = 0usize;
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
            // A following segment exists, so this `\n` is a real separator
            // byte in `text` (not merely the absence of one after the final
            // line) and must be accounted for.
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

/// Tokenizes one `QSyntaxHighlighter` text block (conventionally one line,
/// without its line-terminating character), carrying lexer state across
/// calls the way `previousBlockState`/`setCurrentBlockState` do.
///
/// `state` is the previous block's returned state (or [`LexState::INITIAL`]
/// for the first block, matching Qt's own `-1` convention via
/// [`LexState::from_raw`]). The returned state feeds the next call.
///
/// Every byte of `text` is covered by exactly one returned token — this
/// function never skips whitespace silently — but it never produces
/// [`TokenKind::Error`]: an unterminated string/comment/identifier/literal
/// at the end of `text` is exactly what carrying a non-initial state forward
/// means, and is correct, ordinary behavior for a document that continues
/// on the next block. Only [`tokenize`], which knows where the document
/// truly ends, ever reports [`TokenKind::Error`].
#[must_use]
pub fn tokenize_block(text: &str, state: LexState, dialect: &SqlDialect) -> (Vec<Token>, LexState) {
    let mut tokens = Vec::new();
    let mut cursor = Cursor::new(text);
    let mut mode = state.0;
    let mut at_line_start = matches!(mode, Mode::Normal);

    while !cursor.at_end() {
        let start = cursor.pos;
        let (kind, next_mode) = next_token(&mut cursor, dialect, mode, &mut at_line_start);
        debug_assert!(
            cursor.pos > start,
            "next_token must always consume at least one byte"
        );
        tokens.push(Token {
            kind,
            start,
            end: cursor.pos,
        });
        mode = next_mode;
    }

    (tokens, LexState(mode))
}

/// A cursor over `&str`, advancing by whole `char`s. `Copy` so a call can
/// speculatively look ahead (see [`maybe_exponent`]) and discard the attempt.
#[derive(Clone, Copy)]
struct Cursor<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    const fn new(text: &'a str) -> Self {
        Self { text, pos: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.text[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn peek2(&self) -> Option<char> {
        self.rest().chars().nth(1)
    }

    /// Advances past the current character, returning it.
    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    const fn at_end(&self) -> bool {
        self.pos >= self.text.len()
    }
}

/// Whether `ch` separates tokens without being part of any of them. Includes
/// a byte-order mark, which is not [`char::is_whitespace`] but must not
/// defeat recognition of whatever follows it (matches
/// `oracle-thin`'s `classify::Keywords::skip_noise`).
fn is_lexer_whitespace(ch: char) -> bool {
    ch.is_whitespace() || ch == '\u{feff}'
}

/// The byte length of the identifier at the start of `s` (alphabetic start;
/// alphanumeric/`_`/`$`/`#` continuation), or `0` if `s` does not start with
/// one.
fn leading_identifier_len(s: &str) -> usize {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) if first.is_alphabetic() => s
            .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '$' | '#')))
            .unwrap_or(s.len()),
        _ => 0,
    }
}

/// Advances past a run of ordinary content, stopping (without consuming)
/// when `quote` is found unescaped. `''`/`""` doubling is the escape.
/// Returns whether the quote was actually found (`false` = ran to end of
/// this chunk, still open).
fn scan_simple_quoted(cursor: &mut Cursor, quote: char) -> bool {
    loop {
        match cursor.peek() {
            None => return false,
            Some(ch) if ch == quote => {
                cursor.bump();
                if cursor.peek() == Some(quote) {
                    cursor.bump(); // doubled: an escaped quote, not the end
                } else {
                    return true;
                }
            }
            Some(_) => {
                cursor.bump();
            }
        }
    }
}

/// Advances past a `/* … */` block comment's body, given `close` (`"*/"`).
/// Returns whether the closing delimiter was found.
fn scan_block_comment(cursor: &mut Cursor, close: &str) -> bool {
    loop {
        if cursor.rest().starts_with(close) {
            cursor.pos += close.len();
            return true;
        }
        if cursor.bump().is_none() {
            return false;
        }
    }
}

/// Advances past an alternative-quoted (`q'X…`) literal's body, given the
/// **closing** delimiter character. The literal ends at the first
/// occurrence of that character immediately followed by `'` (Oracle's own
/// rule — a lone closing character is legal content). Returns whether the
/// closing sequence was found.
fn scan_q_string(cursor: &mut Cursor, closing: char) -> bool {
    loop {
        match cursor.peek() {
            None => return false,
            Some(ch) if ch == closing => {
                cursor.bump();
                if cursor.peek() == Some('\'') {
                    cursor.bump();
                    return true;
                }
            }
            Some(_) => {
                cursor.bump();
            }
        }
    }
}

/// Oracle pairs four opening delimiters with a different closing character;
/// every other delimiter (any character that is not whitespace or a quote)
/// closes on itself.
const fn closing_delimiter(open: char) -> char {
    match open {
        '[' => ']',
        '{' => '}',
        '<' => '>',
        '(' => ')',
        other => other,
    }
}

/// The byte length of a bind-variable token starting at `:` (included), or
/// `None` if what follows `:` does not name one (`oracle-thin::rewrite`'s
/// `names_a_bind` answers the same question for the same reason: `:=` must
/// not be misread as a placeholder).
fn bind_variable_len(s: &str) -> Option<usize> {
    debug_assert!(s.starts_with(':'));
    let rest = &s[1..];
    let mut chars = rest.chars();
    match chars.next()? {
        '"' => {
            let after_quote = &rest[1..];
            let end = after_quote.find('"')?;
            Some(1 + 1 + end + 1)
        }
        first if first.is_ascii_digit() => {
            let end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            Some(1 + end)
        }
        first if first.is_alphabetic() => {
            let len = leading_identifier_len(rest);
            (len > 0).then_some(1 + len)
        }
        _ => None,
    }
}

/// The byte length of a substitution-variable token starting at `&`
/// (included; `&&` is checked for too), or `None` if what follows does not
/// name one.
fn substitution_variable_len(s: &str) -> Option<usize> {
    debug_assert!(s.starts_with('&'));
    let mut prefix_len = 1;
    if s[1..].starts_with('&') {
        prefix_len += 1;
    }
    let rest = &s[prefix_len..];
    let name_len = match rest.chars().next() {
        Some(c) if c.is_ascii_digit() => rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len()),
        Some(c) if c.is_alphabetic() => leading_identifier_len(rest),
        _ => 0,
    };
    if name_len == 0 {
        return None;
    }
    let mut len = prefix_len + name_len;
    // `&&var.` / `&var.`: a trailing '.' explicitly ends the substitution so
    // it can be followed immediately by more identifier text.
    if s[len..].starts_with('.') {
        len += 1;
    }
    Some(len)
}

/// Two-character operators worth keeping together for a nicer token stream.
/// Not exhaustive of Oracle's grammar (that would need a real expression
/// parser); splitting these into two single-character operators instead
/// would not change anything [`crate::splitter`] does, since it never
/// inspects [`TokenKind::Operator`] text except for the statement
/// terminator(s) and `/`, neither of which is a prefix of any entry here.
const TWO_CHAR_OPERATORS: [&str; 8] = [":=", "<=", ">=", "<>", "!=", "^=", "||", "**"];

fn advance_by_chars(cursor: &mut Cursor, count: usize) {
    for _ in 0..count {
        cursor.bump();
    }
}

fn maybe_exponent(cursor: &mut Cursor) {
    if matches!(cursor.peek(), Some('e' | 'E')) {
        let mut lookahead = *cursor;
        lookahead.bump();
        if matches!(lookahead.peek(), Some('+' | '-')) {
            lookahead.bump();
        }
        if matches!(lookahead.peek(), Some(c) if c.is_ascii_digit()) {
            *cursor = lookahead;
            while matches!(cursor.peek(), Some(c) if c.is_ascii_digit()) {
                cursor.bump();
            }
        }
    }
}

/// Scans a numeric literal. The caller has already confirmed the current
/// position starts one (a digit, or `.` immediately followed by a digit).
fn scan_number(cursor: &mut Cursor) {
    if cursor.peek() == Some('.') {
        cursor.bump();
    } else {
        while matches!(cursor.peek(), Some(c) if c.is_ascii_digit()) {
            cursor.bump();
        }
        if cursor.peek() == Some('.') && matches!(cursor.peek2(), Some(c) if c.is_ascii_digit()) {
            cursor.bump();
        } else {
            // Not a decimal point after all (e.g. `1..10`'s range operator,
            // or `t.c` member access): the integer part is the whole number.
            maybe_exponent(cursor);
            return;
        }
    }
    while matches!(cursor.peek(), Some(c) if c.is_ascii_digit()) {
        cursor.bump();
    }
    maybe_exponent(cursor);
}

/// Produces exactly one token from `cursor`'s current position, advancing it
/// past that token, and returns the mode for the *next* call.
fn next_token(
    cursor: &mut Cursor,
    dialect: &SqlDialect,
    mode: Mode,
    at_line_start: &mut bool,
) -> (TokenKind, Mode) {
    match mode {
        Mode::BlockComment => {
            // The delimiter is dialect data, but `Mode` does not carry it —
            // resuming needs only "look for `*/`", and every dialect that
            // enables block comments in practice uses that one spelling; a
            // dialect with a different spelling still resumes correctly here
            // because `close` is read fresh from `dialect` on every call.
            let close = dialect
                .comments
                .block_comment
                .map_or("*/", |(_, close)| close);
            let closed = scan_block_comment(cursor, close);
            *at_line_start = false;
            (
                TokenKind::Comment,
                if closed {
                    Mode::Normal
                } else {
                    Mode::BlockComment
                },
            )
        }
        Mode::SingleQuoted => {
            let closed = scan_simple_quoted(cursor, '\'');
            *at_line_start = false;
            (
                TokenKind::String,
                if closed {
                    Mode::Normal
                } else {
                    Mode::SingleQuoted
                },
            )
        }
        Mode::DoubleQuoted => {
            let closed = scan_simple_quoted(cursor, '"');
            *at_line_start = false;
            (
                TokenKind::QuotedIdentifier,
                if closed {
                    Mode::Normal
                } else {
                    Mode::DoubleQuoted
                },
            )
        }
        Mode::QString(closing) => {
            let closed = scan_q_string(cursor, closing);
            *at_line_start = false;
            (
                TokenKind::String,
                if closed {
                    Mode::Normal
                } else {
                    Mode::QString(closing)
                },
            )
        }
        Mode::Normal => next_token_normal(cursor, dialect, at_line_start),
    }
}

#[allow(clippy::too_many_lines)]
fn next_token_normal(
    cursor: &mut Cursor,
    dialect: &SqlDialect,
    at_line_start: &mut bool,
) -> (TokenKind, Mode) {
    let ch = cursor
        .peek()
        .expect("caller ensures the cursor is not at end");

    if is_lexer_whitespace(ch) {
        while matches!(cursor.peek(), Some(c) if is_lexer_whitespace(c)) {
            cursor.bump();
        }
        // Whitespace never changes `at_line_start`: a line of pure
        // whitespace before a `REM` (or before anything else) is still "the
        // start of the line" for the next token.
        return (TokenKind::Whitespace, Mode::Normal);
    }

    if let Some(prefix) = dialect.comments.line_comment
        && cursor.rest().starts_with(prefix)
    {
        cursor.pos += prefix.len();
        while matches!(cursor.peek(), Some(c) if c != '\n') {
            cursor.bump();
        }
        *at_line_start = false;
        return (TokenKind::Comment, Mode::Normal);
    }

    if let Some((open, close)) = dialect.comments.block_comment
        && cursor.rest().starts_with(open)
    {
        cursor.pos += open.len();
        let closed = scan_block_comment(cursor, close);
        *at_line_start = false;
        return (
            TokenKind::Comment,
            if closed {
                Mode::Normal
            } else {
                Mode::BlockComment
            },
        );
    }

    if dialect.comments.sqlplus_rem && *at_line_start && ch.is_alphabetic() {
        let len = leading_identifier_len(cursor.rest());
        let word = &cursor.rest()[..len];
        if word.eq_ignore_ascii_case("REM") || word.eq_ignore_ascii_case("REMARK") {
            cursor.pos += len;
            while matches!(cursor.peek(), Some(c) if c != '\n') {
                cursor.bump();
            }
            *at_line_start = false;
            return (TokenKind::Comment, Mode::Normal);
        }
    }

    if ch == '"' {
        cursor.bump();
        let closed = scan_simple_quoted(cursor, '"');
        *at_line_start = false;
        return (
            TokenKind::QuotedIdentifier,
            if closed {
                Mode::Normal
            } else {
                Mode::DoubleQuoted
            },
        );
    }

    if ch == '\'' {
        cursor.bump();
        let closed = scan_simple_quoted(cursor, '\'');
        *at_line_start = false;
        return (
            TokenKind::String,
            if closed {
                Mode::Normal
            } else {
                Mode::SingleQuoted
            },
        );
    }

    if ch.is_alphabetic() {
        let len = leading_identifier_len(cursor.rest());
        let word = &cursor.rest()[..len];
        let after = &cursor.rest()[len..];

        if after.starts_with('\'') {
            if dialect.quoting.alternative_quoting
                && (word.eq_ignore_ascii_case("Q") || word.eq_ignore_ascii_case("NQ"))
            {
                cursor.pos += len; // the "q"/"nq" prefix
                cursor.bump(); // the opening quote
                return match cursor.bump() {
                    Some(delimiter) => {
                        let closing = closing_delimiter(delimiter);
                        let closed = scan_q_string(cursor, closing);
                        *at_line_start = false;
                        (
                            TokenKind::String,
                            if closed {
                                Mode::Normal
                            } else {
                                Mode::QString(closing)
                            },
                        )
                    }
                    // `q'` at the absolute end of this chunk, with no
                    // delimiter character at all. There is nothing to carry
                    // forward as an open construct (there is no delimiter to
                    // remember), so this chunk's contribution ends exactly
                    // here; a following block, if any, starts fresh.
                    None => {
                        *at_line_start = false;
                        (TokenKind::Error, Mode::Normal)
                    }
                };
            }
            if dialect.quoting.national_prefix && word.eq_ignore_ascii_case("N") {
                cursor.pos += len;
                cursor.bump(); // the opening quote
                let closed = scan_simple_quoted(cursor, '\'');
                *at_line_start = false;
                return (
                    TokenKind::String,
                    if closed {
                        Mode::Normal
                    } else {
                        Mode::SingleQuoted
                    },
                );
            }
        }

        cursor.pos += len;
        *at_line_start = false;
        let kind = if dialect.is_keyword(word) {
            TokenKind::Keyword
        } else {
            TokenKind::Identifier
        };
        return (kind, Mode::Normal);
    }

    // Checked before the leading-dot number rule below: `1..10` is
    // `Number Operator(..) Number`, never `Number Operator(.) Number(.10)` —
    // the second `.` must not be re-read as the start of a decimal once the
    // pair has already been recognized as the range operator.
    if cursor.rest().starts_with("..") {
        cursor.pos += 2;
        *at_line_start = false;
        return (TokenKind::Operator, Mode::Normal);
    }

    if ch.is_ascii_digit() || (ch == '.' && matches!(cursor.peek2(), Some(c) if c.is_ascii_digit()))
    {
        scan_number(cursor);
        *at_line_start = false;
        return (TokenKind::Number, Mode::Normal);
    }

    if ch == ':'
        && dialect.bind_variables
        && let Some(len) = bind_variable_len(cursor.rest())
    {
        cursor.pos += len;
        *at_line_start = false;
        return (TokenKind::BindVariable, Mode::Normal);
    }

    if ch == '&'
        && dialect.substitution_variables
        && let Some(len) = substitution_variable_len(cursor.rest())
    {
        cursor.pos += len;
        *at_line_start = false;
        return (TokenKind::SubstitutionVariable, Mode::Normal);
    }

    if let Some(op) = TWO_CHAR_OPERATORS
        .iter()
        .find(|op| cursor.rest().starts_with(*op))
    {
        advance_by_chars(cursor, op.chars().count());
        *at_line_start = false;
        return (TokenKind::Operator, Mode::Normal);
    }

    cursor.bump();
    *at_line_start = false;
    (TokenKind::Operator, Mode::Normal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::{CommentRules, QuotingRules};

    /// A minimal dialect for unit tests that only exercises the lexer, not
    /// the splitter — no block starters are needed here.
    fn test_dialect() -> SqlDialect {
        SqlDialect {
            statement_terminators: &[';'],
            slash_terminates_block: true,
            block_may_end_without_slash: true,
            block_starters: &[],
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
            keywords: &["SELECT", "FROM", "WHERE", "BEGIN", "END"],
        }
    }

    fn kinds(text: &str) -> Vec<(TokenKind, &str)> {
        tokenize(text, &test_dialect())
            .into_iter()
            .map(|t| (t.kind, &text[t.start..t.end]))
            .collect()
    }

    #[test]
    fn keywords_and_identifiers_are_told_apart_case_insensitively() {
        assert_eq!(
            kinds("select Foo from Bar"),
            [
                (TokenKind::Keyword, "select"),
                (TokenKind::Whitespace, " "),
                (TokenKind::Identifier, "Foo"),
                (TokenKind::Whitespace, " "),
                (TokenKind::Keyword, "from"),
                (TokenKind::Whitespace, " "),
                (TokenKind::Identifier, "Bar"),
            ]
        );
    }

    #[test]
    fn numbers_cover_integers_decimals_and_exponents_but_not_range_dots() {
        assert_eq!(kinds("1")[0], (TokenKind::Number, "1"));
        assert_eq!(kinds("42")[0], (TokenKind::Number, "42"));
        assert_eq!(kinds("3.14")[0], (TokenKind::Number, "3.14"));
        assert_eq!(kinds(".5")[0], (TokenKind::Number, ".5"));
        assert_eq!(kinds("1e10")[0], (TokenKind::Number, "1e10"));
        assert_eq!(kinds("1.5E-3")[0], (TokenKind::Number, "1.5E-3"));
        assert_eq!(
            kinds("2E")[0],
            (TokenKind::Number, "2"),
            "no digits after e: not an exponent"
        );

        // `1..10` is two numbers and a range operator, never one malformed number.
        assert_eq!(
            kinds("1..10"),
            [
                (TokenKind::Number, "1"),
                (TokenKind::Operator, ".."),
                (TokenKind::Number, "10"),
            ]
        );
    }

    #[test]
    fn bind_variables_cover_names_digits_and_quoted_forms_but_not_assignment() {
        assert_eq!(kinds(":name")[0], (TokenKind::BindVariable, ":name"));
        assert_eq!(kinds(":1")[0], (TokenKind::BindVariable, ":1"));
        assert_eq!(
            kinds(":\"Quoted Name\"")[0],
            (TokenKind::BindVariable, ":\"Quoted Name\"")
        );
        assert_eq!(
            kinds(":new.amount"),
            [
                (TokenKind::BindVariable, ":new"),
                (TokenKind::Operator, "."),
                (TokenKind::Identifier, "amount"),
            ]
        );
        // `:=` is assignment, not a placeholder.
        assert_eq!(kinds("x := 1")[2], (TokenKind::Operator, ":="));
    }

    #[test]
    fn substitution_variables_cover_single_and_double_ampersand_and_the_dot_terminator() {
        assert_eq!(
            kinds("&name")[0],
            (TokenKind::SubstitutionVariable, "&name")
        );
        assert_eq!(
            kinds("&&name")[0],
            (TokenKind::SubstitutionVariable, "&&name")
        );
        assert_eq!(kinds("&1")[0], (TokenKind::SubstitutionVariable, "&1"));
        assert_eq!(
            kinds("&&var.suffix"),
            [
                (TokenKind::SubstitutionVariable, "&&var."),
                (TokenKind::Identifier, "suffix"),
            ]
        );
    }

    #[test]
    fn plain_strings_use_doubled_quotes_to_escape() {
        assert_eq!(
            kinds("'it''s fine'")[0],
            (TokenKind::String, "'it''s fine'")
        );
        assert_eq!(
            kinds("\"My Table\"")[0],
            (TokenKind::QuotedIdentifier, "\"My Table\"")
        );
    }

    #[test]
    fn every_alternative_quote_delimiter_form_is_one_string_token() {
        for (source, expected) in [
            ("q'[a]b]'", "q'[a]b]'"),
            ("q'{a}b}'", "q'{a}b}'"),
            ("q'<a>b>'", "q'<a>b>'"),
            ("q'(a)b)'", "q'(a)b)'"),
            ("q'!a!b!'", "q'!a!b!'"),
            ("Q'#a#b#'", "Q'#a#b#'"),
            ("nq'[thai ก]'", "nq'[thai ก]'"),
            ("N'plain national'", "N'plain national'"),
        ] {
            assert_eq!(kinds(source)[0], (TokenKind::String, expected), "{source}");
        }
    }

    #[test]
    fn comments_cover_line_and_block_forms_and_do_not_nest() {
        assert_eq!(
            kinds("-- to end of line\nx")[0],
            (TokenKind::Comment, "-- to end of line")
        );
        assert_eq!(kinds("/* a */ b")[0], (TokenKind::Comment, "/* a */"));
        // Non-nesting: the first `*/` ends it, even with a nested `/*` inside.
        assert_eq!(
            kinds("/* a /* nested */ b */")[0],
            (TokenKind::Comment, "/* a /* nested */")
        );
    }

    #[test]
    fn a_construct_left_open_at_the_end_of_a_block_carries_state_without_becoming_error() {
        let dialect = test_dialect();
        let (tokens, state) = tokenize_block("SELECT 'unterminated", LexState::INITIAL, &dialect);
        assert_eq!(
            tokens.last().expect("at least one token").kind,
            TokenKind::String,
            "not Error mid-document"
        );
        assert!(!state.is_initial());

        // The next block closes it.
        let (tokens2, state2) = tokenize_block("still going'", state, &dialect);
        assert_eq!(tokens2[0].kind, TokenKind::String);
        assert!(state2.is_initial());
    }

    #[test]
    fn tokenize_reports_error_only_when_something_is_open_at_true_end_of_input() {
        let dialect = test_dialect();
        let tokens = tokenize("SELECT 'unterminated", &dialect);
        assert_eq!(
            tokens.last().expect("at least one token").kind,
            TokenKind::Error
        );

        let tokens = tokenize("SELECT 'fine'", &dialect);
        assert_eq!(
            tokens.last().expect("at least one token").kind,
            TokenKind::String
        );
    }

    #[test]
    fn every_byte_of_every_token_stream_is_covered_exactly_once() {
        let dialect = test_dialect();
        for text in [
            "",
            "   ",
            "SELECT 1 FROM t;",
            "SELECT '' FROM t; -- trailing comment",
            "\u{feff}SELECT 1",
            "ข้อความ /* คอมเมนต์ */ 'สตริง'",
        ] {
            let tokens = tokenize(text, &dialect);
            let mut pos = 0usize;
            for token in &tokens {
                assert_eq!(token.start, pos, "{text:?}");
                pos = token.end;
            }
            assert_eq!(pos, text.len(), "{text:?}");
        }
    }

    #[test]
    fn rem_comment_only_fires_at_the_start_of_a_line_and_as_a_whole_word() {
        let mut dialect = test_dialect();
        dialect.comments.sqlplus_rem = true;
        assert_eq!(
            kinds_with(&dialect, "REM a comment\nSELECT 1")[0],
            (TokenKind::Comment, "REM a comment")
        );
        // Not a whole word: "REMOVE" must not be read as "REM" + "OVE".
        assert_eq!(
            kinds_with(&dialect, "REMOVE x")[0],
            (TokenKind::Identifier, "REMOVE"),
            "REM must match a whole word, not a prefix"
        );
        // Not at line start: `SELECT REM FROM t` uses REM as an identifier.
        let mid_line = kinds_with(&dialect, "SELECT REM FROM t");
        assert!(
            mid_line
                .iter()
                .any(|(k, s)| *k == TokenKind::Identifier && *s == "REM"),
            "{mid_line:?}"
        );
    }

    fn kinds_with<'a>(dialect: &SqlDialect, text: &'a str) -> Vec<(TokenKind, &'a str)> {
        tokenize(text, dialect)
            .into_iter()
            .map(|t| (t.kind, &text[t.start..t.end]))
            .collect()
    }
}
