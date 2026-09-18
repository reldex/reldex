# 0001 — Database Driver Strategy

**Status:** Accepted (conditional on Phase 0 spikes S1–S5)
**Date:** 2026-09-19
**Decided by:** project owner, 2026-09-19 — the primary driver is Oracle's official
[`oracle/rust-oracledb`](https://github.com/oracle/rust-oracledb) (crate `oracledb`). If a kill
criterion in the spike plan fires, this ADR is re-opened rather than silently worked around.

All external facts below were checked on **2026-09-19**. Claims that could not be verified from a
primary source are labelled **Unverified**.

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

**C1 — Cancellation is not in the public API.** `SPEC.md` §10 requires every worksheet to expose
Cancel; §24.8 makes it a Definition-of-Done item; `ARCHITECTURE.md` §6 and `phase-0.md` Workstream C
require cancelling from a *different control path*. Searching the crate for any public
cancel/break/interrupt function returns nothing; the public `Connection` surface is
close/commit/rollback/execute/query/ping/statement/set_call_timeout and metadata accessors
([`src/connection/mod.rs`](https://github.com/oracle/rust-oracledb/blob/main/src/connection/mod.rs)).
The protocol machinery exists but is private and reachable only via read-timeout recovery:
`recover_from_error()` sends `MARKER_TYPE_INTERRUPT` then resets
([`src/client/mod.rs`](https://github.com/oracle/rust-oracledb/blob/main/src/client/mod.rs)); the
break marker constant is `_MARKER_TYPE_BREAK`, i.e. unused
([`src/constants.rs`](https://github.com/oracle/rust-oracledb/blob/main/src/constants.rs)). The
client is held as `Arc<Mutex<Client>>` and `execute()` locks it for the duration of the call, so a
second thread could not interleave a break even if one were exposed. **Options:** (a) use
`set_call_timeout` as a coarse stand-in and accept "cancel = short timeout" semantics for Phase 0;
(b) file an upstream enhancement request and gate Phase 1 on it; (c) carry a small fork/patch. This
ADR recommends (a)+(b) for Phase 0 and treats (c) as the contingency.

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
| **Cancel a running statement from another control path** | **Missing** — no public API | S (see C1) |
| Call timeouts | Supported | A; `Connection::set_call_timeout` |
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
— a Definition-of-Done item — has no upstream API today. Enterprise sites using NNE or 11G
verifiers cannot connect at all, and both are common in the 19c estate the product targets; that is
a *market* limitation, not only a technical one, and should reach the README before launch. The
blocking API constrains the concurrency ADR. UTF-8-only client charset must be checked against Thai
and NCHAR data (`SPEC.md` §14).

**Follow-up work.** ADR-0002 on driver API + concurrency model; Reldex contract tests against
`db-driver-api`; an upstream enhancement request for a cancel/break API; a watch on issues #6, #8
and #18; a documented `oracle`-crate fallback path for desktop.

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
- `phase-0.md` Workstream C — record that cancellation may be timeout-based in Phase 0, and make
  Workstream F note NNE and 11G verifiers as known unsupported configurations.

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
| S4 | **Cancel a long-running statement from another control path**; measure latency | 1.5 d | No mechanism achieves cancel within ~2 s and leaves the session usable — even via `set_call_timeout` |
| S5 | REF CURSOR + IN/OUT/IN OUT binds + DBMS_OUTPUT | 1 d | REF CURSOR or OUT binds unusable |
| S6 | Cross-compile checks: `cargo build --target aarch64-linux-android` and `aarch64-apple-ios` | 0.5 d | Either target fails to build and no provider swap fixes it |
| S7 | CLOB/BLOB streaming ≥100 MB with bounded RSS; confirm issue #18 impact | 1 d | Memory grows with LOB size, or non-ASCII CLOBs unreadable |
| S8 | TCPS against a TLS-enabled listener | 1 d | TCPS cannot be established without Instant Client |
| S9 | Network-loss detection and reconnect semantics; N concurrent sessions under load | 1 d | Lost sessions are hidden rather than surfaced |

S4 is the **first kill-criterion spike that matters** — S1–S3 are expected to pass, and S4 is where
the known gap is. Run S1–S4 before committing to Phase 1 scope. S6 is cheap and should be run early
opportunistically because it can be done without a database.

### Test database

**Decided by the owner:** local Phase 0 integration tests use the community Docker image
[`doctorkirk/oracle-19c:19.3`](https://hub.docker.com/r/doctorkirk/oracle-19c) — Oracle 19c EE
Single Instance, built from Oracle's official `oracle/docker-images` procedure, ~2.8 GB compressed,
amd64 only. Setup lives under `tools/oracle-test-db/` (owned by another workstream).

Coverage limitations to remember when reading spike results:

- **19.3 base release, no Release Updates.** Real sites run 19.2x; some fixed bugs will be present.
- **Non-CDB.** No PDB, no service-per-PDB behaviour — which is what most real 19c sites run. Any
  connect-string or service-name conclusion from this image is not the full story.
- **TCPS not configured out of the box** — S8 needs listener work, or a different image.
- **Community-built and unmaintained** (last updated 2021-03); amd64 only.

Optional later additions, not required for Phase 0: the official
`container-registry.oracle.com/database/enterprise:19.3.0.0` for CDB/PDB coverage (requires an
Oracle account, licence click-through and `docker login` the owner must perform personally — the
registry rejects anonymous tag listing, so the exact tag is **Unverified** here), and
`gvenzl/oracle-free:23` (~1.2 GB, updated 2026-08-30) as a fast newer-version smoke target. A
23ai-only run would miss 19c-specific behaviour entirely: JSON storage differs, `BOOLEAN` and
`VECTOR` do not exist in 19c, and 19c verifier/charset defaults differ — so 23ai can never be the
only target.
