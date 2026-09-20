//! A grammar-based differential/property test (M2.4 round-2 adversarial
//! review's "acceptance gate"): [`corpus.rs`]'s fuzz test assembles scripts
//! out of independent, randomly-chosen *fragments*, which is excellent at
//! finding lexing/scanning edge cases but — because a randomly-chosen
//! sequence of fragments essentially never produces a deeply, evenly nested
//! structure — is nearly powerless against a depth-tracking bug that only
//! shows up once nesting is *balanced* (round-2 MUST-FIX #1: 6,000,000 fuzz
//! cases found no instance of it).
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
//! # The trimming rule
//!
//! Every generated unit's own text ends with exactly one terminator
//! character (`;`) as its last byte — a [`StatementKind::Plain`] unit ends
//! with its own `;`, and a [`StatementKind::Block`] unit ends with its
//! closing `END[ qualifier];`. [`StatementSpan::content`] excludes a
//! statement's terminator (for a block, specifically the closing `END`'s own
//! required `;` — see that method's docs), so the expected content is always
//! the unit's own text with that last `;` byte stripped, regardless of unit
//! kind. No other trimming is applied or needed.
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

use reldex_sql_text::{EndedBy, SqlDialect, StatementKind, StatementSpan, split_statements};

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

// -------------------------------------------------------- grammar: bodies

/// One executable statement inside a `BEGIN ... END` body. At `depth == 0`
/// this never recurses into a further nested construct, so generation always
/// terminates; deeper calls (`gen_unit` starts at depth >= 3, per the
/// review's "nested/sibling inner blocks at depth >= 3" ask) recurse through
/// `IF`/`LOOP`/`CASE`/a further labelled `BEGIN` before bottoming out.
fn gen_leaf_stmt(rng: &mut Rng) -> String {
    match rng.below(8) {
        0 => "NULL;".to_string(),
        1 => format!("v_{} := 1;", rng.name()),
        2 => "SELECT 1 INTO x FROM dual;".to_string(),
        3 => "-- a decoy END; and a decoy / on this comment line\n      NULL;".to_string(),
        4 => "EXECUTE IMMEDIATE q'[CREATE PROCEDURE fake AS BEGIN NULL; END;]';".to_string(),
        5 => "INSERT INTO t VALUES ('a;end/*x*/ line with decoys');".to_string(),
        6 => format!("language := {};", rng.below(100)),
        _ => "RAISE_APPLICATION_ERROR(-20001, 'boom; END BEGIN /');".to_string(),
    }
}

fn gen_body(rng: &mut Rng, depth: u32) -> String {
    if depth == 0 || rng.below(3) != 0 {
        return gen_leaf_stmt(rng);
    }
    match rng.below(5) {
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
        _ => {
            let lbl = rng.below(1000);
            format!(
                "<<inner_{lbl}>>\n  BEGIN\n    {}\n  END inner_{lbl};",
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

// --------------------------------------------------------- grammar: units

/// One self-contained top-level unit: its own generated text (always ending
/// with exactly one `;`, per the module docs' "trimming rule") and whether
/// it is block-like ([`StatementKind::Block`]) or plain
/// ([`StatementKind::Plain`]).
fn gen_unit(rng: &mut Rng) -> (String, bool) {
    let depth = 3;
    match rng.below(22) {
        // ---- plain SQL, including CASE expressions and AS aliases -------
        0 => (format!("SELECT {} FROM dual;", rng.below(1000)), false),
        1 => (
            "SELECT CASE WHEN 1 = 1 THEN 'a' ELSE 'b' END AS lbl FROM dual;".to_string(),
            false,
        ),
        2 => (
            "SELECT t.language, t.external FROM (SELECT 1 AS language, 2 AS external FROM dual) t;"
                .to_string(),
            false,
        ),
        3 => (
            format!(
                "UPDATE t SET v = 'a;end/*x*/ -- not a comment inside a string' WHERE id = {};",
                rng.below(1000)
            ),
            false,
        ),
        4 => ("SELECT 10 / 2 FROM dual;".to_string(), false),

        // ---- anonymous blocks: bare, labelled, nested/sibling, EXCEPTION -
        5 => (format!("BEGIN\n  {}\nEND;", gen_body(rng, depth)), true),
        6 => {
            let lbl = rng.below(1000);
            (
                format!(
                    "<<outer_{lbl}>>\nBEGIN\n  {}\nEND outer_{lbl};",
                    gen_body(rng, depth)
                ),
                true,
            )
        }
        7 => (
            format!("BEGIN\n  {}\nEND;", gen_sibling_body(rng, depth)),
            true,
        ),
        8 => (
            format!(
                "BEGIN\n  {}\nEXCEPTION\n  WHEN OTHERS THEN\n    {}\nEND;",
                gen_body(rng, 1),
                gen_body(rng, depth)
            ),
            true,
        ),
        9 => (
            format!(
                "DECLARE\n  v NUMBER;\n  CURSOR c IS SELECT 1 FROM dual;\n  TYPE rec_t IS RECORD (a NUMBER, b VARCHAR2(10));\n  TYPE tbl_t IS TABLE OF NUMBER INDEX BY PLS_INTEGER;\n  TYPE rc_t IS REF CURSOR;\n  SUBTYPE small_t IS NUMBER(3);\nBEGIN\n  OPEN c;\n  {}\n  CLOSE c;\nEND;",
                gen_body(rng, depth)
            ),
            true,
        ),

        // ---- subprograms: forward-decl params, nested subprograms -------
        10 => {
            let n = rng.name();
            (
                format!(
                    "CREATE OR REPLACE PROCEDURE {n}(a NUMBER DEFAULT CAST(1 AS NUMBER), b VARCHAR2 DEFAULT NULL) AS\n  PROCEDURE inner_a IS\n  BEGIN\n    {}\n  END inner_a;\n  PROCEDURE inner_b IS\n    PROCEDURE inner_b_2 IS\n    BEGIN\n      {}\n    END inner_b_2;\n  BEGIN\n    inner_b_2;\n  END inner_b;\nBEGIN\n  inner_a;\n  inner_b;\n  {}\nEND {n};",
                    gen_body(rng, 1),
                    gen_body(rng, 1),
                    gen_body(rng, 1)
                ),
                true,
            )
        }
        11 => {
            let n = rng.name();
            (
                format!(
                    "CREATE OR REPLACE FUNCTION {n}(p NUMBER DEFAULT CASE WHEN 1 = 1 THEN 1 ELSE 2 END) RETURN NUMBER DETERMINISTIC IS\nBEGIN\n  RETURN p;\nEND {n};"
                ),
                true,
            )
        }

        // ---- packages: spec (forward decls, call-spec member), body -----
        12 => {
            let n = rng.name();
            (
                format!(
                    "CREATE OR REPLACE PACKAGE {n} IS\n  PROCEDURE p1;\n  FUNCTION f1 RETURN NUMBER;\n  PROCEDURE p2(a NUMBER DEFAULT CAST(1 AS NUMBER));\n  FUNCTION native_add(a NUMBER, b NUMBER) RETURN NUMBER IS LANGUAGE JAVA NAME 'Adder.add(int,int) return int';\nEND {n};"
                ),
                true,
            )
        }
        13 => {
            let n = rng.name();
            (
                format!(
                    "CREATE OR REPLACE PACKAGE BODY {n} IS\n  PROCEDURE p1 IS\n  BEGIN\n    {}\n  END p1;\n  FUNCTION f1 RETURN NUMBER IS\n  BEGIN\n    RETURN 1;\n  END f1;\nBEGIN\n  {}\nEXCEPTION\n  WHEN OTHERS THEN\n    NULL;\nEND {n};",
                    gen_body(rng, 1),
                    gen_body(rng, 1)
                ),
                true,
            )
        }

        // ---- types: object/varray/table-of/incomplete specs, and bodies -
        14 => {
            let n = rng.name();
            (
                format!("CREATE OR REPLACE TYPE {n} AS OBJECT (x NUMBER, y NUMBER);"),
                true,
            )
        }
        15 => {
            let n = rng.name();
            (
                format!("CREATE OR REPLACE TYPE {n} AS VARRAY(10) OF NUMBER;"),
                true,
            )
        }
        16 => {
            let n = rng.name();
            (
                format!("CREATE OR REPLACE TYPE {n} AS TABLE OF NUMBER;"),
                true,
            )
        }
        17 => {
            let n = rng.name();
            (format!("CREATE OR REPLACE TYPE {n};"), true)
        }
        18 => {
            let n = rng.name();
            (
                format!(
                    "CREATE OR REPLACE TYPE BODY {n} AS\n  MEMBER FUNCTION area RETURN NUMBER IS\n  BEGIN\n    RETURN 1;\n  END area;\n  STATIC FUNCTION make RETURN {n} IS\n  BEGIN\n    RETURN {n}(1, 2);\n  END make;\n  CONSTRUCTOR FUNCTION {n}(x NUMBER, y NUMBER) RETURN SELF AS RESULT IS\n  BEGIN\n    RETURN;\n  END;\nEND;"
                ),
                true,
            )
        }

        // ---- triggers: simple/compound/CALL/INSTEAD OF/WHEN(...IS NULL) -
        19 => {
            let n = rng.name();
            (
                format!(
                    "CREATE OR REPLACE TRIGGER {n}\nBEFORE INSERT ON t\nFOR EACH ROW\nWHEN (new.id IS NOT NULL)\nDECLARE\n  v NUMBER;\nBEGIN\n  {}\nEND;",
                    gen_body(rng, 1)
                ),
                true,
            )
        }
        20 => {
            let n = rng.name();
            (
                format!(
                    "CREATE OR REPLACE TRIGGER {n}\nINSTEAD OF INSERT ON v\nFOR EACH ROW\nCOMPOUND TRIGGER\n  BEFORE STATEMENT IS\n  BEGIN\n    {}\n  END BEFORE STATEMENT;\n  AFTER EACH ROW IS\n  BEGIN\n    {}\n  END AFTER EACH ROW;\nEND {n};",
                    gen_body(rng, 1),
                    gen_body(rng, 1)
                ),
                true,
            )
        }
        _ => {
            let n = rng.name();
            (
                format!("CREATE TRIGGER {n} AFTER INSERT ON t FOR EACH ROW CALL p(:NEW.id);"),
                true,
            )
        }
    }
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
fn join_units(units: &[(String, bool)], mode: JoinMode, rng: &mut Rng) -> (String, Vec<bool>) {
    let mut text = String::new();
    let mut slash_after = Vec::with_capacity(units.len());
    for (unit, _is_block) in units {
        text.push_str(unit);
        let use_slash = match mode {
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
    units: &[(String, bool)],
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

    for (i, (span, (unit, is_block))) in spans.iter().zip(units.iter()).enumerate() {
        assert_span_invariants(&text, span);

        let mut expected_kind = if *is_block {
            StatementKind::Block
        } else {
            StatementKind::Plain
        };
        // `CREATE TYPE t;` (the "incomplete type" forward declaration, unit
        // generator arm 17) is still `StatementKind::Block`
        // (`BlockKind::ParenDelimited` routes through the same
        // slash-or-inferred close as every other block shape) — already
        // reflected by `is_block` for every arm, so this line only documents
        // the fact; it changes nothing.
        let _ = &mut expected_kind;

        assert_eq!(span.kind, expected_kind, "[{label}] unit {i}: {span:?}");

        let expected_ended_by = if *is_block {
            if slash_after[i] {
                EndedBy::SlashLine
            } else {
                EndedBy::InferredBlockEnd
            }
        } else {
            EndedBy::Terminator
        };
        assert_eq!(
            span.ended_by, expected_ended_by,
            "[{label}] unit {i}: {span:?}"
        );
        assert!(span.terminated, "[{label}] unit {i}: {span:?}");

        let mut expected_content = unit.clone();
        assert_eq!(
            expected_content.pop(),
            Some(';'),
            "malformed test unit {unit:?}"
        );
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
    let scripts_per_seed = if cfg!(debug_assertions) { 40 } else { 2_000 };
    let mut total_scripts = 0usize;

    for seed in seeds {
        let mut rng = Rng::new(seed);
        for _ in 0..scripts_per_seed {
            let n_units = 3 + rng.below(6); // 3..=8, per the review
            let units: Vec<(String, bool)> = (0..n_units).map(|_| gen_unit(&mut rng)).collect();

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
