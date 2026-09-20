# Architecture Decision Records

This directory holds Reldex ADRs: short records of decisions that change how the system is built, not general design notes.

## When an ADR is required

Per `AGENTS.md`, create an ADR when a decision:

- changes a core boundary;
- changes session/transaction semantics;
- changes the database-driver strategy;
- changes Qt/Rust integration;
- changes result-storage architecture;
- changes cross-platform support expectations.

## Conventions

- **File naming:** `NNNN-short-title.md`, zero-padded sequence number (e.g. `0001-thin-oracle-driver.md`). Numbers are never reused.
- **Template:** start new ADRs from `0000-adr-template.md`.
- **Status:** one of `Proposed`, `Accepted`, `Superseded`, `Rejected`.
  - A superseding ADR must reference the ADR it replaces, and the replaced ADR's status is updated to `Superseded`.

## Index

| ADR | Title | Status | Date |
| --- | --- | --- | --- |
| [0001](0001-database-driver-strategy.md) | Database driver strategy — primary driver is Oracle's `oracledb` (rust-oracledb) | Accepted — owner decision 2026-09-19: stay on oracledb; pre-armed deadline + honest UI; upstream issues pending — upstream issues #21–#25 filed 2026-09-19 | 2026-09-19 |
| [0002](0002-driver-api-and-concurrency-model.md) | Driver API and concurrency model — blocking object-safe contract, per-session worker thread in `db-core` | Accepted (owner confirmed 2026-09-19) — implemented; independently reviewed twice (API review, db-core session review), must-fix findings applied; amended after the Phase 0 spikes | 2026-09-19 |
| [0003](0003-qt-rust-integration.md) | Qt ↔ Rust integration — hand-written stable C ABI in `crates/ffi`, `cbindgen`-generated header, thin C++/Qt adapter | Proposed — acceptance conditional on spike S15 | 2026-09-20 |
