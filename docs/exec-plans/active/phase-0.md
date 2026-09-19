# Execution Plan — Phase 0 Architecture Validation

**Status:** Active  
**Goal:** Prove the core database architecture before significant Reldex UI development.
**Spike evidence:** [`docs/exec-plans/active/phase-0-spike-results.md`](phase-0-spike-results.md) —
S1–S5, S7 and S9 ran against the live Phase 0 test database on 2026-09-19; the evidence-gap spikes
S10–S14 (network loss/reconnect, NCLOB, developer features, privileged connections, large result) ran
against the same database on 2026-09-19. **Mobile cross-compile spike S6** ran in CI on 2026-09-19
(PR #3) and is written up separately in
[`docs/exec-plans/active/phase-0-s6-mobile-cross-compile.md`](phase-0-s6-mobile-cross-compile.md). All
checkboxes below are ticked only where one of those files (or `db-core`'s own tests) records a pass;
where it records a failure or a limitation, the item is annotated rather than ticked or hidden.

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
| 5 | **Done, with documented limits.** Spikes S2 (conditional go — NUMBER-bind and TIMESTAMP WITH TIME ZONE restrictions contained by refusal, not silent corruption), S5 (pass), S7 (pass) and S11 (NCLOB — pass) all ran against the live database. |
| 6 | **Partial.** Windows x64 fully validated locally (build, connect, full spike matrix). Linux x64 and macOS ARM64 build in CI (fmt/clippy/test) but this branch has not yet gone through a PR/CI run, and neither has database access in CI. |
| 7 | **Android has physical-device evidence; iOS does not.** Spike S6 passed 2026-09-19 (PR #3): all three mobile targets compile and link in CI (`phase-0-s6-mobile-cross-compile.md`). On 2026-09-20 the core plus driver then **ran on a physical arm64 phone** (OPPO CPH2399, Android 16) against the live Oracle 19c test database over USB `adb reverse` — connect, ping, typed data, transaction, 100k-row fetch, TCPS with verification on, and the deadline path, 7/7 (`phase-0-android-device.md`). That is a native CLI binary over `adb shell`, **not** an APK, and not a real network. No iOS device has run anything. |
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
- [x] NCLOB. **Pass (spike S11).** Thai and non-BMP text byte-exact through the lazy stream at six
  buffer sizes, including ones that land inside a surrogate pair; `NULL` and `EMPTY_CLOB()` stay
  distinguishable; 1.2M characters streamed in 55 bounded chunks. Reported as `LobKind::NationalCharacter`,
  never collapsed into `Character`.
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
- [x] metadata dictionary query. **Pass (spike S12).** Ten `ALL_*` dictionary views queried against one
  fixture per `SPEC.md` §16 object group; `LONG`/`LONG RAW` columns (`ALL_VIEWS.TEXT`,
  `ALL_TRIGGERS.TRIGGER_BODY`, `ALL_TAB_COLUMNS.DATA_DEFAULT`) read exact, including a 73,926-character
  value; `DBMS_METADATA.GET_DDL` streamed as a CLOB locator. 63,414-row `all_objects` scan at ≈72,800
  rows/s. Supersedes the earlier `USER_ERRORS`-only incidental evidence from S5.
- [x] V$ query where permissions allow. **Pass (spike S12).** `v$version`, `v$session`, `v$parameter`
  and `v$instance` all queried on the existing `SELECT ANY DICTIONARY`/`SELECT_CATALOG_ROLE` grants.
  Supersedes the earlier S4-incidental `V$SESSION` evidence.
- [x] EXPLAIN PLAN. **Pass (spike S12).** `EXPLAIN PLAN … FOR` writes to `PLAN_TABLE` via the public
  synonym with no extra grant.
- [x] DBMS_XPLAN. **Pass (spike S12).** Both `DBMS_XPLAN.DISPLAY` (26 lines, correct plan) and
  `DBMS_XPLAN.DISPLAY_CURSOR` (35 lines) return real plans.

## Workstream F — Network/security

- [x] Easy Connect. (spike S1)
- [x] service name. (spike S1: `127.0.0.1:1521/RELDEX`)
- [x] connect descriptor. (spike S1: full TNS descriptor with `CONNECT_DATA=(SID=RELDEX)`)
- [x] TCP. (every Phase 0 connection is plaintext TCP; the test DB has no TLS listener)
- [x] TCPS. **Pass with limits** (spike S8, 2026-09-20) — one-way TLS 1.2 (`ECDHE-RSA-AES256-GCM-SHA384`)
  with certificate and host-name verification on, confirmed server-side; trust comes from a
  user-supplied PEM. Not available upstream: mTLS with a private CA, Oracle wallet files, OS trust
  store, revocation, `SSL_SERVER_DN_MATCH` (U-12…U-14). `TlsMode::Required` refuses non-TCPS endpoints
  rather than silently connecting in plaintext, and (2026-09-19) a descriptor that sets
  `SSL_SERVER_CERT_DN` is refused rather than connected with an unenforced pin, with
  `oracle.allow_unenforced_server_cert_dn` as the opt-out; `SSL_SERVER_DN_MATCH` produces a warning.
  Design proposed by the lead, **owner confirmation pending** (results file U-14 and §9 item 7).
- [x] timeout behavior. Per spike S4's findings: a pre-armed deadline stops a long SQL statement
  about 0.5 s past the deadline with the session intact, but a fired deadline on a PL/SQL block the
  server will not interrupt promptly costs the connection entirely (upstream recovery defect U-6,
  load-dependent by construction — see `phase-0-spike-results.md` U-6). This is timeout behavior, not
  cancellation; see Workstream C for why it does not satisfy `SPEC.md` §24.8. **Extended by spike S10:**
  a connect cannot be bounded at all — `ConnectionParams::connect_timeout()` is accepted and ignored
  by the driver (contract gap C-5) — and a black-holed link with no deadline armed never returns
  (U-15, U-17).
- [x] network-loss behavior. **Pass, with two upstream gaps (spike S10).** A dead socket (hard drop)
  is detected in 54 µs–568 µs and reported `NetworkLost`/`Lost`; loss mid-statement, mid-fetch and
  mid-LOB-stream all report and retire the handle cleanly; an in-doubt commit is surfaced as unknown,
  never guessed as success. But a **black-holed** link (sockets stay open) never returns without a
  caller-set deadline (U-17), the only deadline that ends it destroys the session (U-6), and a dead
  client's row lock blocked a second session for the full 20 s measured because
  `SQLNET.EXPIRE_TIME` is unset on the test database and upstream has no keepalive of its own.
- [x] reconnect semantics. **Pass (spike S10).** Nothing reconnects by itself — after a loss every
  call fails and the test proxy saw no second TCP connection. A fresh `connect()` opens a new server
  session (SID/serial# differ) carrying none of the old session's state (an `ALTER SESSION` setting
  was gone). `SPEC.md` §18's "never silently replace a lost transactional session" is satisfied.
- [x] privileged connections. **Pass (spike S13).** `AS SYSDBA` over the listener connects in 119 ms —
  the same as an ordinary connect — through the existing `SessionRole` contract (`AUTH_MODE_SYSDBA`);
  `AS SYSOPER` also connects and is reported as the non-DBA account it is. No contract gap, no
  upstream gap. Privilege does not leak to a concurrently open ordinary session.

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
- [x] Cross-compile. **Pass (spike S6, 2026-09-19, PR #3).** `aarch64-linux-android` compiles and
  links (`cargo-ndk`, API 26) on an ordinary GitHub-hosted runner with no local NDK; `.so` 2,676,128 B
  unstripped / 2,285,896 B stripped. Cross-compile evidence only, not device evidence — see
  [`phase-0-s6-mobile-cross-compile.md`](phase-0-s6-mobile-cross-compile.md).
  Additionally proven 2026-09-20: the same cross-compile needs **no `cargo-ndk`, `cmake`, NASM or
  `bindgen`** on a **Windows** host either — three cargo environment variables pointing at the NDK's
  clang/`llvm-ar` are enough.
- [x] Package minimal native harness. **Pass (2026-09-20).** `reldex-device-check`
  (`crates/reldex-core-poc/src/bin/`), cross-compiled for `aarch64-linux-android` (API 26),
  `adb push`ed and run over `adb shell`. **Native CLI binary, not an APK** — see
  [`phase-0-android-device.md`](phase-0-android-device.md).
- [x] Connect over TCP. **Pass (2026-09-20).** OPPO CPH2399, Android 16, arm64-v8a; connect 282 ms,
  ping 3–8 ms, over USB `adb reverse` to the loopback-bound test database.
- [x] Connect over TCPS. **Pass (2026-09-20).** Certificate **and** host-name verification on;
  confirmed by the server's own `sys_context('USERENV','NETWORK_PROTOCOL')` = `tcps`.
- [x] SQL. **Pass (2026-09-20).** Typed round trips (39-digit NUMBER, DATE, TIMESTAMP(6), Thai and
  non-BMP text through VARCHAR2 and NVARCHAR2, each cross-checked against the server's own
  `TO_CHAR`/`LENGTHB`/`LENGTH`) and a 100 000-row batched fetch at ~15 k rows/s with peak RSS under
  6.2 MB. **PL/SQL was not run on-device.**
- [x] transaction. **Pass (2026-09-20).** Auto-commit off; insert → rollback → gone;
  insert → commit → present.
- [ ] cancellation. **Partial.** The pre-armed **deadline** path passed on-device (`Timeout`,
  `NeedsValidation`, session really recovers). On-demand cancel is an accepted Phase 0 limitation
  (criterion 4) and was not exercised on the device.
- [ ] LOB. Not attempted on-device.
- [ ] background/resume. Not attempted; the process ran in the foreground for seconds, with "stay
  awake" on. Doze/App Standby untested.
- [ ] reconnect/lost-session behavior. Not attempted on-device.
- [ ] **Packaged-app path** (new): Rust core as `cdylib`/`staticlib` behind JNI/Qt, an APK with
  `INTERNET` permission and a network-security config, and a real Wi-Fi/cellular route. None of this
  is covered by the native-binary run.

### iOS/iPadOS ARM64 physical device
- [x] Cross-compile. **Pass (spike S6, 2026-09-19, PR #3).** `aarch64-apple-ios` and
  `aarch64-apple-ios-sim` both compile and link on `macos-latest` CI (Xcode 26.6); device `.dylib`
  2,338,312 B, `.a` 10,692,824 B unstripped / 6,990,520 B stripped. Cross-compile evidence only, not
  device evidence — see [`phase-0-s6-mobile-cross-compile.md`](phase-0-s6-mobile-cross-compile.md).
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
- **Fetch throughput** — **1,000,000 rows** (NUMBER/VARCHAR2(40)/DATE), streamed via `fetch_batch`
  with every batch dropped as it arrives: 52,300–59,400 rows/s at `fetch_rows=100`, 50,500–92,100
  rows/s at `fetch_rows=1,000`, 14,800–17,700 rows/s at `fetch_rows=10,000`. Throughput is **not**
  monotonic in the batch size — 10,000 was consistently ~3.5× slower than the best of the other two
  and cost up to 1.1 s to the first batch, against 5 ms at 100. Method: one session, wall clock from
  before `execute` to after the last `fetch_batch`, two runs on one machine (spike S14). Do not assume
  a larger default batch is faster when Reldex picks one (results file §9 item 12).
- **Memory during large fetch** — LOB streaming: process working set grew **4.7 MB** while streaming
  200 MB (100 MB CLOB + 100 MB BLOB) through a 64 KiB caller buffer (spike S7). Row streaming: the same
  1,000,000-row scan above grew the working set by only **~1 MB** (+480–504 KB at `fetch_rows=100`,
  +996–1,032 KB at 1,000, +1,204–6,772 KB at 10,000) — a driver that materialized the result would need
  at least 56 MB, so `SPEC.md` §12's bounded-memory claim holds (spike S14). Method: working set
  sampled via `tasklist /FI "PID eq <self>" /FO CSV /NH` before the first read/execute and
  periodically thereafter, growth = peak minus the pre-read sample.
- **Cancellation/deadline latency** — a pre-armed 2.0 s deadline stopped a long SQL statement after
  2.5 s with the session intact, but the same deadline on a PL/SQL sleep took 4.0 s and destroyed the
  connection (spike S4, §4 candidate 1). A privileged `ALTER SYSTEM CANCEL SQL` stopped the statement
  server-side in 2.7 ms to issue / ~507 ms to take effect, but the blocked client did not notice until
  its own 20 s safety deadline expired at 23.0 s (spike S4, §4 candidate 2). No mechanism achieves an
  observable on-demand cancel in the general case — see Workstream C. **Connect-time deadlines
  (spike S10):** a connect to a discarded address (`192.0.2.1`) returned after 22.0 s — the operating
  system's own SYN budget, nothing the driver chose — and a connect into a black hole was still
  outstanding after 30 s even with `ConnectionParams::with_connect_timeout(2 s)` set (U-15/C-5); a
  black-holed `ping` with no deadline had not returned after 30 s, while the same call with a 3 s
  deadline returned after 6.0 s and destroyed the session (U-6/U-17).
- **Concurrency** — 8 concurrent sessions, 400 inserts (50 per thread) plus commit and read-back,
  282.8 ms wall clock (spike S9).
- **Binary/package constraints per platform** — Desktop (Windows x64): measured with
  `cargo tree -p reldex-driver-oracle-thin --target x86_64-pc-windows-msvc` and `cargo metadata`:
  **55** third-party crates at run time, **63** including build-only crates (`aws-lc-sys`'s C/assembly
  build pulls in `cc`, `cmake`, `jobserver`, `shlex`, `find-msvc-tools`, `dunce`, `fs_extra`, plus
  `autocfg`). Every licence is permissive (no copyleft); a generated third-party notices file is
  required before any binary distribution (see TASKS.md). Mobile (spike S6, release build, CI):
  Android `aarch64-linux-android` — `libmobile_link_check.so` 2,676,128 B unstripped / 2,285,896 B
  stripped (`llvm-strip --strip-all`), `.a` 15,390,220 B, `reldex-core-poc` 4,849,112 B unstripped /
  3,932,024 B stripped; iOS device `aarch64-apple-ios` — `.dylib` 2,338,312 B, `.a` 10,692,824 B
  unstripped / 6,990,520 B after `strip -S` (34.6% smaller), `reldex-core-poc` 4,258,352 B; iOS
  simulator `aarch64-apple-ios-sim` — `.dylib` 2,373,200 B, `.a` 10,716,016 B, `reldex-core-poc`
  4,308,224 B. Method: `phase-0-s6-mobile-cross-compile.md` — these are intermediate build products
  from the link-check probe crate, not shipped app sizes.

Do not make comparative performance claims without recording the method and environment.

## Phase 0 exit assessment (draft)

This restates the eight success criteria above as a single input for the owner's Phase 1 go/no-go
decision. It is a draft assessment, not the decision itself — see the note at the end. Rewritten
2026-09-20 against the evidence-gap spikes (S10–S14) and the mobile cross-compile spike (S6); no
criterion's honest status is softened to make the table look more finished than the evidence supports.

| # | Criterion | Assessment | Evidence |
| --- | --- | --- | --- |
| 1 | Generic driver/session API exists | **Met** | `db-driver-api`/`db-core`, independently reviewed (commit `86f79d7`) |
| 2 | Selected thin driver connects to the reference database | **Met** | Spike S1 — pass; `phase-0-spike-results.md` §3 |
| 3 | Transaction behavior is correct | **Met** | Spike S3 — pass; `phase-0-spike-results.md` §3 |
| 4 | Query cancellation is demonstrated | **Not met — accepted limitation (owner decision 2026-09-20)** | Spike S4; ADR-0001 "Spike outcome (2026-09-20)" and "Owner decision (2026-09-20)"; `phase-0-spike-results.md` §4 |
| 5 | Required datatypes/PL-SQL behaviors are integration-tested | **Met, with limits** | Spikes S2 (conditional go — NUMBER-bind and TIMESTAMP WITH TIME ZONE restrictions), S5 (pass), S7 (pass), S11 (NCLOB — pass), S12 (developer features — pass, but `CREATE TRIGGER … :NEW` is impossible, U-18) — `phase-0-spike-results.md` §3, §5 |
| 6 | Desktop platform viability is established | **Met, with limits** | Windows x64 fully validated locally; Linux x64 and macOS ARM64 build/fmt/clippy/test green on CI (PR #1) but no database connect exercised in CI — see `README.md` "Current status" |
| 7 | Android/iOS direct-connect feasibility: physical-device evidence or a documented blocker | **Met, with limits — stated honestly.** Android now has real device evidence; iOS still has none and its blocker is documented, not silent | Spike S6 — pass 2026-09-19, PR #3, all three mobile targets compile and link in CI (`phase-0-s6-mobile-cross-compile.md`). **Android device run — pass 2026-09-20**, 7/7 on an OPPO CPH2399 (Android 16, arm64-v8a) against the live database over USB `adb reverse`: connect+ping, NUMBER/DATE/TIMESTAMP/Thai/non-BMP fidelity, rollback+commit, 100 000 rows at ~15 k rows/s with peak RSS under 6.2 MB, TCPS confirmed by the server's `USERENV.NETWORK_PROTOCOL`, and a deadline that returns `Timeout` with a session that really does recover (`phase-0-android-device.md`). **What is still missing:** it is a native CLI binary over `adb shell`, not an APK (no Qt, no JNI, no permission model, `shell` SELinux context) and the transport is USB loopback, not Wi-Fi/cellular; on-device LOB, PL/SQL, cancellation-beyond-deadline, reconnect and background/resume were not run; and **no physical iOS device has been provided** (iOS additionally needs a Mac + Apple Developer account) |
| 8 | No full desktop UI required to prove these results | **Met** | `reldex-core-poc` is a CLI harness; no Qt/QML UI exists |

**What remains before a Phase 1 go decision:**

- **Android physical-device validation** — **done 2026-09-20 for the native-binary path**
  (`phase-0-android-device.md`): 7/7 on an OPPO CPH2399 (Android 16, arm64-v8a) over USB
  `adb reverse`, repeatable via `tools/android-device/run-on-device.sh`. What remains is the
  **packaged-app path**: the Rust core as a `cdylib`/`staticlib` loaded by the Qt/JNI app, an APK
  with `INTERNET` permission and a network-security config, a real Wi-Fi/cellular route to a
  reachable database, and background/resume under Doze — see that file's "What remains for the
  packaged-app path".
- **iOS physical-device validation** — needs a Mac with Xcode (CI already confirms the toolchain), an
  Apple Developer account (a free personal-team identity suffices for a 7-day local debug build), and
  a physical iPhone/iPad.
- TCPS (spike S8) — done 2026-09-20, pass with limits (see Workstream F); the remaining TCPS questions
  are owner decisions (results file §9 items 6–7).
- Driver fix for **C-5** (`ConnectionParams::connect_timeout` accepted and ignored) — **approved
  2026-09-19**: implement on a helper thread, default 15 s, user-configurable per connection
  profile including "no limit" (results file §9 item 8, ADR-0001 2026-09-19 addendum).
  **Implementation pending.**
- Owner approval to submit drafted upstream **issues F and G** (results file §6) — U-15…U-17
  (timeouts/dead-link detection) and U-18 (`CREATE TRIGGER`). **Still outstanding.**
- **Watching for the fixes.** Each upstream defect that can be observed from a test now has a
  canary asserting it is *still there*
  (`crates/drivers/oracle-thin/tests/canary_upstream_{offline,live}.rs`), so a fix arrives as a
  failing test that names the guard it makes removable; a version tripwire fails as soon as the pin
  moves. See `oracledb-upgrade-checklist.md` for the per-U-number map and the manual checks.
- **Owner decisions from results file §9 — updated 2026-09-19.** Items 8–12 are now decided (see
  `phase-0-spike-results.md` §9 and ADR-0001's 2026-09-19 addendum), each made user-configurable per
  the owner's requirement: item 8 (`connect_timeout`, C-5) — helper thread, default 15 s,
  **implementation pending**; item 9 (default per-statement time limit) — default 600 s,
  configurable at three levels (application default, connection profile, per worksheet/statement)
  including "no limit" with an explicit UI warning, `SPEC.md` §10 constraints unchanged; item 10
  (`SQLNET.EXPIRE_TIME`) — documentation recommendation only, Phase 0 test database stays unset;
  item 11 (`CREATE TRIGGER` U-18) — driver auto-rewrites via `EXECUTE IMMEDIATE` by default, always
  reported to the user, off switch at connection level, **implementation pending**; item 12
  (default fetch batch size) — deferred to a Phase 1 benchmark (S14 found throughput is not
  monotonic in batch size), must be a user setting. A new **item 13** (connect-time warning
  channel, contract gap C-6) is approved in principle: one additive
  `take_connect_warnings`-style method on `DatabaseConnection`, collected once by `db-core` after
  connect, recorded as an ADR-0002 amendment when implemented; its detailed write-up arrives with
  pull request #5 (the TCPS descriptor guard), and implementation is sequenced after that PR.
  **Still outstanding and undecided:** item 4 (relax the NUMBER-bind refusal U-1 — recommendation:
  no).
  **Items 6–7 confirmed by the owner 2026-09-19** by accepting pull request #5: TCPS is described
  exactly as `SPEC.md` §8 states it, and the driver guards U-14 — a descriptor carrying
  `SSL_SERVER_CERT_DN` is refused unless `oracle.allow_unenforced_server_cert_dn` is set,
  `SSL_SERVER_DN_MATCH` is accepted with a warning (results file §9 items 6–7, U-14, C-6).

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
