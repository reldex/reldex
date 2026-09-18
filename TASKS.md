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

See `docs/exec-plans/active/phase-0.md`.

- [x] Local Oracle 19c Docker test database (tools/oracle-test-db)

### Driver/core
- [x] Create Rust workspace
- [x] Create `db-driver-api`
- [~] Create `db-core`
- [ ] Create initial database driver implementation — wrap Oracle's official `oracledb` crate (`oracle/rust-oracledb`, pinned exact version) in `crates/drivers/oracle-thin` (ADR-0001)
- [ ] Add driver contract tests that detect behaviour changes on `oracledb` upgrades
- [ ] Draft upstream request to `oracle/rust-oracledb` for a public statement-cancel API (owner to submit)
- [x] Add normalized `DbError`
- [~] Add connection/profile model (driver-level connection params done; user-facing profile model pending)
- [~] Add session abstraction (contract in db-driver-api; db-core implementation pending)
- [~] Add transaction abstraction (contract in db-driver-api; db-core implementation pending)
- [x] Add cancellation abstraction (contract; driver-level cancel pending spike S4)

### Functional POC
- [ ] Connect/disconnect/ping
- [ ] SELECT
- [ ] DML
- [ ] PL/SQL anonymous block
- [ ] IN/OUT/IN OUT binds
- [ ] REF CURSOR
- [ ] COMMIT/ROLLBACK/SAVEPOINT
- [ ] CLOB/NCLOB/BLOB
- [ ] DBMS_OUTPUT
- [ ] Long-running query cancellation
- [ ] TCPS
- [ ] Network-loss behavior
- [ ] Concurrent independent sessions

### Platform validation
- [ ] Windows x64
- [ ] Linux x64
- [ ] macOS ARM64
- [ ] Android ARM64 physical device
- [ ] iOS/iPadOS ARM64 physical device

## P1 — Desktop MVP

- [ ] Qt Quick application shell
- [ ] Thin C++ ↔ Rust FFI adapter
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
