# Execution Plan — Phase 0 Architecture Validation

**Status:** Active  
**Goal:** Prove the core database architecture before significant Reldex UI development.

## Success criteria

Phase 0 is complete when:

1. the generic driver/session API exists;
2. the selected thin driver can connect to the reference database;
3. transaction behavior is correct;
4. query cancellation is demonstrated;
5. required datatypes/PLSQL behaviors are integration-tested;
6. desktop platform viability is established;
7. Android/iOS direct-connect feasibility has physical-device evidence or a clearly documented blocker;
8. no full desktop UI implementation was required to prove these results.

## Workstream A — Rust workspace

- [ ] Initialize Cargo workspace.
- [ ] Add `db-driver-api`.
- [ ] Add `db-core`.
- [ ] Add `drivers/oracle/thin`.
- [ ] Add test-support/mock driver.
- [ ] Define normalized `DbError`.
- [ ] Define connection/session/query/result identifiers.

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
- [ ] Cancellation from another control path.

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
