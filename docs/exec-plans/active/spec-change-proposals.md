# Proposed `SPEC.md` changes — for the owner

**`SPEC.md` is the owner's document and is not edited here or anywhere else in this change.** This
file lists edits the Phase 0 spike evidence suggests, with rationale and evidence links, for the
owner to accept, amend or reject. Section numbers below are `SPEC.md`'s current numbering as of this
writing; all evidence is from
[`docs/exec-plans/active/phase-0-spike-results.md`](phase-0-spike-results.md) (spikes run
2026-09-19 against a live Oracle 19.3 database) unless noted otherwise.

## 1. §8 — driver test matrix limitations of the current upstream beta

`SPEC.md` §8's "Driver test matrix" lists connectivity, SQL, procedural SQL, types, transactions and
operations Phase 0 must validate, without qualifying which are actually met by the chosen driver.
Spike evidence shows the matrix is met with specific, documented exceptions:

- **Types** — `TIMESTAMP WITH TIME ZONE` is refused at describe time by default (see proposal 5
  below); `JSON where applicable` needs the split in proposal 5; `CLOB/NCLOB/BLOB` is proven for
  CLOB/BLOB (spike S7) but NCLOB was not directly spiked.
- **Connectivity** — TCPS is listed but not run (spike S8 — the Phase 0 test database has no TCPS
  listener); privileged connections are only lightly exercised (a `SYSTEM` connection for spike S4's
  cancellation candidate, not a general privileged-connection test).
- **Operations** — `cancellation` is not met (see proposal 2); `network loss` and `reconnect` were
  not evidenced at all in Phase 0; `metadata/dictionary access` has only incidental evidence
  (`USER_ERRORS`, `V$SESSION`, both used for other tests' own purposes, not tested as a capability in
  their own right); `EXPLAIN PLAN / DBMS_XPLAN` was not run.

**Proposed edit:** add a short note to §8 (or a linked appendix) recording that the matrix is
validated against `oracledb` 26.0.0-beta.3 specifically, with the exceptions above, rather than
implying every row is proven. `phase-0.md` Workstreams C–G already carry this detail; §8 currently
does not point to it.

**Evidence:** `phase-0-spike-results.md` §2 ("Results at a glance") and §3 (per-spike detail);
`phase-0.md` Workstreams C, D, F, G (as updated in this change).

## 2. §10 / §24.8 — cancellation wording options

`SPEC.md` §10 says "Every worksheet must expose Execute, Cancel, Commit, and Rollback" and §24.8 lists
"cancel running statements" as a Definition-of-Done item, both without qualification — read literally,
they promise an on-demand cancel that stops a running statement while leaving the session usable.
Spike S4 found that `oracledb` 26.0.0-beta.3 cannot do this in the general case: the only mechanism is
a deadline armed *before* the call starts, and it destroys the session for any statement the server
will not interrupt promptly (a PL/SQL block, in particular). [ADR-0001](../../decisions/0001-database-driver-strategy.md)
is re-opened over exactly this gap.

**Proposed edit options, exactly as `phase-0-spike-results.md` §9 frames the owner's decision (not
narrowed here):**

1. Reword §10/§24.8 to describe what Reldex can honestly ship: a per-statement **limit** (a deadline
   the user sets before running a statement) rather than an on-demand **cancel**, with the UI stating
   the distinction plainly. This matches shipping today with a pre-armed deadline and an honest UI —
   recommended by the spike author and the lead.
2. Leave §10/§24.8 as the target requirement and mark the Definition-of-Done item **not yet met**,
   pending upstream work (the four drafted issues in `phase-0-spike-results.md` §6) or a different
   driver. This treats the current gap as temporary rather than redefining the requirement.
3. If the owner instead reopens ADR-0001's rejected alternatives (a different driver, embedding a
   non-Rust thin driver, ODPI-C + Instant Client, or a from-scratch Rust driver) and one of those
   supports on-demand cancellation, no SPEC wording change would be needed — but each alternative
   carries its own costs recorded in ADR-0001's "Alternatives considered".

**This document does not recommend which option carries the SPEC edit** — that is the same owner
decision ADR-0001 is re-opened for; a wording change should follow the decision, not substitute for
it.

**Evidence:** `phase-0-spike-results.md` §4 ("S4 in full") and §9 ("Go/no-go per kill criterion" —
"What the owner has to decide", item 1); [ADR-0001 "Spike outcome (2026-09-20)"](../../decisions/0001-database-driver-strategy.md).

## 3. §24.14 — compile-error highlighting scoped to PL/SQL

`SPEC.md` §24.14 ("view compile errors") is read by the team as implying "highlight the offending
token in the worksheet" for SQL errors generally. Upstream defect U-8 makes this unreachable for
ordinary SQL: `oracledb` 26.0.0-beta.3 reads the wire's error position (`resp.read_ub2()?`) and
discards it, so a character offset for a plain SQL error is unavailable at any price. Only the
`line n, column m` that `ORA-06550` embeds in its own message text — the PL/SQL compilation-error
case — can be recovered.

**Proposed edit:** scope §24.14's error-position/highlighting language to PL/SQL compilation errors
specifically, until U-8 is fixed upstream (it is one of the four drafted issues,
`phase-0-spike-results.md` §6, Issue A's related-issues note references it; U-8 itself is not yet
drafted as a standalone issue and should be added if the owner wants it filed). For ordinary SQL
errors, Reldex can report the native `ORA-nnnnn` code and message but not a token-level position.

**Evidence:** `phase-0-spike-results.md` §5, U-8; §7, contract note C-2 (`db-driver-api`'s
`Capabilities::error_position` has no finer grain than one boolean — see contract note C-3 alongside
it).

## 4. §22 — Pro / third-party notices requirement

`SPEC.md` §22 describes the Community/Pro edition split and entitlement policy but says nothing about
third-party notices. The oracle-thin driver's dependency graph is 55 third-party crates at run time
(63 including build-only crates), all permissively licensed (MIT, Apache-2.0, ISC, BSD-3-Clause,
UPL-1.0, CDLA-Permissive-2.0, BSL-1.0, CC0-1.0, MIT-0) with no copyleft — a closed-source Pro edition
is legally unobstructed, **but licence text and copyright notices must be reproduced in the
distributed product** (MIT, BSD-3-Clause, ISC, Apache-2.0 and UPL-1.0 all require this).

**Proposed edit:** add a requirement to §22 (or a new subsection) that any distributed build —
Community or Pro — ships a generated third-party notices file (e.g. via `cargo about`) covering the
full transitive dependency graph, not just direct dependencies, before first binary distribution.
This is already tracked as a `TASKS.md` P0 follow-up; a SPEC line would make it a product requirement
rather than only an engineering task.

**Evidence:** `phase-0-spike-results.md` §1 ("Dependencies and licences").

## 5. TIMESTAMP WITH TIME ZONE / JSON support caveats

Neither §8's type matrix nor any other section currently distinguishes "supported", "supported with a
caveat" and "refused" for individual types. Two cases the spikes found need one of those three labels,
not a blanket "supported":

- **`TIMESTAMP WITH TIME ZONE`.** A named-region value aborts the whole process in `oracledb`
  26.0.0-beta.3 (upstream defect U-3: an unimplemented decode path, `todo!()`). The driver contains
  this by refusing the column at describe time, before any value is decoded — the session stays
  usable, but the type is unreadable as a typed value by default. The offset-only form decodes
  correctly and is proven to (to the exact instant, after a separate driver fix, U-5), but is reachable
  only behind an opt-in extension (`oracle.allow_timestamp_with_time_zone`) that Reldex does not enable
  by default, because the two wire forms are indistinguishable before decoding.
- **`JSON`, `XMLType`, `VECTOR`, object types, `BFILE`.** Oracle 19c (the Phase 0 target) has no native
  `JSON` column type — JSON there is `VARCHAR2`/`CLOB`/`BLOB` with `IS JSON`, which all work normally.
  A genuine `JSON` column type (21c+), and `XMLType`/`VECTOR`/object types/`BFILE` on any version, are
  refused at describe time: upstream's row decoder has no branch for any of them, so there is nothing
  to fetch, not merely nothing to render as text (contrast with `INTERVAL`/`ROWID`/
  `TIMESTAMP WITH LOCAL TIME ZONE`, which render as best-effort text under `db-driver-api`'s
  `ColumnData::Unsupported`, per [ADR-0002](../../decisions/0002-driver-api-and-concurrency-model.md)
  amendment M1 and its Phase 0 carve-out).

**Proposed edit:** add a short caveats list to §8's type matrix (or a footnote) naming both cases and
linking to the spike results, so a future reader of §8 does not read "TIMESTAMP WITH TIME ZONE" and
"JSON where applicable" as unconditionally supported.

**Evidence:** `phase-0-spike-results.md` §3 (S2 table, rows for `TIMESTAMP WITH TIME ZONE` and
`XMLTYPE`/JSON/VECTOR/object types/BFILE) and §5, U-3;
[ADR-0002](../../decisions/0002-driver-api-and-concurrency-model.md) amendment M1 and its Phase 0
carve-out.

## Summary table

| SPEC section | Proposed change | Owner action needed |
| --- | --- | --- |
| §8 | Note the matrix is validated against `oracledb` 26.0.0-beta.3 with named exceptions | Accept/amend wording |
| §10 / §24.8 | Reword to "limit" vs. "cancel", or keep as an unmet target, or reopen the driver decision | **Decide the cancellation path (ADR-0001)** |
| §24.14 | Scope "highlight the offending token" to PL/SQL | Accept/amend wording |
| §22 | Add a third-party notices requirement | Accept/amend wording |
| §8 (types) | Add TIMESTAMP WITH TIME ZONE / JSON caveats | Accept/amend wording |
