# Reldex Tasks

This file is the human-readable current task board. Detailed active execution plans live under `docs/exec-plans/active/`.

## Status legend

- [ ] Not started
- [~] In progress
- [x] Done
- [!] Blocked

## P0 — Repository bootstrap

- [x] Create GitHub Organization and `reldex/reldex` repository
- [x] Prepare initial project documentation
- [x] Commit `README.md`, `SPEC.md`, `ROADMAP.md`, `AGENTS.md`
- [x] Add `.gitignore` and `.editorconfig`
- [x] Add repository-local skill under `.agents/skills/reldex-development/`
- [ ] Enable branch protection when repository becomes collaborative/public
- [x] Decide Community/Pro licensing before public release — decided 2026-09-24: GPL-3.0-or-later for Community, Pro separate (ADR-0005)
- [ ] Run trademark/domain clearance before commercial launch

## P0 — Architecture validation

See `docs/exec-plans/active/phase-0.md` and, for the spike evidence behind the statuses below,
`docs/exec-plans/active/phase-0-spike-results.md` (run 2026-09-19).

- [x] Local Oracle 19c Docker test database (tools/oracle-test-db)

### Driver/core
- [x] Create Rust workspace
- [x] Create `db-driver-api`
- [x] Create `db-core` (session layer implemented: worker-thread-per-session, FIFO queue, out-of-band cancel, conservative transaction tracking incl. locking queries, `CloseDisposition`, terminal `Lost`/`Closed` lifecycle, core-owned `ResultId`/`LobHandle`, bounded `Drop`, `SessionLimits`, panic containment; independently reviewed, 7 must-fix applied, commit `86f79d7`)
- [x] Create initial database driver implementation — wrap Oracle's official `oracledb` crate (`oracle/rust-oracledb`, pinned exact version `=26.0.0-beta.3`) in `crates/drivers/oracle-thin` (ADR-0001); independently reviewed, 4 must-fix applied, commit `db38c14`
- [x] Add driver contract tests that detect behaviour changes on `oracledb` upgrades — upstream canary suite (`crates/drivers/oracle-thin/tests/canary_upstream_{offline,live}.rs`): one canary per observable upstream defect, process-abort defects run in a child process, version tripwire on the pin; procedure in `docs/exec-plans/active/oracledb-upgrade-checklist.md`
- [x] **(S4 critical path)** Draft upstream request to `oracle/rust-oracledb` for a public statement-cancel/break API — plus accessors for `OracleNumber` digits and the connection's transaction-in-progress flag (seven issues drafted in full: `docs/exec-plans/active/phase-0-spike-results.md` §6 — A–E submitted 2026-09-19 (#21–#25); F and G not submitted, owner decision 2026-09-19 — drafts kept for tracking only; see the owner decision task below)
- [x] Add normalized `DbError`
- [~] Add connection/profile model (driver-level connection params done; user-facing profile model pending)
- [x] Add session abstraction (`db-core` `DatabaseSession` implemented and hardened; see above)
- [x] Add transaction abstraction (`db-core` conservative tracking, incl. locking queries, implemented and proven by spike S3)
- [x] Add cancellation abstraction (contract implemented; driver-level mechanism evaluated in spike S4 and **fails for the requirement** — only a pre-armed per-round-trip deadline exists, no on-demand cancel; owner decision 2026-09-19: accepted as a limitation, see ADR-0001 "Owner decision" section)

### Functional POC
- [x] Connect/disconnect/ping (spike S1 — pass)
- [x] SELECT (spikes S2/S3/S5 — pass)
- [x] DML (spike S3 — pass)
- [x] PL/SQL anonymous block (spike S5 — pass)
- [x] IN/OUT/IN OUT binds (spike S5 — pass)
- [x] REF CURSOR (spike S5 — pass, after contract fix C-1)
- [x] COMMIT/ROLLBACK/SAVEPOINT (spike S3 — pass)
- [x] CLOB/NCLOB/BLOB (spike S7 — pass, CLOB/BLOB; 100 MB each streamed, +4.7 MB working set. Spike S11 — pass, NCLOB; Thai/non-BMP text byte-exact, 1.2M characters in 55 bounded chunks)
- [x] DBMS_OUTPUT (spike S5 — pass, including Thai text)
- [!] Long-running query cancellation — accepted limitation (owner decision 2026-09-19): pre-armed deadline only; blocked on upstream cancel API (see ADR-0001 "Spike outcome" and "Owner decision" sections and `phase-0-spike-results.md` §4)
- [x] TCPS (spike S8 — pass with limits, 2026-09-19: one-way TLS 1.2, verification on, PEM-supplied trust; no mTLS + private CA, no Oracle wallet files, no revocation — upstream U-12…U-14)
- [x] Network-loss behavior (spike S10 — pass, with two upstream gaps: a black-holed link never returns without a caller-set deadline, U-17; a dead client's row lock blocked a second session for the full 20 s measured because `SQLNET.EXPIRE_TIME` is unset and upstream has no keepalive — see `phase-0-spike-results.md` §3 S10)
- [x] Concurrent independent sessions (spike S9 — pass; 8 sessions, 400 inserts, 283 ms)

### Platform validation
- [x] Windows x64 (build, connect, and the full spike matrix all run locally, 2026-09-19)
- [~] Linux x64 (build + fmt + clippy -D warnings + `cargo test --workspace` green on CI `ubuntu-latest`, PR #1, 2026-09-19 — https://github.com/reldex/reldex/actions/runs/35419377082; no database connect yet)
- [~] macOS ARM64 (build + fmt + clippy -D warnings + `cargo test --workspace` green on CI `macos-latest` (Apple Silicon), PR #1, 2026-09-19 — https://github.com/reldex/reldex/actions/runs/35419377082; no database connect yet)
- [~] Android ARM64 physical device — native-binary evidence on a physical phone, 2026-09-20 (OPPO CPH2399, Android 16, arm64-v8a): connect/ping, exact typed data incl. Thai + emoji, transaction, 100k-row fetch (+0.6 MB RSS), TCPS with verification on, deadline path — 7/7 pass (`docs/exec-plans/active/phase-0-android-device.md`). Still open: the packaged-app path (APK, app sandbox, real Wi-Fi/cellular network) — SPEC §25 stays unmet until then
- [ ] iOS/iPadOS ARM64 physical device (cross-compile + link proven in CI — spike S6, PR #3; physical-device evidence still needed, needs a Mac + Apple Developer account + device)

- [x] Owner: decide cancellation path (ADR-0001 re-opened) [decision 2026-09-19: stay on `oracledb`; ship the pre-armed per-statement deadline with an honest UI; pursue upstream fixes via the four drafted issues — see ADR-0001 "Owner decision (2026-09-19)"]
- [x] Owner: submit the drafted upstream issues — five submitted 2026-09-19 (#21–#25) (`docs/exec-plans/active/phase-0-spike-results.md` §6). Maintainer replies reconciled 2026-09-23: #21 fixed on `main`; #22's poisoned-lock panic fixed on `main`, two follow-up issues (NUMBER OOB, region TSZ) drafted per the maintainer's invitation, not yet posted; #23 explained as expected behaviour, CPU-bound re-test pending; #24 acknowledged as a known limitation; #25 items 1 and 3 accepted, item 2 clarification drafted, not yet posted
- [x] Owner decision 2026-09-19: issues F and G are NOT submitted for now; drafts kept for tracking (results file §6)
- [x] Owner: Phase 0 go/no-go — GO for Phase 1 (2026-09-19)
- [ ] Track upstream `oracle/rust-oracledb` releases; re-run the integration suite and the ignored abort-repro tests on each new beta
- [ ] Third-party notices file before any binary distribution (dependency licences — 63 third-party crates in the oracle-thin graph, all permissive but attribution is required; see `phase-0-spike-results.md` §1)
- [x] Test DB: TCPS listener (S8) — `127.0.0.1:2484`, idempotent startup hook, wallet material untracked
- [x] Test DB: container memory cap — 1.5 GiB SGA / 512 MiB PGA, `mem_limit: 4g` (~2.0 GiB resident)
- [x] Owner: decide how TCPS support is described to users and whether to refuse descriptors carrying `SSL_SERVER_DN_MATCH` (results file §9 items 6–7)
- [x] Driver: TCPS descriptor guard (U-14) — refuse `SSL_SERVER_CERT_DN` unless explicitly allowed, warn on `SSL_SERVER_DN_MATCH` — implemented as the lead proposed and independently reviewed (3 must-fix applied); confirmed by the owner and merged 2026-09-19 (pull request #5).
- [x] S6 Android/iOS cross-compile check — pass, kill criterion did not fire (PR #3: https://github.com/reldex/reldex/pull/3); physical-device validation still needed
- [x] Driver: honour `connect_timeout` (C-5) — helper thread, default 15 s, capped at 1 h, "no limit" only through the extension `oracle.connect_timeout_unbounded`; a late success is closed and never adopted — done 2026-09-20 (PR `phase-0/driver-carryover`, independently reviewed)
- [x] Owner decisions §9 items 8–12 — decided 2026-09-19: default per-statement deadline 600 s (three-level setting), `EXPIRE_TIME` documentation-only recommendation, trigger DDL auto-rewrite with an off switch, default fetch batch size deferred to a Phase 1 benchmark, C-5 helper-thread approach — every one made user-configurable per the owner's requirement (`phase-0-spike-results.md` §9)
- [x] Driver: `CREATE TRIGGER` auto-rewrite for `:NEW`/`:OLD` (U-18) — sent as `BEGIN EXECUTE IMMEDIATE q'[…]'; END;`, on by default, off via `oracle.rewrite_trigger_ddl`, always reported with the text sent; trailing SQL*Plus `/` and a `CALL …;` terminator normalised for every trigger; limits: 32767-byte trigger text, syntax-error positions refer to the wrapper — done 2026-09-20 (PR `phase-0/driver-carryover`, independently reviewed)
- [x] Contract: connect-time warning channel (C-6) — `DatabaseConnection::take_connect_warnings` (additive, defaulted), collected once by `db-core`, read through `DatabaseSession::connect_warnings`; ADR-0002 amendment W1/W2 — done 2026-09-20 (PR `phase-0/driver-carryover`, independently reviewed)
- [ ] Docs: recommend `SQLNET.EXPIRE_TIME` (e.g. 10 minutes) in user-facing connection troubleshooting docs (owner decision 2026-09-19, `phase-0-spike-results.md` §9 item 10)
- [ ] Docs: note in `tools/oracle-test-db/README.md` that `SQLNET.EXPIRE_TIME` is left unset on the Phase 0 test database on purpose, so S10's measurements remain valid (follow-up; not edited in this change)
- [x] Android physical-device harness — `tools/android-device/run-on-device.sh` (bash-first; `.ps1` twin): cross-builds with the local NDK (no cargo-ndk/cmake needed on Windows), pushes, `adb reverse`, runs 7 checks, redacted transcript

## P1 — Desktop MVP

Milestone plan M1–M6 per `docs/exec-plans/active/phase-1.md` §C.2 (which also carries the full
owner/inputs/outputs/deps/acceptance table per task). ★ = independent review mandatory.

### M1 — De-risk: toolchain, ADR-0003, and a real virtualized table

- [x] M1.1 Owner approval + toolchain install (Qt, CMake, Ninja, cbindgen) (owner + sonnet) — approved and installed 2026-09-20: Qt 6.8.3 msvc2022_64, CMake 4.4.3, Ninja 1.13.2, cbindgen 0.29.4; no GPL-only module present (`docs/exec-plans/active/phase-1-toolchain.md`, `tools/dev-env/env.sh`)
- [x] M1.2 ★ Draft ADR-0003: Qt ↔ Rust integration (opus, review mandatory) — `docs/decisions/0003-qt-rust-integration.md` (Proposed)
- [x] M1.3 ★ `crates/ffi` skeleton: hub, session open/execute/fetch, batch views, errors, waker (opus, review mandatory) — done 2026-09-20: `reldex-ffi` (cdylib+staticlib), 31 exports, ABI 2, cbindgen header `crates/ffi/include/reldex.h` checked in CI; independently reviewed (3 must-fix + 9 should-fix fixed); interim per-session pump until M2.5; Miri/ASan not yet run (ADR-0003 A9); ABI 3 the same day from the first consumers' findings: NUMBER/TIMESTAMP mirrors only on request (they cost 62 B/row nobody read), `reldex_live_counts`, column descriptions at EXECUTED, result id on FETCHED — independently reviewed (2 must-fix fixed)
- [x] M1.4 C smoke harness (`ui/tests/ffi_smoke`), no Qt, mock driver, ASan on Linux (sonnet) — done 2026-09-20: `ui/tests/ffi_smoke` (C11 + C++17, 149 checks each, no Qt), ASan/UBSan clean on Linux CI; `.github/workflows/ui.yml` builds CMake+Corrosion+Qt 6.8.3 and runs the offscreen tests on windows/ubuntu/macos, every job under 3m05s vs K7's 25 min budget
- [x] M1.5 CMake + Corrosion + Qt project skeleton; QML module for the adapter (sonnet) — done 2026-09-20: `ui/` (CMake + Corrosion v0.6.1 pinned by commit, adapter QML module `Reldex.Adapter`, `Reldex` executable, offscreen QTest), `bash ui/build.sh --test`; verified on Windows only — Linux/macOS build unverified until the UI CI workflow exists (K7)
- [x] M1.6 ★ `ResultTableModel` over borrowed batch views; `Bridge` waker→`invokeMethod` drain (opus, review mandatory) — done 2026-09-20: `Bridge` (waker → coalesced queued drain, 256 events / 4 ms budget, re-posts itself), `SessionController` (bounded fetches in flight), `ResultTableModel` (borrowed batch views; text = pointer reads, other kinds bulk-formatted per 1,024-row window into an LRU cache ≤ 8 MiB), `Metrics` hooks for S15; independently reviewed (3 must-fix fixed); K5 flood teardown 10,000 iterations with live-handle counts back to zero; ASan not available on the Windows dev machine (x64 runtime missing)
- [x] M1.7 Mock driver: 1M-row generator of the S14 shape with controllable latency and a 10 s blocking statement (sonnet) — done 2026-09-20: `Action::GeneratedQuery` / `GeneratedQuerySpec` (lazy, O(batch) memory, `expected_cell` for random-access checks); 1M rows stream in ~0.55 s (release)
- [x] M1.8 ★ Spike S15 measurement run + report against kill criteria K1–K7 (opus, review mandatory) — done 2026-09-20: `docs/exec-plans/active/phase-1-s15-ffi-spike.md`; K1/K3/K4/K5/K6/K7 pass (K1/K5 with a named gap), **K2 fails as written** (cold first paint 903.55 ms vs a 150 ms threshold; warm path 15.85 ms passes by ~9×, cause outside the boundary); no criterion's failure is located in the boundary
- [~] M1.9 Accept or re-open ADR-0003; update `ARCHITECTURE.md` §13 items 2/3/10, `TASKS.md`, `Task.html` (sonnet) — S15 recorded; ADR-0003 stays Proposed, awaiting owner ruling on K1 p99 wording, K2 warm/cold, K3 baseline

### M2 — Core readiness: events, async open, settings, SQL text, driver leftovers

- [x] M2.1 ★ Driver: honour `connect_timeout` on a helper thread (C-5/U-15) (opus, review mandatory) — carried over from Phase 0 and done 2026-09-20; M2 consumes the result
- [x] M2.2 Driver: `CREATE TRIGGER` `:NEW`/`:OLD` auto-rewrite (U-18) (sonnet) — carried over from Phase 0 and done 2026-09-20; M2 consumes the result
- [x] M2.3 ★ Contract: `take_connect_warnings` (C-6) + ADR-0002 amendment (opus, review mandatory) — carried over from Phase 0 and done 2026-09-20; M2 consumes the result
- [x] M2.4 `crates/sql-text`: lexer + statement splitter driven by a `SqlDialect` descriptor the driver supplies (sonnet) — done 2026-09-21: `reldex-sql-text` (zero deps, no unsafe) — lexer + statement splitter driven by a driver-supplied `SqlDialect`; a lone `/` is authoritative, ambiguity fails safe, every span says how it ended (`ended_by`); adversarially reviewed three rounds (a panic and several mis-splits fixed); grammar-based differential test, 248,000 scripts; ADR-0002 amendment J
- [x] M2.5 ★ `EventQueue`/`EventSink`/`SessionEvent`/`Waker` + `ReplyTo` refactor of the worker (opus, review mandatory) — done 2026-09-21: `EventQueue`/`EventSink`/`SessionEvent`/`Waker` + `ReplyTo` in `db-core` (no thread per session); ordering rules 1–5 and exactly-once `Terminal` tested; back-pressure bound `2R + U + 3` made true (slot released when the consumer pops); 42 session tests run on both reply paths; independently reviewed twice (3 must-fix fixed); FFI pump switch is M2.11 (`phase-1-m2-5-event-queue.md`)
- [x] M2.6 ★ `SessionRegistry` + non-blocking `open`, `abandon` semantics (opus, review mandatory) — done 2026-09-21: `SessionRegistry` — `open` never blocks (one `Opened`/`OpenFailed`, one `Terminal`, last); `abandon` never blocks and is never refused, a late connect is closed by the worker (0 leaks in 960 randomized sessions); `Terminal.transaction_possibly_lost` computed on the worker covers abandon/retire/drop/lost; one-deadline teardown; independently reviewed twice (3 must-fix fixed); ADR-0002 amendment R
- [ ] M2.7 ★ Server output capability (DBMS_OUTPUT) in contract + driver + core polling when enabled (opus, review mandatory)
- [x] M2.8 Metadata catalog descriptor (`MetadataCatalog`) + Oracle dictionary SQL for the 9 object groups (sonnet) — done 2026-09-24: `MetadataCatalog` descriptor (`db-driver-api::metadata`) — one `prepare(MetadataRequest)` returning a `Statement` + declared column contract + error classifier; Oracle `ALL_*` SQL only in the driver; filter is a bind (case-insensitive contains, wildcards literal), `limit+1` truncation, invisible columns excluded, composed `type_name`; ORA-00942/01039 on catalog queries → `Permission`; independently reviewed twice (2 must-fix fixed); real-DB 9/9
- [ ] M2.9 ★ Settings model: three-level resolution with provenance; profile model; SQLite store (opus, review mandatory)
- [ ] M2.10 ★ Credential store: `CredentialStore` trait + Windows Credential Manager implementation (opus, review mandatory)
- [ ] M2.11 FFI surface for M2.5–M2.10 + regenerate and verify header (sonnet)

### M3 — Connect: shell, connection manager, first real session

- [ ] M3.1 App shell: window, docking-free fixed layout (sidebar / worksheet tabs / output panes), light+dark theme, high-DPI (sonnet)
- [ ] M3.2 Connection manager UI: list, create/edit/delete, environment, test-connect (sonnet)
- [ ] M3.3 ★ Connect flow over the async path, with a bounded timeout and a cancellable "Connecting…" state (opus, review mandatory)
- [ ] M3.4 Production indicator: persistent, not colour-only (icon + text + tab badge) (sonnet)
- [ ] M3.5 ★ TCPS UI described exactly as `SPEC.md` §8; surfaces the descriptor-guard warnings from C-6 (opus, review mandatory)
- [ ] M3.6 Settings UI: application defaults, per-profile overrides, provenance shown (sonnet)
- [ ] M3.7 ★ Logging/diagnostics: `tracing` + rotating file sink, redaction layer, Qt messages forwarded through the FFI (opus, review mandatory)

### M4 — Worksheet: editor, execution, transactions, honest limits

- [ ] M4.1 ★ Editor component: `TextArea` + `QSyntaxHighlighter` on `QQuickTextDocument`, tokens from `reldex-sql-text` over FFI (opus, review mandatory)
- [ ] M4.2 Editor essentials: line numbers, current-line, bracket matching, indentation, search/replace, font and theme settings (sonnet)
- [ ] M4.3 Statement detection and run modes: current statement, selection, whole script (sonnet)
- [ ] M4.4 Bind-variable dialog: detected placeholders, typed entry, IN/OUT/IN OUT (sonnet)
- [ ] M4.5 ★ Transaction UX: auto-commit OFF, Commit/Rollback, savepoints, close-with-pending-transaction dialog (opus, review mandatory)
- [ ] M4.6 ★ The honest no-Cancel UX: three-level time-limit control, "no limit" with its consequence, no Cancel button (opus, review mandatory) — on-demand Cancel itself stays out of scope/blocked (ADR-0001 owner decision; upstream issue #24)
- [ ] M4.7 DBMS_OUTPUT pane: per-worksheet enable, size, clear, truncation notice (sonnet)
- [ ] M4.8 ★ Error presentation: kind, ORA code, message, cause chain; caret highlighting only for PL/SQL positions (opus, review mandatory)
- [ ] M4.9 Multiple independent worksheets: N sessions, per-tab state, one busy tab never blocks another (sonnet)
- [ ] M4.10 Query history (per profile, SQLite), re-run into the current worksheet (sonnet)

### M5 — Results at scale

- [ ] M5.1 ★ ADR-0004 — Result Store representation, paging and bounded-memory policy (opus, review mandatory)
- [ ] M5.2 ★ Result Store implementation in `db-core` + FFI batch lifetime rules (opus, review mandatory)
- [ ] M5.3 Grid features: row numbers, NULL visualization, column resize/reorder, type-aware formatting via the bulk formatter, search-in-results (sonnet)
- [ ] M5.4 Copy: cell, row, range, with/without headers (sonnet)
- [ ] M5.5 CLOB/BLOB viewers over `read_lob_chunk`, paged, with a size warning (sonnet)
- [ ] M5.6 ★ Fetch-batch benchmark across row shapes and a real network; pick and record the shipped default (opus, review mandatory)
- [ ] M5.7 Perf gate re-run on the real database; record against M1's numbers (sonnet)
- [ ] M5.8 ★ Scrolling while a result is still streaming drops ≈ 0.3% of frames (GUI-thread bound: drains + view work) — budget the drain per frame / insert coalescing (opus, review mandatory)

### M6 — Browse, prove, package

- [ ] M6.1 Object browser: lazy tree over the 9 `SPEC.md` §16 groups, server-side filter, row cap, columns of a selected table (sonnet)
- [ ] M6.2 Workspace persistence: open worksheets, text, layout, active profile — non-transactional state only (sonnet)
- [ ] M6.3 i18n baseline: `qsTr` everywhere, EN + TH catalogues, `lrelease` in the build; Thai rendering test (sonnet)
- [ ] M6.4 Accessibility baseline: focus order, keyboard-only operation, `Accessible` properties, contrast check (sonnet)
- [ ] M6.5 Third-party notices: `cargo about` for the Rust graph + Qt/LGPL attribution, shipped in the installer and an About dialog (sonnet)
- [ ] M6.6 ★ Windows packaging: `windeployqt6`, unsigned installer, first-run layout (opus, review mandatory)
- [ ] M6.7 CI: build the Qt project on all three OS; run offscreen QML/QTest and the C smoke harness; cache Qt and cargo (sonnet)
- [ ] M6.8 ★ Phase 1 DoD review against `SPEC.md` §24, honest status per item; update `TASKS.md`, `phase-1.md`, `Task.html` (opus, review mandatory)
- [ ] M6.9 Cold first paint ≈ 800–900 ms (D3D11 device creation ≈ 250 ms + first delegate-instantiation polish ≈ 551 ms) vs `SPEC.md` §19 startup target — investigate fix candidates named in the S15 report (sonnet)

### Owner decisions (Phase 1)

- [x] Owner: install Qt and the build tools per `phase-1.md` §C.0 — approved 2026-09-20, without Qt Creator (decision C.3 #1)
- [x] Owner: licence position — LGPLv3-compliant dynamic linking, ban GPL-only Qt modules, commercial licence deferred (decision C.3 #2) — approved 2026-09-20 as recommended
- [x] Owner: Qt version policy — pin one exact Qt version, treat an upgrade as a reviewed change (decision C.3 #3) — approved 2026-09-20 as recommended
- [x] Owner: approve the Phase 1 defer list (decision C.3 #4) — approved 2026-09-20 as recommended
- [x] Owner: app identifier and branding — reverse-DNS id, executable/display name, installer publisher string, placeholder icon (decision C.3 #5) — approved 2026-09-20 as recommended
- [x] Owner: code signing for Windows — ship Phase 1 unsigned? (decision C.3 #6) — approved 2026-09-20 as recommended
- [x] Owner: secrets storage approach — Windows Credential Manager for Phase 1, no plaintext fallback ever (decision C.3 #7) — approved 2026-09-20 as recommended
- [x] Owner: local store format — one SQLite file for profiles/settings/history/workspace (decision C.3 #8) — approved 2026-09-20 as recommended
- [x] Owner: telemetry and logging policy — no telemetry, local rotating log, opt-in SQL-text debug logging (decision C.3 #9) — approved 2026-09-20 as recommended
- [ ] Owner: fetch-batch default sign-off once the M5.6 benchmark produces a number (decision C.3 #10)
- [ ] Owner: wording sign-off for the no-Cancel UX and "no limit" strings, M4.6 (decision C.3 #11)
- [x] Owner: Phase-0 leftovers (C-5, U-18, C-6) carried into Phase 1 M2 — confirmed 2026-09-19; done 2026-09-20 (decision C.3 #12)
- [x] Owner: upstream issues F and G — decided 2026-09-19 not to submit for now; drafts kept for tracking only, results file §6 (decision C.3 #13). Refreshed against `main` 2026-09-23: both confirmed still fully present, no facts changed; still awaiting owner go-ahead to submit
- [x] Owner: mobile test hardware — resolved 2026-09-19: Android arm64 phone (OPPO CPH2399) provided, NDK 28.2 installed; physical-device validation itself is separate Phase-0 tail work in progress on `phase-0/android-device`; iOS still needs a Mac + Apple Developer account + device, not provided (decision C.3 #14)
- [x] Owner: Community/Pro licensing decision before M6.6 so the notices file and About dialog are right the first time (decision C.3 #15) — decided 2026-09-24: GPL-3.0-or-later for Community, Pro separate (ADR-0005)
- [ ] Owner: Rule on S15 (K1 p99 wording, K2 warm vs cold, K3 baseline/metric) and accept or re-open ADR-0003

## P2 — IDE capabilities

- [ ] PL/SQL object editor
- [ ] Compile source
- [ ] Map compile errors to editor positions
- [ ] Table Inspector
- [ ] DDL viewer
- [ ] Explain Plan tree/text
- [ ] Metadata cache
- [ ] Basic autocomplete
- [ ] Streaming CSV/TSV/JSON export
- [ ] macOS packaging
- [ ] Linux packaging

## P3 — Performance / DBA

- [ ] Session Monitor
- [ ] Locks / blockers
- [ ] Long operations
- [ ] Wait events
- [ ] SQL statistics
- [ ] DBMS_XPLAN display cursor
- [ ] Plan comparison
- [ ] Evaluate Arrow-backed Result Store
- [ ] Add benchmark suite
- [ ] Establish memory budgets from measurements

## P4 — Tablet

- [ ] Android tablet UI
- [ ] iPad UI
- [ ] Adaptive workspace layout
- [ ] External keyboard shortcuts
- [ ] Trackpad/mouse support
- [ ] Touch-specific grid behavior
- [ ] Validate background/resume session handling

## P5 — Phone

- [ ] Phone navigation model
- [ ] Editor/result/output adaptive pages
- [ ] Result card mode
- [ ] Direct connection on supported platforms
- [ ] Mobile secure credential storage

## P6 — Pro candidates

Do not implement until Community architecture is stable.

- [ ] Schema Compare
- [ ] Data Compare
- [ ] Advanced Monitoring
- [ ] Performance Analyzer
- [ ] Automation
- [ ] Advanced Export
- [ ] AI Assistant
- [ ] Optional Gateway
- [ ] Team features

## Task rules

1. Every implementation task must map back to `SPEC.md`.
2. Architecture-changing work requires an ADR under `docs/decisions/`.
3. A task is not done without appropriate tests or a documented reason tests are not applicable.
4. Performance claims require a benchmark.
5. Mobile support requires a physical-device result, not emulator-only evidence.
6. Do not start downstream UI work if a Phase 0 architecture gate is unresolved.
