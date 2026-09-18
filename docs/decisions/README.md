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
| [0001](0001-database-driver-strategy.md) | Database driver strategy — primary driver is Oracle's `oracledb` (rust-oracledb) | Accepted (conditional on spikes S1–S5) | 2026-09-19 |
| [0002](0002-driver-api-and-concurrency-model.md) | Driver API and concurrency model — blocking object-safe contract, per-session worker thread in `db-core` | Accepted (provisional — implemented; independent API review in progress; owner review pending) | 2026-09-19 |
