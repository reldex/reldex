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
| [0003](0003-qt-rust-integration.md) | Qt ↔ Rust integration — hand-written stable C ABI in `crates/ffi`, `cbindgen`-generated header, thin C++/Qt adapter | Proposed — spike S15 complete 2026-09-20; owner ruling requested | 2026-09-20 |
| [0004](0004-result-store.md) | Result Store — the core retains the fetched prefix as compacted columnar segments (scaled-`i64` `NUMBER`, exact-size text, LOB ids); fetch on demand with one batch of read-ahead; per-result row/byte caps as settings (desktop 1,000,000 rows / 512 MiB, mobile 100,000 / 64 MiB) with an honest limit state; spill, eviction and Arrow deferred to P3; C ABI 4 shape for M5.2 | Proposed — M5.1 draft 2026-09-25; lead acceptance pending; owner review requested on caps, mobile caps and the P3 deferral | 2026-09-25 |
| [0005](0005-public-repository-for-hosted-ci.md) | Public repository to keep hosted CI — Actions minutes exhausted 2026-09-21; repository goes public rather than reducing the CI matrix; `tools/gates.sh` added as a local pre-push/pre-PR check; also settles Community/Pro licensing (GPL-3.0-or-later) | Accepted — owner decision 2026-09-24 | 2026-09-24 |
| [0006](0006-local-persistence-settings-profiles-sqlite.md) | Local persistence — typed settings with three-level resolution and provenance, connection profiles without secrets, one SQLite file (`crates/workspace`) | Accepted — implements owner decisions 2026-09-19/20 (§C.3 items 5, 7, 8) | 2026-09-24 |
| [0007](0007-credential-store.md) | Credential storage — `CredentialStore` trait (`crates/secrets`), Windows Credential Manager (`Reldex/profile/<uuid>`, local-machine persistence, a session-wide mutex against measured concurrent-write loss), no store means prompt each time; `unsafe` allowed in that one file; `Secret` wipes with `zeroize` | Accepted — implements owner decision 2026-09-20 (§C.3 item 7); settles ADR-0002 item 4 and `ARCHITECTURE.md` §13 item 9 | 2026-09-25 |
