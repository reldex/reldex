# 0002 — Driver API and Concurrency Model

**Status:** Accepted (provisional — implemented; independent API review completed 2026-09-19 and its
must-fix findings applied, see "Amendments after API review"; amended again after the ADR-0001
Phase 0 spikes, see "Amendments after the Phase 0 spikes"; owner review pending)
**Date:** 2026-09-19
**Amended:** 2026-09-19 (API review), 2026-09-19 (Phase 0 spikes)

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
- `&mut self` on every statement-issuing method serialises calls *through the connection value*,
  which is a useful property but a narrower one than this ADR originally claimed — see amendment M3
  and "What `&mut self` does not prove" below.

**What `&mut self` does not prove** (corrected 2026-09-19). The first version of this ADR asserted
that `&mut self` makes "one session's calls are serialised" a *compile-time* property. It does not,
and the crate no longer says so. A connection hands out independent `Send` handles that borrow
nothing from it — `Box<dyn Cursor>`, `LobLocator`/`Box<dyn LobStream>`, and a nested cursor inside a
`Value` — each of which issues its own protocol traffic. Two cursors on one connection can exist
simultaneously (confirmed in the primary driver, which gives each a cloned `Arc<Mutex<Client>>` with
no lifetime tie, and whose `Connection` methods take `&self`). The borrow checker has nothing to say
about which thread they are used on.

The real invariant is a runtime one, and `db-core` enforces it:

> Every object derived from a connection is used **only on that connection's owning worker thread**.
> Of everything a fetch produces, only `RowBatch` — plain data, no handles — crosses a thread
> boundary.

`Cursor::connection_id()` and `LobStream::connection_id()` exist so this can be asserted rather than
assumed. The threading model is unchanged; only the claim about how it is enforced was wrong.

Keeping the runtime question out of `db-driver-api` also means a future async-native driver is
adapted by a blocking shim inside that driver crate, without reopening the contract.

### D2 — Session ownership, threading, and the cancellation contract

- `DatabaseDriver`: `Send + Sync`. One instance may be shared; `connect` takes `&self`.
- `DatabaseConnection`: `Send`, **not** `Sync`. It is moved to its owning worker thread and never
  touched from anywhere else. Every statement-issuing method takes `&mut self`.
- `Cursor`, `LobStream`, `RowBatch`, `Value`, `DbError`: `Send`. Batches are produced on the worker
  thread and moved to the Result Store. `Cursor` and `LobStream` are `Send` for construction and
  ownership transfer, **not** as permission to use them off the owning worker thread — see D1's
  "What `&mut self` does not prove". Both report `connection_id()` so `db-core` can assert it.
- `CancelHandle`: `Send + Sync`, obtained from the connection as `Arc<dyn CancelHandle>` before the
  call blocks, so it is cloneable and usable from any control path while `execute`/`fetch_batch`
  blocks. `Arc` supplies `Clone`; `Clone` is not object-safe.
- `DatabaseSession` (`SPEC.md` §6) is **not** a driver-contract trait. It is the `db-core` type that
  owns a connection, its worker thread, its cancel handle and its conservative transaction state.
  The driver contract stops at `DatabaseConnection` (`ARCHITECTURE.md` §3).

**Lifecycle of derived handles** (added 2026-09-19, amendment M4). A cursor or LOB stream never
outlives the usefulness of its connection, and every boundary case is a *report*, never a panic and
never a block:

- After **any error** from `Cursor::fetch_batch`, the only legal call on that cursor is `close()`.
  After any error from `LobStream::read_chunk`, the only legal action is to drop it. Drivers enforce
  this by returning a `DbError` on a further call, not by trusting the caller.
- After `DatabaseConnection::close`, every outstanding cursor and LOB stream returns
  `DbError::connection_closed(...)` — `ErrorKind::DriverInternal`, `SessionState::Lost` — from every
  operation. `close` consumes the connection, so `db-core` is expected to have dropped them first; a
  driver must survive it not having done so.
- `commit`, `rollback` and `rollback_to_savepoint` **may invalidate** open cursors and LOB locators.
  Most servers scope a LOB locator to its transaction, and `ROLLBACK` commonly closes cursors. The
  driver must report that as an ordinary `DbError` (`ErrorKind::Transaction` when the server says so)
  rather than returning a short result that looks complete. `db-core` therefore treats an open cursor
  or locator as transaction-scoped and must not promise the user otherwise.

Cancellation semantics, stated precisely so no layer has to guess:

1. **`request_cancel` must never block on the connection's own call lock, and must return
   promptly.** A "cancel" that waits for the statement it is cancelling is worse than no cancel: it
   freezes the control path that was meant to stay responsive, and `SPEC.md` §24.17 requires the rest
   of the application to keep working. A driver that cannot signal without taking the lock the
   running call holds must report `PreArmedDeadline` or `Unsupported` and return immediately.
2. **Best-effort.** `CancelOutcome::Requested` means the request was delivered or armed, not that
   anything stopped. It never reports the outcome of the cancelled call.
3. **Honest about impotence.** `request_cancel` returns `DbResult<CancelOutcome>`, not `DbResult<()>`
   — "I asked and nothing can come of it" is a value the UI can render, not an `Ok` that reads as
   success.
4. **Idempotent.** Repeated calls, and calls when nothing is running, are a successful no-op.
5. **Outcome travels through the blocked call.** The blocked `execute`/`fetch_batch` returns
   `Err(DbError)` with `kind() == ErrorKind::Cancelled`. If the statement finished first, it returns
   normally and the cancel is discarded; `db-core` must handle "cancel requested, call succeeded".
6. **Session state afterwards is reported, not assumed.** Every `DbError` carries
   `SessionState::{Usable, NeedsValidation, Lost}`, whose *initial* value is now derived from the
   `ErrorKind` (amendment M2) and which the driver may override. `db-core` must `ping()` before reuse
   on `NeedsValidation` and surface the loss on `Lost` (`SPEC.md` §18 — never silently replace a lost
   transactional session).
7. **Transactions are not implicitly resolved.** A cancelled statement does not commit or roll back
   the transaction. After a cancel, `transaction_state()` is authoritative and is usually `Unknown`.
8. **A driver that cannot cancel does not lie.** `Capabilities::cancel` is
   `CancelKind::{Native, PreArmedDeadline, Unsupported}`:
   - `Native` — interrupts a running statement over a separate control path. `request_cancel`
     returns `CancelOutcome::Requested` promptly and the blocked call fails with
     `ErrorKind::Cancelled`. **This is the only class that meets `SPEC.md` §24.8.**
   - `PreArmedDeadline` — can only enforce a deadline armed *before* the call started, via
     `Statement::with_deadline`. `request_cancel` returns
     `CancelOutcome::NotInterruptible { deadline_remaining }` — it does not block, does not send
     anything, and does not pretend. When the deadline fires the call fails with `ErrorKind::Timeout`,
     because that is what happened; relabelling it `Cancelled` to make the UI look better is
     forbidden. This is the primary driver's verified class today (ADR-0001 C1) and it **does not**
     satisfy `SPEC.md` §10/§24.8.
   - `Unsupported` — `request_cancel` returns `ErrorKind::Unsupported` immediately so the UI can
     disable Cancel instead of pretending.

  `CancelKind::interrupts_running_call()` and `Capabilities::can_interrupt_running_call()` are the
  predicates the UI asks *before* offering a Cancel button. This replaces the earlier
  `Capabilities::supports_cancel()`, which answered a question nobody should ask: "is `cancel`
  something other than `Unsupported`" lumped `PreArmedDeadline` in with `Native` and would have let
  the UI offer a Cancel that cannot work.

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
- `SqlPosition { char_offset, line, column }` is optional and is what later maps compile/parse errors
  to editor positions (`TASKS.md` P2). The offset unit is **characters**, not bytes, because that is
  what servers report; `SqlPosition::byte_offset_in(sql)` converts once against the exact submitted
  text. `SPEC.md` §14 makes this concrete rather than theoretical — every non-ASCII character in a
  Thai statement makes the two units diverge (amendment S3).
- `source` chains through `std::error::Error`.
- `retryable` is a flag, not an inference: it means "transient, retrying is not obviously harmful";
  policy stays in `db-core`, which must never auto-retry inside an open transaction.
- `session_state` is reported by the driver, but its **initial value is derived from `kind`** rather
  than defaulting to `Usable` (amendment M2): `NetworkLost` → `Lost`;
  `Connection`/`Timeout`/`Cancelled`/`DriverInternal` → `NeedsValidation`; everything else → `Usable`.
  `SessionState::initial_for(kind)` is public so the rule is one place. A driver still overrides with
  `with_session_state`, which is now the only way to *narrow* the claim — and narrowing is a
  deliberate act. The previous `Usable` default meant a driver that forgot one builder call silently
  told the core that a dead session was fine, which is exactly the failure `SPEC.md` §18 forbids.
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
acceptable, a silent commit or a hidden rollback is not. `TransactionState::default()` is therefore
`Unknown`, not `Inactive` (amendment S2): the default must be the safe answer, since a
default-constructed value is by definition one nobody has classified.

**DDL commits regardless of auto-commit, and the contract says so** (added 2026-09-19, amendment S9).
Most SQL servers commit the open transaction before and after a DDL statement whatever the client
asked for. This ADR cannot forbid it and must not hide it: `SPEC.md` §10's "never silently commit" is
a rule about *Reldex's* behaviour, and the honest way to keep it in the face of a server-side commit
is to tell the user it happened. `ExecutionOutcome::statement_kind` therefore carries a
`StatementKind` — `Query | Dml | Ddl | PlSqlBlock | TransactionControl | SessionControl | Other`,
`#[non_exhaustive]`, default `Other` — reported by the driver, with
`ExecutionOutcome::committed_implicitly()` as the predicate `db-core` acts on. The driver classifies,
not the core, because D6 forbids the core parsing SQL. This costs the wrapper nothing it was not
already paying: the primary driver's `Connection::execute` rejects queries outright and its own
statement parser is private, so the wrapper has to classify statements to route them at all.

### D5 — Value and type model

- **Three shapes, not two** (amendment S10). `BindValue` (owned, plain data, **`Clone`**) is what a
  bind carries *in*; `Value` (owned, may hold a live `LobLocator` or `Box<dyn Cursor>`, not `Clone`)
  is what a driver hands *out* through OUT binds; `ValueRef<'_>` borrows a batch cell. The original
  single `Value` made every bound statement single-use, because `Value` cannot be `Clone` without
  lying about a live handle — and re-executing a statement is the normal case, not an exotic one.
  Splitting also *deletes* a rule the contract previously stated in prose and could not enforce ("a
  driver must reject `Value::Lob`/`Value::Cursor` used as an IN bind"): there is now no way to write
  one. Sizes measured and asserted in tests: `Number` 44 B, `Value` 48 B, `BindValue` 48 B,
  `ValueRef` 24 B.
- NULL is an explicit variant plus a per-column validity mask, never a sentinel. Two further
  *absences* are distinct from it and from each other (amendments M1, M6): `ValueRef::Unsupported`
  (a type the contract cannot represent, as driver-rendered text) and `ValueRef::Taken` (a LOB
  locator moved out of the batch). Collapsing either into NULL would make an exporter write an empty
  cell where the database holds a value.
- **NUMBER is lossless and allocation-free.** `Number` stores sign, up to **40** significant decimal
  digits one per byte, and a decimal exponent (`value = ±0.d₁…dₙ × 10^exponent`,
  `exponent ∈ [-129, 126]`). It is `Copy` and 44 bytes (a `Value` is 48 either way — it was already
  padded, so widening the mantissa cost nothing at the `Value` level), so a numeric cell costs no heap
  allocation. 40 rather than 38 (amendment S6): 38 is Oracle NUMBER's *declarable* precision, but the
  stored form is 20 base-100 mantissa bytes, which carries up to 40 decimal digits, and computed
  values — division in particular — reach it. The primary driver uses a 40-digit buffer for the same
  reason. Matching the storage rather than the documentation means a value the server sends can never
  fail with `TooManyDigits`. The exponent range is unchanged and correct: `NUMBER` spans `1E-130` to
  `9.99…E125`, which normalises to `exponent ∈ [-129, 126]`.
  Parsing rejects >40 digits and out-of-range exponents rather than rounding silently, accepts every
  literal shape a driver actually emits (`.5`, `-.5`, `1E+2`, `1.5E-130`), and `Display`/`FromStr`
  round-trip. `Display` writes **one canonical form — plain positional decimal, never scientific**
  (amendment S6): the previous `PLAIN_ZERO_LIMIT` threshold was a presentation policy, and choosing
  when a number "becomes" `1.23E+100` depends on locale and column width, so it belongs to the UI
  (`SPEC.md` §13), not to a transport contract. `to_f64_lossy()` is named for what it is; nothing
  converts through `f64` implicitly, and `from_f64_lossy` is gone (nothing on the fetch path has an
  `f64` to start from). No decimal crate is added (`AGENTS.md` dependency rule): the required
  semantics are a fixed-width digit buffer plus an exponent, and arbitrary-precision arithmetic is
  not needed in a transport contract.
- **Date/time without a dependency.** One `Timestamp` type (civil fields + nanoseconds +
  `TimeZone::{Unspecified, Offset}`) covers DATE, TIMESTAMP and TIMESTAMP WITH TIME ZONE; the
  declared `SqlType` distinguishes them. Validation follows **the calendar of the source database**,
  which is the historical mixed calendar, not proleptic Gregorian (amendment M7): the Julian leap rule
  before 1582, no year zero, and the ten days the Gregorian reform skipped. Two constructors, because
  the two directions want different strictness: `Timestamp::new`/`date` are **strict** (values Reldex
  or a user invents) and reject the reform gap; `Timestamp::from_source` is the **decode** path and
  accepts it, because failing an entire fetch of a large table over one historical row serves nobody.
  Range checks still apply on both — a month of 13 is a decoder bug either way. Known gap: named IANA
  regions are normalised to an offset — recorded as an open item rather than pulling `chrono`/`time`
  into the contract; `TimeZone` is `#[non_exhaustive]` so a `Region` case can be added later.
- **LOBs are never materialised.** `Value::Lob(LobLocator)` wraps a driver-owned
  `Box<dyn LobStream>` read in caller-sized chunks, so memory is bounded by the caller's buffer
  (`SPEC.md` §12, §21).
- **REF CURSOR is a nested cursor** (`Value::Cursor(Box<dyn Cursor>)`) delivered through OUT binds,
  and it is **owned out** of the outcome rather than borrowed — see amendment P1. Cursor-typed
  *result columns* are out of scope for now; implicit result sets are cut (see D8).
- JSON is carried as UTF-8 JSON text (`Value::Json`), leaving room for a binary form later without
  a parser in this crate.
- **An unrepresentable column does not kill the fetch** (amendment M1). `SqlType::Unsupported` used
  to mean "report this type, then fail the fetch if anyone reads the value", which made `SELECT *`
  over a table with an `INTERVAL`, `ROWID`, `TIMESTAMP WITH LOCAL TIME ZONE`, `XMLType` or `VECTOR`
  column a hard error — one unsupported column hiding an entire table. The driver now delivers
  `ColumnData::Unsupported(TextColumn)`, a best-effort text rendering, read back as
  `ValueRef::Unsupported(&str)` — never as `Text`, so nothing downstream mistakes it for character
  data — with the server's own type name in `ColumnMetadata::native_type_name`. Display and export
  only: it must not be parsed back into a typed value and such a cell must not be offered for editing.
- `ColumnMetadata.scale` carries **fractional-seconds precision** for `Timestamp` and
  `TimestampWithTimeZone` (`TIMESTAMP(6)` is `scale == 6`), documented on both the setter and the
  getter rather than given a second field, because that is how servers report it (amendment S5).

### D6 — Execution and results

One entry point: `execute(&mut self, &Statement) -> DbResult<ExecutionOutcome>`, because a worksheet
cannot know whether arbitrary user text produces rows, and `db-core` must not parse SQL to find out.
`ExecutionOutcome { cursor, rows_affected, statement_kind, out_values, warnings }`; `Warning`
carries PL/SQL "compiled with errors" with its own `SqlPosition`. Binds are positional or named,
each `Bind::In(BindValue) | Out(OutBindSpec) | InOut`, with a declared `SqlType` and size for OUT.
`Statement` is `Clone`, so a bound call can be executed again. The cursor and any driver-owned
output value are **taken** out of the outcome (`take_cursor`, `take_out_values`,
`OutValues::take_named`/`take_positional`), never borrowed; see amendment P1.

**`Statement` carries the two options a driver needs before it executes** (amendments M5, S4):

- `with_deadline(Duration)` — the per-call deadline. On a `PreArmedDeadline` driver this is the only
  moment a limit can be established at all, which is what makes such a statement stoppable; on any
  driver it is the caller's own bound. Firing reports `ErrorKind::Timeout`, because that is what
  happened.
- `with_fetch_rows(NonZeroUsize)` — how many rows per round trip. This cannot be deferred to
  `fetch_batch`, because drivers size their array fetch and read-ahead when the statement executes
  (`fetch_array_size`/`prefetch_rows` in the primary driver) — by the time rows are first asked for,
  the first round trip has already happened with whatever default the driver chose. `fetch_batch`
  still bounds each individual batch; this bounds what the wire does underneath.

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

`ConnectionId` and `ResultSetId` are `u64` newtypes with a process-local allocator, so nothing is
passed across layers as a bare integer. Only those two: `SessionId` and `StatementId` were cut
(amendment C1) because neither appeared in any signature below the driver boundary — a session id
belongs to `db-core`, which owns sessions, and a statement id was a concept nothing used.

`Capabilities` reports `cancel`, `savepoints`, `named_binds`, `out_binds`, `ref_cursor`,
`lob_streaming`, `tls`, `exact_transaction_state` and `error_position`. Its `Default` supports
nothing — a driver must opt in, so a missing feature cannot be advertised by omission. Fields are
**private, set through a builder** (amendment S1), so adding a capability is not a breaking change to
every driver that constructs it.

`ConnectionParams { endpoint, credentials, role, tls, connect_timeout, extensions }` is
vendor-neutral; anything vendor-specific goes in `Extensions`, a keyed bag that is **opaque to
`db-core`** and passed through to the driver.

`Secret` wraps a password, redacts in `Debug` and implements no `Display` — the property `AGENTS.md`
actually requires, and the one that is tested. It also overwrites its buffer on `Drop` without
`unsafe`, but the documentation no longer calls that a guarantee (amendment S7): `Vec::fill(0)` before
a free can be elided, the `String` the caller passed in was already a separate copy, `Clone` makes
more, and the allocator, page cache and swap file are outside this crate's reach. Claiming memory
hygiene the code does not deliver is exactly the kind of overclaim `SPEC.md` §2 rules out. Adopting
`zeroize` stays deferred to the credential-storage ADR (`ARCHITECTURE.md` §13 item 9, lead decision 4
below), where secure storage at rest (`SPEC.md` §17) is decided as a whole.

### D8 — Explicitly out of scope

Connection pooling, `MetadataProvider`, script/statement-boundary parsing, the FFI surface, the
auto-commit toggle, array/batch DML, scrollable cursors, **implicit result sets**, object types and
collections, cursor-typed result columns, named time-zone regions, server-version introspection, and
Arrow. Each is either another ADR's subject (`ARCHITECTURE.md` §13 items 2, 6, 8, 12) or unproven in
Phase 0. "Keep public APIs small until the architecture stabilizes" (`AGENTS.md`) applies literally
here: every trait method is a compatibility promise to `db-core`, the mock driver, the oracle-thin
driver and later the FFI, and Phase 0 exists to find out which promises we can keep.

Two gaps are worth naming explicitly, because they are real and deliberately deferred rather than
overlooked:

- **DML `RETURNING INTO` with array results.** A single `UPDATE … RETURNING id INTO :ids` yields one
  value per affected row, which the current `OutValues` shape (one `Value` per bind) cannot express.
- **Array binds / batch DML.** Executing one statement against N sets of bind values is the standard
  way to make bulk insert fast, and `Binds` has no shape for it.

Both need a wider `Bind`/`OutValues` shape; neither is needed for Phase 0 feasibility, and adding a
shape before the driver has proven the simple case would be guessing. `Binds`, `Bind` and `OutValues`
are `#[non_exhaustive]`, so either can arrive without a breaking change.

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

## Amendments after API review (2026-09-19)

An independent senior review of the implemented contract returned "ship after must-fix". The lead
accepted every finding. This section records what changed and why; the body above has been corrected
so the document and the code agree. Numbering: `M` = must-fix, `S` = should-fix, `C` = cut.

### Must-fix

**M1 — An unsupported column type no longer kills the fetch.** `types.rs` told drivers to report
`SqlType::Unsupported`, but `ColumnData`/`ValueRef` had no per-cell representation for one, so
`SELECT *` over a table containing an `INTERVAL`, `ROWID`, `TIMESTAMP WITH LOCAL TIME ZONE`,
`XMLType` or `VECTOR` column was a hard error — one column the contract cannot type hiding an entire
table from the user. Added `ColumnData::Unsupported(TextColumn)`, `ColumnKind::Unsupported` and
`ValueRef::Unsupported(&str)` carrying the driver's best-effort rendering, with the real type name
still in `native_type_name`. Deliberately a *separate* variant from `Text`, so nothing downstream can
treat the rendering as character data, edit it, or parse it back.

**M2 — `DbError::new` no longer defaults `session_state` to `Usable`.** The initial value is derived
from `kind` (`SessionState::initial_for`): `NetworkLost` → `Lost`;
`Connection`/`Timeout`/`Cancelled`/`DriverInternal` → `NeedsValidation`; others `Usable`. A driver
that forgot one builder call previously told the core that a dead session was fine — silently
defeating `SPEC.md` §18. `with_session_state` still overrides, and is now the only way to *narrow*
the claim.

**M3 — The false "`&mut self` makes serialisation a compile-time property" claim is deleted.** See
D1. `Box<dyn Cursor>`, LOB locators/streams and cursor values are independent `Send` handles that
borrow nothing from the connection; two cursors on one connection can coexist. The real invariant —
derived objects are used only on the owning connection's worker thread, and only `RowBatch` crosses
threads — is now stated on `Cursor`, `LobStream`, `LobLocator` and the `session` module, and
`Cursor::connection_id()` / `LobStream::connection_id()` let `db-core` assert it instead of hoping.

**M4 — The lifecycle of cursors and locators is defined.** Across errors, `commit`/`rollback` and
`DatabaseConnection::close`; see D2 "Lifecycle of derived handles". The rule everywhere is *report,
never panic, never block*, and `DbError::connection_closed(handle)` is the one canonical shape for
"used after its connection was closed" (`DriverInternal` + `SessionState::Lost`).

**M5 — The cancel contract is re-scoped to what is actually achievable.** The reviewer verified in
`oracledb` source that `Connection::set_call_timeout` locks the same `Arc<Mutex<Client>>` that
`execute` holds for the whole round trip, so an on-demand `request_cancel()` implemented via
call-timeout would block until the statement finished — the opposite of cancelling. Therefore:

- **Explicit contract requirement:** `request_cancel` must never block on the connection's own call
  lock and must return promptly (D2 rule 1).
- `CancelKind::CallTimeout` is **renamed and redefined** as `CancelKind::PreArmedDeadline`: a
  per-call deadline the caller arms *before* executing (`Statement::with_deadline`). Such a driver
  cannot interrupt a running call and must say so. The old name and the old description — "arms a
  short call timeout, maps the failure to `Cancelled`" — described something the driver cannot do.
- `request_cancel` returns `DbResult<CancelOutcome>` rather than `DbResult<()>`.
  `CancelOutcome::NotInterruptible { deadline_remaining }` is the honest answer, and it is a value
  the UI can render ("this statement cannot be interrupted; it will stop by 14:32:07") rather than an
  `Ok` that reads as success. `CancelKind::interrupts_running_call()` and
  `Capabilities::can_interrupt_running_call()` let the UI decide what to offer *before* anything is
  running. `Capabilities::supports_cancel()` is gone: it lumped `PreArmedDeadline` in with `Native`
  and would have licensed exactly the lie `SPEC.md` forbids.
- A deadline that fires reports `ErrorKind::Timeout`, not `Cancelled`. Relabelling it to make the UI
  look better is forbidden.
- **A `PreArmedDeadline` driver does not satisfy `SPEC.md` §10/§24.8.** ADR-0001 C1 and spike S4 are
  updated accordingly, including the kill criterion.

**M6 — `Column::take_lob` no longer fakes a NULL.** It used to call `NullMask::set_null` on the row
it emptied, making a consumed LOB indistinguishable from SQL NULL — so an exporter re-reading the
batch would write an empty cell where the database holds a value. The mask is now untouched and the
cell reads back as `ValueRef::Taken` (`Column::is_taken(row)`), a state distinct from `Null`. Taking
from a genuinely NULL row returns `None`.

**M7 — Oracle-valid dates are no longer rejected.** `is_leap_year` was proleptic Gregorian. Servers
use the historical mixed calendar, so `DATE '1500-02-29'` is a real, storable value that the contract
refused. Implemented: the Julian leap rule before 1582, correct BC handling (no year zero, so the
divisibility test shifts by one for negative years), and a documented decision on the
`1582-10-05..14` reform gap — **strict on construction** (`Timestamp::new`/`date` reject it, because
sending an impossible date to a server only produces a worse error later) and **lenient on decode**
(`Timestamp::from_source` accepts it, because failing an entire fetch over one historical row serves
nobody). Range checks still apply on both paths. Crate documentation says "the calendar rules of the
source database", since this crate is vendor-neutral; the tests name the concrete cases.

### Should-fix

**S1 — `#[non_exhaustive]` where growth is expected**, plus private fields where public ones would
block it. Applied to `TimeZone`, `Value`, `ValueRef`, `BindValue`, `ColumnData`, `ColumnKind`,
`OutValues`, `Bind`, `Binds`, `Endpoint`, `Credentials`, `ExtensionValue`, `TlsMode`, `LobKind`,
`CancelKind`, `CancelOutcome` and `StatementKind`. `Capabilities` gets private fields, `Default` and
`with_*` setters instead.

`SessionState` and `TransactionState` are **deliberately left exhaustive**, as the review permitted
with justification. Both are tiny closed state machines that exist precisely so `db-core` handles
every case: `Usable`/`NeedsValidation`/`Lost` is the complete "is this session reusable" ladder, and
`Inactive`/`Active`/`Unknown` is the complete truth table for "is a transaction open". A new variant
in either would be a semantic change that every call site must revisit anyway, and exhaustive
matching is what forces that revisit at compile time instead of routing it into a `_` arm that
silently does the wrong thing. The value of the compile error exceeds the cost of the breaking change,
which is the opposite of the trade-off for a growing data enum like `Value`.

**S2 — `TransactionState::default()` is now `Unknown`.** A default-constructed value is by definition
one nobody has classified, so it must be the answer that makes the core prompt (`may_be_open()`).

**S3 — `SqlPosition` counts characters, not bytes.** `byte_offset` → `char_offset`, with
`byte_offset_in(sql)` to convert against the submitted text, and `at_offset` → `at_char_offset`.
Tested with Thai (`SPEC.md` §14). The conversion lives here rather than being required of drivers,
because a driver reports what the server gave it and only the caller holds the exact text.

**S4 — A fetch-size hint is available before execute.** `Statement::with_fetch_rows`; see D6.
`fetch_batch(max_rows)` is unchanged.

**S5 — `ColumnMetadata.scale` documented as fractional-seconds precision** for timestamp types, on
both the setter and the getter, rather than adding a second field that would be another name for the
same number.

**S6 — `Number` widened to 40 significant digits**, `Display` reduced to one canonical plain-decimal
form, `PLAIN_ZERO_LIMIT` removed, `from_f64_lossy` removed, exponent range reviewed and confirmed
correct, size assertions updated. See D5.

**S7 — `Secret` no longer promises zeroing it cannot deliver.** Redaction and a clearly-labelled
best-effort wipe remain; the guarantee is removed from the documentation. `zeroize` adoption stays
deferred to the credential-storage ADR (`ARCHITECTURE.md` §13 item 9) per lead decision 4.

**S8 — `NullMask::set_null` returns `DbResult<()>`.** It used to ignore an out-of-range row silently,
which turns a decoder bug into a batch whose NULLs read back as data — surfacing as wrong values in a
grid, far from the cause. `Result` over `debug_assert!` because it keeps the failure reportable
through the normal error path in release builds, it is testable without `should_panic` (which would
behave differently under `--release`), and it matches `Column::new`, which already refuses to
propagate an inconsistent mask. It is not the hot path: it is called once per NULL cell, not once per
cell.

**S9 — DDL's server-side commit is documented and reported.** `StatementKind` on
`ExecutionOutcome`; see D4.

**S10 — Bind inputs are `Clone`.** `BindValue` split out of `Value`; see D5. `Statement`, `Binds`,
`Bind` and `NamedBind` are `Clone` as a result.

**S11 — `WarningKind` stays small, and the docs say kind detection may be text-based in a driver.**
*Deviation, deliberate:* the cut list marked `WarningKind::Informational` for removal "if unused". It
is unused in this crate today, but removing it would leave a driver whose upstream exposes warnings
as a bare `String` — which is exactly the primary driver — with nowhere to put a warning it cannot
classify, forcing it to either drop the warning or mislabel it as `CompiledWithErrors`. Both are
`SPEC.md` §2 failures. It is kept, documented as the honest destination for an unclassified warning,
and now covered by a test.

### Cuts

**C1 — Removed:** `SessionId` and `StatementId` (neither appeared in a contract signature; a session
id belongs to `db-core`); `ExecutionOutcome::implicit_results` / `take_implicit_results` /
`with_implicit_results` and `Capabilities::implicit_results` (the primary driver has none;
`#[non_exhaustive]` lets them return later); `SqlPosition::with_offset` (redundant once the offset
unit is explicit); `NullMask::null_count`; `Number::from_f64_lossy`; `TextColumn::total_bytes`.

Two small deviations, both in the removal direction and both noted here rather than silently:
`Column::null_count` was removed alongside `NullMask::null_count` (it existed only to delegate, so
keeping it would have meant re-implementing the loop for no caller), and `BytesColumn::total_bytes`
was removed alongside `TextColumn::total_bytes` (identical shape, identical lack of callers; an
asymmetric pair in a contract is worse than neither).

**C2 — Recorded as known, deliberately deferred gaps:** DML `RETURNING INTO` arrays and array binds.
See D8.

## Amendments after the Phase 0 spikes (2026-09-19)

The ADR-0001 spikes S1–S5, S7 and S9 ran against a live Oracle Database 19.3 with the
`oracle-thin` driver and found one **blocking** gap in this contract and one factual error in its
driver notes. Both are recorded in `docs/exec-plans/active/phase-0-spike-results.md` §7 as C-1 and
C-2. Numbering: `P` = Phase 0 spike.

**P1 — Output values can be owned, so a REF CURSOR is readable (spike contract gap C-1).**
`ExecutionOutcome::out_values()` returned `&OutValues`, and `OutValues::named`/`positional` returned
`Option<&Value>`. A `Value::Cursor` holds a `Box<dyn Cursor>`, and every useful method on `Cursor`
needs ownership — `fetch_batch(&mut self)`, `close(self: Box<Self>)`. So a REF CURSOR delivered
through an OUT bind could be opened and *described* but never read: `Capabilities::ref_cursor` was
unmeetable and `SPEC.md` §11's "REF CURSOR" unimplementable. The same argument applies to a
`Value::Lob`. This was a defect in the contract, not in the driver, which had the whole path
implemented behind the missing accessor.

The fix is additive and follows the shape `Column::take_lob` already established:

- `OutValues::take_named(&mut self, name) -> Option<Value>` and
  `OutValues::take_positional(&mut self, index) -> Option<Value>` move the value out of its slot.
- `ExecutionOutcome::take_out_values(&mut self) -> OutValues` is the wholesale form — the one
  `db-core` uses — and `ExecutionOutcome::out_values_mut()` the borrowing one, mirroring
  `RowBatch::column_mut`.
- **A consumed slot is `Value::Taken`, never `Value::Null`.** This is M6's rule applied to output
  binds: a bind that carried a value must not read back as SQL NULL once something took it, or an
  exporter writes an empty cell where the database held data. `take_out_values` therefore leaves a
  *same-shaped* container of `Taken` slots rather than `OutValues::None`, so the outcome still
  reports that the statement had output binds. Taking twice returns `None`, which is what makes
  owning one live cursor twice impossible.

`Value` gains the `Taken` variant (it is `#[non_exhaustive]`, so this is not a breaking change) and
`Value::is_taken()`; `Value::is_null()` stays false for it. Sizes are unchanged (unit variant).

`db-core` mirrors this in `ExecuteOutcome::out_values`: a nested cursor is a handle derived from the
connection, so it never leaves the worker thread — the core registers it in the same result map an
ordinary query's cursor goes into and reports a `ResultSetId`, so fetching a REF CURSOR is the same
API call as fetching anything else and closing the session releases it the same way (D1/D2).

*Evidence:* `a_ref_cursor_out_value_can_be_owned_and_fetched` and
`a_taken_out_value_is_distinguishable_from_null_and_cannot_be_taken_twice` in `db-driver-api`;
`a_ref_cursor_out_bind_is_fetched_while_the_parent_connection_stays_usable` and
`a_ref_cursor_outliving_its_connection_reports_rather_than_panics` in the oracle-thin spike S5;
`crates/db-core/tests/out_values.rs` against the mock driver.

**P2 — The driver notes are corrected against `=26.0.0-beta.3` (spike contract gap C-2).** The
"Notes for driver implementers" below described a structured upstream `DbError { code, offset }`
that does not exist in the pinned version; see that section, which now names the version it
describes and marks each note with what the spikes found.

## Notes for driver implementers

Findings from reading `oracle/rust-oracledb` **`=26.0.0-beta.3`** — the version this repository pins
(`crates/drivers/oracle-thin/Cargo.toml`) and the only version any of this was checked against.
First written from the API review's reading on 2026-09-19 and **corrected on 2026-09-19 against the
same version** after the Phase 0 spikes ran against a live database (spike contract gap C-2: three
of these notes described upstream `main`, not `beta.3`). **Re-verify on upgrade** — the crate is
pre-GA and its API is explicitly subject to change. These are recorded so the next worker does not
rediscover them, not as contract requirements.

- **Cursors and LOBs own a cloned `Arc<Mutex<Client>>`.** *Confirmed.* There is no lifetime tie to
  the `Connection`, `Connection`'s methods take `&self`, and two cursors can coexist on one
  connection. This is the concrete reason D1's old `&mut self` claim was wrong, and the reason
  `Cursor::connection_id()` exists.
- **`Connection::set_call_timeout` takes the same mutex `execute` holds for the whole round trip.**
  *Confirmed, and now measured:* called 500 ms into a 5-second statement it blocked for 4.8 s, the
  whole remainder of the call (spike S4). An on-demand cancel built on it would block until the
  statement finished. This is the evidence behind `CancelKind::PreArmedDeadline` (M5) and behind
  ADR-0001's revised C1.
- **`OracleNumber`'s fields are private.** *Confirmed.* Conversion goes through its `Display` into
  one reusable `String` per column, then `Number::parse`. That is why `Number::parse` must accept
  every shape `Display` can emit (`.5`, `-.5`, `1E+2`, `1.5E-130`) — see S6. An upstream request for
  digit/exponent accessors is worthwhile: it would remove a string allocation per numeric column.
- **`OracleNumber`'s *encoder* is unsafe for two families of value.** *New; found by spike S2 and
  not visible from the API.* A value with an odd number of leading zeros after the decimal point is
  written one base-100 place out and the server stores it **ten times too large, silently**; a value
  of magnitude ≥ 1E40 indexes past the encoder's 40-byte digit array and **aborts the process**. A
  wrapper must refuse both rather than bind them (`oracledb` U-1, U-2). Reading is exact in both
  cases, verified to 126 digits.
- **`transaction_in_progress` is tracked internally but not exposed.** *Confirmed* — a private field
  on `Client` with no accessor. The wrapper must therefore report
  `Capabilities::exact_transaction_state == false`. An upstream request to expose it is worthwhile;
  it would materially improve `SPEC.md` §10 prompting.
- **Fetch is row-at-a-time**, `DbRow { column_values: Vec<Option<DbValue>> }`, with a per-cell
  `String` for character data (`DbValue::String`). *Confirmed.* Building D6's column batches
  therefore costs one extra copy on top. Measure before optimising: the copy may be cheaper than the
  allocations it removes downstream, and `phase-0.md` "Measurements" requires a number before any
  claim.
- **Prefetching decodes rows during `execute`.** *New; the property the U-3 mitigation turns on.*
  A query's execute round trip carries `Statement::prefetch_rows` rows (default 2) and `oracledb`
  decodes them before the caller sees anything, so a wrapper cannot inspect the described column
  types before the first values have been through the decoder. `prefetch_rows(0)` makes the execute
  a describe: the columns arrive, no value is decoded, and the row source only runs on the first
  `fetch` — which also moves where a long query blocks, and where an armed deadline fires, from
  `execute` to `fetch_batch`. A nested cursor from an OUT bind never prefetches at all.
- **LOBs are materialised by default.** *Confirmed.* The wrapper must call `Statement::fetch_lobs()`
  to get locators instead of buffers, or `SPEC.md` §12's bounded-memory rule is violated silently.
- **`Lob` implements `io::Read`, and its request boundary can split a surrogate pair** on a non-BMP
  CLOB (upstream issue #18 territory). *Confirmed, with a correction to the earlier note:* the read
  sizes its request as `buf.len() / 3` **UCS-2 units**, and when the boundary lands inside a pair the
  UTF-16 decode fails and the **whole read** fails — there are no halves to re-join, because nothing
  is returned. The wrapper absorbs it by retrying with one fewer unit, out of a fixed staging buffer
  that also fixes the `InvalidInput`-instead-of-short-read behaviour when the decoded UTF-8 exceeds
  the caller's buffer. That is what makes `LobStream::read_chunk`'s contract meetable.
- **`Connection::execute` does not reject a query — it discards its rows.** *Corrected:* the earlier
  note said "rejects". `Statement::execute`'s documentation says the statement "may not be a query",
  but nothing enforces it: a `SELECT` executes and `ExecResult` keeps rows only for PL/SQL out binds
  and DML `RETURNING`, so the rows are simply unreachable. Since the crate's own statement parser is
  private, the wrapper must classify statements itself to route them — which is what
  `ExecutionOutcome::statement_kind` (S9) asks for, so it is not extra work, but getting it wrong is
  silent rather than loud.
- **No `rows_affected` on `Cursor`**; *confirmed* — it exists only on `ExecResult` and
  `ExecBatchResult`, the non-query path.
- **`DbError` does *not* expose `code` and `offset`.** *Corrected; this note described upstream
  `main`, not `beta.3`.* The public shape is `ErrorKind::DbError(String)`: `response/error_info.rs`
  parses the error number only to build the message text and reads the wire's error position with
  `resp.read_ub2()?; // error position` and throws it away (`oracledb` U-8). So a wrapper must
  recover the ORA code by parsing `ORA-nnnnn` out of the message, and a character offset for a plain
  SQL error is **unavailable at any price** — only the `line n, column m` that ORA-06550 puts in its
  own text can be recovered, which happens to be the PL/SQL case `SPEC.md` §24.14 needs.
  `SqlPosition::at_char_offset` therefore has no upstream source on this version, and `SPEC.md`
  §24.14's "highlight the offending token" is achievable for PL/SQL only. `Capabilities::error_position`
  has no finer grain than one boolean; see spike contract note C-3.
- **A panic inside a round trip becomes a process abort.** *New; found by spike S2 (`oracledb` U-4).*
  `impl Drop for StatementHolder` does `self.client_ref.lock().unwrap()`, so a panic that poisoned
  the client mutex panics again during unwinding. **No wrapper can contain an upstream panic**, and
  `catch_unwind` does not help. Everything a driver knows to be a panicking input must therefore be
  refused *before* it reaches the crate.
- **Warnings are a plain `String`** — `last_warning() -> Result<Option<String>, Error>`, with no code
  and no structure. *Confirmed.* See S11.

## Consequences

**Easier.** The contract matches the primary driver's real shape, so the oracle-thin wrapper is
adaptation, not emulation. No async runtime anywhere below the FFI line: smaller mobile binaries, no
runtime lifetime to manage across the C ABI, simpler stack traces. Calls through a connection value
are serialised by `&mut self`, and the wider thread-affinity rule is asserted at runtime through
`connection_id()`. The mock driver is trivial, so `db-core` is testable without a database.
Cancellation honesty is structural — capability class plus `CancelOutcome` plus `Cancelled` plus
reported session state — so the UI can degrade instead of lying.

**Harder.** `db-core` must implement its own worker-thread/channel plumbing and its own conservative
transaction tracking; neither is free, and it now also owns the `connection_id()` assertions that
keep derived handles on the right thread. A blocking `fetch_batch` means back-pressure and progress
reporting are core's problem. Column-oriented batches are more code in every driver than a
`Vec<Vec<Value>>` would be. `Number` is 44 bytes, so `Value` is 48 rather than the 16 an `f64`-based
model would need — accepted deliberately, since silent NUMBER precision loss is a `SPEC.md` §2
correctness failure, and the Result Store may re-encode more compactly later. And the contract now
forces the UI to say "this statement cannot be interrupted" on a `PreArmedDeadline` driver, which is
uncomfortable and correct.

**Follow-up.** `db-core` session/worker implementation and the mock driver (next task); the
oracle-thin wrapper, whose cancellation will be `PreArmedDeadline` until ADR-0001 spike S4 says
otherwise; contract tests every driver must pass, including the handle-lifecycle rules in D2 and the
"`request_cancel` does not block" requirement; benchmarks for batch size and allocation counts before
the Result Store design is fixed.

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
  a 44-byte `Number` dominates fetch memory — then re-encode `Number` (packed BCD halves the size)
  or box it, without changing the exponent/digit semantics.
- A driver appears that cannot express `transaction_state()` at all, or that requires `Sync`
  connections — both would force a wider contract.
- ADR-0001 spike S4 finds no mechanism that interrupts a running statement, so every driver is
  `CancelKind::PreArmedDeadline` — then `SPEC.md` §10/§24.8 "Cancel" is not met and **ADR-0001 must
  be re-opened with the owner**. That is an ADR-0001 decision, not a contract change: the contract is
  already shaped to report the limitation honestly rather than to hide it.

## Evidence/benchmarks

None yet for performance; this ADR is design work ahead of Phase 0 measurement, and it deliberately
records the measurements that would overturn it. Its factual base is ADR-0001's source review of
`oracle/rust-oracledb` (blocking `std::net::TcpStream`, `Arc<Mutex<Client>>`, `set_call_timeout`, no
public cancel API) checked on 2026-09-19, extended by the API review's second reading of the same
source on 2026-09-19 — see "Notes for driver implementers", in particular the verified finding that
`set_call_timeout` contends with `execute` for the same mutex, which is what forced M5.

The batch-shape claim is reasoned, not measured, and must be benchmarked before the Result Store
design is fixed (`ARCHITECTURE.md` §13 item 6). The `Number`/`Value` sizes quoted in D5 *are*
measured: `size_of` assertions in `crates/db-driver-api/src/value/mod.rs` fail if they change.
