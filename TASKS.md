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
- [ ] Decide Community/Pro licensing before public release
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
- [x] **(S4 critical path)** Draft upstream request to `oracle/rust-oracledb` for a public statement-cancel/break API — plus accessors for `OracleNumber` digits and the connection's transaction-in-progress flag (seven issues drafted in full: `docs/exec-plans/active/phase-0-spike-results.md` §6 — A–E submitted 2026-09-20 (#21–#25), F and G await owner go-ahead; see the separate owner task below)
- [x] Add normalized `DbError`
- [~] Add connection/profile model (driver-level connection params done; user-facing profile model pending)
- [x] Add session abstraction (`db-core` `DatabaseSession` implemented and hardened; see above)
- [x] Add transaction abstraction (`db-core` conservative tracking, incl. locking queries, implemented and proven by spike S3)
- [x] Add cancellation abstraction (contract implemented; driver-level mechanism evaluated in spike S4 and **fails for the requirement** — only a pre-armed per-round-trip deadline exists, no on-demand cancel; owner decision 2026-09-20: accepted as a limitation, see ADR-0001 "Owner decision" section)

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
- [!] Long-running query cancellation — accepted limitation (owner decision 2026-09-20): pre-armed deadline only; blocked on upstream cancel API (see ADR-0001 "Spike outcome" and "Owner decision" sections and `phase-0-spike-results.md` §4)
- [x] TCPS (spike S8 — pass with limits, 2026-09-20: one-way TLS 1.2, verification on, PEM-supplied trust; no mTLS + private CA, no Oracle wallet files, no revocation — upstream U-12…U-14)
- [x] Network-loss behavior (spike S10 — pass, with two upstream gaps: a black-holed link never returns without a caller-set deadline, U-17; a dead client's row lock blocked a second session for the full 20 s measured because `SQLNET.EXPIRE_TIME` is unset and upstream has no keepalive — see `phase-0-spike-results.md` §3 S10)
- [x] Concurrent independent sessions (spike S9 — pass; 8 sessions, 400 inserts, 283 ms)

### Platform validation
- [x] Windows x64 (build, connect, and the full spike matrix all run locally, 2026-09-19)
- [~] Linux x64 (build + fmt + clippy -D warnings + `cargo test --workspace` green on CI `ubuntu-latest`, PR #1, 2026-09-20 — https://github.com/reldex/reldex/actions/runs/35419377082; no database connect yet)
- [~] macOS ARM64 (build + fmt + clippy -D warnings + `cargo test --workspace` green on CI `macos-latest` (Apple Silicon), PR #1, 2026-09-20 — https://github.com/reldex/reldex/actions/runs/35419377082; no database connect yet)
- [ ] Android ARM64 physical device (cross-compile + link proven in CI — spike S6, PR #3; physical-device evidence still needed, needs a test device from the owner)
- [ ] iOS/iPadOS ARM64 physical device (cross-compile + link proven in CI — spike S6, PR #3; physical-device evidence still needed, needs a Mac + Apple Developer account + device)

- [x] Owner: decide cancellation path (ADR-0001 re-opened) [decision 2026-09-20: stay on `oracledb`; ship the pre-armed per-statement deadline with an honest UI; pursue upstream fixes via the four drafted issues — see ADR-0001 "Owner decision (2026-09-20)"]
- [x] Owner: submit the drafted upstream issues — five submitted 2026-09-20 (#21–#25) (`docs/exec-plans/active/phase-0-spike-results.md` §6)
- [ ] Owner: approve submission of drafted upstream issues F and G (results file §6)
- [ ] Track upstream `oracle/rust-oracledb` releases; re-run the integration suite and the ignored abort-repro tests on each new beta
- [ ] Third-party notices file before any binary distribution (dependency licences — 63 third-party crates in the oracle-thin graph, all permissive but attribution is required; see `phase-0-spike-results.md` §1)
- [x] Test DB: TCPS listener (S8) — `127.0.0.1:2484`, idempotent startup hook, wallet material untracked
- [x] Test DB: container memory cap — 1.5 GiB SGA / 512 MiB PGA, `mem_limit: 4g` (~2.0 GiB resident)
- [x] Owner: decide how TCPS support is described to users and whether to refuse descriptors carrying `SSL_SERVER_DN_MATCH` (results file §9 items 6–7)
- [x] Driver: TCPS descriptor guard (U-14) — refuse `SSL_SERVER_CERT_DN` unless explicitly allowed, warn on `SSL_SERVER_DN_MATCH` — implemented as the lead proposed and independently reviewed (3 must-fix applied); confirmed by the owner and merged 2026-09-19 (pull request #5).
- [x] S6 Android/iOS cross-compile check — pass, kill criterion did not fire (PR #3: https://github.com/reldex/reldex/pull/3); physical-device validation still needed
- [ ] Driver: honour `connect_timeout` (C-5) — `ConnectionParams::connect_timeout` is accepted and ignored (`phase-0-spike-results.md` §7 C-5); approved 2026-09-19: helper thread, default 15 s, user-configurable per connection profile including "no limit"
- [x] Owner decisions §9 items 8–12 — decided 2026-09-19: default per-statement deadline 600 s (three-level setting), `EXPIRE_TIME` documentation-only recommendation, trigger DDL auto-rewrite with an off switch, default fetch batch size deferred to a Phase 1 benchmark, C-5 helper-thread approach — every one made user-configurable per the owner's requirement (`phase-0-spike-results.md` §9)
- [ ] Driver: `CREATE TRIGGER` auto-rewrite for `:NEW`/`:OLD` (U-18) — rewrite to `BEGIN EXECUTE IMMEDIATE q'[…]'; END;` on by default, always reported to the user as a warning with the statement actually sent available for inspection, off switch at connection level (approved 2026-09-19, `phase-0-spike-results.md` §9 item 11)
- [ ] Contract: connect-time warning channel (C-6) — additive `take_connect_warnings`-style method on `DatabaseConnection`, collected once by `db-core` after connect; approved in principle 2026-09-19, detailed write-up in `phase-0-spike-results.md` §7 C-6; sequenced after pull request #5 (TCPS descriptor guard); record as an ADR-0002 amendment when implemented
- [ ] Docs: recommend `SQLNET.EXPIRE_TIME` (e.g. 10 minutes) in user-facing connection troubleshooting docs (owner decision 2026-09-19, `phase-0-spike-results.md` §9 item 10)
- [ ] Docs: note in `tools/oracle-test-db/README.md` that `SQLNET.EXPIRE_TIME` is left unset on the Phase 0 test database on purpose, so S10's measurements remain valid (follow-up; not edited in this change)
- [ ] Android physical-device harness (needs a test device from the owner + local NDK)

## P1 — Desktop MVP

- [ ] Qt Quick application shell
- [ ] Thin C++ ↔ Rust FFI adapter
- [ ] Non-blocking `open_session` + per-session completion/event queue for the Qt adapter (deferred by ADR-0002 amendment)
- [ ] Connection Manager
- [ ] Workspace shell
- [ ] SQL Worksheet
- [ ] Multiple independent sessions
- [ ] Run current statement
- [ ] Run selection
- [ ] Run script
- [ ] PL/SQL execution
- [ ] Bind-variable dialog
- [ ] Cancel
- [ ] UI (P1): statement time-limit control + explicit "Cancel unavailable with current driver" messaging (SPEC §10 interim note)
- [ ] Default per-statement time limit (600 s) as a three-level setting — application default, connection profile, per worksheet/statement — including a "no limit" option with an explicit UI warning about the consequence (owner decision 2026-09-19, SPEC §10)
- [ ] Commit/Rollback
- [ ] Result Store
- [ ] Fetch batch size as a user setting (application default + per connection profile, bounded range), informed by a Phase 1 benchmark to pick the shipped default (owner decision 2026-09-19, SPEC §12; `phase-0-spike-results.md` S14)
- [ ] Virtualized Result Grid
- [ ] Query History
- [ ] DBMS_OUTPUT panel
- [ ] Basic Object Browser
- [ ] Production connection indicator
- [ ] Secure credential integration on Windows

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
