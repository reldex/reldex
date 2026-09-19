//! Rewriting DDL whose body upstream's bind scan misreads (upstream gap U-18).
//!
//! `oracledb`'s SQL parser scans **every** statement for `:name` and turns each
//! hit into a bind placeholder — DDL included, with no branch that stops and no
//! option to turn the scan off (`statement/sql_parser.rs`). `:NEW` and `:OLD`
//! are how a trigger body refers to the row, so the `CREATE TRIGGER` every
//! Oracle IDE has to be able to run comes back as "1 positional bind values are
//! required but 0 were provided": a complaint about a placeholder the caller
//! never wrote. `SPEC.md` §16 lists Triggers as a first-class object group.
//!
//! Spike S12 proved the workaround: submit the same text inside
//! `BEGIN EXECUTE IMMEDIATE q'[…]'; END;`, where the parser skips it because a
//! quoted string is invisible to it. The owner's decision (results file §9
//! item 11, `SPEC.md` §8) is that the driver applies that workaround itself,
//! **on by default**, always telling the user it did and what it sent, with an
//! off switch ([`crate::EXT_REWRITE_TRIGGER_DDL`]) that restores the
//! explanatory refusal.
//!
//! # Why this is vendor code and nothing above it changes
//!
//! Everything here is a property of one version of one Oracle crate. The core
//! never parses SQL (ADR-0002 D6), the statement is still reported as
//! [`StatementKind::Ddl`](reldex_db_driver_api::StatementKind::Ddl) because
//! that is what the user submitted and what the server ran, and the
//! vendor-neutral [`Warning`] type is used exactly as it is.
//!
//! # Scope, and how it grows
//!
//! Only `CREATE … TRIGGER` is rewritten today, because only triggers are known
//! to need it and a rewrite that fires on a statement that did not need one is
//! a silent change to what the user typed. The shape is deliberately general:
//! [`Rewritable`] is the list of DDL kinds whose body upstream misreads, and
//! `CREATE … COMPOUND TRIGGER` and `REFERENCING NEW AS n` (which makes the
//! placeholder `:n`) are already covered, because the rule is "any
//! `CREATE … TRIGGER` in which upstream would see a placeholder", never a list
//! of placeholder names.

use reldex_db_driver_api::{DbError, DbResult, ErrorKind, Warning, WarningKind};

use crate::classify::Keywords;

/// The largest DDL this can wrap, in bytes.
///
/// `EXECUTE IMMEDIATE` of a *literal* passes a PL/SQL `VARCHAR2`, whose limit
/// is 32767 bytes — bytes, not characters, which is why Thai or emoji in a
/// trigger body reaches it sooner than its length suggests. Beyond that the
/// server rejects the block for a reason that has nothing to do with the
/// trigger, so this refuses first and says why. Verified against the live
/// database by `s12b_trigger_rewrite.rs`.
const MAX_WRAPPED_BYTES: usize = 32767;

/// Alternative-quote delimiters, tried in order.
///
/// A `q'X…X'` literal ends at the first occurrence of its **closing sequence**
/// — the closing delimiter followed by a quote — so the only requirement is
/// that the body does not contain that two-character sequence. The paired forms
/// come first because they are what a reader expects; the unpaired ones are the
/// escape hatch for a body that contains `]'`, `}'`, `>'` and `)'` alike.
/// Oracle forbids a space, tab, newline or single quote as the delimiter, and
/// none of these is one.
const DELIMITERS: [char; 12] = ['[', '{', '<', '(', '!', '~', '^', '#', '|', '+', '@', '$'];

/// A kind of DDL whose body upstream's bind scan misreads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rewritable {
    /// `CREATE [OR REPLACE] [EDITIONABLE | NONEDITIONABLE] TRIGGER …`,
    /// including the compound form.
    Trigger,
}

impl Rewritable {
    /// What the user called it, for the warning.
    const fn what(self) -> &'static str {
        match self {
            Self::Trigger => "CREATE TRIGGER",
        }
    }

    /// Recognizes this kind from a statement's leading keywords.
    ///
    /// Uses [`Keywords`], the same scanner `classify` uses, so a statement
    /// cannot be classified by one set of rules and rewritten by another.
    /// Leading whitespace, a byte-order mark and `--` / `/* … */` comments are
    /// skipped by that scanner, so none of them defeats detection.
    fn detect(sql: &str) -> Option<Self> {
        let mut words = Keywords::new(sql);
        if words.next().as_deref() != Some("CREATE") {
            return None;
        }
        let mut word = words.next();
        if word.as_deref() == Some("OR") {
            if words.next().as_deref() != Some("REPLACE") {
                return None;
            }
            word = words.next();
        }
        if matches!(word.as_deref(), Some("EDITIONABLE" | "NONEDITIONABLE")) {
            word = words.next();
        }
        (word.as_deref() == Some("TRIGGER")).then_some(Self::Trigger)
    }
}

/// A statement to send in place of what the caller submitted.
#[derive(Debug, Clone)]
pub(crate) struct Rewrite {
    kind: Rewritable,
    /// The PL/SQL block actually sent to the server.
    text: String,
}

impl Rewrite {
    /// The text to send instead of the caller's.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// The warning that must travel with the outcome.
    ///
    /// The exact text sent is **in the message**. [`Warning`] carries a kind, a
    /// message, an optional native error and an optional position, and none of
    /// those is a "the driver substituted this text" slot; adding one to the
    /// vendor-neutral type for a single driver's workaround would be a poor
    /// trade, so the message carries it and says that it is doing so. If more
    /// drivers turn out to rewrite statements, a structured field is the right
    /// answer and this is what it would replace.
    pub(crate) fn warning(&self) -> Warning {
        Warning::new(
            WarningKind::Informational,
            format!(
                "this {} statement was rewritten before it was sent. The Oracle crate this \
                 driver wraps scans every statement for `:name` and treats each hit as a bind \
                 placeholder, DDL included, so a trigger body that mentions `:NEW` or `:OLD` \
                 is rejected as missing a bind value the caller never wrote (upstream gap \
                 U-18). It was therefore submitted inside `BEGIN EXECUTE IMMEDIATE q'…'; \
                 END;`, where a quoted string is invisible to that parser. The trigger the \
                 server created is exactly the one written here; what changed is how it was \
                 transmitted. Set the connection extension \"{}\" to false to have such a \
                 statement refused instead of rewritten. The exact text sent was:\n{}",
                self.kind.what(),
                crate::EXT_REWRITE_TRIGGER_DDL,
                self.text
            ),
        )
    }
}

/// Decides whether `sql` has to be rewritten, and builds the replacement.
///
/// `Ok(None)` means send it unchanged — which is the answer for everything that
/// is not a trigger, and for a trigger upstream's scan would leave alone.
///
/// # Errors
///
/// [`ErrorKind::Unsupported`] when the statement needs the rewrite and is too
/// large to wrap ([`MAX_WRAPPED_BYTES`]). Refusing with that reason is better
/// than sending a block the server rejects for an unrelated one.
pub(crate) fn plan(sql: &str) -> DbResult<Option<Rewrite>> {
    let Some(kind) = Rewritable::detect(sql) else {
        return Ok(None);
    };
    if !upstream_finds_a_bind_placeholder(sql) {
        // Nothing to work around. Leaving it alone keeps the statement's error
        // positions and its reported kind exactly as the server produced them.
        return Ok(None);
    }

    let body = strip_sqlplus_terminator(sql);
    if body.len() > MAX_WRAPPED_BYTES {
        return Err(DbError::new(
            ErrorKind::Unsupported,
            format!(
                "this {} statement is {} bytes, and the only way this driver can submit a \
                 trigger body containing `:NEW` or `:OLD` is inside \
                 `EXECUTE IMMEDIATE q'…'`, whose PL/SQL string literal holds at most \
                 {MAX_WRAPPED_BYTES} bytes (upstream gap U-18: the Oracle crate reads those \
                 as bind placeholders and offers no way to stop it). Shorten the trigger — \
                 moving its body into a package procedure the trigger calls is the usual \
                 way — or create it with another client",
                kind.what(),
                body.len()
            ),
        ));
    }

    Ok(Some(Rewrite {
        kind,
        text: wrap(body),
    }))
}

/// Builds `BEGIN EXECUTE IMMEDIATE <literal>; END;` around `body`.
fn wrap(body: &str) -> String {
    let literal = match DELIMITERS
        .iter()
        .copied()
        .find(|delimiter| !body.contains(&closing_sequence(*delimiter)))
    {
        Some(delimiter) => format!("q'{delimiter}{body}{}'", closing(delimiter)),
        // Every alternative-quote delimiter collides with the body. An ordinary
        // literal with its quotes doubled always works — it is only second
        // choice because it changes the text, where a q-string does not.
        None => format!("'{}'", body.replace('\'', "''")),
    };
    format!("BEGIN EXECUTE IMMEDIATE {literal}; END;")
}

/// The closing delimiter for an opening one; Oracle pairs four of them and
/// leaves every other character its own closer.
const fn closing(delimiter: char) -> char {
    match delimiter {
        '[' => ']',
        '{' => '}',
        '<' => '>',
        '(' => ')',
        other => other,
    }
}

/// What actually terminates a `q'X…` literal: the closing delimiter followed by
/// a quote. A body may contain the closing delimiter alone as often as it likes.
fn closing_sequence(delimiter: char) -> String {
    format!("{}'", closing(delimiter))
}

/// Removes a trailing SQL\*Plus `/` terminator line and any trailing
/// whitespace.
///
/// A `/` on a line of its own is SQL\*Plus telling *itself* to submit the
/// buffer; it is not part of the statement, and leaving it inside
/// `EXECUTE IMMEDIATE` makes the server reject the trigger. The trigger's own
/// final `END;` is kept, because that one is PL/SQL.
fn strip_sqlplus_terminator(sql: &str) -> &str {
    let trimmed = sql.trim_end();
    let Some(last_line) = trimmed.rfind('\n') else {
        // A single line: a lone `/` would be the whole statement, which is not
        // a trigger, so there is nothing to strip.
        return trimmed;
    };
    if trimmed[last_line + 1..].trim() == "/" {
        trimmed[..last_line].trim_end()
    } else {
        trimmed
    }
}

/// Whether `oracledb` would find at least one bind placeholder in this text.
///
/// A **literal** transcription of `statement/sql_parser.rs`'s `parse` loop and
/// its `parse_bind_name`, `parse_qstring`, `parse_quoted_string`,
/// `parse_multiple_line_comment` and `skip_to_end_of_line` helpers in
/// `=26.0.0-beta.3`, written the way
/// [`crate::classify::upstream_would_return_rows`] is and for the same reason:
/// the question "would upstream misread this statement" must be answered by
/// upstream's own rules, not by an approximation of them. Getting it wrong is
/// silent in both directions — a rewrite that was not needed changes what the
/// user typed, and a missing one hands back the bind-count complaint U-18 is
/// about.
///
/// The parts that matter and are easy to get wrong:
///
/// - `:=` is **not** a placeholder. After the colon upstream skips whitespace
///   and then requires `"`, a digit or an *alphabetic* character; `=` is none
///   of those, so `:NEW.made := SYSDATE` contains exactly one placeholder.
/// - A colon that follows a **string** is JSON constant syntax, not a bind, and
///   whitespace between them does not change that.
/// - Comments and quoted strings — including `q'…'` — are skipped, which is the
///   whole reason the `EXECUTE IMMEDIATE` workaround works.
pub(crate) fn upstream_finds_a_bind_placeholder(sql: &str) -> bool {
    let text: Vec<char> = sql.chars().collect();
    let mut position = 0_usize;
    let mut last_was_string = false;
    let mut last_ch = ' ';

    while position < text.len() {
        let ch = text[position];
        if ch == '\'' {
            last_was_string = true;
            if last_ch.eq_ignore_ascii_case(&'q') {
                position = skip_q_string(&text, position);
            } else {
                position = skip_quoted(&text, position, '\'');
            }
        } else if !ch.is_whitespace() {
            if ch == '-' && last_ch == '-' {
                position = skip_to_end_of_line(&text, position);
            } else if ch == '*' && last_ch == '/' {
                position = skip_block_comment(&text, position);
            } else if ch == '"' {
                position = skip_quoted(&text, position, '"');
            } else if ch == ':' && !last_was_string && names_a_bind(&text, position) {
                return true;
            }
            last_was_string = false;
        }
        last_ch = ch;
        position += 1;
    }
    false
}

/// Upstream's `parse_bind_name`, reduced to the one question asked here: does a
/// bind get added at all?
fn names_a_bind(text: &[char], colon: usize) -> bool {
    let mut position = colon + 1;
    while position < text.len() {
        let ch = text[position];
        if ch.is_whitespace() {
            position += 1;
            continue;
        }
        // Upstream: a quoted name, a digits-only name, or one starting with an
        // alphabetic character. Anything else restores the position and adds
        // nothing — which is what keeps `:=` out.
        return ch == '"' || ch.is_ascii_digit() || ch.is_alphabetic();
    }
    false
}

/// Leaves the position **on** the closing quote, as upstream's
/// `parse_quoted_string` does; the caller's `position += 1` steps past it.
fn skip_quoted(text: &[char], opening: usize, quote: char) -> usize {
    let mut position = opening + 1;
    while position < text.len() && text[position] != quote {
        position += 1;
    }
    position.min(text.len())
}

/// Upstream's `parse_qstring`: the literal ends at the first closing delimiter
/// immediately followed by a quote, and the position is left on that quote.
fn skip_q_string(text: &[char], opening: usize) -> usize {
    let mut position = opening + 1;
    let Some(separator) = text.get(position).copied() else {
        return position;
    };
    position += 1;
    let end = closing(separator);
    while position < text.len() {
        let ch = text[position];
        position += 1;
        if ch == end && text.get(position) == Some(&'\'') {
            return position;
        }
    }
    text.len()
}

/// Upstream's `skip_to_end_of_line` consumes the newline **and** the caller's
/// loop then steps over one more character. Transcribed as it is, including
/// that: a rule this function is not entitled to improve on.
fn skip_to_end_of_line(text: &[char], from: usize) -> usize {
    let mut position = from;
    while position < text.len() {
        let ch = text[position];
        position += 1;
        if ch == '\n' {
            break;
        }
    }
    position
}

/// Upstream's `parse_multiple_line_comment`, entered on the `*` of `/*` and
/// leaving the position on the `/` of `*/`.
fn skip_block_comment(text: &[char], star: usize) -> usize {
    let mut position = star + 1;
    let mut previous = ' ';
    while position < text.len() {
        if text[position] == '/' && previous == '*' {
            return position;
        }
        previous = text[position];
        position += 1;
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewritten(sql: &str) -> String {
        plan(sql)
            .unwrap_or_else(|error| panic!("{sql}\n  refused: {error}"))
            .unwrap_or_else(|| panic!("{sql}\n  was not rewritten"))
            .text()
            .to_owned()
    }

    fn unchanged(sql: &str) -> bool {
        matches!(plan(sql), Ok(None))
    }

    // ----------------------------------------------------------- detection

    #[test]
    fn every_spelling_of_create_trigger_with_a_placeholder_is_rewritten() {
        for sql in [
            "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN :NEW.a := 1; END;",
            "create or replace trigger t before insert on x for each row \
             begin :new.a := 1; end;",
            "CREATE OR REPLACE EDITIONABLE TRIGGER t BEFORE INSERT ON x FOR EACH ROW \
             BEGIN :NEW.a := 1; END;",
            "CREATE OR REPLACE NONEDITIONABLE TRIGGER t BEFORE INSERT ON x FOR EACH ROW \
             BEGIN :NEW.a := 1; END;",
            "CREATE EDITIONABLE TRIGGER t BEFORE INSERT ON x FOR EACH ROW \
             BEGIN :OLD.a := 1; END;",
            // The placeholder name is irrelevant: `REFERENCING NEW AS n` makes
            // it `:n`, and a compound trigger is still a trigger.
            "CREATE TRIGGER t BEFORE INSERT ON x REFERENCING NEW AS n FOR EACH ROW \
             BEGIN :n.a := 1; END;",
            "CREATE OR REPLACE TRIGGER t FOR INSERT ON x COMPOUND TRIGGER \
             BEFORE EACH ROW IS BEGIN :NEW.a := 1; END BEFORE EACH ROW; END t;",
        ] {
            assert!(
                plan(sql).is_ok_and(|planned| planned.is_some()),
                "not rewritten: {sql}"
            );
        }
    }

    #[test]
    fn leading_comments_and_whitespace_do_not_defeat_detection() {
        for sql in [
            "  \n\t CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN :NEW.a := 1; END;",
            "-- make the trigger\nCREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW \
             BEGIN :NEW.a := 1; END;",
            "/* a block comment */ CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW \
             BEGIN :NEW.a := 1; END;",
            "\u{feff}CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN :NEW.a := 1; END;",
        ] {
            assert!(
                plan(sql).is_ok_and(|planned| planned.is_some()),
                "not rewritten: {sql}"
            );
        }
    }

    #[test]
    fn nothing_but_a_trigger_upstream_would_misread_is_rewritten() {
        for sql in [
            // Not a trigger.
            "CREATE PACKAGE p AS PROCEDURE go; END;",
            "CREATE OR REPLACE PROCEDURE p IS BEGIN NULL; END;",
            "CREATE TABLE t (id NUMBER)",
            "ALTER TRIGGER t DISABLE",
            "DROP TRIGGER t",
            // A PL/SQL block with a real bind the caller *did* write.
            "BEGIN UPDATE t SET a = :value; END;",
            // A trigger with no placeholder at all: upstream runs it happily,
            // so rewriting it would change the statement for nothing.
            "CREATE TRIGGER t BEFORE INSERT ON x BEGIN NULL; END;",
            // The words inside a string literal are not a statement.
            "SELECT 'CREATE TRIGGER t BEFORE INSERT ON x BEGIN :NEW.a := 1; END;' FROM dual",
            "INSERT INTO log VALUES ('CREATE TRIGGER … :NEW …')",
        ] {
            assert!(unchanged(sql), "wrongly rewritten: {sql}");
        }
    }

    // ------------------------------------------- upstream's placeholder scan

    #[test]
    fn the_upstream_scan_agrees_with_upstreams_own_rules() {
        // Found.
        for sql in [
            ":NEW.made := SYSDATE",
            "BEGIN :OLD.a := 1; END;",
            "SELECT * FROM t WHERE id = :1",
            "SELECT * FROM t WHERE id = :\"Quoted Name\"",
            // Upstream skips whitespace after the colon before deciding.
            "SELECT * FROM t WHERE id = :   value",
        ] {
            assert!(upstream_finds_a_bind_placeholder(sql), "missed: {sql}");
        }

        // Not found.
        for sql in [
            // Assignment, not a placeholder: `=` is not alphabetic.
            "BEGIN x := 1; END;",
            // Inside strings and comments of every shape.
            "SELECT ':NEW' FROM dual",
            "SELECT q'[:NEW]' FROM dual",
            "SELECT q'{:NEW}' FROM dual",
            "-- :NEW\nSELECT 1 FROM dual",
            "/* :NEW */ SELECT 1 FROM dual",
            "SELECT \":NEW\" FROM dual",
            // A colon straight after a string is JSON constant syntax upstream,
            // and whitespace between them does not change that.
            "SELECT JSON_OBJECT('a' : 1) FROM dual",
            // Nothing after the colon at all.
            "SELECT 1 FROM dual WHERE x = :",
        ] {
            assert!(!upstream_finds_a_bind_placeholder(sql), "false hit: {sql}");
        }
    }

    #[test]
    fn a_wrapped_trigger_is_invisible_to_the_scan_which_is_why_this_works() {
        let sql = "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN :NEW.a := 1; END;";
        assert!(upstream_finds_a_bind_placeholder(sql));
        assert!(
            !upstream_finds_a_bind_placeholder(&rewritten(sql)),
            "the rewrite must leave upstream nothing to misread"
        );
    }

    // -------------------------------------------------------- the delimiter

    #[test]
    fn the_first_delimiter_that_cannot_collide_with_the_body_is_used() {
        let plain = "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN :NEW.a := 1; END;";
        assert_eq!(
            rewritten(plain),
            format!("BEGIN EXECUTE IMMEDIATE q'[{plain}]'; END;")
        );

        // A body that contains the `[` form's closing sequence moves to `{`.
        let bracket = "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW \
                       BEGIN :NEW.a := q'[x]'; END;";
        let wrapped = rewritten(bracket);
        assert!(
            wrapped.starts_with("BEGIN EXECUTE IMMEDIATE q'{"),
            "{wrapped}"
        );
        assert!(wrapped.ends_with("}'; END;"), "{wrapped}");

        // A body containing `]'` and `}'` moves again.
        let two = "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW \
                   BEGIN :NEW.a := ']''}'''; END;";
        let wrapped = rewritten(two);
        assert!(
            wrapped.starts_with("BEGIN EXECUTE IMMEDIATE q'<"),
            "{wrapped}"
        );
    }

    #[test]
    fn a_body_that_collides_with_every_delimiter_falls_back_to_doubled_quotes() {
        // One `X'` for every candidate, so no alternative quote can be used.
        let collisions: String = DELIMITERS
            .iter()
            .map(|delimiter| format!("{}'", closing(*delimiter)))
            .collect();
        let sql = format!(
            "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW \
             BEGIN :NEW.a := '{collisions}'; END;"
        );
        let wrapped = rewritten(&sql);
        assert!(
            wrapped.starts_with("BEGIN EXECUTE IMMEDIATE '"),
            "expected the doubled-quote fallback: {wrapped}"
        );
        assert!(
            !upstream_finds_a_bind_placeholder(&wrapped),
            "the fallback must hide the placeholder just as well: {wrapped}"
        );
        // Doubling every quote is the whole of the escaping — stated as the
        // exact text rather than reconstructed, because the statement's own
        // `'; END;` and the wrapper's are indistinguishable from the outside.
        assert_eq!(
            wrapped,
            format!(
                "BEGIN EXECUTE IMMEDIATE '{}'; END;",
                strip_sqlplus_terminator(&sql).replace('\'', "''")
            )
        );
    }

    // --------------------------------------------------------- the trimming

    #[test]
    fn a_trailing_sqlplus_terminator_is_stripped_and_the_triggers_own_end_is_kept() {
        let body = "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW\nBEGIN :NEW.a := 1; END;";
        for submitted in [
            format!("{body}\n/"),
            format!("{body}\n/\n"),
            format!("{body}\n  /  \n\n"),
            format!("{body}\n/\r\n"),
        ] {
            assert_eq!(strip_sqlplus_terminator(&submitted), body, "{submitted:?}");
        }
        assert!(
            strip_sqlplus_terminator(body).ends_with("END;"),
            "the trigger's own final END; is PL/SQL and stays"
        );
        // A `/` that is not alone on the last line is part of the statement.
        let divides = "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW\n\
                       BEGIN :NEW.a := 1 / 2; END;";
        assert_eq!(strip_sqlplus_terminator(divides), divides);
    }

    // ------------------------------------------------------------ the limit

    #[test]
    fn a_body_too_large_for_a_plsql_literal_is_refused_with_the_reason() {
        let filler = "x".repeat(MAX_WRAPPED_BYTES);
        let sql = format!(
            "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW \
             BEGIN :NEW.a := '{filler}'; END;"
        );
        let error = match plan(&sql) {
            Ok(planned) => panic!("expected a refusal, got {planned:?}"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(
            error.message().contains(&MAX_WRAPPED_BYTES.to_string()),
            "the message must name the limit: {error}"
        );

        // Just inside the limit still works.
        let body = "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN :NEW.a := 1; END;";
        let padding = MAX_WRAPPED_BYTES - body.len();
        let sql = format!(
            "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN :NEW.a := 1; END;{}",
            " ".repeat(padding)
        );
        // Trailing whitespace is trimmed before the measurement, so this one is
        // comfortably inside it.
        assert!(plan(&sql).is_ok_and(|planned| planned.is_some()));
    }

    // ---------------------------------------------------------- the warning

    #[test]
    fn the_warning_names_the_cause_the_switch_and_the_exact_text_sent() {
        let sql = "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN :NEW.a := 1; END;";
        let planned = plan(sql)
            .expect("a plain trigger is rewritable")
            .expect("a plain trigger is rewritten");
        let warning = planned.warning();
        assert_eq!(warning.kind(), WarningKind::Informational);
        let message = warning.message();
        assert!(message.contains("U-18"), "{message}");
        assert!(message.contains(":NEW"), "{message}");
        assert!(message.contains("EXECUTE IMMEDIATE"), "{message}");
        assert!(
            message.contains(crate::EXT_REWRITE_TRIGGER_DDL),
            "the off switch must be named: {message}"
        );
        assert!(
            message.contains(planned.text()),
            "the exact text sent must be available for inspection: {message}"
        );
    }
}
