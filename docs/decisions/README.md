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
