# 0001 — Database Driver Strategy

**Status:** Accepted — owner decision 2026-09-19: stay on oracledb; pre-armed deadline + honest UI; upstream issues pending — upstream issues #21–#25 filed 2026-09-19
**Date:** 2026-09-19
**Amended:** 2026-09-19 — **C1 revised and spike S4 widened.** The ADR-0002 API review re-read
`oracledb`'s source and established that `set_call_timeout` locks the same `Arc<Mutex<Client>>` that
`execute` holds for the whole round trip. The "cancel = short call timeout on demand" fallback this
ADR originally recommended therefore does not exist. See C1 and the S4 row.
**Amended:** 2026-09-19 — **Spike outcome recorded; ADR re-opened.** Spikes S1–S5, S7 and S9 ran
against the live Phase 0 test database on 2026-09-19. S4's kill criterion fired: no mechanism stops a
running statement and keeps the session in the general case. Per this ADR's own rule ("If a kill
criterion in the spike plan fires, this ADR is re-opened rather than silently worked around"), it is
now re-opened for the owner. See "Spike outcome (2026-09-19)" below. The owner has not changed
drivers — `oracledb` remains the chosen driver — only the cancellation mechanism is undecided.
**Amended:** 2026-09-19 — **Owner decision recorded; ADR re-closed.** See "Owner decision
(2026-09-19)" below.
**Decided by:** project owner, 2026-09-19 — the primary driver is Oracle's official
[`oracle/rust-oracledb`](https://github.com/oracle/rust-oracledb) (crate `oracledb`). If a kill
criterion in the spike plan fires, this ADR is re-opened rather than silently worked around.

All external facts below were checked on **2026-09-19**. Claims that could not be verified from a
primary source are labelled **Unverified**.

## Spike outcome (2026-09-19)

Spikes S1–S5, S7 and S9 ran against the live Phase 0 test database (Oracle 19.3 EE, container
`reldex-oracle19c`) on **2026-09-19**; S6 (mobile cross-compile) ran in CI on 2026-09-19 (PR #3); the
evidence-gap spikes S10–S14 (network loss/reconnect, NCLOB, developer features, privileged
connections, large result) ran against the same test database on 2026-09-19. Full detail,
measurements and evidence are in
[`docs/exec-plans/active/phase-0-spike-results.md`](../exec-plans/active/phase-0-spike-results.md)
and, for S6, [`docs/exec-plans/active/phase-0-s6-mobile-cross-compile.md`](../exec-plans/active/phase-0-s6-mobile-cross-compile.md);
what follows is the verdict summary only — read those files for the numbers behind each line.

| Spike | Verdict |
| --- | --- |
| S1 connect / auth | **Pass** |
| S2 type fidelity | **Partial** — reads exact to 40 digits and byte-exact Thai/non-BMP text; NUMBER binds unsafe for two upstream shapes (U-1, U-2), refused rather than corrupted; `TIMESTAMP WITH TIME ZONE` refused at describe time (U-3 containment) |
| S3 session / transaction | **Pass** |
| S4 cancellation | **Fail for the requirement** — no mechanism stops a running statement and keeps the session in the general case |
| S5 PL/SQL | **Pass** (REF CURSOR included, after contract fix C-1) |
| S7 LOB streaming | **Pass** |
| S9 concurrency | **Pass** |
| S8 | **Pass with limits** (2026-09-19): pure-Rust TCPS session with certificate and host-name verification on, TLS 1.2 `ECDHE-RSA-AES256-GCM-SHA384`, confirmed server-side (`NETWORK_PROTOCOL = tcps`); trust via a user-supplied PEM only — no mTLS combined with a private CA (U-13), no `ewallet.p12`/OS trust store (U-12), `SSL_SERVER_DN_MATCH` and `SSL_SERVER_CERT_DN` ignored upstream (U-14 — since 2026-09-19 the driver refuses the second and warns about the first rather than forwarding either silently; neither is implemented), no revocation — kill criterion did NOT fire |
| S6 mobile cross-compile | **Pass** (2026-09-19, PR #3): `aarch64-linux-android`, `aarch64-apple-ios` and `aarch64-apple-ios-sim` all compile and link in CI, no extra tools for `aws-lc-sys`, provider swap to `ring` impossible without forking — kill criterion did NOT fire. Cross-compile evidence only, not mobile support; physical-device validation is still open (`phase-0-s6-mobile-cross-compile.md`) |
| S10 network loss / reconnect | **Pass, two upstream gaps** (2026-09-19, evidence-gap spike, no kill criterion of its own): a dead socket is detected in microseconds and reported `Lost`; nothing reconnects by itself; an in-doubt commit is surfaced, not guessed. A black-holed link never returns without a deadline (U-17), a connect cannot be bounded at all (U-15), and a dead client's row lock blocked a second session for the full 20 s measured |
| S11 NCLOB | **Pass** (2026-09-19) — Thai and non-BMP text byte-exact through the lazy stream at six buffer sizes; `NULL`/`EMPTY_CLOB()` stay distinguishable |
| S12 developer features | **Pass, one upstream blocker** (2026-09-19) — EXPLAIN PLAN, both `DBMS_XPLAN` entry points, `V$`, ten dictionary views, `LONG`/`LONG RAW` and `DBMS_METADATA.GET_DDL` all work; `CREATE TRIGGER` with `:NEW` does not (U-18) |
| S13 privileged connection | **Pass** (2026-09-19) — `AS SYSDBA` over the listener through the existing `SessionRole` contract; no contract gap, no upstream gap |
| S14 large result | **Pass** (2026-09-19) — 1,000,000 rows streamed for ~1 MB of working-set growth; throughput not monotonic in batch size |

**S4 is the spike this ADR's decision rests on, and its kill criterion fired.** A pre-armed
per-round-trip deadline (`CancelKind::PreArmedDeadline`) is the only mechanism that works at all, and
it costs the session whenever the server cannot answer the interrupt promptly (a PL/SQL block, in
particular). `SPEC.md` §10/§24.8 "Cancel" is **not met** by `oracledb` 26.0.0-beta.3 — not by this
wrapper, not with the privileged `ALTER SYSTEM CANCEL SQL` extra, and not by a small fork of the
upstream crate (assessed, not built; see the results file §4 candidate 4). Per this ADR's own rule,
**it is re-opened for the owner.**

**Options, exactly as the results file §9 frames them:**

1. **Ship with a pre-armed deadline and an honest UI, and submit the upstream issues.** The driver
   reports `CancelKind::PreArmedDeadline` and the UI says plainly that a running statement can only be
   stopped by a limit set before it starts. **Recommended by the spike author and the lead.**
2. **Wait for upstream.** Of the seven drafted issues (results file §6), five were submitted
   2026-09-19 by the owner's account (SupawitNu) — A (public break/interrupt API) →
   [#24](https://github.com/oracle/rust-oracledb/issues/24), B (NUMBER bind ×10) →
   [#21](https://github.com/oracle/rust-oracledb/issues/21), C (process aborts) →
   [#22](https://github.com/oracle/rust-oracledb/issues/22), D (call-timeout recovery / cancel not
   observed) → [#23](https://github.com/oracle/rust-oracledb/issues/23), E (TCPS trust) →
   [#25](https://github.com/oracle/rust-oracledb/issues/25) — and lead time from here is weeks to
   months regardless. F (U-15…U-17: connect cannot be bounded, timeout cause discarded, no
   keepalive/`EXPIRE_TIME`) and G (U-18: `CREATE TRIGGER … :NEW` impossible) are drafted but await the
   owner's go-ahead to submit — a separate owner action regardless of which option is chosen.
3. **Re-open this ADR's rejected alternatives** (embedding a non-Rust thin driver, ODPI-C + Instant
   Client, writing a Rust driver from scratch) — see "Alternatives considered" above. Each was
   rejected for reasons independent of cancellation and those reasons still hold.

**The lead did not change drivers.** The owner explicitly chose `oracledb` (see "Decided by" above),
and nothing in the S4 result changes that choice — only the cancellation mechanism the product can
offer is in question. Adopting a different driver is alternative 3 above, not a decision made here.

The other kill criteria did not fire: S1, S3, S5, S7, S9 and S6 (mobile cross-compile) pass outright;
S2's NUMBER and `TIMESTAMP WITH TIME ZONE` findings were upstream defects contained by refusal (see
U-1, U-2, U-3 in the results file), not silent precision loss or corruption, so S2 is a **conditional
go**, not a kill. The evidence-gap spikes S10–S14 carry no ADR-0001 kill criterion of their own; they
added four more upstream defects — **U-15** (a connect cannot be bounded in time), **U-16** (a socket
timeout is misreported as a call timeout, cause discarded), **U-17** (no dead-link detection: no
keepalive, `EXPIRE_TIME` unused), and **U-18** (`CREATE TRIGGER … :NEW` cannot be executed) — drafted
as issues F and G above.

## Owner decision (2026-09-19)

The project owner reviewed the lead's summary of the spike outcome above and, in chat on
2026-09-19, answered "as you recommended" to the open items this ADR was re-opened for:

1. **Stay on `oracledb`.** The 2026-09-19 driver decision stands; the rejected alternatives under
   "Alternatives considered" are not reopened and no fork is pursued.
2. **Ship with the pre-armed per-statement deadline (`CancelKind::PreArmedDeadline`) and an honest
   UI.** The product must state plainly that on-demand Cancel is unavailable with the current driver.
   `SPEC.md` §10 carries this as an interim note, and §24.8 stays an **unmet target** — marked "not
   yet met — blocked on upstream driver (ADR-0001)" rather than redefined as satisfied.
3. **Pursue the upstream fixes via the drafted issues** (`phase-0-spike-results.md` §6), Issue B
   (the silent NUMBER-bind corruption, U-1) first. **Update 2026-09-19:** issues A–E, including B,
   were submitted by the owner's account — see the links in "Spike outcome" above; F and G (from the
   later evidence-gap spikes) are drafted and await the owner's go-ahead to submit
   (`TASKS.md`).

Spike S4's kill criterion fired, and the owner's decision is to **accept the limitation rather than
change drivers**: the gap is real, but none of the rejected alternatives (a different driver, an
embedded non-Rust thin driver, ODPI-C + Instant Client, or a from-scratch Rust driver) is judged worth
its own cost for this gap alone, and the upstream maintainer has been responsive to prior reports.

This ADR is **re-closed** as `Accepted` with this decision recorded. It would be **re-opened again**
if:

- upstream declines to add a cancel/break API (Issue A, submitted as [#24](https://github.com/oracle/rust-oracledb/issues/24)) after a reasonable review period, or
- a data-corruption defect surfaces with no wrapper-side guard — i.e. a value upstream mis-handles
  silently that Reldex cannot detect and refuse the way U-1/U-2 are refused today, or
- a new kill criterion fires in a spike run after this decision (S6 and S8 have since passed, S8 with
  limits; S10–S14 carry no ADR-0001 kill criterion of their own).

Everything in "Spike outcome (2026-09-19)" above remains the technical record of what was found; this
section records only what the owner decided to do about it. The "undecided"/"re-opened for the owner"
language above describes the state as of 2026-09-19 before this decision; it is now decided.

### Addendum — owner decisions on results file §9 items 8–12 (2026-09-19)

Separately from the cancellation decision above, on 2026-09-19 the owner answered items 8–12 of
`phase-0-spike-results.md` §9 ("What the owner has to decide"), accepting the lead's recommendation
for each with one added product requirement: every one of these behaviours must be
user-configurable. Full evidence and wording are in `phase-0-spike-results.md` §9 and in `SPEC.md`;
summarized here, one line each:

- **Item 8 (`connect_timeout`, C-5/U-15):** implement on a helper thread, default 15 s,
  user-configurable per connection profile including "no limit". Implementation pending.
- **Item 9 (default per-statement time limit):** default 600 s, configurable at three levels
  (application default, connection profile, per worksheet/statement), including "no limit" with an
  explicit UI warning about the consequence. The existing `SPEC.md` §10 interim-limitation
  constraints are unchanged.
- **Item 10 (`SQLNET.EXPIRE_TIME`):** documentation recommendation only — Reldex cannot enforce or
  detect it; the Phase 0 test database stays unset so S10's measurements remain valid.
- **Item 11 (`CREATE TRIGGER` U-18):** the driver auto-rewrites the DDL to
  `EXECUTE IMMEDIATE`, on by default, always reported to the user, with a per-connection off switch.
  Implementation pending.
- **Item 12 (default fetch batch size):** not chosen now; deferred to a Phase 1 benchmark (S14 found
  throughput is not monotonic in batch size). Must be a user setting (application default + per
  connection profile) within a bounded range.

This addendum does not change this ADR's `Accepted` status, and it does not alter the cancellation
decision recorded above; it records separate, narrower owner decisions from the same results file.

## Context

`ARCHITECTURE.md` §13 open question 1 asks which pure/thin driver backs `drivers/oracle/thin`.
`SPEC.md` §7 requires the default connection path to need no Instant Client/OCI, targets Oracle
Database 19c+, and states "Mobile must never require OCI". `SPEC.md` §8 requires validation on
Windows x64, Linux x64, macOS ARM64, Android ARM64 and iOS ARM64. This is the single biggest
project risk: every Phase 0 workstream depends on it.

The project owner added one constraint: **use a driver that Oracle itself maintains.**

The landscape changed materially in 2026. Oracle now publishes a **pure-Rust thin driver**:
[`oracle/rust-oracledb`](https://github.com/oracle/rust-oracledb), crate
[`oracledb`](https://crates.io/crates/oracledb). Repo created 2026-07-20; first release
v26.0.0-beta.1 on 2026-08-06; latest v26.0.0-beta.3 on 2026-09-08; last push 2026-09-16; 24 stars;
6 open issues. Oracle also publishes [`oracle/go-oracledb`](https://github.com/oracle/go-oracledb)
(UPL-1.0, v26.0.1-beta, 2026-08-31). This means the owner's constraint, `SPEC.md` §7 and a Rust
core are **simultaneously satisfiable** — which was not true before August 2026.

Note on provenance: the crates.io name `oracledb` was previously a community crate by `MuhDur`
(v0.1.0–v0.9.1, repo `MuhDur/rust-oracledb`, described as a "clean-room port of python-oracledb
thin mode"). Those versions are **yanked**; v26.0.0-beta.* are published by `anthony-tuininga` with
repository `oracle/rust-oracledb`. The name was handed to Oracle; the code lineage of the current
crate is Oracle's own (`git log` shows 72 commits, all by Anthony Tuininga).
Sources: [crates.io owners API](https://crates.io/api/v1/crates/oracledb/owners),
[v0.9.1](https://crates.io/api/v1/crates/oracledb/0.9.1),
[v26.0.0-beta.3](https://crates.io/api/v1/crates/oracledb/26.0.0-beta.3).

## Decision

**Accepted by the owner on 2026-09-19** (originally written as a recommendation).

1. Adopt the Oracle-maintained crate **`oracledb`** (`oracle/rust-oracledb`) as the thin driver
   implementation hosted by `crates/drivers/oracle-thin` (package `reldex-driver-oracle-thin`).
2. Pin an **exact** version (`=26.0.0-beta.3` or later beta) — it is pre-GA and the API is
   explicitly "subject to change".
3. Keep it strictly behind `db-driver-api` per `SPEC.md` §7, with Reldex-owned contract tests so an
   upgrade that changes behaviour is detectable in CI.
4. Treat the **missing statement-cancellation API** as a gate, not a detail (see Conflicts).
5. Document the community `oracle` crate (ODPI-C + Instant Client) as the **desktop-only fallback**
   if a kill criterion fires. Mobile keeps no OCI fallback.

This decision is **conditional on the Phase 0 spikes below**: the owner has chosen the driver, but
the status only becomes unconditional `Accepted` once spikes S1–S5 pass.

## Conflicts the owner must resolve

The owner's "Oracle-maintained" constraint and `SPEC.md` §7 no longer conflict. Three real
conflicts remain.

**C1 — Cancellation is not in the public API, and the obvious stand-in does not work.** *(Revised
2026-09-19 after the ADR-0002 API review re-read the source; the original text is superseded because
it was too optimistic.)*

`SPEC.md` §10 requires every worksheet to expose Cancel; §24.8 makes it a Definition-of-Done item;
`ARCHITECTURE.md` §6 and `phase-0.md` Workstream C require cancelling from a *different control
path*. Searching the crate for any public cancel/break/interrupt function returns nothing; the public
`Connection` surface is close/commit/rollback/execute/query/ping/statement/set_call_timeout and
metadata accessors
([`src/connection/mod.rs`](https://github.com/oracle/rust-oracledb/blob/main/src/connection/mod.rs)).
The protocol machinery exists but is private and reachable only via read-timeout recovery:
`recover_from_error()` sends `MARKER_TYPE_INTERRUPT` then resets
([`src/client/mod.rs`](https://github.com/oracle/rust-oracledb/blob/main/src/client/mod.rs)); the
break marker constant is `_MARKER_TYPE_BREAK`, i.e. unused
([`src/constants.rs`](https://github.com/oracle/rust-oracledb/blob/main/src/constants.rs)).

**What the re-read established.** The client is held as `Arc<Mutex<Client>>`, and `execute()` locks
it for the duration of the round trip. `Connection::set_call_timeout` locks **the same mutex**.
Therefore the option this ADR originally recommended — "use `set_call_timeout` as a coarse stand-in,
arming a short timeout when the user presses Cancel" — **cannot work**: the call that arms the
timeout would block until the statement it is meant to stop has finished. It is not a degraded
cancel; it is a hang on the control path that was supposed to stay responsive (`SPEC.md` §24.17).

**What remains true.** `set_call_timeout` *can* bound a call if it is armed **before** the call
starts. That is a per-call deadline, not cancellation: the user cannot change their mind once a
statement is running, and the latency of "stopping" is whatever deadline was set in advance.
ADR-0002 therefore models this honestly as `CancelKind::PreArmedDeadline` with an explicit
`CancelOutcome::NotInterruptible` result, and the contract requires `request_cancel` never to block
on the connection's own call lock. A driver in that class **does not satisfy `SPEC.md` §10/§24.8**,
and the UI must say so rather than show a Cancel button that does nothing.

**Options, in the order spike S4 must evaluate them** (see the revised S4 row):

(a) **Pre-armed call timeout.** Works today, costs nothing, and gives a bounded worst case. Latency
    equals the deadline, so it is a *limit*, not a cancel. Phase 0 baseline, not a solution.
(b) **Privileged control session issuing `ALTER SYSTEM CANCEL SQL 'sid, serial#'`** (18c+) over a
    second connection. This is a genuine cancel from a different control path, but it requires the
    `ALTER SYSTEM` privilege, which most application accounts do not and should not have. It can only
    ever be an **opt-in extra** for sites that choose to grant it, never the default path.
(c) **Upstream enhancement request** for a public cancel/break API. The right long-term answer, and
    cheap to file; the upstream maintainer has been responsive (see C2). Not something Phase 0 can
    depend on.
(d) **Minimal fork/patch** exposing a break handle that writes the TTC break/interrupt marker on a
    **cloned socket, outside the mutex**. This is how a real break works at the protocol level and is
    why it can be delivered while `execute` holds the lock. Contingency: carrying a patch against a
    pre-GA crate in the most correctness-critical layer is a real maintenance cost (C2).

This ADR now recommends running S4 through (a)→(d) in that order and treats (c)+(d) as the only
paths to meeting `SPEC.md` §24.8 for an unprivileged account. If only (a) works, the kill criterion
fires: this ADR is re-opened with the owner.

**C2 — Beta maturity vs. correctness-first priorities.** `SPEC.md` §2 ranks database and
transaction correctness above everything. The driver is a ~2-month-old pre-release ("APIs and
functionalities are subject to change"), and its short issue history already includes two
transaction/session-correctness bugs — a pool that omitted rollback on return (#15) and hangs on
repeated PL/SQL OUT binds (#17) — both fixed within days. Responsiveness is excellent; absolute
maturity is low. Mitigation: pinning, encapsulation, and Reldex contract tests.

**C3 — Mobile is unproven.** Oracle documents testing against Rust 1.89 and Oracle Database 26ai,
21c and 19c, but names **no target platforms**
([`doc/introduction.md`](https://github.com/oracle/rust-oracledb/blob/main/doc/introduction.md)).
Android/iOS are almost certainly untested upstream. The dependency set is encouraging (below) but
`SPEC.md` §25 requires physical-device evidence regardless.

## Evidence

### Oracle-maintained drivers and libraries

| Product | Thin (no Instant Client)? | Licence | Usable from a Rust core? | 5 targets |
| --- | --- | --- | --- | --- |
| [rust-oracledb](https://github.com/oracle/rust-oracledb) (`oracledb`) | **Yes** | UPL-1.0 OR Apache-2.0 | **Native** | Win/Linux/macOS yes; Android/iOS **Unknown** |
| [go-oracledb](https://github.com/oracle/go-oracledb) | Yes | UPL-1.0 | Only via cgo c-shared | Go runtime on mobile is awkward |
| [python-oracledb](https://github.com/oracle/python-oracledb) | Yes (thin) + thick | UPL-1.0 OR Apache-2.0 | Only via embedded CPython | Embedding CPython on iOS is impractical |
| [node-oracledb](https://github.com/oracle/node-oracledb) | Yes (thin) + thick | UPL-1.0 OR Apache-2.0 | Only via embedded Node | Rejected: runtime size, no JIT on iOS |
| Oracle JDBC thin (`ojdbc11`) | Yes | Oracle Free Use Terms (not OSI) — **Unverified** exact terms | Only via embedded JVM/JNI | No JVM on iOS |
| ODP.NET Core managed | Yes | Closed source, Oracle licence — **Unverified** | Only via hosted .NET/NativeAOT | High cost, closed source |
| [ODPI-C](https://github.com/oracle/odpi) | **No** (wraps OCI) | UPL-1.0 OR Apache-2.0 | Yes, via `oracle` crate | Desktop only |
| Instant Client / OCI | No (is the client) | Oracle licence | Via ODPI-C | **No Android, no iOS** |

Instant Client is published for Linux x86-64/ARM64, Windows x64, macOS x86-64 and
[macOS ARM64](https://www.oracle.com/database/technologies/instant-client/macos-arm64-downloads.html);
Oracle's Instant Client documentation
([26ai](https://docs.oracle.com/en/database/oracle/oracle-database/26/mxcli/installing-and-removing-oracle-database-client.html))
mentions no Android or iOS build. Any OCI-based path therefore cannot satisfy `SPEC.md` §7's mobile
rule — which is why the OCI options are fallbacks, not candidates.

### Rust candidates

| Crate | Thin? | Maintainer | Licence | Latest | Stars / downloads | Verdict |
| --- | --- | --- | --- | --- | --- | --- |
| `oracledb` | Yes | **Oracle Corp** (Anthony Tuininga, sole committer; also python-oracledb/ODPI-C lead) | UPL-1.0 OR Apache-2.0 | 26.0.0-beta.3, 2026-09-08 | 24★ / 5.5k | **Recommended** |
| [`oracle-rs`](https://github.com/stiang/oracle-rs) | Yes, async/tokio | Community, 1 person | MIT OR Apache-2.0 | 0.1.7, 2026-03-24 | 26★ / 13k | Stale 6 months; no |
| [`oracle`](https://github.com/kubo/rust-oracle) | **No** — ODPI-C, needs C compiler + Oracle Client 11.2+ | Community (kubo) | UPL-1.0/Apache-2.0 | 0.6.3, 2025-01-02 | 229★ / 2.9M | Desktop fallback only |
| [`sibyl`](https://github.com/quietboil/sibyl) | **No** — OCI | Community, 1 person | MIT | 0.7.1, 2026-06-26 | 43★ / 58k | Fallback only |
| `oraclemcp-driver-cx` | Yes | Community (MuhDur) | MIT OR Apache-2.0 | 0.9.3, 2026-08-06 | — | Superseded by Oracle's |
| sqlx | — | — | — | — | — | No Oracle support ([#3219](https://github.com/launchbadge/sqlx/issues/3219)) |

### `oracledb` shape and dependencies

Pure Rust, **synchronous/blocking** — `std::net::TcpStream`, no tokio/async-std anywhere in
`Cargo.toml`. TLS is **rustls 0.23** + `webpki-roots`; the remaining deps are `aes`, `cbc`,
`pbkdf2`, `pkcs8`, `sha2`, `rand`, `chrono`, `uuid`, `whoami`, `base16ct`, `base64ct`, and optional
`arrow-array`/`arrow-schema` behind an `arrow` feature. **No `build.rs`, no direct C dependency,
no OpenSSL.** Transitively, rustls's default `aws-lc-rs` provider pulls in `aws-lc-sys`, which
compiles C/assembly and therefore needs a C toolchain per target (see Platform viability; the
`ring` provider — also C/assembly, but a lighter build — is the swap candidate for spike S6 if
`aws-lc-sys` fails on a target). ~20,500 lines across 85 source files
([Cargo.toml](https://github.com/oracle/rust-oracledb/blob/main/Cargo.toml)).

*Implication for the next ADR (do not decide here):* a blocking driver forces the core's concurrency
model toward dedicated per-session threads rather than an async runtime, which suits
`ARCHITECTURE.md` §5's "worksheet owns a stable session" model but must be weighed against mobile
battery/binary cost (open question §4). `Connection` holds `Arc<Mutex<Client>>`, so it is movable
between threads, but one connection serialises its own calls.

The optional `arrow` feature is a notable bonus for `SPEC.md` §12 / `ROADMAP` Phase 3, but Arrow
must still earn its place by benchmark and must not reach UI APIs.

### SPEC §8 driver test matrix vs `oracledb`

Sources: `A` = [Appendix A feature table](https://github.com/oracle/rust-oracledb/blob/main/doc/appendix_a.md),
`D` = driver docs under `doc/`, `R` = [release notes](https://github.com/oracle/rust-oracledb/blob/main/doc/release_notes.md),
`S` = source, `I` = GitHub issue.

| Item | Status | Source |
| --- | --- | --- |
| Easy Connect / service name / connect descriptor / tnsnames | Supported | D `connection_handling.md` §2.2; S `tnsnames_file_parser.rs` |
| Listener redirects (remote listener) | Supported | R beta.2; I [#2](https://github.com/oracle/rust-oracledb/issues/2) |
| TCP | Supported | A |
| TCPS — one-way TLS and mTLS, wallet (`ewallet.pem`) | Supported | A; S `transport.rs` |
| Username/password auth | Supported | A |
| **11G password verifier** | **Missing** (12C only) | I [#6](https://github.com/oracle/rust-oracledb/issues/6) — open |
| **Native Network Encryption (NNE)** | **Missing** — "No - use TLS instead" | A |
| External auth / Kerberos / token / LDAP / SEPS | Missing | A |
| Privileged connections (SYSDBA) | Partial — only a raw `set_auth_mode(u8)`, constants undocumented | A "Yes"; S `config/base.rs` |
| SELECT / INSERT / UPDATE / DELETE / MERGE / DDL | Supported | D `sql_execution.md` |
| PL/SQL blocks, procedures, functions, packages | Supported | D `plsql_execution.md` |
| IN / OUT / IN OUT binds | Supported | R beta.3 (pure OUT binds by type) |
| REF CURSOR | Supported | R beta.3; S `DB_TYPE_CURSOR` |
| NUMBER | Supported — `OracleNumber` plus `i8..i128`/`u8..u128`; no lossy f64 default | D `sql_execution.md` §192 |
| CHAR / VARCHAR2 / NVARCHAR2 / NCHAR | Supported; **client charset is UTF-8 only** | A |
| DATE / TIMESTAMP | Supported | D |
| TIMESTAMP WITH TIME ZONE | **Partial** — explicit offset dropped when binding | I [#8](https://github.com/oracle/rust-oracledb/issues/8) — open |
| RAW / LONG RAW / ROWID / UROWID | Supported | R beta.4 (UROWID) |
| CLOB / NCLOB / BLOB, temporary LOBs, locator ops | **Partial** — `Lob::read()` fails on non-ASCII CLOBs | A; I [#18](https://github.com/oracle/rust-oracledb/issues/18) — open |
| JSON, VECTOR, JSON-Relational Duality | Supported | A |
| SQL/PL-SQL object types and collections | Missing | A |
| COMMIT / ROLLBACK; auto-commit OFF by default | Supported — matches `SPEC.md` §10 | D `txn_management.md` |
| SAVEPOINT / ROLLBACK TO SAVEPOINT | **Unknown** — no API; presumably plain SQL, untested | D (absent) |
| **Cancel a running statement from another control path** | **Missing** — no public API, and `set_call_timeout` cannot substitute (it takes the same mutex `execute` holds) | S (see C1) |
| Call timeouts | Supported, but only **armed before the call** | A; `Connection::set_call_timeout` (see C1) |
| Network-loss detection / reconnect semantics | Unknown | D `ha.md` defers to Oracle Net config |
| Concurrent independent sessions | Supported in principle (separate `Connection`s); unmeasured | S |
| Large results / fetch batching (`fetch_array_size`, `prefetch_rows`) | Supported | D `tuning.md` |
| Large LOB streaming with bounded memory | Partial — API exists, memory profile unmeasured | D `lob.md` |
| DBMS_OUTPUT | Supported — worked example via OUT binds | D `plsql_execution.md` §4.6 |
| Metadata/dictionary, V$, EXPLAIN PLAN, DBMS_XPLAN | Supported (ordinary SQL) | A |
| Scrollable cursors, implicit result sets, AQ, CQN, TAF, App Continuity | Missing | A |
| Native ORA- error codes preserved | Supported — `DbError` struct added in beta.4 | R beta.4 |

For comparison, python-oracledb *thin* supports scrollable cursors, implicit result sets, CQN, AQ,
TPC and direct path loads, and also lacks NNE
([appendix A](https://python-oracledb.readthedocs.io/en/latest/user_guide/appendix_a.html)). The
Rust driver is therefore thinner than Oracle's Python thin mode, not equivalent to it.

### Platform viability

| Target | `oracledb` | Notes |
| --- | --- | --- |
| Windows x64 | Expected OK | Upstream names no tested targets — **Unverified** |
| Linux x64 | Expected OK | Issue reporters use Linux successfully |
| macOS ARM64 | Expected OK | No direct C deps; `aws-lc-sys` needs the Xcode C toolchain |
| Android ARM64 | **Unknown** | rustls default provider is `aws-lc-rs`, which lists `aarch64-linux-android` as build+tested |
| iOS ARM64 | **Unknown** | `aws-lc-rs` lists `aarch64-apple-ios` as build+tested |

rustls 0.23's default features enable `aws_lc_rs`
([crates.io features](https://crates.io/api/v1/crates/rustls/0.23.42)); the driver calls
`ClientConfig::builder()` without selecting a provider, so the process must install a default
provider (issue #6's reporter calls `aws_lc_rs::default_provider().install_default()`). Because
Cargo features are additive, `aws-lc-rs` will be compiled for every target. Non-FIPS `aws-lc-rs`
builds need "CMake **never**, bindgen **never**, Go **never**"
([docs.rs](https://docs.rs/aws-lc-rs/latest/aws_lc_rs/)) and its
[platform support table](https://aws.github.io/aws-lc-rs/platform_support.html) marks
`aarch64-linux-android` and `aarch64-apple-ios` as build+test. This is the best available evidence
that mobile is feasible, but it is upstream CI evidence, not Reldex evidence — spike S6 must prove
it, and `SPEC.md` §25 still demands physical-device runs.

### Licence

`LICENSE.txt` is "dual-licensed … under the Universal Permissive License (UPL) 1.0 … and Apache
License 2.0. You may choose either license." GitHub reports `NOASSERTION` only because it cannot
auto-classify the dual header; crates.io records `UPL-1.0 OR Apache-2.0`. Both are permissive,
non-copyleft, and impose no source-disclosure obligation — compatible with a closed-source Pro
edition and with app-store redistribution, subject to attribution. `SPEC.md` §22 is unaffected.

## Consequences

**Easier.** The owner's constraint, `SPEC.md` §7 and the Rust core are satisfied by one component.
No Instant Client to ship, license or install — installer size, support load and the macOS/Linux
packaging story all shrink. TLS is `rustls`, so there is no OpenSSL to cross-compile, which is the
usual reason mobile database drivers fail. Native ORA- codes are preserved, satisfying
`ARCHITECTURE.md` §4 and invariant 9. Optional Arrow support may later serve `SPEC.md` §12.

**Harder.** Reldex depends on pre-GA software for its most correctness-critical layer. Cancellation
— a Definition-of-Done item — has no upstream API today, and (revised 2026-09-19) no workable
substitute either: the pre-armed call timeout bounds a statement but cannot stop one on request, so
until spike S4 finds a mechanism, Reldex ships without the Cancel `SPEC.md` §24.8 requires and must
say so in the UI rather than pretend. Enterprise sites using NNE or 11G
verifiers cannot connect at all, and both are common in the 19c estate the product targets; that is
a *market* limitation, not only a technical one, and should reach the README before launch. The
blocking API constrains the concurrency ADR. UTF-8-only client charset must be checked against Thai
and NCHAR data (`SPEC.md` §14).

**Follow-up work.** ADR-0002 on driver API + concurrency model; Reldex contract tests against
`db-driver-api`; an upstream enhancement request for a cancel/break API; a watch on issues #6, #8
and #18; a documented `oracle`-crate fallback path for desktop.

**Tracking upstream releases.** Every upstream defect this ADR depends on has a canary in
`crates/drivers/oracle-thin/tests/canary_upstream_{offline,live}.rs` that asserts the defect is
still present, so a fix upstream shows up as a *failing* test naming the guard it makes removable.
A version tripwire in the offline target fails as soon as the pin moves. The procedure — which
canary covers which U-number, which guard depends on it, and what is only a manual check — is
`docs/exec-plans/active/oracledb-upgrade-checklist.md`, and it must be followed before the pin is
bumped.

## Alternatives considered

- **Write our own thin driver in Rust.** Oracle's own thin implementations are readable references
  under permissive licences (python-oracledb and node-oracledb are UPL-1.0 OR Apache-2.0; go-ora is
  MIT), so a port is legally viable. But the protocol surface is large — O5LOGON and verifier types,
  TTC function codes, data-type codecs, LOB ops, REF CURSOR, OOB break, TCPS, charset/NCHAR — and
  Oracle's own Rust effort is ~20,500 lines after two months of full-time work by the engineer who
  wrote ODPI-C and python-oracledb. A realistic Reldex estimate is **9–18 person-months to a usable
  subset and longer to trustworthy**, which would consume the entire project. Rejected: it also
  violates the owner's constraint outright.
- **Embed a non-Rust thin driver behind `db-driver-api`** (go-ora or go-oracledb as c-shared;
  JDBC thin via JNI; ODP.NET via NativeAOT; python-oracledb via embedded CPython). Each adds a
  second runtime with its own GC, threading and signal behaviour, inflates binary size, complicates
  cancellation across the FFI boundary, and is impractical or impossible on iOS. Rejected now that a
  native Rust option exists.
- **ODPI-C + Instant Client via the `oracle` crate.** Oracle-maintained C layer, mature, and it
  would satisfy "Oracle-maintained" — but it needs a C toolchain at build time and Instant Client at
  runtime, and Instant Client has no Android or iOS build. It cannot be the default path without
  amending `SPEC.md` §7 and abandoning mobile direct-connect. **Retained as the documented
  desktop-only fallback.**
- **ODBC or another bridge.** Requires an Oracle ODBC driver, which requires Instant Client.
  Dismissed for the same reason.

## Required document changes

**If the recommendation is accepted, nothing in `SPEC.md` needs amending** — this is the key result.
Only these updates are needed:

- `ARCHITECTURE.md` §13 item 1 — mark resolved, referencing this ADR.
- `ARCHITECTURE.md` §11 — name the concrete crate hosted by `crates/drivers/oracle-thin`.
- `TASKS.md` — "Create initial database driver implementation" becomes adopting and wrapping
  `oracledb`; add tasks for contract tests and the upstream cancel request.
- `phase-0.md` Workstream C — **revised 2026-09-19:** record that Phase 0 may have no cancellation at
  all, only a *pre-armed per-call deadline* (ADR-0002 `CancelKind::PreArmedDeadline`), and that this
  does not satisfy `SPEC.md` §24.8; make Workstream F note NNE and 11G verifiers as known unsupported
  configurations.
- `TASKS.md` / `Task.html` — S4's scope and budget changed (1.5 d → 2 d) and it now has four ordered
  candidates; the "upstream cancel/break enhancement request" should become a task in its own right,
  because its lead time is weeks.

**If a kill criterion fires and the ODPI-C fallback is chosen instead**, these would need amending:
`SPEC.md` §7 (default path is thin), §8 (mobile platform validation), §18 and §25 (mobile
direct-connect), §24.3 ("connect … through the thin path"); `ARCHITECTURE.md` §4 and invariants 10
and 11; `phase-0.md` Workstream G Android/iOS; `TASKS.md` P0 platform validation and P5.

## Phase 0 spike plan

Ordered, time-boxed, cheapest-kill-first. Run against the Phase 0 test database. Fallback for every
kill is the same: re-open this ADR, evaluate the ODPI-C/`oracle`-crate desktop fallback, and treat
mobile as a separate decision (gateway mode per `SPEC.md` §18).

| # | Spike | Box | Kill criterion |
| --- | --- | --- | --- |
| S1 | Connect + authenticate to 19c: Easy Connect, service name, full descriptor | 0.5 d | Cannot authenticate against a stock 19c account after verifier workaround |
| S2 | SELECT fidelity: NUMBER precision at 38 digits, DATE, TIMESTAMP, TIMESTAMP TZ, NVARCHAR2/NCLOB with Thai text, RAW | 1 d | Silent precision loss in NUMBER, or Thai/NCHAR corruption |
| S3 | Session/transaction correctness: auto-commit OFF, uncommitted state across statements, SAVEPOINT + ROLLBACK TO via SQL, second session cannot see uncommitted data | 1 d | SAVEPOINT unusable, or session state leaks between connections |
| S4 | **Cancel a long-running statement from another control path**; measure latency. Evaluate, in order: (1) **pre-armed call timeout** — `set_call_timeout` before `execute`; latency *is* the deadline, and it cannot be changed once the statement is running; (2) **privileged control session** issuing `ALTER SYSTEM CANCEL SQL 'sid, serial#'` (18c+) on a second connection — a real cancel, but needs `ALTER SYSTEM`, so only ever an opt-in extra, never the default path; (3) **upstream public cancel/break API** — file the enhancement request and record the response; (4) **minimal fork/patch** exposing a break handle that writes the TTC break/interrupt marker on a cloned socket **outside** the `Arc<Mutex<Client>>` | 2 d | Only (1) works. A pre-armed deadline is a *limit*, not a cancel: the user cannot stop a statement already running, so `SPEC.md` §10/§24.8 "Cancel" is **not met** and this ADR must be re-opened with the owner. Also killed if any mechanism leaves the session unusable, or if (2)/(4) cannot cancel within ~2 s |
| S5 | REF CURSOR + IN/OUT/IN OUT binds + DBMS_OUTPUT | 1 d | REF CURSOR or OUT binds unusable |
| S6 | Cross-compile checks: `cargo build --target aarch64-linux-android` and `aarch64-apple-ios` | 0.5 d | Either target fails to build and no provider swap fixes it |
| S7 | CLOB/BLOB streaming ≥100 MB with bounded RSS; confirm issue #18 impact | 1 d | Memory grows with LOB size, or non-ASCII CLOBs unreadable |
| S8 | TCPS against a TLS-enabled listener | 1 d | TCPS cannot be established without Instant Client |
| S9 | Network-loss detection and reconnect semantics; N concurrent sessions under load | 1 d | Lost sessions are hidden rather than surfaced |

S4 is the **first kill-criterion spike that matters** — S1–S3 are expected to pass, and S4 is where
the known gap is. Run S1–S4 before committing to Phase 1 scope. S6 is cheap and should be run early
opportunistically because it can be done without a database.

S4's scope widened on 2026-09-19: the ADR-0002 API review verified from source that
`set_call_timeout` contends with `execute` for the same `Arc<Mutex<Client>>`, so the "cancel = short
timeout on demand" fallback this ADR originally assumed does not exist (revised C1). S4 must now
work through four candidates rather than confirm one, and its kill criterion is stated in terms of
`SPEC.md` §24.8 rather than a latency number alone. Budget raised from 1.5 d to 2 d accordingly.
Options (3) and (4) are the only ones that deliver a real cancel for an unprivileged account, so S4
should file the upstream request early — its lead time is measured in weeks, not hours.

### Test database

**Decided by the owner:** local Phase 0 integration tests use the community Docker image
[`doctorkirk/oracle-19c:19.3`](https://hub.docker.com/r/doctorkirk/oracle-19c) — Oracle 19c EE
Single Instance, built from Oracle's official `oracle/docker-images` procedure, ~2.8 GB compressed,
amd64 only. Setup lives under `tools/oracle-test-db/` (owned by another workstream).

Coverage limitations to remember when reading spike results:

- **19.3 base release, no Release Updates.** Real sites run 19.2x; some fixed bugs will be present.
- **Non-CDB.** No PDB, no service-per-PDB behaviour — which is what most real 19c sites run. Any
  connect-string or service-name conclusion from this image is not the full story.
- **TCPS not configured out of the box** — resolved 2026-09-19: `tools/oracle-test-db/startup/` adds a TCPS listener on `127.0.0.1:2484` (test CA, orapki-built wallet) and S8 has been run.
- **Community-built and unmaintained** (last updated 2021-03); amd64 only.

Optional later additions, not required for Phase 0: the official
`container-registry.oracle.com/database/enterprise:19.3.0.0` for CDB/PDB coverage (requires an
Oracle account, licence click-through and `docker login` the owner must perform personally — the
registry rejects anonymous tag listing, so the exact tag is **Unverified** here), and
`gvenzl/oracle-free:23` (~1.2 GB, updated 2026-08-30) as a fast newer-version smoke target. A
23ai-only run would miss 19c-specific behaviour entirely: JSON storage differs, `BOOLEAN` and
`VECTOR` do not exist in 19c, and 19c verifier/charset defaults differ — so 23ai can never be the
only target.
