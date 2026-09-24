//! A grammar-based differential/property test (M2.4 round-2 adversarial
//! review's "acceptance gate", extended in round 3): [`corpus.rs`]'s fuzz
//! test assembles scripts out of independent, randomly-chosen *fragments*,
//! which is excellent at finding lexing/scanning edge cases but — because a
//! randomly-chosen sequence of fragments essentially never produces a
//! deeply, evenly nested structure — is nearly powerless against a
//! depth-tracking bug that only shows up once nesting is *balanced*
//! (round-2 MUST-FIX #1: 6,000,000 fuzz cases found no instance of it).
//!
//! This test instead generates whole, structurally-**valid**, self-contained
//! PL/SQL "units" from a small grammar, concatenates several of them, and
//! asserts [`split_statements`] recovers **exactly** the units that went in:
//! same count, in order, each one's own [`StatementSpan::content`] equal
//! (see "The trimming rule" below) to that unit's own generated text, with
//! the expected [`StatementKind`]/[`EndedBy`], and every invariant
//! `corpus.rs`'s `assert_span_invariants` checks. A unit's span reaching
//! into a neighboring unit's own text — swallowing it, or being carved out
//! of the middle of it — is exactly the dangerous mis-split class both
//! review rounds were hunting for, and a plain `spans.len() == units.len()`
//! plus per-index content equality catches it directly: either failure
//! changes the count, or (in the essentially-impossible case of two
//! mis-splits exactly canceling out) misaligns content at some index.
//!
//! **Round 3** added: conditional-compilation directives around a
//! statement/a whole nested block/only the outer `BEGIN`/only the closing
//! `END`; an opaque-source (`JAVA SOURCE`) unit; mixed-case keywords and
//! keyword-named identifiers (randomized on every generated unit); a
//! forward declaration with a dangerous parameter default nested inside
//! *another subprogram's own* declare section (round-2 MUST-FIX #4's exact
//! shape, previously only generated inside a package spec); guaranteed
//! double-labelled sibling blocks; a forced depth 3-5 nesting generator (the
//! plain recursive one picks a leaf 2 times out of 3, so deep nesting was
//! under-represented); and a second, deliberately non-Oracle-shaped dialect
//! with no block starters at all, to exercise vendor neutrality
//! behaviorally rather than only by grep.
//!
//! # The trimming rule
//!
//! Every generated unit's own text ends with exactly one terminator
//! character (`;`) as its last byte, **except** an opaque-source unit,
//! which has none (there is no PL/SQL terminator inside `JAVA SOURCE` text —
//! see [`UnitKind::Opaque`]). A [`StatementKind::Plain`] unit ends with its
//! own `;`; a [`StatementKind::Block`] unit ends with its closing
//! `END[ qualifier];`. [`StatementSpan::content`] excludes a statement's
//! terminator (for a block, specifically the closing `END`'s own required
//! `;`), so the expected content is always the unit's own text with that
//! last `;` byte stripped — or, for an opaque-source unit, the unit's own
//! text verbatim (its `content_end` is computed purely by trimming trailing
//! *whitespace* before a `/` line, never a terminator character, because
//! `BlockKind::OpaqueSource` scanning has no terminator concept at all).
//!
//! # Joining
//!
//! Units are joined three ways, matching the review's ask: **always** a
//! lone `/` line between units (the conventional SQL\*Plus script style),
//! **never** one (relying purely on each unit's own terminator or — for a
//! block, per [`SqlDialect::block_may_end_without_slash`] — its own closing
//! `END;`), and **mixed** (an independent coin flip per boundary). A `/`
//! line is allowed after a *plain* unit too in "mixed" mode — deliberately:
//! since round-2 MUST-FIX #2, a lone `/` line with nothing pending produces
//! **no span at all** (see `splitter.rs`'s module docs), so it is skipped
//! rather than either merging into or displacing the next unit, and the
//! exact-recovery property still holds. Each joining is additionally tried
//! with both LF and CRLF line endings.
//!
//! An opaque-source unit is a special case regardless of join mode: without
//! a `/` line, nothing closes it at all (it runs to end of input, per
//! [`BlockKind::OpaqueSource`]'s own rule), which would swallow every unit
//! after it and break the exact-recovery property for reasons that have
//! nothing to do with this test's actual target. A `/` is therefore always
//! forced immediately after an opaque-source unit, regardless of what the
//! chosen [`JoinMode`] would otherwise pick for that boundary — see
//! [`opaque_source_without_a_following_slash_runs_to_end_of_input`] for the
//! dedicated, single-unit test of the no-`/` case this exclusion exists to
//! avoid conflating with everything else here.

use reldex_sql_text::{
    CommentRules, EndedBy, QuotingRules, SqlDialect, StatementKind, StatementSpan, split_statements,
};

#[path = "support/mod.rs"]
mod support;

// ------------------------------------------------------------------- rng

/// A tiny, dependency-free xorshift64 PRNG — this crate stays at zero
/// dependencies, same rationale as `corpus.rs`'s own `Xorshift32`. A
/// different constant/width than that one only so the two fuzz sources never
/// happen to walk in lockstep.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() as usize) % bound
    }

    fn name(&mut self) -> String {
        const WORDS: &[&str] = &["p", "f", "t", "x", "helper", "worker", "calc", "tmp", "obj"];
        format!("{}{}", WORDS[self.below(WORDS.len())], self.below(1000))
    }

    fn coin(&mut self) -> bool {
        self.below(2) == 0
    }
}

// ---------------------------------------------------- keyword-case fuzzing

/// Every keyword this grammar ever emits literally, compared
/// case-insensitively — re-cased randomly wherever it appears (round 3:
/// "randomise the case of every keyword the generator emits"). Deliberately
/// a flat word list rather than reusing `support::oracle_like().keywords`:
/// this file must stand on its own, and the point is to re-case *this
/// grammar's* vocabulary, not chase the dialect's list if it grows.
const CASE_RANDOMIZED_WORDS: &[&str] = &[
    "BEGIN",
    "END",
    "DECLARE",
    "PROCEDURE",
    "FUNCTION",
    "PACKAGE",
    "BODY",
    "TYPE",
    "TRIGGER",
    "IS",
    "AS",
    "IF",
    "THEN",
    "ELSE",
    "ELSIF",
    "LOOP",
    "WHILE",
    "FOR",
    "CASE",
    "WHEN",
    "EXCEPTION",
    "OTHERS",
    "CREATE",
    "REPLACE",
    "OR",
    "AND",
    "NOT",
    "MEMBER",
    "STATIC",
    "CONSTRUCTOR",
    "RETURN",
    "SELF",
    "RESULT",
    "COMPOUND",
    "INSTEAD",
    "OF",
    "BEFORE",
    "AFTER",
    "CALL",
    "LANGUAGE",
    "JAVA",
    "EXTERNAL",
    "LIBRARY",
    "NAME",
    "NAMED",
    "SOURCE",
    "CURSOR",
    "RECORD",
    "TABLE",
    "REF",
    "SUBTYPE",
    "EXECUTE",
    "IMMEDIATE",
    "NULL",
    "NUMBER",
    "VARCHAR2",
    "DEFAULT",
    "CAST",
    "SELECT",
    "FROM",
    "INTO",
    "INSERT",
    "VALUES",
    "UPDATE",
    "SET",
    "WHERE",
    "COMMIT",
    "EXIT",
    "RAISE",
    "DETERMINISTIC",
    "OBJECT",
    "VARRAY",
    "CONSTANT",
    "NEW",
    "EACH",
    "ROW",
    "AND_COMPILE",
    "NOFORCE",
    "RESOLVE",
    "COMPILE",
    "AND",
];

/// Re-cases `word` uniformly (`UPPER`, `lower`, `Title`, or `MiXeD`).
fn randomize_case(rng: &mut Rng, word: &str) -> String {
    match rng.below(4) {
        0 => word.to_uppercase(),
        1 => word.to_lowercase(),
        2 => {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |c| {
                c.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
            })
        }
        _ => word
            .chars()
            .enumerate()
            .map(|(i, c)| {
                if i % 2 == 0 {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect(),
    }
}

/// Scans `text` word by word (a maximal run of ASCII alphanumerics/`_`) and
/// re-cases every word that case-insensitively matches
/// [`CASE_RANDOMIZED_WORDS`], leaving everything else (identifiers, numbers,
/// punctuation, string/comment contents) byte-for-byte untouched. Applied
/// once to a whole generated unit's text, after all templating, so the
/// result — whatever case each keyword landed in — becomes that unit's own
/// canonical text for the exact-recovery check: correctness here is about
/// the splitter's recovered content matching *this* text exactly, not about
/// matching any particular casing.
fn randomize_keyword_case(rng: &mut Rng, text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = text[i..]
            .chars()
            .next()
            .expect("i < bytes.len(), so at least one char remains at this valid char boundary");
        if c.is_ascii_alphabetic() {
            let start = i;
            let mut j = i;
            while j < bytes.len() {
                let cj = text[j..].chars().next().expect(
                    "j < bytes.len(), so at least one char remains at this valid char boundary",
                );
                if cj.is_ascii_alphanumeric() || cj == '_' {
                    j += cj.len_utf8();
                } else {
                    break;
                }
            }
            let word = &text[start..j];
            if CASE_RANDOMIZED_WORDS
                .iter()
                .any(|k| k.eq_ignore_ascii_case(word))
            {
                out.push_str(&randomize_case(rng, word));
            } else {
                out.push_str(word);
            }
            i = j;
        } else {
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

// -------------------------------------------------------- grammar: bodies

/// One executable statement inside a `BEGIN ... END` body.
fn gen_leaf_stmt(rng: &mut Rng) -> String {
    match rng.below(9) {
        0 => "NULL;".to_string(),
        1 => format!("v_{} := 1;", rng.name()),
        2 => "SELECT 1 INTO x FROM dual;".to_string(),
        3 => "-- a decoy END; and a decoy / on this comment line\n      NULL;".to_string(),
        4 => "EXECUTE IMMEDIATE q'[CREATE PROCEDURE fake AS BEGIN NULL; END;]';".to_string(),
        5 => "INSERT INTO t VALUES ('a;end/*x*/ line with decoys');".to_string(),
        6 => format!("language := {};", rng.below(100)),
        7 => format!("external := {};", rng.below(100)),
        // A leaf stuffing several decoy BEGIN/END pairs and a `/`-like
        // character into one `q'[...]'` string, per round 3 item (f).
        _ => "EXECUTE IMMEDIATE q'[BEGIN NULL; END; / DECLARE v NUMBER; BEGIN v := 1; END;]';"
            .to_string(),
    }
}

/// The plain recursive body generator: at `depth == 0`, or 2 times out of 3
/// otherwise, bottoms out at a leaf. Kept as-is (rather than always
/// recursing) so shallow bodies stay common too — [`gen_deep_body`] below is
/// the round-3 answer to "raise the density of depth >= 3 nesting" without
/// removing shallow coverage from this one.
fn gen_body(rng: &mut Rng, depth: u32) -> String {
    if depth == 0 || rng.below(3) != 0 {
        return gen_leaf_stmt(rng);
    }
    match rng.below(6) {
        0 => format!(
            "IF 1 = 1 THEN\n  {}\nELSIF 2 = 2 THEN\n  {}\nELSE\n  {}\nEND IF;",
            gen_body(rng, depth - 1),
            gen_body(rng, depth - 1),
            gen_body(rng, depth - 1)
        ),
        1 => format!(
            "LOOP\n  {}\n  EXIT WHEN 1 = 1;\nEND LOOP;",
            gen_body(rng, depth - 1)
        ),
        2 => format!(
            "WHILE 1 = 1 LOOP\n  {}\nEND LOOP;",
            gen_body(rng, depth - 1)
        ),
        3 => format!(
            "CASE\n  WHEN 1 = 1 THEN {}\n  ELSE {}\nEND CASE;",
            gen_body(rng, depth - 1),
            gen_body(rng, depth - 1)
        ),
        4 => {
            let lbl = rng.below(1000);
            format!(
                "<<inner_{lbl}>>\n  BEGIN\n    {}\n  END inner_{lbl};",
                gen_body(rng, depth - 1)
            )
        }
        // Round 3 item (a): a `$IF`/`$ELSIF`/`$ELSE`/`$END` directive
        // wrapping an ordinary statement.
        _ => {
            let n = rng.below(1000);
            format!(
                "$IF $$flag_{n} $THEN\n    {}\n  $ELSIF $$flag2_{n} $THEN\n    {}\n  $ELSE\n    {}\n  $END",
                gen_body(rng, depth - 1),
                gen_body(rng, depth - 1),
                gen_body(rng, depth - 1)
            )
        }
    }
}

/// A `BEGIN ... END` body with several siblings at the given depth — the
/// exact shape MUST-FIX #1 was found through (N siblings, pre-fix, produced
/// N+1 spans instead of 1).
fn gen_sibling_body(rng: &mut Rng, depth: u32) -> String {
    let n = 2 + rng.below(3);
    let mut parts = Vec::with_capacity(n);
    for _ in 0..n {
        parts.push(format!(
            "BEGIN\n    {}\n  END;",
            gen_body(rng, depth.saturating_sub(1))
        ));
    }
    parts.join("\n  ")
}

/// Round 3 item 3 ("raise the density of depth >= 3 nesting"): unlike
/// [`gen_body`], this never bottoms out early — every level down to `depth
/// == 0` is a real nesting construct, guaranteeing exactly `depth` levels
/// rather than leaving it to a 1-in-3 chance per level.
fn gen_deep_body(rng: &mut Rng, depth: u32) -> String {
    if depth == 0 {
        return gen_leaf_stmt(rng);
    }
    match rng.below(5) {
        0 => format!(
            "IF 1 = 1 THEN\n  {}\nELSE\n  {}\nEND IF;",
            gen_deep_body(rng, depth - 1),
            gen_leaf_stmt(rng)
        ),
        1 => format!(
            "LOOP\n  {}\n  EXIT WHEN 1 = 1;\nEND LOOP;",
            gen_deep_body(rng, depth - 1)
        ),
        2 => format!(
            "WHILE 1 = 1 LOOP\n  {}\nEND LOOP;",
            gen_deep_body(rng, depth - 1)
        ),
        3 => format!(
            "CASE\n  WHEN 1 = 1 THEN {}\n  ELSE {}\nEND CASE;",
            gen_deep_body(rng, depth - 1),
            gen_leaf_stmt(rng)
        ),
        _ => {
            let lbl = rng.below(1000);
            format!(
                "<<inner_{lbl}>>\n  BEGIN\n    {}\n  END inner_{lbl};",
                gen_deep_body(rng, depth - 1)
            )
        }
    }
}

// --------------------------------------------------------- grammar: units

/// What kind of statement a generated unit is, and — since the three kinds
/// close differently — how to compute its expected content/`ended_by`. See
/// the module docs' "The trimming rule".
#[derive(Clone, Copy, PartialEq, Eq)]
enum UnitKind {
    Plain,
    Block,
    /// A block whose own generated text places trailing directive content
    /// (`$END`) *after* its true structural close (`END;`), with nothing
    /// else to attribute that trailing text to any span at all once no `/`
    /// follows: exactly like [`Opaque`](Self::Opaque), this is always forced
    /// to be followed by a `/` line regardless of [`JoinMode`] — not because
    /// scanning it is wrong without one (`content()` already stops, correctly,
    /// right after `END`, excluding both its `;` and the trailing
    /// directive), but because *which statement the raw trailing directive
    /// itself belongs to* is genuinely undecidable from the raw text alone
    /// without a `/` to anchor it, the same ambiguity a stray comment
    /// between two statements' terminators would have — not a splitter
    /// defect to assert one specific answer for. A forced `/` sidesteps the
    /// ambiguity entirely (the same technique [`Opaque`](Self::Opaque)
    /// uses) rather than asserting an arbitrary answer to an ill-posed
    /// question. Used only by the "directive wraps only the closing `END`"
    /// grammar arm (round 3 item (a)).
    BlockDirectiveTail,
    /// `BlockKind::OpaqueSource` (`CREATE ... JAVA SOURCE ...`): always
    /// forced to be followed by a `/` line regardless of [`JoinMode`] — see
    /// the module docs' final paragraph.
    Opaque,
}

/// One self-contained top-level unit: its own generated text and its
/// [`UnitKind`].
fn gen_unit(rng: &mut Rng) -> (String, UnitKind) {
    let depth = 3;
    let (text, kind) = match rng.below(31) {
        // ---- plain SQL, including CASE expressions and AS aliases -------
        0 => (format!("SELECT {} FROM dual;", rng.below(1000)), UnitKind::Plain),
        1 => (
            "SELECT CASE WHEN 1 = 1 THEN 'a' ELSE 'b' END AS lbl FROM dual;".to_string(),
            UnitKind::Plain,
        ),
        2 => (
            "SELECT t.language, t.external FROM (SELECT 1 AS language, 2 AS external FROM dual) t;"
                .to_string(),
            UnitKind::Plain,
        ),
        3 => (
            format!(
                "UPDATE t SET v = 'a;end/*x*/ -- not a comment inside a string' WHERE id = {};",
                rng.below(1000)
            ),
            UnitKind::Plain,
        ),
        4 => ("SELECT 10 / 2 FROM dual;".to_string(), UnitKind::Plain),
        // Round 3 item (c): keyword-named identifiers as quoted identifiers
        // and (where legal) unquoted column aliases.
        5 => (
            "SELECT 1 AS \"Begin\", 2 AS \"End\", 3 AS Language, 4 AS External, 5 AS Before, 6 AS After FROM dual;"
                .to_string(),
            UnitKind::Plain,
        ),

        // ---- anonymous blocks: bare, labelled, nested/sibling, EXCEPTION -
        6 => (format!("BEGIN\n  {}\nEND;", gen_body(rng, depth)), UnitKind::Block),
        7 => {
            let lbl = rng.below(1000);
            (
                format!(
                    "<<outer_{lbl}>>\nBEGIN\n  {}\nEND outer_{lbl};",
                    gen_body(rng, depth)
                ),
                UnitKind::Block,
            )
        }
        8 => (
            format!("BEGIN\n  {}\nEND;", gen_sibling_body(rng, depth)),
            UnitKind::Block,
        ),
        9 => (
            format!(
                "BEGIN\n  {}\nEXCEPTION\n  WHEN OTHERS THEN\n    {}\nEND;",
                gen_body(rng, 1),
                gen_body(rng, depth)
            ),
            UnitKind::Block,
        ),
        10 => (
            format!(
                "DECLARE\n  v NUMBER;\n  CURSOR c IS SELECT 1 FROM dual;\n  TYPE rec_t IS RECORD (a NUMBER, b VARCHAR2(10));\n  TYPE tbl_t IS TABLE OF NUMBER INDEX BY PLS_INTEGER;\n  TYPE rc_t IS REF CURSOR;\n  SUBTYPE small_t IS NUMBER(3);\nBEGIN\n  OPEN c;\n  {}\n  CLOSE c;\nEND;",
                gen_body(rng, depth)
            ),
            UnitKind::Block,
        ),
        // Round 3 item (e): guaranteed double-labelled sibling nested
        // blocks under one outer label (rather than left to recursive
        // chance).
        11 => {
            let outer_lbl = rng.below(1000);
            let (inner1, inner2) = (rng.below(1000), rng.below(1000));
            (
                format!(
                    "<<outer_{outer_lbl}>>\nBEGIN\n  <<inner_{inner1}>>\n  BEGIN\n    {}\n  END inner_{inner1};\n  <<inner_{inner2}>>\n  BEGIN\n    {}\n  END inner_{inner2};\nEND outer_{outer_lbl};",
                    gen_body(rng, 1),
                    gen_body(rng, 1)
                ),
                UnitKind::Block,
            )
        }

        // ---- subprograms: forward-decl params, nested subprograms -------
        12 => {
            let n = rng.name();
            (
                format!(
                    "CREATE OR REPLACE PROCEDURE {n}(a NUMBER DEFAULT CAST(1 AS NUMBER), b VARCHAR2 DEFAULT NULL) AS\n  PROCEDURE inner_a IS\n  BEGIN\n    {}\n  END inner_a;\n  PROCEDURE inner_b IS\n    PROCEDURE inner_b_2 IS\n    BEGIN\n      {}\n    END inner_b_2;\n  BEGIN\n    inner_b_2;\n  END inner_b;\nBEGIN\n  inner_a;\n  inner_b;\n  {}\nEND {n};",
                    gen_body(rng, 1),
                    gen_body(rng, 1),
                    gen_body(rng, 1)
                ),
                UnitKind::Block,
            )
        }
        13 => {
            let n = rng.name();
            (
                format!(
                    "CREATE OR REPLACE FUNCTION {n}(p NUMBER DEFAULT CASE WHEN 1 = 1 THEN 1 ELSE 2 END) RETURN NUMBER DETERMINISTIC IS\nBEGIN\n  RETURN p;\nEND {n};"
                ),
                UnitKind::Block,
            )
        }
        // Round 3 item (d): the exact round-2 MUST-FIX #4 shape (a forward
        // declaration with a `CAST(... AS ...)`/`CASE ... END`/`x IS NULL`
        // default), nested inside *another subprogram's own* declare
        // section ahead of that subprogram's own `BEGIN` — as opposed to
        // arm 17 below, which is the same shape inside a package *spec*
        // (which has no enclosing body waiting to be opened at all).
        14 => {
            let n = rng.name();
            (
                format!(
                    "CREATE OR REPLACE PROCEDURE {n} AS\n  PROCEDURE fwd_helper(a NUMBER DEFAULT CAST(1 AS NUMBER), b NUMBER DEFAULT CASE WHEN 1 = 1 THEN 1 ELSE 2 END, c NUMBER DEFAULT CASE WHEN c IS NULL THEN 0 ELSE 1 END);\nBEGIN\n  {}\nEND {n};",
                    gen_body(rng, 1)
                ),
                UnitKind::Block,
            )
        }

        // ---- packages: spec (forward decls, call-spec member), body -----
        15 => {
            let n = rng.name();
            (
                format!(
                    "CREATE OR REPLACE PACKAGE {n} IS\n  PROCEDURE p1;\n  FUNCTION f1 RETURN NUMBER;\n  PROCEDURE p2(a NUMBER DEFAULT CAST(1 AS NUMBER));\n  FUNCTION native_add(a NUMBER, b NUMBER) RETURN NUMBER IS LANGUAGE JAVA NAME 'Adder.add(int,int) return int';\nEND {n};"
                ),
                UnitKind::Block,
            )
        }
        16 => {
            let n = rng.name();
            (
                format!(
                    "CREATE OR REPLACE PACKAGE BODY {n} IS\n  PROCEDURE p1 IS\n  BEGIN\n    {}\n  END p1;\n  FUNCTION f1 RETURN NUMBER IS\n  BEGIN\n    RETURN 1;\n  END f1;\nBEGIN\n  {}\nEXCEPTION\n  WHEN OTHERS THEN\n    NULL;\nEND {n};",
                    gen_body(rng, 1),
                    gen_body(rng, 1)
                ),
                UnitKind::Block,
            )
        }

        // ---- types: object/varray/table-of/incomplete specs, and bodies -
        17 => {
            let n = rng.name();
            (
                format!("CREATE OR REPLACE TYPE {n} AS OBJECT (x NUMBER, y NUMBER);"),
                UnitKind::Block,
            )
        }
        18 => {
            let n = rng.name();
            (
                format!("CREATE OR REPLACE TYPE {n} AS VARRAY(10) OF NUMBER;"),
                UnitKind::Block,
            )
        }
        19 => {
            let n = rng.name();
            (
                format!("CREATE OR REPLACE TYPE {n} AS TABLE OF NUMBER;"),
                UnitKind::Block,
            )
        }
        20 => {
            let n = rng.name();
            (format!("CREATE OR REPLACE TYPE {n};"), UnitKind::Block)
        }
        21 => {
            let n = rng.name();
            (
                format!(
                    "CREATE OR REPLACE TYPE BODY {n} AS\n  MEMBER FUNCTION area RETURN NUMBER IS\n  BEGIN\n    RETURN 1;\n  END area;\n  STATIC FUNCTION make RETURN {n} IS\n  BEGIN\n    RETURN {n}(1, 2);\n  END make;\n  CONSTRUCTOR FUNCTION {n}(x NUMBER, y NUMBER) RETURN SELF AS RESULT IS\n  BEGIN\n    RETURN;\n  END;\nEND;"
                ),
                UnitKind::Block,
            )
        }

        // ---- triggers: simple/compound/CALL/INSTEAD OF/WHEN(...IS NULL) -
        22 => {
            let n = rng.name();
            (
                format!(
                    "CREATE OR REPLACE TRIGGER {n}\nBEFORE INSERT ON t\nFOR EACH ROW\nWHEN (new.id IS NOT NULL)\nDECLARE\n  v NUMBER;\nBEGIN\n  {}\nEND;",
                    gen_body(rng, 1)
                ),
                UnitKind::Block,
            )
        }
        23 => {
            let n = rng.name();
            (
                format!(
                    "CREATE OR REPLACE TRIGGER {n}\nINSTEAD OF INSERT ON v\nFOR EACH ROW\nCOMPOUND TRIGGER\n  BEFORE STATEMENT IS\n  BEGIN\n    {}\n  END BEFORE STATEMENT;\n  AFTER EACH ROW IS\n  BEGIN\n    {}\n  END AFTER EACH ROW;\nEND {n};",
                    gen_body(rng, 1),
                    gen_body(rng, 1)
                ),
                UnitKind::Block,
            )
        }
        24 => {
            let n = rng.name();
            (
                format!("CREATE TRIGGER {n} AFTER INSERT ON t FOR EACH ROW CALL p(:NEW.id);"),
                UnitKind::Block,
            )
        }

        // ---- round 3: directives around structural keywords -------------
        25 => {
            // Around a whole nested block.
            let lbl = rng.below(1000);
            (
                format!(
                    "BEGIN\n  $IF $$flag_{lbl} $THEN\n    BEGIN\n      {}\n    END;\n  $ELSE\n    NULL;\n  $END\n  COMMIT;\nEND;",
                    gen_body(rng, 1)
                ),
                UnitKind::Block,
            )
        }
        26 => {
            // Around only the outer BEGIN.
            let lbl = rng.below(1000);
            (
                format!(
                    "$IF $$flag_{lbl} $THEN\nBEGIN\n$END\n  {}\nEND;",
                    gen_body(rng, 1)
                ),
                UnitKind::Block,
            )
        }
        27 => {
            // Around only the closing END — see UnitKind::BlockDirectiveTail
            // for why this shape is always forced onto a `/` boundary.
            let lbl = rng.below(1000);
            (
                format!(
                    "BEGIN\n  {}\n$IF $$flag_{lbl} $THEN\nEND;\n$END",
                    gen_body(rng, 1)
                ),
                UnitKind::BlockDirectiveTail,
            )
        }

        // ---- round 3: forced depth 3-5 nesting ---------------------------
        28 => {
            let forced_depth = 3 + rng.below(3) as u32;
            (
                format!("BEGIN\n  {}\nEND;", gen_deep_body(rng, forced_depth)),
                UnitKind::Block,
            )
        }
        29 => {
            let lbl = rng.below(1000);
            let forced_depth = 3 + rng.below(3) as u32;
            (
                format!(
                    "<<outer_{lbl}>>\nBEGIN\n  {}\nEND outer_{lbl};",
                    gen_deep_body(rng, forced_depth)
                ),
                UnitKind::Block,
            )
        }

        // ---- round 3: opaque source (JAVA SOURCE) ------------------------
        _ => {
            let n = rng.name();
            (
                format!(
                    "CREATE OR REPLACE AND COMPILE JAVA SOURCE NAMED \"Worker{n}\" AS\npublic class Worker{n} {{\n  // decoy tokens: BEGIN; END; a semicolon; and braces {{ }}\n  public static int compute(int a, int b) {{\n    String s = \"BEGIN END; {{ }}\";\n    return a + b;\n  }}\n}}"
                ),
                UnitKind::Opaque,
            )
        }
    };
    // Round 3 item (c): randomize the case of every keyword this unit's
    // text contains, on every unit (including opaque-source: harmless,
    // since its Java text is never structurally interpreted — see the
    // module docs' "trimming rule").
    (randomize_keyword_case(rng, &text), kind)
}

// ---------------------------------------------------------------- joining

#[derive(Clone, Copy)]
enum JoinMode {
    AlwaysSlash,
    NeverSlash,
    Mixed,
}

/// Joins `units`, returning the joined text and, per unit, whether an
/// explicit `/` line was placed immediately after it (needed to predict
/// [`EndedBy`] for a block unit — see the module docs' "Joining" section).
/// An opaque-source unit always gets a `/`, regardless of `mode` — see the
/// module docs' final paragraph.
fn join_units(units: &[(String, UnitKind)], mode: JoinMode, rng: &mut Rng) -> (String, Vec<bool>) {
    let mut text = String::new();
    let mut slash_after = Vec::with_capacity(units.len());
    for (unit, kind) in units {
        text.push_str(unit);
        let use_slash = matches!(kind, UnitKind::Opaque | UnitKind::BlockDirectiveTail)
            || match mode {
                JoinMode::AlwaysSlash => true,
                JoinMode::NeverSlash => false,
                JoinMode::Mixed => rng.coin(),
            };
        slash_after.push(use_slash);
        if use_slash {
            text.push_str("\n/\n");
        } else {
            text.push_str("\n\n");
        }
    }
    (text, slash_after)
}

// ------------------------------------------------------------- assertions

/// Checks the invariants `corpus.rs`'s `assert_span_invariants` documents,
/// duplicated here (rather than shared via `#[path]`, which would need both
/// files to agree on a third helper module) since this file must stand on
/// its own as the reviewer-requested acceptance gate.
fn assert_span_invariants(text: &str, span: &StatementSpan) {
    assert!(text.is_char_boundary(span.content_start), "{span:?}");
    assert!(text.is_char_boundary(span.content_end), "{span:?}");
    assert!(text.is_char_boundary(span.full_end), "{span:?}");
    assert!(span.content_start <= span.content_end, "{span:?}");
    assert!(span.content_end <= span.full_end, "{span:?}");
    assert!(span.full_end <= text.len(), "{span:?}");
    let _ = span.content(text);
    let _ = span.full(text);
}

/// Runs one generated script through [`split_statements`] and checks exact
/// recovery of `units` against it, per the module docs' "trimming rule".
fn check_exact_recovery(
    units: &[(String, UnitKind)],
    mode: JoinMode,
    crlf: bool,
    rng: &mut Rng,
    dialect: &SqlDialect,
    label: &str,
) {
    let (mut text, slash_after) = join_units(units, mode, rng);
    if crlf {
        text = text.replace('\n', "\r\n");
    }

    let spans = split_statements(&text, dialect);

    assert_eq!(
        spans.len(),
        units.len(),
        "[{label}] expected {} spans (one per generated unit), got {} — a unit was \
         swallowed or split; text={text:?} spans={spans:?}",
        units.len(),
        spans.len()
    );

    for (i, (span, (unit, kind))) in spans.iter().zip(units.iter()).enumerate() {
        assert_span_invariants(&text, span);

        let expected_kind = match kind {
            UnitKind::Plain => StatementKind::Plain,
            UnitKind::Block | UnitKind::BlockDirectiveTail | UnitKind::Opaque => {
                StatementKind::Block
            }
        };
        assert_eq!(span.kind, expected_kind, "[{label}] unit {i}: {span:?}");

        let expected_ended_by = match kind {
            UnitKind::Plain => EndedBy::Terminator,
            UnitKind::Block | UnitKind::BlockDirectiveTail | UnitKind::Opaque => {
                if slash_after[i] {
                    EndedBy::SlashLine
                } else {
                    EndedBy::InferredBlockEnd
                }
            }
        };
        assert_eq!(
            span.ended_by, expected_ended_by,
            "[{label}] unit {i}: {span:?}"
        );
        assert!(span.terminated, "[{label}] unit {i}: {span:?}");

        let mut expected_content = unit.clone();
        if *kind != UnitKind::Opaque {
            // Ordinarily the unit's own terminator is its very last byte, but
            // a unit wrapping only its closing `END` in a directive (round 3
            // item (a): "`$IF ... $THEN ... $END` around only the closing
            // `END`") has trailing directive text (the closing `$END`) after
            // that terminator — the `;` closing the real `END` is the *last
            // `;` in the string*, not necessarily its last byte, since
            // `content_end` for a block stops right after `END`, before its
            // `;`, regardless of what trivial (directive/whitespace) content
            // the generator placed after it.
            let last_semicolon = expected_content
                .rfind(';')
                .unwrap_or_else(|| panic!("malformed test unit, no ';' found: {unit:?}"));
            expected_content.truncate(last_semicolon);
        }
        if crlf {
            expected_content = expected_content.replace('\n', "\r\n");
        }
        assert_eq!(
            span.content(&text),
            expected_content,
            "[{label}] unit {i}: content mismatch; full text={text:?} span={span:?}"
        );
    }
}

// -------------------------------------------------------------- the gate

#[test]
fn grammar_based_units_are_recovered_exactly_across_join_modes_and_line_endings() {
    let dialect = support::oracle_like();
    // The reviewer's own 5 seeds, plus 3 more for headroom past the
    // minimum of 8 the review asked for.
    let seeds: [u64; 8] = [1, 42, 3, 987, 2024, 0x00C0_FFEE, 0x5EED_5EED, 0xABCD_EF01];
    let scripts_per_seed = if cfg!(debug_assertions) { 60 } else { 5_000 };
    let mut total_scripts = 0usize;

    for seed in seeds {
        let mut rng = Rng::new(seed);
        for _ in 0..scripts_per_seed {
            let n_units = 3 + rng.below(6); // 3..=8, per the review
            let units: Vec<(String, UnitKind)> = (0..n_units).map(|_| gen_unit(&mut rng)).collect();

            for mode in [JoinMode::AlwaysSlash, JoinMode::NeverSlash, JoinMode::Mixed] {
                for crlf in [false, true] {
                    check_exact_recovery(
                        &units,
                        mode,
                        crlf,
                        &mut rng,
                        &dialect,
                        &format!("seed={seed:#x}"),
                    );
                    total_scripts += 1;
                }
            }
        }
    }

    // `cargo test -p reldex-sql-text --release` (per the review's gate) runs
    // the larger count: `seeds.len() * scripts_per_seed * 3 join modes * 2
    // line endings` — this workspace denies `clippy::print_stdout`, so the
    // count is reported by the caller from this arithmetic rather than
    // printed here; the assertion below only guards that every planned
    // combination actually ran (no join mode/line-ending silently skipped).
    let expected_total = seeds.len() * scripts_per_seed * 3 * 2;
    assert_eq!(total_scripts, expected_total);
}

#[test]
fn opaque_source_without_a_following_slash_runs_to_end_of_input() {
    // Round 3 item (b): an opaque-source unit with no `/` at all has
    // nothing to close it — `BlockKind::OpaqueSource` scanning ends only at
    // a lone `/` line or end of input, never at its own content (which may
    // itself contain `;`/`{`/`}`/the words `BEGIN`/`END`, none of which mean
    // anything inside Java source text).
    let dialect = support::oracle_like();
    let text = "CREATE OR REPLACE AND COMPILE JAVA SOURCE NAMED \"NoSlash\" AS\n\
                public class NoSlash {\n  // BEGIN; END; decoys, and a { } pair\n  void m() { int a = 1; }\n}\n";
    let spans = split_statements(text, &dialect);
    assert_eq!(spans.len(), 1, "{spans:?}");
    assert_eq!(spans[0].kind, StatementKind::Block);
    assert_eq!(spans[0].ended_by, EndedBy::EndOfInput);
    assert!(!spans[0].terminated, "{spans:?}");
}

// ------------------------------------------------ a second, non-Oracle dialect

/// A minimal dialect that is deliberately **not** Oracle-shaped: no block
/// starters at all, `;` the only terminator, and no `/` handling whatsoever
/// (round 3 item 2 — "exercise vendor neutrality behaviorally, not just by
/// grep"). With no `block_starters`, nothing this dialect's own words name
/// — including `BEGIN`/`END`/`COMMIT`, which are meaningful Oracle keywords
/// but carry no special role in *this* descriptor's data — ever opens or
/// closes a block: every statement here is `StatementKind::Plain`, ending at
/// its own `;`. If `reldex_sql_text::splitter`'s logic ever hard-coded an
/// Oracle word instead of reading it from `SqlDialect`, this dialect would
/// be the one to expose it, because it supplies none of those words at all.
fn minimal_non_oracle() -> SqlDialect {
    SqlDialect {
        statement_terminators: &[';'],
        slash_terminates_block: false,
        slash_terminates_plain: false,
        block_may_end_without_slash: true,
        block_starters: &[],
        label_delimiters: None,
        block_body_opener: "BEGIN",
        block_nesting_openers: &[],
        block_end_keyword: "END",
        subprogram_header_keywords: &[],
        body_intro_keywords: &[],
        body_intro_exceptions: &[],
        call_spec_phrases: &[],
        body_less_markers: &[],
        sectioned_body_marker: None,
        section_header_starters: &[],
        directive_prefix: None,
        quoting: QuotingRules::default(),
        comments: CommentRules {
            line_comment: Some("--"),
            block_comment: Some(("/*", "*/")),
            line_comment_words: &[],
        },
        bind_variables: false,
        substitution_variables: false,
        keywords: &[
            "SELECT", "FROM", "WHERE", "BEGIN", "END", "COMMIT", "ROLLBACK", "INSERT", "INTO",
            "VALUES", "UPDATE", "SET", "NULL",
        ],
    }
}

fn gen_plain_only_unit(rng: &mut Rng) -> String {
    match rng.below(6) {
        0 => "BEGIN;".to_string(),
        1 => "END;".to_string(),
        2 => "COMMIT;".to_string(),
        3 => "ROLLBACK;".to_string(),
        4 => format!("SELECT {} FROM t;", rng.below(1000)),
        _ => format!("INSERT INTO t VALUES ({});", rng.below(1000)),
    }
}

#[test]
fn a_dialect_with_no_block_starters_reads_begin_end_commit_as_plain_statements() {
    let dialect = minimal_non_oracle();
    let seeds: [u64; 4] = [7, 77, 777, 7777];
    let scripts_per_seed = if cfg!(debug_assertions) { 30 } else { 2_000 };

    for seed in seeds {
        let mut rng = Rng::new(seed);
        for _ in 0..scripts_per_seed {
            let n_units = 3 + rng.below(6);
            let units: Vec<String> = (0..n_units)
                .map(|_| gen_plain_only_unit(&mut rng))
                .collect();
            let text = units.join("\n");

            let spans = split_statements(&text, &dialect);
            assert_eq!(spans.len(), units.len(), "text={text:?} spans={spans:?}");
            for (span, unit) in spans.iter().zip(units.iter()) {
                assert_span_invariants(&text, span);
                assert_eq!(span.kind, StatementKind::Plain, "{span:?}");
                assert_eq!(span.ended_by, EndedBy::Terminator, "{span:?}");
                assert!(span.terminated, "{span:?}");
                let expected = &unit[..unit.len() - 1];
                assert_eq!(span.content(&text), expected, "text={text:?} span={span:?}");
            }
        }
    }
}
