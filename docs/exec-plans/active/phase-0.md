# Execution Plan — Phase 0 Architecture Validation

**Status:** Active  
**Goal:** Prove the core database architecture before significant Reldex UI development.

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

## Driver decision

The primary database driver is Oracle's official [`oracle/rust-oracledb`](https://github.com/oracle/rust-oracledb)
(crate `oracledb`) — see [ADR-0001](../../decisions/0001-database-driver-strategy.md), including its ordered spike plan
(S1–S9) and kill criteria. Local integration tests run against the Docker database in `tools/oracle-test-db/`.

## Workstream A — Rust workspace

- [x] Initialize Cargo workspace.
- [x] Add `db-driver-api`. (implemented per ADR-0002: 5 traits — `DatabaseDriver`, `DatabaseConnection`, `CancelHandle`, `Cursor`, `LobStream` — ~45 types, zero production dependencies, 68 unit + 6 doc tests, fmt/clippy/test green; independent API review in progress, owner review pending)
- [ ] Add `db-core`. (skeleton crate created at `crates/db-core`; session/worker-thread implementation against the now-implemented `db-driver-api` contract not started)
- [ ] Add `drivers/oracle/thin` wrapping Oracle's `oracledb` crate, pinned to an exact version (ADR-0001). (skeleton crate created at `crates/drivers/oracle-thin` — path deviates from this plan's `drivers/oracle/thin`; see `docs/architecture/ARCHITECTURE.md` §11)
- [ ] Add Reldex-owned driver contract tests so an `oracledb` upgrade that changes behaviour is detected (ADR-0001).
- [ ] Add test-support/mock driver. (skeleton crate created at `crates/drivers/mock`)
- [x] Define normalized `DbError`. (implemented in `db-driver-api` per ADR-0002 D3: `DbError{kind, message, native, position, session_state, retryable, source}`)
- [x] Define connection/session/query/result identifiers. (implemented in `db-driver-api` per ADR-0002 D7: `ConnectionId`/`SessionId`/`StatementId`/`ResultSetId` as `u64` newtypes)

## Workstream B — Session correctness

- [ ] Connect/disconnect/ping.
- [ ] Keep session identity stable across worksheet-like commands.
- [ ] COMMIT.
- [ ] ROLLBACK.
- [ ] SAVEPOINT.
- [ ] Demonstrate uncommitted state survives multiple statements on the same session.
- [ ] Demonstrate a second session cannot accidentally inherit session state.

## Workstream C — Query execution

- [ ] SELECT.
- [ ] DML.
- [ ] DDL.
- [ ] PL/SQL anonymous block.
- [ ] IN/OUT/IN OUT binds.
- [ ] REF CURSOR.
- [ ] Multiple concurrent sessions.
- [ ] Long-running query.
- [ ] Cancellation from another control path. (ADR-0001 C1 / spike S4: `oracledb` has no public cancel API yet; Phase 0 may demonstrate cancellation via call-timeout semantics while an upstream request is pending — the limitation must be reported, not hidden.)

## Workstream D — Data types

- [ ] NUMBER.
- [ ] CHAR/VARCHAR2/NVARCHAR2.
- [ ] DATE.
- [ ] TIMESTAMP.
- [ ] TIMESTAMP WITH TIME ZONE.
- [ ] RAW.
- [ ] CLOB.
- [ ] NCLOB.
- [ ] BLOB.
- [ ] JSON where applicable.

For every mapping, document:
- Rust representation;
- NULL handling;
- precision/loss risk;
- streaming/lazy behavior for large values.

## Workstream E — Database-specific development features

- [ ] DBMS_OUTPUT.
- [ ] metadata dictionary query.
- [ ] V$ query where permissions allow.
- [ ] EXPLAIN PLAN.
- [ ] DBMS_XPLAN.

## Workstream F — Network/security

- [ ] Easy Connect.
- [ ] service name.
- [ ] connect descriptor.
- [ ] TCP.
- [ ] TCPS.
- [ ] timeout behavior.
- [ ] network-loss behavior.
- [ ] reconnect semantics.

Known unsupported configurations of the primary driver (ADR-0001): Native Network Encryption and
11G password verifiers. Record them as documented limitations, not as driver failures.

## Workstream G — Platforms

### Windows x64
- [ ] Build.
- [ ] Connect.
- [ ] Full POC matrix.

### Linux x64
- [ ] Build.
- [ ] Connect.
- [ ] Core smoke matrix.

### macOS ARM64
- [ ] Build.
- [ ] Connect.
- [ ] Core smoke matrix.

### Android ARM64 physical device
- [ ] Cross-compile.
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
- [ ] Cross-compile.
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

Record at least:
- connection latency;
- fetch throughput;
- memory during large fetch;
- cancellation latency;
- binary/package constraints per platform.

Do not make comparative performance claims without recording the method and environment.

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
