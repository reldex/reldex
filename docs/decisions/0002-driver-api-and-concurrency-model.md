# 0002 — Driver API and Concurrency Model

**Status:** Accepted (provisional — implemented; independent API review in progress; owner review pending)
**Date:** 2026-09-19

## Context

`ARCHITECTURE.md` §13 leaves items 3 (threading/callbacks across FFI), 4 (async runtime vs. threads),
5 (cancellation mechanism), 7 (error model shape) and 11 (type mapping) open. `phase-0.md`
Workstream A requires the generic driver/session API, a normalized `DbError`, and
connection/session/query/result identifiers. This ADR defines the vendor-neutral contract in
`crates/db-driver-api` and the concurrency model that contract implies.

Binding constraints:

- `SPEC.md` §2 ranks database correctness and session/transaction correctness above UI
  responsiveness, performance and convenience.
- `SPEC.md` §9 — a worksheet owns a *stable, stateful* session; §10 — auto-commit OFF, never a
  silent commit, closing with an active transaction must prompt.
- `SPEC.md` §11/§19 — no database or network I/O on the UI thread.
- `SPEC.md` §12 — cursor/batch results, bounded memory, lazy large values; Arrow internal only.
- `SPEC.md` §24.8/§24.17 — cancel a running statement; keep working while other sessions execute.
- `AGENTS.md` — explicit typed APIs, no driver-native types above the driver, native error codes
  preserved, no credentials in logs, no undocumented production dependencies, small public APIs.
- ADR-0001 — the intended primary driver (`oracledb`) is **pure Rust, synchronous/blocking**
  (`std::net::TcpStream`, no async runtime), holds `Arc<Mutex<Client>>` so one connection serialises
  its own calls, offers `set_call_timeout`, and has **no public cancel API** (gap, spike S4).

This ADR does not add `oracledb` as a dependency. It is the contract only.

## Decision

Recommended (proposal — the owner decides). All eight parts below are one decision; they are
mutually dependent.

### D1 — A blocking, object-safe trait contract; no async runtime in `db-driver-api`

`db-driver-api` exposes plain blocking traits (`DatabaseDriver`, `DatabaseConnection`, `Cursor`,
`CancelHandle`, `LobStream`) and has **zero production dependencies**. Concurrency is owned by
`db-core`: **one dedicated worker thread per session**, which owns the `Box<dyn DatabaseConnection>`
for that session's lifetime and drains a command channel.

Why:

- The primary driver is blocking. An async contract would be a lie implemented with
  `spawn_blocking`: still one OS thread per in-flight call, plus a runtime, plus a second failure
  mode (runtime shutdown) in the most correctness-critical layer.
- `SPEC.md` §9's "worksheet owns a stable stateful session" is *thread affinity by another name*.
  Session state (transaction, `ALTER SESSION`, NLS, package state, temp tables, `DBMS_OUTPUT`) must
  never migrate between connections; a long-lived owning thread expresses that directly, while a
  task pool expresses the opposite by default.
- Sessions are counted in tens (open worksheets), not thousands. Async wins at high connection
  counts Reldex will never have. At N≈20 a 512 KiB-stack worker thread costs less than a
  multi-threaded runtime, which brings its own pool plus roughly a megabyte of binary — a real cost
  on iOS/Android where binary size is scrutinised.
- FFI: futures cannot cross a C ABI. The C++/Qt adapter needs completion *events* marshalled to the
  Qt thread either way. A worker thread that pushes a completion message onto an outbound queue the
  adapter drains (via a `QEvent`/queued signal) is the whole mechanism; with a runtime that
  mechanism still exists, plus runtime lifetime management across the boundary.
- Testability: a blocking mock driver is a few dozen lines. Object safety matters more — `db-core`
  needs `Box<dyn DatabaseDriver>` / `Box<dyn DatabaseConnection>`, and `async fn` in traits is not
  `dyn`-compatible without boxing crates.
- `&mut self` on every statement-issuing method makes "one session's calls are serialised" a
  *compile-time* property rather than a documented hope.

Keeping the runtime question out of `db-driver-api` also means a future async-native driver is
adapted by a blocking shim inside that driver crate, without reopening the contract.

### D2 — Session ownership, threading, and the cancellation contract

- `DatabaseDriver`: `Send + Sync`. One instance may be shared; `connect` takes `&self`.
- `DatabaseConnection`: `Send`, **not** `Sync`. It is moved to its owning worker thread and never
  touched from anywhere else. Every statement-issuing method takes `&mut self`.
- `Cursor`, `LobStream`, `RowBatch`, `Value`, `DbError`: `Send`. Batches are produced on the worker
  thread and moved to the Result Store.
- `CancelHandle`: `Send + Sync`, obtained from the connection as `Arc<dyn CancelHandle>` before the
  call blocks, so it is cloneable and usable from any control path while `execute`/`fetch_batch`
  blocks. `Arc` supplies `Clone`; `Clone` is not object-safe.
- `DatabaseSession` (`SPEC.md` §6) is **not** a driver-contract trait. It is the `db-core` type that
  owns a connection, its worker thread, its cancel handle and its conservative transaction state.
  The driver contract stops at `DatabaseConnection` (`ARCHITECTURE.md` §3).

Cancellation semantics, stated precisely so no layer has to guess:

1. **Best-effort.** `request_cancel()` returning `Ok` means the request was delivered or armed, not
   that anything stopped. It never reports the outcome of the cancelled call.
2. **Idempotent.** Repeated calls, and calls when nothing is running, are a successful no-op.
3. **Outcome travels through the blocked call.** The blocked `execute`/`fetch_batch` returns
   `Err(DbError)` with `kind() == ErrorKind::Cancelled`. If the statement finished first, it returns
   normally and the cancel is discarded; `db-core` must handle "cancel requested, call succeeded".
4. **Session state afterwards is reported, not assumed.** Every `DbError` carries
   `SessionState::{Usable, NeedsValidation, Lost}`. The driver decides which; `db-core` must
   `ping()` before reuse on `NeedsValidation` and surface the loss on `Lost` (`SPEC.md` §18 — never
   silently replace a lost transactional session).
5. **Transactions are not implicitly resolved.** A cancelled statement does not commit or roll back
   the transaction. After a cancel, `transaction_state()` is authoritative and is usually `Unknown`.
6. **A driver without native cancel does not lie.** `Capabilities::cancel` is
   `CancelKind::{Native, CallTimeout, Unsupported}`. With `CallTimeout` (today's `oracledb`,
   ADR-0001 C1), `request_cancel` arms a short call timeout; the driver must then map the resulting
   failure to `ErrorKind::Cancelled` — not `Timeout` — and report `SessionState::NeedsValidation`,
   because the connection may be mid-protocol. With `Unsupported`, `request_cancel` returns
   `ErrorKind::Unsupported` immediately so the UI can disable Cancel instead of pretending.

### D3 — Error model

`DbError { kind, message, native, position, session_state, retryable, source }`.

- `ErrorKind` is a stable, `#[non_exhaustive]`, vendor-neutral category: `Configuration`,
  `Connection`, `Authentication`, `NetworkLost`, `Timeout`, `Cancelled`, `Syntax`, `Constraint`,
  `Permission`, `Transaction`, `Resource`, `DataConversion`, `Unsupported`, `DriverInternal`,
  `Other`. `Other` exists so an unmapped native code is never forced into a wrong category;
  `Permission` keeps permission-dependent metadata/monitoring failures separable from driver
  failures (`ARCHITECTURE.md` §4).
- `NativeError { code, message }` preserves the vendor code (`ORA-00942` → `942`) and the vendor
  text verbatim (invariant 9).
- `SqlPosition { byte_offset, line, column }` is optional and is what later maps compile/parse
  errors to editor positions (`TASKS.md` P2).
- `source` chains through `std::error::Error`.
- `session_state` and `retryable` are flags, not inferences. `retryable` means "transient, retrying
  is not obviously harmful"; policy stays in `db-core`, which must never auto-retry inside an open
  transaction.
- Credentials never appear. `DbError` has no parameter fields, and `Secret`/`ConnectionParams`
  redact in `Debug` (tested).

### D4 — Transaction semantics

Auto-commit is OFF and there is **no toggle** in this contract. A driver that cannot open a
connection with auto-commit disabled must fail `connect` with `ErrorKind::Unsupported`. Explicit
`commit`, `rollback`, `savepoint`, `rollback_to_savepoint`; `SavepointName` is a validated identifier
newtype so the driver never interpolates arbitrary text into `SAVEPOINT …`.

`transaction_state()` returns `Inactive | Active | Unknown` and is honest about driver limits: a
driver that cannot observe server-side transaction state returns `Unknown` after any statement that
could have opened one, and `Inactive` only immediately after a successful commit or rollback.
`Capabilities::exact_transaction_state` says which kind of driver this is. `db-core` must treat
`Unknown` as *may be open* and prompt on worksheet close (`SPEC.md` §10) — over-prompting is
acceptable, a silent commit or a hidden rollback is not.

### D5 — Value and type model

- `Value` (owned; binds and OUT values) and `ValueRef<'_>` (borrowed; batch cells) with
  `Null`, `Boolean`, `Number`, `Double`, `Float`, `Text`, `Bytes`, `Timestamp`, `Json`, `Lob`
  (`Value` additionally has `Cursor`). NULL is an explicit variant plus a per-column validity mask,
  never a sentinel.
- **NUMBER is lossless and allocation-free.** `Number` stores sign, up to 38 significant decimal
  digits one per byte, and a decimal exponent (`value = ±0.d₁…dₙ × 10^exponent`,
  `exponent ∈ [-129, 126]`), which is exactly Oracle NUMBER's domain. It is `Copy` and 42 bytes (a
  `Value` is 48), so a numeric cell costs no heap allocation. Parsing rejects >38 digits and out-of-range
  exponents rather than rounding silently, and `Display`/`FromStr` round-trip. `to_f64_lossy()` is
  named for what it is; nothing converts through `f64` implicitly. No decimal crate is added
  (`AGENTS.md` dependency rule): the required semantics are a fixed-width digit buffer plus an
  exponent, and arbitrary-precision arithmetic is not needed in a transport contract.
- **Date/time without a dependency.** One `Timestamp` type (civil fields + nanoseconds +
  `TimeZone::{Unspecified, Offset}`) covers DATE, TIMESTAMP and TIMESTAMP WITH TIME ZONE; the
  declared `SqlType` distinguishes them. Constructors validate ranges including leap years. Known
  gap: named IANA regions are normalised to an offset — recorded as an open item rather than
  pulling `chrono`/`time` into the contract.
- **LOBs are never materialised.** `Value::Lob(LobLocator)` wraps a driver-owned
  `Box<dyn LobStream>` read in caller-sized chunks, so memory is bounded by the caller's buffer
  (`SPEC.md` §12, §21).
- **REF CURSOR is a nested cursor** (`Value::Cursor(Box<dyn Cursor>)`) delivered through OUT binds
  and implicit results. Cursor-typed *result columns* are out of scope for now.
- JSON is carried as UTF-8 JSON text (`Value::Json`), leaving room for a binary form later without
  a parser in this crate.

### D6 — Execution and results

One entry point: `execute(&mut self, &Statement) -> DbResult<ExecutionOutcome>`, because a worksheet
cannot know whether arbitrary user text produces rows, and `db-core` must not parse SQL to find out.
`ExecutionOutcome { cursor, rows_affected, out_values, implicit_results, warnings }`; `Warning`
carries PL/SQL "compiled with errors" with its own `SqlPosition`. Binds are positional or named,
each `Bind::In | Out(OutBindSpec) | InOut`, with a declared `SqlType` and size for OUT.

**Batches are column-oriented** (`RowBatch` → `Column { NullMask, ColumnData }`,
`fetch_batch(max_rows: NonZeroUsize)`; an empty batch means exhausted). Justification against
`SPEC.md` §12/§13: a column has one type, so the type discriminant is stored once per column instead
of once per cell; nulls are a bitmask instead of a per-cell `Option`; text and bytes columns use one
contiguous buffer plus offsets, so a 1000-row `VARCHAR2` batch is two allocations rather than a
thousand; and per-column formatting, sorting and export all read a contiguous run. Random access by
`(row, column)` remains O(1). It is also the layout an Arrow experiment would slot into — but Arrow
stays an internal, benchmark-gated option (`SPEC.md` §12, `ROADMAP` Phase 3) and no Arrow type
appears in this crate.

### D7 — Identifiers, capabilities, parameters

`ConnectionId`, `SessionId`, `StatementId`, `ResultSetId` are `u64` newtypes with a process-local
allocator, so nothing is passed across layers as a bare integer. `Capabilities` is a plain flag
struct (`cancel`, `savepoints`, `named_binds`, `out_binds`, `ref_cursor`, `implicit_results`,
`lob_streaming`, `tls`, `exact_transaction_state`, `error_position`) whose `Default` supports
nothing — a driver must opt in, so a missing feature cannot be advertised by omission.
`ConnectionParams { endpoint, credentials, role, tls, connect_timeout, extensions }` is
vendor-neutral; anything vendor-specific goes in `Extensions`, a keyed bag that is **opaque to
`db-core`** and passed through to the driver. `Secret` wraps a password, redacts in `Debug`,
implements no `Display`, and zeroes its buffer on drop without `unsafe`.

### D8 — Explicitly out of scope

Connection pooling, `MetadataProvider`, script/statement-boundary parsing, the FFI surface, the
auto-commit toggle, array/batch DML, scrollable cursors, object types and collections, cursor-typed
result columns, named time-zone regions, server-version introspection, and Arrow. Each is either
another ADR's subject (`ARCHITECTURE.md` §13 items 2, 6, 8, 12) or unproven in Phase 0. "Keep public
APIs small until the architecture stabilizes" (`AGENTS.md`) applies literally here: every trait
method is a compatibility promise to `db-core`, the mock driver, the oracle-thin driver and later
the FFI, and Phase 0 exists to find out which promises we can keep.

## Lead decisions (2026-09-19)

During implementation the implementer raised five open questions. The lead decided each below under
the owner's delegation; all five are open to owner review.

1. **No auto-commit toggle in the V1 contract (D4).** Accepted.
2. **`TIMESTAMP WITH TIME ZONE` named regions (D5).** Accepted as a documented limitation — normalized
   to a UTC offset for now — on condition the type stays extensible so a named region can be added
   later without a breaking change.
3. **Cursor-typed result columns, i.e. nested `CURSOR(...)` in a select list (D5, D8).** Accepted as
   out of scope for V1, provided a driver reports `ErrorKind::Unsupported` rather than silently
   dropping the column.
4. **`Secret` zeroing stays best-effort, no `zeroize` dependency for now (D7).** Accepted; revisit in
   the credential-storage ADR (`ARCHITECTURE.md` §13 item 9).
5. **`SavepointName` restricted to a portable simple identifier of at most 30 ASCII characters (D4).**
   Accepted.

## Consequences

**Easier.** The contract matches the primary driver's real shape, so the oracle-thin wrapper is
adaptation, not emulation. No async runtime anywhere below the FFI line: smaller mobile binaries, no
runtime lifetime to manage across the C ABI, simpler stack traces. Serialisation per session is
enforced by `&mut self`. The mock driver is trivial, so `db-core` is testable without a database.
Cancellation honesty is structural — capability flag plus `Cancelled` plus reported session state —
so the UI can degrade instead of lying.

**Harder.** `db-core` must implement its own worker-thread/channel plumbing and its own conservative
transaction tracking; neither is free. A blocking `fetch_batch` means back-pressure and progress
reporting are core's problem. Column-oriented batches are more code in every driver than a
`Vec<Vec<Value>>` would be. `Number` is 42 bytes, so `Value` is 48 rather than the 16 an `f64`-based
model would need — accepted deliberately, since silent NUMBER precision loss is a `SPEC.md` §2
correctness failure, and the Result Store may re-encode more compactly later.

**Follow-up.** `db-core` session/worker implementation and the mock driver (next task); the
oracle-thin wrapper including the `CallTimeout` cancel fallback; contract tests every driver must
pass; benchmarks for batch size and allocation counts before the Result Store design is fixed.

## Alternatives considered

- **Async traits + tokio.** Rejected for D1's reasons: the driver is blocking, so async buys
  nothing but `spawn_blocking` indirection; `async fn` in traits is not `dyn`-compatible, and
  `db-core` needs trait objects; futures do not cross the FFI boundary; and the runtime is pure cost
  at Reldex's session counts. Reconsider only against measurements, not taste.
- **Enum-based message protocol instead of traits** (`Command`/`Response` channels as the driver
  contract). Attractive because it is already the shape of the worker-thread plumbing and is
  trivially `Send`. Rejected: it pushes every driver into one `match`, makes adding a capability a
  breaking change to a shared enum, loses compile-time checking of which operations a driver
  supports, and is the "stringly/tagged cross-layer protocol" `AGENTS.md` warns against. The message
  protocol still exists — inside `db-core`, between the session and its worker — where it belongs.
- **Exposing Arrow record batches at the driver boundary.** Rejected now: it would make a heavy
  dependency part of the contract, contradicts `SPEC.md` §12 ("UI APIs must not depend directly on
  Arrow") by making it the only currency below the UI, and would prejudge the Result Store before
  any benchmark exists. The chosen column layout keeps the option open.
- **Row-oriented batches** (`Vec<Row>` or `Vec<Vec<Value>>`). Simpler for drivers and for export,
  but pays a type tag and usually an allocation per cell, which is exactly what `AGENTS.md`
  forbids on result paths.
- **`DatabaseSession` as a driver trait.** Rejected: it would duplicate `DatabaseConnection` and
  invite drivers to implement session policy (transaction tracking, reconnect) that `SPEC.md` §9/§18
  assign to the core.

## Evidence that would reverse this decision

- Phase 0 measures per-session worker threads as unaffordable on Android/iOS at realistic worksheet
  counts (memory, battery, or background-execution limits) — then reconsider a shared runtime.
- The primary driver ships a native async API and deprecates the blocking one, or upstream
  cancellation lands only as an async-reactor construct (ADR-0001 spike S4).
- Qt integration turns out to require an event loop *inside* Rust rather than message passing to the
  Qt thread (`ARCHITECTURE.md` §13 item 3) — that would change the completion path, not necessarily
  the trait shape.
- Benchmarks show column-oriented batches lose to row-oriented ones on the real fetch path, or that
  a 42-byte `Number` dominates fetch memory — then re-encode `Number` (packed BCD halves the size)
  or box it, without changing the exponent/digit semantics.
- A driver appears that cannot express `transaction_state()` at all, or that requires `Sync`
  connections — both would force a wider contract.

## Evidence/benchmarks

None yet; this ADR is design work ahead of Phase 0 measurement, and it deliberately records the
measurements that would overturn it. Its factual base is ADR-0001's source review of
`oracle/rust-oracledb` (blocking `std::net::TcpStream`, `Arc<Mutex<Client>>`, `set_call_timeout`, no
public cancel API) checked on 2026-09-19. The batch-shape and `Number`-size claims above are
reasoned, not measured, and must be benchmarked before the Result Store design is fixed
(`ARCHITECTURE.md` §13 item 6).
