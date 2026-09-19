# Execution Plan — Phase 0 Architecture Validation

**Status:** Active  
**Goal:** Prove the core database architecture before significant Reldex UI development.
**Spike evidence:** [`docs/exec-plans/active/phase-0-spike-results.md`](phase-0-spike-results.md) —
S1–S5, S7 and S9 ran against the live Phase 0 test database on 2026-09-19. All checkboxes below are
ticked only where that file (or `db-core`'s own tests) records a pass; where it records a failure or
a limitation, the item is annotated rather than ticked or hidden.

## Success criteria

Phase 0 is complete when:

1. the generic driver/session API exists;
2. the selected thin driver — Oracle's official `oracledb` crate (`oracle/rust-oracledb`), per
   ADR-0001 — can connect to the reference database;
3. transaction behavior is correct;
4. query cancellation is demonstrated;
5. required datatypes/PLSQL behaviors are integration-tested;
6. desktop platform viability is established;
7. Android/iOS direct-connect feasibility has physical-device evidence or a clearly documented blocker;
8. no full desktop UI implementation was required to prove these results.

### Status per criterion (2026-09-19 spike evidence)

| # | Status |
| --- | --- |
| 1 | **Done.** `db-driver-api` (ADR-0002) and `db-core`'s session/worker layer are implemented and independently reviewed (7 must-fix applied, commit `86f79d7`). |
| 2 | **Done.** Spike S1 — pass: Easy Connect and a full TNS descriptor both authenticate; 119.5 ms median connect (10 sequential cycles). |
| 3 | **Done.** Spike S3 — pass. |
| 4 | **Not met — accepted limitation (owner decision 2026-09-20).** Spike S4 fails for the requirement: only a pre-armed per-round-trip deadline exists, and it destroys the session whenever the server cannot answer promptly. `SPEC.md` §10/§24.8 is not satisfied by `oracledb` 26.0.0-beta.3. The owner reviewed ADR-0001's re-opening and decided to stay on `oracledb`, ship the pre-armed deadline with an honest UI, and pursue upstream fixes rather than change drivers — see ADR-0001 "Owner decision (2026-09-20)". This criterion remains **not met** for the Phase 0 go/no-go decision below; the decision was to accept the gap, not to close it. |
| 5 | **Done, with documented limits.** Spikes S2 (conditional go — NUMBER-bind and TIMESTAMP WITH TIME ZONE restrictions contained by refusal, not silent corruption), S5 (pass) and S7 (pass) all ran against the live database. |
| 6 | **Partial.** Windows x64 fully validated locally (build, connect, full spike matrix). Linux x64 and macOS ARM64 build in CI (fmt/clippy/test) but this branch has not yet gone through a PR/CI run, and neither has database access in CI. |
| 7 | **Documented blocker, no evidence yet.** Spike S6 (cross-compile) has not run — it needs the Android NDK (owner approval to download) and a macOS host for iOS. No physical-device testing has been attempted for either platform. |
| 8 | **Done.** `reldex-core-poc` is a ping/query/exec CLI harness; no Qt/QML UI exists. |

## Driver decision

The primary database driver is Oracle's official [`oracle/rust-oracledb`](https://github.com/oracle/rust-oracledb)
(crate `oracledb`) — see [ADR-0001](../../decisions/0001-database-driver-strategy.md), including its ordered spike plan
(S1–S9) and kill criteria. Local integration tests run against the Docker database in `tools/oracle-test-db/`.

## Workstream A — Rust workspace

All crates this workstream names now exist and are implemented, not skeletons.

- [x] Initialize Cargo workspace.
- [x] Add `db-driver-api`. (implemented per ADR-0002: 5 traits — `DatabaseDriver`, `DatabaseConnection`, `CancelHandle`, `Cursor`, `LobStream` — ~45 types, zero production dependencies; independently reviewed, must-fix findings applied, owner review pending)
- [x] Add `db-core`. (session/worker-thread layer implemented against the `db-driver-api` contract: worker-thread-per-session, FIFO command queue, out-of-band cancel, conservative transaction tracking, `CloseDisposition`, `Lost`/`Closed` lifecycle, core-owned `ResultId`/`LobHandle`, bounded `Drop`, `SessionLimits`, panic containment; independently reviewed, 7 must-fix applied, commit `86f79d7`)
- [x] Add `drivers/oracle/thin` wrapping Oracle's `oracledb` crate, pinned to an exact version (ADR-0001). (implemented at `crates/drivers/oracle-thin` — path deviates from this plan's `drivers/oracle/thin`; see `docs/architecture/ARCHITECTURE.md` §11. Pinned to `=26.0.0-beta.3`; independently reviewed, 4 must-fix applied, commit `db38c14`; spikes S1–S5, S7, S9 run against it — see `phase-0-spike-results.md`)
- [ ] Add Reldex-owned driver contract tests so an `oracledb` upgrade that changes behaviour is detected (ADR-0001).
- [x] Add test-support/mock driver. (implemented at `crates/drivers/mock`, used by `db-core`'s own test suite)
- [x] Define normalized `DbError`. (implemented in `db-driver-api` per ADR-0002 D3: `DbError{kind, message, native, position, session_state, retryable, source}`)
- [x] Define connection/session/query/result identifiers. (implemented in `db-driver-api` per ADR-0002 D7: `ConnectionId`/`ResultSetId` as `u64` newtypes; `SessionId` belongs to `db-core` and `StatementId` was cut after the API review)

## Workstream B — Session correctness

Proven by spike S3 against the live database, plus `db-core`'s own session-layer tests.

- [x] Connect/disconnect/ping. (spike S1)
- [x] Keep session identity stable across worksheet-like commands. (spike S3: `session_state_does_not_leak_between_connections`)
- [x] COMMIT. (spike S3)
- [x] ROLLBACK. (spike S3)
- [x] SAVEPOINT. (spike S3: SAVEPOINT + ROLLBACK TO)
- [x] Demonstrate uncommitted state survives multiple statements on the same session. (spike S3)
- [x] Demonstrate a second session cannot accidentally inherit session state. (spike S3: NLS setting, temp-table rows and package state all confirmed not to leak)

## Workstream C — Query execution

- [x] SELECT. (spikes S2/S3/S5)
- [x] DML. (spike S3)
- [x] DDL. (spike S3: `CREATE INDEX` — implicit commit reported via `StatementKind::Ddl`/`committed_implicitly()`)
- [x] PL/SQL anonymous block. (spike S5)
- [x] IN/OUT/IN OUT binds. (spike S5)
- [x] REF CURSOR. (spike S5, after contract fix C-1)
- [x] Multiple concurrent sessions. (spike S9: 8 sessions, 400 inserts, 282.8 ms)
- [x] Long-running query. (spike S4 drives a 3-way cartesian join over `ALL_OBJECTS` and a 20-second `DBMS_SESSION.SLEEP`; both execute — see below for the cancellation outcome)
- [ ] Cancellation from another control path. **Ran 2026-09-19; fails for the requirement (ADR-0001 C1 / spike S4). Accepted limitation (owner decision 2026-09-20).** No mechanism stops a running statement and keeps the session in the general case: a pre-armed deadline (the only mechanism `oracledb` 26.0.0-beta.3 offers) stops a long SQL statement with the session intact, but destroys the connection for a PL/SQL block the server will not interrupt promptly (upstream recovery defect U-6); a privileged `ALTER SYSTEM CANCEL SQL` stops the statement server-side but the client is never notified (U-7); a minimal fork was assessed and is not recommended. `SPEC.md` §10/§24.8 is **not met** — the owner decided to stay on `oracledb`, ship the pre-armed deadline with an honest UI, and pursue the upstream fixes rather than change drivers; see ADR-0001 "Spike outcome (2026-09-20)" and "Owner decision (2026-09-20)", and `phase-0-spike-results.md` §4.

## Workstream D — Data types

Per spike S2's fidelity table (`phase-0-spike-results.md` §3); marked to match what was actually
found, not what was hoped for.

- [x] NUMBER. Reads exact to 40 digits, verified against the server's own `TO_CHAR(v,'TM')` to 126
  digits. Binding is restricted: an odd count of leading zeros below 0.1 would silently store ten
  times too large upstream (U-1), and a 40-digit value at an odd/positive decimal-point index or
  magnitude ≥1E40 would abort the process (U-2) — both refused (`ErrorKind::Unsupported`) rather than
  bound or corrupted.
- [x] CHAR/VARCHAR2/NVARCHAR2. Byte-exact for Thai, non-BMP and mixed text, with no `NLS_LANG` set on
  the client; `CHAR(20 CHAR)` blank padding preserved; `NCHAR(20)` pads to UTF-16 code units per the
  server's own rule.
- [x] DATE. Pass, including time-of-day, BC dates and pre-1582 Julian-calendar dates.
- [x] TIMESTAMP. Pass, `TIMESTAMP(9)` fractional-second precision confirmed.
- [x] TIMESTAMP WITH TIME ZONE. **Tested and honestly limited, not silently unsupported.** A
  named-region value aborts the process upstream (U-3); the driver refuses the column at describe
  time, before any value is decoded, so the crash is contained but the type is unreadable by default.
  The offset-only form decodes correctly and is proven to, but is reachable only behind the
  `oracle.allow_timestamp_with_time_zone` opt-in extension that Reldex must not enable by default,
  because the two forms are indistinguishable before decoding. Not fixed upstream as of
  `oracledb` 26.0.0-beta.3.
- [x] RAW. Pass, byte-exact.
- [x] CLOB. Pass (spike S7): 100 MB streamed via `LobStream::read_chunk` with +4.7 MB working set
  across 200 MB.
- [ ] NCLOB. **Not directly tested.** CLOB/BLOB streaming (S7) and NVARCHAR2/Thai character fidelity
  (S2) were each tested separately; no NCLOB-specific spike ran.
- [x] BLOB. Pass (spike S7).
- [x] JSON where applicable. **Honest split.** 19c has no native `JSON` column type — JSON there is
  `VARCHAR2`/`CLOB`/`BLOB` with `IS JSON`, all of which are covered by the character/LOB rows above.
  A genuine `JSON` column type (21c+) is refused at describe time, by the same route as `XMLType`,
  `VECTOR`, object types and `BFILE`: upstream's row decoder has no branch for any of them, so there
  is nothing to fetch, not just nothing to render.

For every mapping, document:
- Rust representation;
- NULL handling;
- precision/loss risk;
- streaming/lazy behavior for large values.

## Workstream E — Database-specific development features

- [x] DBMS_OUTPUT. (spike S5 — pass, including a Thai-text line through an OUT bind)
- [x] metadata dictionary query. **Partial evidence, not a dedicated spike.** `USER_ERRORS` was
  queried successfully as part of S5's PL/SQL compile-error test (`PLS-00201` at the correct line and
  column). No broader dictionary-query spike ran.
- [x] V$ query where permissions allow. **Partial evidence, not a dedicated spike.** `V$SESSION` was
  queried successfully as part of S4's privileged-cancel candidate (locating a session by
  `DBMS_APPLICATION_INFO.SET_CLIENT_INFO` tag), which incidentally confirms V$ queries work through
  the driver; this was not tested as a Workstream E capability in its own right.
- [ ] EXPLAIN PLAN. Not run; no evidence in the results file.
- [ ] DBMS_XPLAN. Not run; no evidence in the results file.

## Workstream F — Network/security

- [x] Easy Connect. (spike S1)
- [x] service name. (spike S1: `127.0.0.1:1521/RELDEX`)
- [x] connect descriptor. (spike S1: full TNS descriptor with `CONNECT_DATA=(SID=RELDEX)`)
- [x] TCP. (every Phase 0 connection is plaintext TCP; the test DB has no TLS listener)
- [ ] TCPS. **Not run** (spike S8) — the Phase 0 test database has no TCPS listener configured; the
  driver reports `tls = false` and refuses `TlsMode::Required` rather than silently connecting in
  plaintext.
- [x] timeout behavior. Per spike S4's findings: a pre-armed deadline stops a long SQL statement
  about 0.5 s past the deadline with the session intact, but a fired deadline on a PL/SQL block the
  server will not interrupt promptly costs the connection entirely (upstream recovery defect U-6,
  load-dependent by construction — see `phase-0-spike-results.md` U-6). This is timeout behavior, not
  cancellation; see Workstream C for why it does not satisfy `SPEC.md` §24.8.
- [ ] network-loss behavior. **Not evidenced.** Only recovery-path failure modes seen incidentally
  during S4 (U-6, U-7) — no dedicated network-loss spike ran.
- [ ] reconnect semantics. **Not evidenced.** No reconnect spike ran.

Known unsupported configurations of the primary driver (ADR-0001): Native Network Encryption and
11G password verifiers. Record them as documented limitations, not as driver failures.

## Workstream G — Platforms

### Windows x64
- [x] Build. (fmt + `clippy -D warnings` clean; `cargo test --workspace` = 267 passed, 0 failed, 1 ignored, DB-free)
- [x] Connect. (spike S1, live against Oracle 19.3 in Docker)
- [x] Full POC matrix. (spikes S1–S5, S7, S9 all run locally, 2026-09-19)

### Linux x64
- [x] Build. Green on CI `ubuntu-latest` for PR #1 on 2026-09-20 (fmt, `clippy -D warnings`, `cargo test --workspace`, incl. `aws-lc-sys`): https://github.com/reldex/reldex/actions/runs/35419377082
- [ ] Connect. Not attempted; CI has no database access by design (unit tests only, no real DB).
- [ ] Core smoke matrix. Not attempted.

### macOS ARM64
- [x] Build. Green on CI `macos-latest` (Apple Silicon) for PR #1 on 2026-09-20 (same steps): https://github.com/reldex/reldex/actions/runs/35419377082
- [ ] Connect. Not attempted; no database access in CI.
- [ ] Core smoke matrix. Not attempted.

### Android ARM64 physical device
- [ ] Cross-compile. **Not started** — spike S6 has not run; it needs the Android NDK, which needs
  owner approval to download.
- [ ] Package minimal native harness.
- [ ] Connect over TCP.
- [ ] Connect over TCPS.
- [ ] SQL/PLSQL.
- [ ] transaction.
- [ ] cancellation.
- [ ] LOB.
- [ ] background/resume.
- [ ] reconnect/lost-session behavior.

### iOS/iPadOS ARM64 physical device
- [ ] Cross-compile. **Not started** — needs a macOS host, not available to this workstream.
- [ ] Package minimal native harness.
- [ ] Connect over TCP.
- [ ] Connect over TCPS.
- [ ] SQL/PLSQL.
- [ ] transaction.
- [ ] cancellation.
- [ ] LOB.
- [ ] background/resume.
- [ ] reconnect/lost-session behavior.

## Measurements

Recorded 2026-09-19 against the live Phase 0 test database (Oracle 19.3 EE, `doctorkirk/oracle-19c:19.3`,
Windows 11 Pro, `rustc 1.98.1` MSVC target). Full method notes are in
`docs/exec-plans/active/phase-0-spike-results.md`; this is a pointer summary, not a restatement.

- **Connection latency** — 119.5 ms median connect (118.6–123.7 ms range, 10 sequential
  connect/ping/close cycles, `Instant::now()` around each, median of sorted samples); `ping` 769 µs
  median. Method: `connection_latency_is_measured_over_ten_attempts` (spike S1).
- **Fetch throughput** — not comprehensively benchmarked; LOB streaming rates only (see below).
- **Memory during large fetch** — LOB streaming: process working set grew **4.7 MB** while streaming
  200 MB (100 MB CLOB + 100 MB BLOB) through a 64 KiB caller buffer. Method: working set sampled via
  `tasklist /FI "PID eq <self>" /FO CSV /NH` before the first read and every 200 chunks, peak taken as
  the maximum sample (spike S7).
- **Cancellation latency** — a pre-armed 2.0 s deadline stopped a long SQL statement after 2.5 s with
  the session intact, but the same deadline on a PL/SQL sleep took 4.0 s and destroyed the connection
  (spike S4, §4 candidate 1). A privileged `ALTER SYSTEM CANCEL SQL` stopped the statement server-side
  in 2.7 ms to issue / ~507 ms to take effect, but the blocked client did not notice until its own
  20 s safety deadline expired at 23.0 s (spike S4, §4 candidate 2). No mechanism achieves an
  observable on-demand cancel in the general case — see Workstream C.
- **Concurrency** — 8 concurrent sessions, 400 inserts (50 per thread) plus commit and read-back,
  282.8 ms wall clock (spike S9).
- **Binary/package constraints per platform** — measured with `cargo tree -p reldex-driver-oracle-thin
  --target x86_64-pc-windows-msvc` and `cargo metadata`: **55** third-party crates at run time, **63**
  including build-only crates (`aws-lc-sys`'s C/assembly build pulls in `cc`, `cmake`, `jobserver`,
  `shlex`, `find-msvc-tools`, `dunce`, `fs_extra`, plus `autocfg`). Every licence is permissive (no
  copyleft); a generated third-party notices file is required before any binary distribution (see
  TASKS.md).

Do not make comparative performance claims without recording the method and environment.

## Phase 0 exit assessment (draft)

This restates the eight success criteria above as a single input for the owner's Phase 1 go/no-go
decision. It is a draft assessment, not the decision itself — see the note at the end.

| # | Criterion | Assessment | Evidence |
| --- | --- | --- | --- |
| 1 | Generic driver/session API exists | **Met** | `db-driver-api`/`db-core`, independently reviewed (commit `86f79d7`) |
| 2 | Selected thin driver connects to the reference database | **Met** | Spike S1 — pass; `phase-0-spike-results.md` §3 |
| 3 | Transaction behavior is correct | **Met** | Spike S3 — pass; `phase-0-spike-results.md` §3 |
| 4 | Query cancellation is demonstrated | **Not met — accepted limitation (owner decision 2026-09-20)** | Spike S4; ADR-0001 "Spike outcome (2026-09-20)" and "Owner decision (2026-09-20)"; `phase-0-spike-results.md` §4 |
| 5 | Required datatypes/PL-SQL behaviors are integration-tested | **Partially met** | Spikes S2 (conditional go), S5 (pass), S7 (pass); NCLOB not directly tested — `phase-0-spike-results.md` §3 |
| 6 | Desktop platform viability is established | **Partially met** | Windows x64 fully validated locally; Linux x64 and macOS ARM64 build/fmt/clippy/test green on CI (PR #1) but no database connect exercised in CI — see `README.md` "Current status" |
| 7 | Android/iOS direct-connect feasibility: physical-device evidence or a documented blocker | **Not met, documented blocker** | Spike S6 not run — needs Android NDK (owner approval to download) and a macOS host for iOS; no physical-device evidence for either platform |
| 8 | No full desktop UI required to prove these results | **Met** | `reldex-core-poc` is a CLI harness; no Qt/QML UI exists |

**What remains before a Phase 1 go decision:**

- TCPS (spike S8) — pending a TCPS listener on the test database (tracked in `TASKS.md`; a concurrent
  workstream owns this).
- Android/iOS cross-compile (spike S6) — pending Android NDK approval and a macOS host; criterion 7
  needs either physical-device evidence or to remain a clearly documented blocker, not silence.
- Network-loss/reconnect behavior — not evidenced at all in Phase 0 (Workstream F).
- NCLOB — not directly spiked (CLOB/BLOB streaming and NVARCHAR2/Thai character fidelity were each
  tested separately; no NCLOB-specific spike ran).
- EXPLAIN PLAN / DBMS_XPLAN — not run (Workstream E).
- Metadata/dictionary access as a capability in its own right — only incidental evidence so far
  (`USER_ERRORS`, `V$SESSION`, each exercised only for another test's own purpose).

This is a draft assessment for the owner's use, not a go/no-go decision — per "Deliverables" below,
that decision is the owner's to make.

## Deliverables

- working `reldex-core-poc`;
- integration tests;
- platform validation notes;
- ADRs for driver/FFI decisions;
- updated `TASKS.md`;
- go/no-go decision for Phase 1.

## Explicit non-goals

Do not spend Phase 0 time on:
- polished editor UI;
- visual design system;
- object browser UI;
- Pro features;
- AI;
- plugin API;
- full export/import;
- mobile UX.
