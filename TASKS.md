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
- [ ] Add driver contract tests that detect behaviour changes on `oracledb` upgrades
- [x] **(S4 critical path)** Draft upstream request to `oracle/rust-oracledb` for a public statement-cancel/break API — plus accessors for `OracleNumber` digits and the connection's transaction-in-progress flag (four issues drafted in full: `docs/exec-plans/active/phase-0-spike-results.md` §6 — nothing has been submitted; see the separate owner task below)
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
- [x] CLOB/NCLOB/BLOB (spike S7 — pass; 100 MB CLOB and BLOB streamed, +4.7 MB working set)
- [x] DBMS_OUTPUT (spike S5 — pass, including Thai text)
- [!] Long-running query cancellation — accepted limitation (owner decision 2026-09-20): pre-armed deadline only; blocked on upstream cancel API (see ADR-0001 "Spike outcome" and "Owner decision" sections and `phase-0-spike-results.md` §4)
- [ ] TCPS (spike S8 — not run; the Phase 0 test DB has no TCPS listener)
- [ ] Network-loss behavior (not directly spiked; only recovery-path failure modes seen incidentally during S4, recorded as U-6/U-7)
- [x] Concurrent independent sessions (spike S9 — pass; 8 sessions, 400 inserts, 283 ms)

### Platform validation
- [x] Windows x64 (build, connect, and the full spike matrix all run locally, 2026-09-19)
- [~] Linux x64 (build + fmt + clippy -D warnings + `cargo test --workspace` green on CI `ubuntu-latest`, PR #1, 2026-09-20 — https://github.com/reldex/reldex/actions/runs/35419377082; no database connect yet)
- [~] macOS ARM64 (build + fmt + clippy -D warnings + `cargo test --workspace` green on CI `macos-latest` (Apple Silicon), PR #1, 2026-09-20 — https://github.com/reldex/reldex/actions/runs/35419377082; no database connect yet)
- [ ] Android ARM64 physical device (spike S6 not run — needs the Android NDK; owner approval to download)
- [ ] iOS/iPadOS ARM64 physical device (not started)

- [x] Owner: decide cancellation path (ADR-0001 re-opened) [decision 2026-09-20: stay on `oracledb`; ship the pre-armed per-statement deadline with an honest UI; pursue upstream fixes via the four drafted issues — see ADR-0001 "Owner decision (2026-09-20)"]
- [ ] Owner: submit the four drafted upstream issues (`docs/exec-plans/active/phase-0-spike-results.md` §6, Issue B first)
- [ ] Track upstream `oracle/rust-oracledb` releases; re-run the integration suite and the ignored abort-repro tests on each new beta
- [ ] Third-party notices file before any binary distribution (dependency licences — 63 third-party crates in the oracle-thin graph, all permissive but attribution is required; see `phase-0-spike-results.md` §1)
- [ ] Test DB: TCPS listener (S8)
- [ ] Test DB: container memory cap
- [ ] S6 Android/iOS cross-compile check (needs NDK / macOS)

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
- [ ] Commit/Rollback
- [ ] Result Store
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
