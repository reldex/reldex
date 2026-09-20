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

Per [ADR-0002](../decisions/0002-driver-api-and-concurrency-model.md) D2, `DatabaseConnection` is the
driver-contract trait (`db-driver-api`; `Send`, not `Sync`; `&mut self`), while `DatabaseSession` is a
`db-core` type, not a driver-contract trait — it owns a connection, that connection's dedicated
worker thread, its `Arc<dyn CancelHandle>`, and its conservative transaction state. The driver
contract stops at `DatabaseConnection`.

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

The concrete `DbError` shape and category set are now settled by
[ADR-0002](../decisions/0002-driver-api-and-concurrency-model.md) D3:
`DbError{kind, message, native, position, session_state, retryable, source}`, with a stable,
`#[non_exhaustive]` `ErrorKind` whose `Permission` variant keeps permission-dependent metadata and
monitoring failures distinguishable from genuine driver/connection failures.

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

`db-core`'s `DatabaseSession` implements this rather than leaving it to the FFI/UI layer. `close`
takes a `CloseDisposition` and decides whether a transaction is open on the session's own worker
thread, after every command queued ahead of the close has run; a commit or rollback that fails inside
`close` leaves the session open rather than discard a transaction the user asked to keep (never a
silent commit — [ADR-0002](../decisions/0002-driver-api-and-concurrency-model.md) K4). A session's
lifecycle adds `Lost` (unrecoverable, terminal, no reconnect) and `Closed` (closed on purpose) states
on top of the driver contract's own `SessionState`, so a closed or dead session is never mistaken for
one still usable (ADR-0002 K8).

## 6. Concurrency and threading

- No database or network I/O on the UI thread, ever (`SPEC.md` §11, §19).
- Sessions execute independently and concurrently; one busy session must not block the UI or other
  sessions (`SPEC.md` §24.17).
- Cancellation must be triggerable from a control path other than the one blocked on execution
  (`phase-0.md` Workstream C). Cancellation latency is a tracked metric.
- Long/large operations are streamed and incremental, not blocking whole-result operations.
- The core owns its execution model; the UI submits work and observes results, it does not drive
  threads across the FFI boundary.

The execution model is chosen: per [ADR-0002](../decisions/0002-driver-api-and-concurrency-model.md)
D1/D2, `db-core` runs one dedicated worker thread per session (no async runtime below the FFI line),
and cancellation from another control path goes through a separate, cloneable `Arc<dyn CancelHandle>`
obtained before a call blocks — never through the worker thread itself. The driver-level cancellation
mechanism and its latency per platform remain open (§13 item 5).

Only plain data crosses a worker-thread boundary in practice, not just in intent: before a fetched
batch leaves its owning thread, `db-core` takes any live LOB locator out of it and parks it behind a
core-owned, opaque `LobHandle` scoped to the session that issued it — a handle from one session is
rejected on another rather than mistaken for a stale one (ADR-0002 K1, K8). Dropping a session never
hangs the caller: `Drop` requests a cancel, asks the worker to abandon and release everything, waits
at most a bounded timeout, then detaches the worker thread, which still releases the connection and
its resources whenever the blocked call eventually returns (ADR-0002 K5). Explicit `close` remains the
only path that can commit; `Drop` only abandons.

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

Only crates the current documents name. Layout is provisional until an ADR records it. Phase 0
Workstream A landed the crates below as skeletons first; `db-driver-api`, `db-core` (session/worker
layer) and `drivers/oracle-thin` (the `oracledb` wrapper) are now implemented and independently
reviewed against the Phase 0 test database — see §13 item 10 for the still-open layout question.

```text
crates/db-driver-api/          vendor-neutral driver contract + DbError
crates/db-core/                sessions, transactions, query, results, metadata, workspace
crates/drivers/oracle-thin/    thin driver: wraps Oracle's `oracledb` crate (ADR-0001); vendor code isolated here
crates/drivers/mock/           test-support/mock driver for core tests
crates/ffi/                    reldex-ffi: the stable C ABI (ADR-0003); the only crate allowed `unsafe`
crates/reldex-core-poc/        Phase 0 validation harness (no full UI)
docs/architecture/, docs/decisions/, docs/exec-plans/
```

`crates/ffi` exists, is exercised against the mock driver, and has been independently reviewed
(M1.3), but ADR-0003 is still **Proposed**: spike S15 is the gate, and until it runs the boundary's
shape is a proposal that compiles, not an accepted decision. Its committed header is
`crates/ffi/include/reldex.h`, regenerated by `crates/ffi/gen-header.sh`; it reports ABI version 3
(ADR-0003 amendments A11 and A19), and §7's "thread affinity" is a real obligation on the adapter
rather than a convention — the rules an adapter must keep are stated per function in that header
and summarised at its top.

Being still Proposed is what makes the boundary cheap to correct, and it has been corrected once
already: the first two consumers (the Qt adapter, M1.6, and the C smoke harness, M1.4) found the
batch view building per-column `NUMBER`/`TIMESTAMP` mirrors nobody read — a measured 62.0 bytes per
row retained, about 59 MiB per million rows, roughly 30% of spike S15's 200 MB memory gate spent on
nothing — and found result column names unavailable until the first batch arrived. ADR-0003 amendments A19–A25 record what
changed. The rule the amendment leaves behind is the one to keep: **the boundary allocates only
when a caller asks for something that does not already exist**, and every such call says in the
header what it costs per element.

Note: `docs/exec-plans/active/phase-0.md` Workstream A names the thin driver path as
`drivers/oracle/thin`; the actual crate lives at `crates/drivers/oracle-thin` (flattened, no nested
`oracle/` directory). This layout, including that naming, remains provisional pending the crate
layout ADR (§13 item 10).

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

1. **Thin driver selection — RESOLVED by [ADR-0001](../decisions/0001-database-driver-strategy.md).**
   The primary driver is Oracle's official `oracledb` crate (`oracle/rust-oracledb`; pure Rust, thin,
   blocking), pinned to an exact version and wrapped by `crates/drivers/oracle-thin`. Still open:
   statement cancellation (no public cancel API yet; accepted as a Phase 0 limitation, ADR-0001 "Owner
   decision (2026-09-19)") and beta maturity. TCPS has since passed with limits (spike S8: one-way
   TLS 1.2, PEM-supplied trust; no mTLS+private CA, no Oracle wallet files, no revocation — U-12…U-14).
   Android/iOS viability has cross-compile-and-link evidence (spike S6, `phase-0-s6-mobile-cross-compile.md`)
   but still needs physical-device evidence before any mobile-support claim.
2. **FFI mechanism.** Which Rust/C++ interop approach provides a stable, typed, small boundary, and
   how is ABI/version compatibility guaranteed? Proposed resolution:
   [ADR-0003](../decisions/0003-qt-rust-integration.md) (**Proposed** — acceptance conditional on
   spike S15; not yet accepted).
3. **Threading and callbacks across FFI — core side settled; FFI event delivery still open.**
   [ADR-0002](../decisions/0002-driver-api-and-concurrency-model.md) D1/D2 are now implemented in
   `db-core`, not just decided: one dedicated worker thread per session drains a FIFO command queue,
   and every call returns a `Completion` the caller polls or waits on. That settles the core side —
   each session's calls run on that session's owning worker thread, and only plain `RowBatch` data
   (never a driver handle) crosses to another thread. What remains open for Phase 1 is the FFI half:
   the concrete mechanism that marshals a worker thread's completion to the Qt thread (e.g. a queue
   the C++ adapter drains via a `QEvent`/queued signal, per ADR-0002's "Deferred" section), plus the
   thread-affinity and reentrancy rules the adapter must enforce. Nothing here depends on which
   cancellation mechanism ADR-0001 lands on. Proposed resolution for the FFI half:
   [ADR-0003](../decisions/0003-qt-rust-integration.md) (**Proposed** — acceptance conditional on
   spike S15; not yet accepted).
4. **Async runtime vs. threads — RESOLVED by [ADR-0002](../decisions/0002-driver-api-and-concurrency-model.md) D1.**
   No async runtime below the FFI line; `db-core` uses one dedicated worker thread per session, owning
   `Box<dyn DatabaseConnection>` for that session's lifetime.
5. **Cancellation mechanism — contract RESOLVED by [ADR-0002](../decisions/0002-driver-api-and-concurrency-model.md) D2;
   driver-level mechanism FAILED spike S4 — accepted limitation per owner decision, pending
   upstream.** The contract is a separate `Arc<dyn CancelHandle>`,
   `CancelKind::{Native, PreArmedDeadline, Unsupported}`, and
   `SessionState::{Usable, NeedsValidation, Lost}` reported on every `DbError`; that part is settled
   and implemented. Spike S4 (run 2026-09-19 against a live Oracle 19.3 database; see
   `docs/exec-plans/active/phase-0-spike-results.md` §4) found that `oracledb` 26.0.0-beta.3 offers
   only `CancelKind::PreArmedDeadline` — a deadline armed before a call starts, which stops a SQL
   statement with the session intact but destroys the connection for a PL/SQL block the server will
   not interrupt promptly. No mechanism it evaluated (pre-armed deadline, a privileged
   `ALTER SYSTEM CANCEL SQL`, a minimal fork) delivers an on-demand cancel that keeps the session
   usable in the general case. `SPEC.md` §10/§24.8 "Cancel" is therefore **not met**; ADR-0001's kill
   criterion fired and the ADR re-opened, and on 2026-09-19 the owner decided to accept the limitation
   — stay on `oracledb`, ship the pre-armed deadline with an honest UI, and pursue upstream fixes —
   rather than change drivers (ADR-0001 "Owner decision (2026-09-19)").
6. **Result store representation.** What is the in-memory row/batch layout, bounded-memory policy,
   and spill/eviction behavior? Does Arrow earn its place by benchmark (deferred to Phase 3)?
7. **Error model shape — RESOLVED by [ADR-0002](../decisions/0002-driver-api-and-concurrency-model.md) D3.**
   `DbError{kind, message, native, position, session_state, retryable, source}`; `ErrorKind` is a
   stable, `#[non_exhaustive]`, vendor-neutral category set including `Permission`, which keeps
   permission-dependent metadata/monitoring failures separable from driver failures.
8. **Metadata cache design.** Cache keying and per-database identity isolation, TTL and
   invalidation strategy, and behavior at 100,000+ objects.
9. **Credential storage abstraction.** What single core abstraction spans the four platform secure
   stores, and what is the fallback when none is available?
10. **Crate layout.** Final workspace layout, crate boundaries, and feature flags (provisional in §11).
    Proposed resolution in part (the FFI/UI tier's crate layout — `crates/ffi`, `crates/sql-text`,
    `ui/`): [ADR-0003](../decisions/0003-qt-rust-integration.md) (**Proposed** — acceptance conditional
    on spike S15; not yet accepted).
11. **Type mapping — PARTLY RESOLVED by [ADR-0002](../decisions/0002-driver-api-and-concurrency-model.md) D5.**
    The API-level representation is decided: a lossless, allocation-free 38-digit `Number` (no `f64`
    path), one `Timestamp` type covering DATE/TIMESTAMP/TIMESTAMP WITH TIME ZONE (named IANA regions
    normalized to an offset, a documented limitation), explicit NULL via variant plus a per-column
    validity mask, and lazy `LobStream` for LOBs. Still open: per-type driver-mapping evidence against
    a real database, pending the Workstream D spikes (`phase-0.md`).
12. **Script/statement boundary parsing.** Where the SQL/PL/SQL block-boundary parser lives and how
    it is shared between core and editor (`SPEC.md` §15).
