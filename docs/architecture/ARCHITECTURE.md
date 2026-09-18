# Reldex Architecture

**Status:** Draft — derived from SPEC.md; to be refined by Phase 0 findings and ADRs

## 1. Purpose and scope

This document is the architecture source of truth referenced by `README.md`, `AGENTS.md`, and
`.agents/skills/reldex-development/SKILL.md`. It defines layers, boundaries, dependency direction,
threading rules, ownership of sessions and transactions, and the invariants every change must
preserve.

It does not restate product requirements. For product detail see `SPEC.md`: platforms §4, core
modules §6, driver strategy §7, sessions §9, transactions §10, UI §11, results §12, metadata §16,
credentials §17, mobile §18, local persistence §20.

Decisions that change a boundary listed here require an ADR under `docs/decisions/`
(`AGENTS.md`, "Documentation"). This document is a Phase 0 draft: items in §13 are deliberately
unresolved and must be answered by `reldex-core-poc` evidence, not by implementation convenience.

## 2. Layers and dependency rules

```text
+-----------------------------------------------------------+
|  Presentation          Qt Quick / QML                      |  Tier: UI
+-----------------------------------------------------------+
|  Integration           Thin C++ / Qt Adapter               |  QObject, QAbstractItemModel
+-----------------------------------------------------------+
|  Boundary              Stable Rust FFI                     |  explicit, typed, versioned
+-----------------------------------------------------------+
|  Core (Rust)           db-core                             |  vendor-neutral, UI-independent
|                        sessions, transactions, query,      |
|                        results, metadata, workspace        |
+-----------------------------------------------------------+
|  Driver contract       db-driver-api                       |  vendor-neutral traits + DbError
+-----------------------------------------------------------+
|  Driver impls          drivers/oracle/thin, mock driver    |  vendor-specific code lives here
+-----------------------------------------------------------+
|  Database protocol / Database server                       |
+-----------------------------------------------------------+
```

Dependencies point downward only:

- QML depends on the adapter; it never reaches the core or a driver directly.
- The C++/Qt adapter depends only on the stable FFI surface.
- `db-core` depends on `db-driver-api`, never on a concrete driver crate.
- Driver implementations depend on `db-driver-api` and never on `db-core` or the UI.
- Nothing below the FFI line may depend on Qt, QML, or any UI type.

Forbidden edges: core -> UI, core -> concrete driver, driver -> core, QML -> driver,
adapter -> business rules.

## 3. Core modules and responsibilities

Vendor-neutral names (`SPEC.md` §6). Vendor types belong only to driver/provider implementations.

| Module | Responsibility |
| --- | --- |
| `DatabaseDriver` | Factory/entry point for a driver implementation; creates connections. |
| `DatabaseConnection` | A physical/logical connection produced by a driver. |
| `DatabaseSession` | A stable, stateful database session; the unit a worksheet owns. |
| `QueryExecutor` | Statement and block execution, binds, cursors, cancellation control. |
| `TransactionManager` | Commit, rollback, savepoint, and transaction-state tracking per session. |
| `ResultStore` | Typed, batched, bounded-memory storage of fetched rows. |
| `MetadataProvider` | Generic metadata queries; vendor dictionary SQL stays in the vendor provider. |
| `WorkspaceService` | Non-transactional workspace, profiles, history, settings, layout state. |

`reldex-core-poc` exercises these modules without a full UI (`SPEC.md` §26).

## 4. Driver boundary

The driver API (`db-driver-api`) is the only contract between the core and any database vendor.

- The concrete driver crate is encapsulated so it can be swapped as maturity and platform support
  change (`SPEC.md` §7). No generic API exposes driver-native structs or types.
- The default connection path is pure/thin and must not require vendor client libraries such as
  Instant Client/OCI. Desktop may later add an optional native compatibility driver; mobile must
  never require one.
- A test-support/mock driver implements the same contract so core logic is testable without a
  database. Integration tests requiring a real database stay separated from unit tests.

Error normalization: driver errors are normalized into the internal error model (`DbError`) while
preserving native database error codes for diagnostics (`AGENTS.md`, "Code quality").

```text
driver-native error --> normalize --> DbError
                                        |- classified for core/UI behavior
                                        `- native database error code preserved
```

Only the two properties above are settled; the concrete `DbError` shape and its category set are
open (§13). Permission-dependent metadata and monitoring failures must be distinguishable from
genuine driver/connection failures.

## 5. Session and transaction model

A worksheet owns a stable database session (`SPEC.md` §9). Sessions are never pooled in a way that
allows arbitrary replacement.

```text
Connection Profile
 |- Worksheet A     -> Session A   (transaction, schema, ALTER SESSION, NLS,
 |- Worksheet B     -> Session B    package state, temp tables, DBMS_OUTPUT, app context)
 |- Object Browser  -> Metadata Session
 `- Monitor         -> Monitoring Session
```

Rules:

- Auto-commit defaults to OFF.
- Every worksheet exposes Execute, Cancel, Commit, Rollback.
- Closing a worksheet with an active transaction prompts Commit / Rollback / Cancel close.
  Reldex never silently commits and never hides transaction loss.
- A generic connection pool must not arbitrarily replace a worksheet session.
- Session identity is stable across worksheet-like commands; a second session must not inherit
  another session's state (Phase 0 Workstream B).
- A reconnect creates a *new* database session and is surfaced as such.

## 6. Concurrency and threading

- No database or network I/O on the UI thread, ever (`SPEC.md` §11, §19).
- Sessions execute independently and concurrently; one busy session must not block the UI or other
  sessions (`SPEC.md` §24.17).
- Cancellation must be triggerable from a control path other than the one blocked on execution
  (`phase-0.md` Workstream C). Cancellation latency is a tracked metric.
- Long/large operations are streamed and incremental, not blocking whole-result operations.
- The core owns its execution model; the UI submits work and observes results, it does not drive
  threads across the FFI boundary.

The concrete execution model (async runtime vs. dedicated per-session threads) and the
cancellation mechanism are open (§13) and must be chosen from Phase 0 measurements.

## 7. FFI and Qt adapter boundary

```text
QML  ->  QObject / QAbstractItemModel  ->  Thin C++ Adapter  ->  Stable Rust FFI  ->  Reldex Core
```

- The FFI surface is explicit and typed; stringly typed cross-layer protocols are avoided
  (`AGENTS.md`, "Code quality"). Keep the public surface small until the architecture stabilizes.
- The C++ adapter contains integration code only — marshalling, model adapters, lifetime and
  thread affinity. No business rules, no SQL, no transaction policy.
- QML contains presentation logic only. Data reaches the view through model/view interfaces, not
  large JS/QML arrays.
- Platform-specific behavior stays behind the adapter/platform layer.

## 8. Result pipeline

```text
Database Cursor -> Batch Fetch -> Result Store -> Virtual Table Model -> Visible Rows/Cells
                                                                        -> Qt Quick TableView
```

- Never one QML object per row for a large result; grids are row/cell virtualized.
- The result store supports typed values, explicit NULL representation, batches, bounded memory,
  lazy large-value (LOB) access, streaming export, and efficient random access (`SPEC.md` §12).
- Export streams; it never materializes a full result in memory (`SPEC.md` §21).
- Apache Arrow may be used internally *only where benchmarks justify it*, and UI APIs must not
  depend on Arrow (`SPEC.md` §12; `ROADMAP.md` Phase 3).
- Avoid unnecessary allocation and copying here; changes require a recorded baseline and
  measurement (`AGENTS.md`, "Performance").

## 9. Metadata, local persistence, credentials

- All metadata loading is lazy and flows through the generic `MetadataProvider`; vendor dictionary
  SQL lives in the vendor provider. A local SQLite metadata cache may be used with TTL, refresh,
  invalidation, and per-database identity isolation (`SPEC.md` §16).
- SQLite local persistence covers profiles (without plaintext secrets), workspace, query history,
  metadata cache, favorites, snippets, settings, UI layout, feature state (`SPEC.md` §20).
- Credentials use platform secure storage (Windows Credential Manager, Apple Keychain, Android
  Keystore, Linux Secret Service) behind a single core abstraction. Passwords, credentials, keys,
  and tokens are never logged.
- Entitlements resolve through one centralized feature service, not scattered `is_pro` checks
  (`SPEC.md` §22).

## 10. Platform tiers and mobile lifecycle

Tier 1 Windows / macOS / Linux (Windows first), Tier 2 iPadOS / Android tablets, Tier 3 Android
phones / iPhone. Mobile is not monitoring-only; differences come from screen size, platform
lifecycle, or technical limits — never from a reduced database core. Mobile and desktop share the
same core; mobile connects directly where driver and platform allow, with an optional gateway mode
possible later.

On mobile resume (`SPEC.md` §18):

```text
resume -> validate existing session
            |- alive     -> continue
            `- not alive -> mark disconnected (surface the loss)
                            reconnect creates a NEW session
```

Mobile direct-connect is not supported until physical-device evidence exists for connect,
SQL/PL/SQL, binds, REF CURSOR, LOB, commit/rollback, cancel, TCPS, background/resume, and
reconnect (`SPEC.md` §25; `phase-0.md` Workstream G). Emulator/simulator results do not count.

## 11. Planned repository layout (provisional)

Only crates the current documents name. Layout is provisional until Phase 0 Workstream A lands and
an ADR records it.

```text
db-driver-api/          vendor-neutral driver contract + DbError
db-core/                sessions, transactions, query, results, metadata, workspace
drivers/oracle/thin/    thin driver implementation (vendor code isolated here)
drivers/<mock>/         test-support/mock driver for core tests
reldex-core-poc/        Phase 0 validation harness (no full UI)
docs/architecture/, docs/decisions/, docs/exec-plans/
```

## 12. Architectural invariants

A change is non-compliant if it breaks any of these without an approved ADR:

1. Product and core layers are vendor-neutral.
2. Vendor-specific behavior lives only in drivers/providers; native types never leak upward.
3. The Rust core is UI-independent and has no Qt dependency.
4. The C++/Qt adapter is thin; business rules are not in C++ or QML.
5. No database or network I/O on the UI thread.
6. A worksheet owns a stable stateful session; it is never silently replaced.
7. Auto-commit defaults to OFF; never silently commit or hide transaction loss.
8. Large results are cursor/batch based and virtualized; never one QML object per row.
9. Driver errors are normalized while native error codes are preserved.
10. Mobile and desktop share one database core.
11. Mobile direct-connect is unsupported without physical-device evidence.
12. Performance claims require a recorded baseline and benchmark.
13. Cross-layer APIs are explicit and typed, and public surfaces stay small.

## 13. Open questions and pending ADRs

Each item must be resolved by Phase 0 evidence and recorded as an ADR under `docs/decisions/`.
Until then, no code should assume an answer.

1. **Thin driver selection.** Which pure/thin Rust driver implementation backs `drivers/oracle/thin`,
   and what are its maturity, platform, and TCPS constraints across Tier 1-3 targets?
2. **FFI mechanism.** Which Rust/C++ interop approach provides a stable, typed, small boundary, and
   how is ABI/version compatibility guaranteed?
3. **Threading and callbacks across FFI.** How does the core deliver completion and progress to the
   Qt thread — polling, callbacks, or a queue — and what are the thread-affinity and reentrancy rules?
4. **Async runtime vs. threads.** Does the core use an async runtime or dedicated per-session
   threads, and what does each cost on mobile (binary size, battery, background behavior)?
5. **Cancellation mechanism.** How is a running statement cancelled from another control path, and
   what cancellation latency is achievable per platform?
6. **Result store representation.** What is the in-memory row/batch layout, bounded-memory policy,
   and spill/eviction behavior? Does Arrow earn its place by benchmark (deferred to Phase 3)?
7. **Error model shape.** What are the `DbError` categories, the retryability/classification rules,
   and how are permission-dependent failures separated from driver failures?
8. **Metadata cache design.** Cache keying and per-database identity isolation, TTL and
   invalidation strategy, and behavior at 100,000+ objects.
9. **Credential storage abstraction.** What single core abstraction spans the four platform secure
   stores, and what is the fallback when none is available?
10. **Crate layout.** Final workspace layout, crate boundaries, and feature flags (provisional in §11).
11. **Type mapping.** Rust representation, NULL handling, precision-loss risk, and lazy/streaming
    behavior per database type (`phase-0.md` Workstream D).
12. **Script/statement boundary parsing.** Where the SQL/PL/SQL block-boundary parser lives and how
    it is shared between core and editor (`SPEC.md` §15).
