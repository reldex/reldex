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
//! # Two jobs, one switch
//!
//! [`plan`] decides two things about a statement, and both are governed by that
//! one extension:
//!
//! 1. **Normalisation**, for every trigger this module recognises: the
//!    punctuation a SQL\*Plus user types for their own client is removed, and
//!    the PL/SQL the server needs is not. See [`normalize`].
//! 2. **Wrapping**, only for the triggers upstream's scan would misread.
//!
//! The first applies whether or not the second does, which is the point:
//! two statements that differ only in a `:NEW` must not differ in whether their
//! terminator is understood.
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
/// trigger, so this refuses first and says why.
///
/// # What is measured, and why it is the body
///
/// The limit is on what the literal is **worth**, not on how long it is
/// *written*, and the body is exactly its value on both wrapping paths: `q'X…X'`
/// quotes the body verbatim, and the doubled-quote fallback un-escapes back to
/// it. The source text is longer than the value on both — four characters for a
/// q-string, one per quote for the fallback, so a body at the limit made almost
/// entirely of quotes is written out in nearly 64 KB — and none of that counts.
///
/// Measured against the live database rather than reasoned about, by
/// `the_plsql_string_literal_limit_is_on_the_value_not_on_the_source_text` in
/// `s12b_trigger_rewrite.rs`: a 32767-byte value is accepted when written as
/// 32772, as 36769 and as 65512 bytes of source, and a 32768-byte value is
/// rejected with `PLS-00172` however it is written. Measuring the emitted
/// literal instead would refuse statements the server accepts.
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

/// What the driver decided to send in place of the text the caller submitted.
#[derive(Debug, Clone)]
pub(crate) enum Plan<'a> {
    /// Send the statement exactly as it was written.
    AsSubmitted,
    /// Send this: the same statement with client-side punctuation removed — a
    /// trailing SQL\*Plus `/` line, and the statement terminator after a trigger
    /// body that is not a PL/SQL block. See [`normalize`] for what that means
    /// and why it is not reported as a change.
    Trimmed(&'a str),
    /// Send the wrapped form, and tell the caller that it was wrapped.
    Wrapped(Rewrite),
}

impl Plan<'_> {
    /// The text to send, given the text the caller submitted.
    pub(crate) fn text<'b>(&'b self, submitted: &'b str) -> &'b str {
        match self {
            Self::AsSubmitted => submitted,
            Self::Trimmed(text) => text,
            Self::Wrapped(rewrite) => &rewrite.text,
        }
    }

    /// The rewrite, when the statement was wrapped.
    ///
    /// A wrapped statement arrives back from the server as a PL/SQL block's
    /// outcome rather than as the DDL's, so the caller has repairs to make that
    /// the other two plans do not; see `conn::finish_rewritten`.
    pub(crate) fn wrapped(&self) -> Option<&Rewrite> {
        match self {
            Self::Wrapped(rewrite) => Some(rewrite),
            Self::AsSubmitted | Self::Trimmed(_) => None,
        }
    }
}

/// A statement wrapped so upstream's bind scan cannot see into it.
#[derive(Debug, Clone)]
pub(crate) struct Rewrite {
    kind: Rewritable,
    /// The PL/SQL block actually sent to the server.
    text: String,
}

impl Rewrite {
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

/// Decides what to send for `sql`.
///
/// [`Plan::AsSubmitted`] is the answer for everything that is not a recognised
/// trigger. Trigger DDL is normalised either way ([`Plan::Trimmed`]) and wrapped
/// only when upstream's scan would misread it ([`Plan::Wrapped`]), so a trigger
/// that needs no rewrite keeps the server's own error positions and reported
/// kind.
///
/// # Errors
///
/// [`ErrorKind::Unsupported`] when the statement needs the rewrite and is too
/// large to wrap ([`MAX_WRAPPED_BYTES`]). Refusing with that reason is better
/// than sending a block the server rejects for an unrelated one.
pub(crate) fn plan(sql: &str) -> DbResult<Plan<'_>> {
    let Some(kind) = Rewritable::detect(sql) else {
        return Ok(Plan::AsSubmitted);
    };

    // Normalising before the scan, not after: the punctuation removed here
    // cannot contain a placeholder, and the text that gets scanned has to be the
    // text that gets sent.
    let body = normalize(sql);
    if !upstream_finds_a_bind_placeholder(body) {
        // Nothing to work around, so nothing is wrapped. The normalisation still
        // applies — see `normalize` for why it has to apply to every trigger the
        // driver recognises rather than only to the ones it wraps.
        return Ok(if body.len() == sql.len() {
            Plan::AsSubmitted
        } else {
            Plan::Trimmed(body)
        });
    }

    if body.len() > MAX_WRAPPED_BYTES {
        return Err(DbError::new(
            ErrorKind::Unsupported,
            format!(
                "this {} statement is {} bytes, and the only way this driver can submit a \
                 trigger body containing `:NEW` or `:OLD` is inside \
                 `EXECUTE IMMEDIATE q'…'`, whose PL/SQL string literal can be worth at most \
                 {MAX_WRAPPED_BYTES} bytes (upstream gap U-18: the Oracle crate reads those \
                 as bind placeholders and offers no way to stop it). It is the trigger text \
                 itself that has to fit; how long the literal is written out does not \
                 matter. Shorten the trigger — moving its body into a package procedure the \
                 trigger calls is the usual way — or create it with another client",
                kind.what(),
                body.len()
            ),
        ));
    }

    Ok(Plan::Wrapped(Rewrite {
        kind,
        text: wrap(body),
    }))
}

/// Removes the punctuation a SQL\*Plus user types that the server must not see.
///
/// Applied to **every** trigger this module recognises, not only to the ones it
/// wraps. Doing it only on the wrapped path made the driver's behaviour depend
/// on something the user cannot see: `CREATE TRIGGER … :NEW … END;\n/` worked
/// because it was wrapped and trimmed on the way, while the same trigger without
/// a placeholder was sent with its `/` attached and failed `ORA-00911`. Two
/// statements that differ only in a `:NEW` must not differ in whether their
/// terminator is understood.
///
/// Two things are removed, both of them addressed to the client rather than to
/// the server:
///
/// - a trailing `/` on a line of its own, which is SQL\*Plus telling *itself* to
///   submit the buffer ([`strip_sqlplus_terminator`]);
/// - the statement terminator after a trigger body that is **not** a PL/SQL
///   block — `… FOR EACH ROW CALL p(:NEW.id);` — which the server rejects with
///   `ORA-00911` whether it is sent directly or inside `EXECUTE IMMEDIATE`
///   ([`strip_statement_terminator`]).
///
/// The `END;` that closes a PL/SQL body is *not* removed: that semicolon is part
/// of the language, not punctuation for the client.
fn normalize(sql: &str) -> &str {
    strip_statement_terminator(strip_sqlplus_terminator(sql))
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

/// Removes a trailing statement terminator that PL/SQL would not have written.
///
/// A trigger whose body is a PL/SQL block ends with that block's own `END;`, and
/// that semicolon has to stay. A trigger whose body is a `CALL` ends with the
/// call, and the `;` a user types after it is SQL\*Plus punctuation the server
/// rejects (`ORA-00911`) — directly and inside `EXECUTE IMMEDIATE` alike.
///
/// **Conservative by construction:** the semicolon is removed only when the text
/// before it clearly does not close a PL/SQL block. Anything this cannot read —
/// a quoted trailing identifier such as `END "My Trigger"`, and equally
/// `CALL "My Proc"` — is left exactly as the user wrote it, because sending a
/// statement the server rejects is a far smaller harm than silently deleting a
/// character that mattered.
fn strip_statement_terminator(text: &str) -> &str {
    match text.strip_suffix(';') {
        Some(without) if !closes_a_plsql_block(without) => without.trim_end(),
        _ => text,
    }
}

/// Whether `text` ends where a PL/SQL block ends: `END`, `END name` or
/// `END "name"`.
fn closes_a_plsql_block(text: &str) -> bool {
    let text = text.trim_end();
    // A quoted name is the case this deliberately does not try to read: both
    // `END "My Trigger"` and `CALL "My Proc"` end this way, telling them apart
    // means parsing the statement, and "leave it alone" is the safe answer.
    if text.ends_with('"') {
        return true;
    }
    let (before_last, last) = split_last_word(text);
    if last.eq_ignore_ascii_case("END") {
        return true;
    }
    if last.is_empty() {
        // The body ends in punctuation — `CALL p(:NEW.id)` — so there is no
        // `END` to be closing anything.
        return false;
    }
    // `END my_trigger`, which is how a compound trigger closes.
    split_last_word(before_last.trim_end())
        .1
        .eq_ignore_ascii_case("END")
}

/// Splits off the trailing run of identifier characters, which is empty when the
/// text ends in punctuation.
fn split_last_word(text: &str) -> (&str, &str) {
    let start = text
        .char_indices()
        .rev()
        .take_while(|(_, ch)| is_identifier_char(*ch))
        .last()
        .map_or(text.len(), |(index, _)| index);
    text.split_at(start)
}

/// Oracle's unquoted-identifier characters, minus the rule that the first must
/// be a letter — which does not matter for reading the last word of a statement.
fn is_identifier_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '$' | '#')
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
        match plan(sql) {
            Ok(Plan::Wrapped(rewrite)) => rewrite.text.clone(),
            Ok(other) => panic!("{sql}\n  was not rewritten: {other:?}"),
            Err(error) => panic!("{sql}\n  refused: {error}"),
        }
    }

    fn unchanged(sql: &str) -> bool {
        matches!(plan(sql), Ok(Plan::AsSubmitted))
    }

    /// What would actually be sent for `sql`.
    fn sent(sql: &str) -> String {
        plan(sql)
            .unwrap_or_else(|error| panic!("{sql}\n  refused: {error}"))
            .text(sql)
            .to_owned()
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
                matches!(plan(sql), Ok(Plan::Wrapped(_))),
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
                matches!(plan(sql), Ok(Plan::Wrapped(_))),
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
                normalize(&sql).replace('\'', "''")
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
    }

    #[test]
    fn a_slash_that_is_not_a_terminator_line_of_its_own_is_never_stripped() {
        for kept in [
            // Division, on the last line.
            "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW\nBEGIN :NEW.a := 1 / 2; END;",
            // A `/` at the end of a line that also holds the statement.
            "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW\nBEGIN :NEW.a := 1; END;/",
            // A `/` inside a literal on the last line.
            "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW\nBEGIN :NEW.a := '/'; END;",
            // A trailing comment line is not a terminator either.
            "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW\nBEGIN :NEW.a := 1; END;\n-- /",
        ] {
            assert_eq!(strip_sqlplus_terminator(kept), kept, "{kept:?}");
        }
    }

    #[test]
    fn the_terminator_is_stripped_for_every_trigger_the_driver_recognises_not_only_wrapped_ones() {
        // The asymmetry this exists to prevent: two statements that differ only
        // in a `:NEW` must not differ in whether their `/` is understood.
        let with_placeholder =
            "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW\nBEGIN :NEW.a := 1; END;\n/";
        let without = "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW\nBEGIN NULL; END;\n/";

        assert!(
            !sent(with_placeholder).contains("END;\n/"),
            "a wrapped trigger loses its terminator: {}",
            sent(with_placeholder)
        );
        assert_eq!(
            sent(without),
            "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW\nBEGIN NULL; END;",
            "so must a trigger that needs no wrapping"
        );
        assert!(
            matches!(plan(without), Ok(Plan::Trimmed(_))),
            "trimming without wrapping is its own plan, and carries no warning"
        );
        assert!(
            plan(without)
                .expect("a plain trigger is not refused")
                .wrapped()
                .is_none(),
            "trimming must not be mistaken for a rewrite"
        );
    }

    #[test]
    fn a_statement_terminator_after_a_call_body_goes_and_a_plsql_end_stays() {
        // `CALL` is a trigger body that is not a PL/SQL block, so the `;` a
        // SQL*Plus user types after it is punctuation the server rejects.
        assert_eq!(
            sent("CREATE TRIGGER t AFTER INSERT ON x FOR EACH ROW CALL p(:NEW.id);"),
            "BEGIN EXECUTE IMMEDIATE \
             q'[CREATE TRIGGER t AFTER INSERT ON x FOR EACH ROW CALL p(:NEW.id)]'; END;"
        );
        // …including behind a `/` line, and when no rewrite is needed.
        assert_eq!(
            sent("CREATE TRIGGER t AFTER INSERT ON x FOR EACH ROW CALL p(1);\n/"),
            "CREATE TRIGGER t AFTER INSERT ON x FOR EACH ROW CALL p(1)"
        );

        // A PL/SQL block's own `END;` is the language, not punctuation.
        for kept in [
            "BEGIN :NEW.a := 1; END;",
            "BEGIN :NEW.a := 1; END t;",
            "BEGIN :NEW.a := 1; end My_Trigger;",
            "BEGIN :NEW.a := 1; END \"My Trigger\";",
            // Uncertain on purpose: a quoted name could be either, so it stays.
            "CALL \"My Proc\";",
        ] {
            assert_eq!(strip_statement_terminator(kept), kept, "{kept:?}");
        }
        for (submitted, stripped) in [
            ("CALL p(1);", "CALL p(1)"),
            ("CALL p;", "CALL p"),
            ("CALL p(1)  ;  ", "CALL p(1)"),
            // A doubled terminator loses one and keeps PL/SQL's own.
            ("BEGIN NULL; END;;", "BEGIN NULL; END;"),
        ] {
            assert_eq!(
                strip_statement_terminator(submitted.trim_end()),
                stripped,
                "{submitted:?}"
            );
        }
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
        assert!(matches!(plan(&sql), Ok(Plan::Wrapped(_))));
    }

    // ---------------------------------------------------------- the warning

    #[test]
    fn the_warning_names_the_cause_the_switch_and_the_exact_text_sent() {
        let sql = "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN :NEW.a := 1; END;";
        let planned = match plan(sql) {
            Ok(Plan::Wrapped(rewrite)) => rewrite,
            other => panic!("a plain trigger is rewritten, got {other:?}"),
        };
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
            message.contains(&planned.text),
            "the exact text sent must be available for inspection: {message}"
        );
    }
}
