# 0002 — Driver API and Concurrency Model

**Status:** Accepted (owner confirmed 2026-09-19) — implemented; independent API review completed
2026-09-19 and its must-fix findings applied, see "Amendments after API review"; amended after the
ADR-0001 Phase 0 spikes, see "Amendments after the Phase 0 spikes"; amended again after the
independent review of the `db-core` session layer, see "Amendments after the db-core session review";
amended again by the owner's connect-time-warning decision, see "Amendment: the connect-time warning
channel"; amended again to say where a connection is *created*, see "Amendment: a connection is
created on a helper thread"; amended again to make a batch's column storage readable and shareable,
see "Amendment: a batch's column storage is readable, not only indexable"; amended again to record
where the SQL/PL-SQL dialect descriptor lives, see "Amendment: the `SqlDialect` descriptor"; amended
again after an independent adversarial review of the splitter found a panic and several unsafe
mis-splits, see "the `SqlDialect` descriptor" §J4
**Date:** 2026-09-19
**Amended:** 2026-09-19 (API review), 2026-09-19 (Phase 0 spikes), 2026-09-19 (db-core session
review), 2026-09-19 (owner confirmation), 2026-09-20 (connect-time warning channel, C-6),
2026-09-20 (connection created on a helper thread, C-5), 2026-09-20 (column storage readable, M1.3),
2026-09-20 (`LobStream: Sync` and `ExecuteOutcome` non-exhaustive, M1.3 review),
2026-09-20 (`SqlDialect` descriptor, M2.4), 2026-09-21 (`SqlDialect` splitter-safety review, M2.4),
2026-09-24 (server output, M2.7 — amendment T), 2026-09-24 (server-output framing moved to
`RAW`/`LENGTHB` with per-line UTF-8 decoding, M2.12 — amendment T update), 2026-09-25 (every
transaction end releases results, from ADR-0004's review — amendment X; implemented with M5.2
on 2026-09-26, together with three additive batch accessors the Result Store needs)

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
`CancelHandle`, `LobStream`) and has **zero production dependencies** (since 2026-09-25 one,
`zeroize`, private to `Secret`: see the amendment to accepted item 4 below, and ADR-0007 S6).
Concurrency is owned by
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
the owner's delegation. **Confirmed by the owner on 2026-09-19** (owner reviewed the lead's summary
and answered "as you recommended" to all five).

1. **No auto-commit toggle in the V1 contract (D4).** Accepted.
2. **`TIMESTAMP WITH TIME ZONE` named regions (D5).** Accepted as a documented limitation — normalized
   to a UTC offset for now — on condition the type stays extensible so a named region can be added
   later without a breaking change.
3. **Cursor-typed result columns, i.e. nested `CURSOR(...)` in a select list (D5, D8).** Accepted as
   out of scope for V1, provided a driver reports `ErrorKind::Unsupported` rather than silently
   dropping the column.
4. **`Secret` zeroing stays best-effort, no `zeroize` dependency for now (D7).** Accepted; revisit in
   the credential-storage ADR (`ARCHITECTURE.md` §13 item 9).
   **Revisited 2026-09-25 by [ADR-0007](0007-credential-store.md) S6.** `Secret` now wipes its
   buffer with `zeroize` on drop. That makes `zeroize` the contract's one production dependency:
   D1's "zero production dependencies" now means "one: `zeroize`, private to `Secret`".
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

**Carve-out found by the Phase 0 spikes (2026-09-19): not every unrepresentable type gets a text
rendering.** M1's `ColumnData::Unsupported` path assumes the driver can produce *some* text for the
value — that is true for `INTERVAL DAY TO SECOND`, `INTERVAL YEAR TO MONTH`, `ROWID` and
`TIMESTAMP WITH LOCAL TIME ZONE`, which the oracle-thin wrapper renders exactly this way (spike S2).
It is not true for `XMLType`, `JSON`, `VECTOR`, object types and `BFILE` on `oracledb`
26.0.0-beta.3: upstream's own row decoder (`DbValue::from_response`) has no branch for them at all, so
there is no value to render — the **fetch itself would fail**, not just the typed conversion. Failing
from `fetch_batch` would still kill a result set after earlier batches had already reached the caller,
so the wrapper refuses the column **at describe time**, before the first batch is requested: the
session stays `Usable`, and the column is simply absent rather than shown as `Unsupported` text. This
is a *third* outcome alongside M1's "rendered as text" and an ordinary `DbError`, not a special case
of either, and a driver implementer should not expect `ColumnData::Unsupported` to be reachable for
every type a server can send. See `docs/exec-plans/active/phase-0-spike-results.md` §3 (S2) for the
evidence.

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

## Amendments after the db-core session review (2026-09-19)

An independent review of the implemented `db-core` session layer (commit `3c7bcac`) returned "merge
after must-fix" and reproduced every finding. The lead accepted all of them. This section records
what changed in the *model* this ADR describes; findings that were purely `db-core` bugs against an
unchanged contract are not repeated here. Numbering: `K` = db-core review.

### K1 — Only plain data crosses threads, and now that is literally true

D1 has always claimed that "of everything a fetch produces, only `RowBatch` — plain data, no handles
— crosses a thread boundary". It was not true. A driver puts live `LobLocator`s straight into a
batch's LOB columns, so the batch `db-core` handed to its caller carried driver handles, and the
caller's thread could read them, and — worse, because it needs no API call at all — **drop** them.
Dropping a locator runs the driver's release path; with the primary driver's `Arc<Mutex<Client>>`
that is a deadlock or a soundness hazard, not a diagnosable error.

The model is corrected rather than the claim weakened:

- Before a batch leaves the worker thread, `db-core` takes **every** locator out of it with
  `Column::take_lob` (the contract already had the `Taken` state for exactly this, amendment M6) and
  parks it in a side table beside the cursors.
- The caller receives a core-owned, opaque `LobHandle` in its place and reads through
  `DatabaseSession::read_lob_chunk(LobHandle, max_bytes)` / `close_lob(LobHandle)`. The handle for a
  cell is found with `FetchedBatch::lob(row, column)`; a `LobLocator` delivered through an OUT bind
  becomes a handle the same way (`OutValue::Lob`).
- Handles die with their result and with their session, so a locator is always released on the
  thread that owns it.
- `db-core` no longer re-exports `LobLocator`: nothing above the core should be able to name one.

The contract itself is unchanged — this is a `db-core` responsibility — but its **documentation was
wrong by omission** and now says so: `RowBatch`, `ColumnData::Lob`, `Column::take_lob`, `LobStream`
and `LobLocator` all state that a batch still holding un-taken locators must be consumed *or dropped*
on the owning worker thread, and that dropping a derived handle is driver work like any other.

*Evidence:* `only_plain_data_crosses_threads_even_when_batches_and_handles_are_dropped_elsewhere` in
`crates/db-core/tests/lob_handles.rs` moves batches and handles between threads and asserts the mock
saw exactly one thread id for the connection.

### K2 — `close()` on a derived handle is idempotent and infallible in spirit

D2 and the `Cursor` trait documentation said that after the connection is closed, *every* operation
on an outstanding cursor reports `DbError::connection_closed` — including `close()`. The Oracle
wrapper returns `Ok(())` instead, and a spike test asserts that it does. Doc and code disagreed.

**Decided: the wrapper is right and the ADR was wrong.** The rule is now:

> `Cursor::close` is idempotent and infallible in spirit: when there is nothing left to release —
> because the connection is gone, or the fetch already failed — it reports `Ok(())`. Only a real
> failure to release something is an error. **Every other method** on a handle whose connection has
> been closed reports `DbError::connection_closed`.

The asymmetry is the useful one. `close` exists to let go; a release path that fails because there is
nothing to release only teaches callers to ignore its result, and `db-core` closes cursors on paths
(session close, commit invalidation, an undeliverable reply) where there is nothing sensible to do
with such an error. Applied to the `db-driver-api` doc-comments (`Cursor::close`,
`DatabaseConnection`, `DatabaseConnection::close`, `DbError::connection_closed`) and to the mock.
**The oracle-thin wrapper needs no change**; the spike test that asserts `Ok(())` is now asserting
the documented rule rather than contradicting it.

The same paragraph also makes explicit what "report, never look complete" means for a *failed*
cursor: after an error, `fetch_batch` must keep returning that error and must never return the empty
batch that means "exhausted", because that turns a partial result into one that looks whole.

### K3 — A cancel cannot be aimed at a statement, and the ADR now says so

D2 rule 4 makes `request_cancel` idempotent and a no-op when nothing is running, and rule 6 makes the
"statement finished first" race the caller's to handle. What neither said is the consequence: because
nothing in the contract carries **statement identity**, a driver is free to latch a cancel that
arrived while nothing was running and apply it to the *next* statement, and no layer above can tell
that apart from a legitimate cancellation.

A per-statement generation guard was considered and **cannot be implemented in `db-core`**: the
cancel goes straight to the driver's `Arc<dyn CancelHandle>` — deliberately, because routing it
through the command queue would let a blocked statement stall its own cancellation — so the core has
no point at which it could attach or check a generation. Closing the race needs statement identity in
the driver contract, and the primary driver could not honour it today (spike S4: it has no break API
at all).

So the race is **narrowed and documented, not hidden**:

- `DatabaseSession::cancel` answers the "no command in flight" case itself with the
  `CancelOutcome::Requested` no-op rule 4 already permits, instead of handing the request to a driver
  that might keep it. This removes the whole class of "cancel issued while idle, lands on the next
  statement".
- The window that remains — the statement finishing between that check and the driver's own — is
  stated on `DatabaseSession::cancel`: a caller that sees `ErrorKind::Cancelled` for a statement it
  did not cancel is seeing this race, not a bug.
- The mock can model the nasty driver (`Scenario::set_late_cancel_lands_on_next_statement`), so the
  behaviour is tested rather than assumed.

### K4 — `close` decides on the worker, and a failed disposition never costs the transaction

`SPEC.md` §10 turns out to need three things this ADR left implicit. All three are now part of the
model:

1. **The "is a transaction open?" decision belongs on the worker thread**, after every command queued
   ahead of the close has run. Deciding it on the caller's thread reads a flag that is by
   construction a snapshot from before the statements still in flight, so a `close(None)` racing a
   blocked DML discarded a live transaction and reported success.
2. **A failed commit or rollback inside `close` leaves the session open.** Closing anyway would
   destroy a transaction the user asked to keep because the attempt to keep it failed — the silent
   data loss §10 exists to prevent, arrived at from the other direction. `CloseError` now names which
   step failed (`CommitFailed`, `RollbackFailed`, `Failed`, `DecisionRequired`) and
   `session_is_still_open()` says whether the caller can retry.
3. **Closing a lost session reports the loss and never demands a disposition.** Asking a user to
   choose Commit or Rollback for a transaction the server has already rolled back is a question with
   no true answer, and answering `Ok(())` to `close(Some(Commit))` on a dead session told them their
   work was saved when nothing was committed. The error says so in as many words, and resources are
   released either way.

### K5 — `Drop` bounds its wait and detaches; `close` is the only path that can commit

Dropping a session must never hang, and `db-core` cannot interrupt a driver call — it can only ask,
through `CancelHandle`, which on a `PreArmedDeadline` driver achieves nothing at all (spike S4). The
model is therefore: **request the cancel, ask the worker to abandon and release everything, wait at
most `DROP_SHUTDOWN_TIMEOUT` (500 ms), then detach the worker thread.** A detached worker still owns
the connection, its cursors and its parked large objects and closes all of them when the blocked call
returns, so the release is late but never lost.

`Drop` never commits and never rolls back explicitly; it abandons, and the server's own
rollback-on-disconnect is what protects the data. That makes "dropping a session cannot commit"
structural rather than a promise, and **explicit `close` is the only path that can commit anything**.

### K6 — Panic containment, and the driver class it applies to

A caught driver panic marks the connection *torn*: `db-core` drops it on the worker thread and
deliberately does **not** call `close()` on it, because the object's internal state is by definition
unknown and a second call into it is as likely to panic again as to release anything. The same rule
applies to every handle derived from it.

This guarantee holds only for drivers that unwind. The intended primary driver does not: `oracledb`
26.0.0-beta.3 locks a poisoned mutex in `impl Drop for StatementHolder`, so a panic inside a round
trip panics again during unwinding and **aborts the process** (spike U-4). No wrapper can contain
that and `catch_unwind` does not help, which is why the driver must refuse known-panicking inputs
before they reach the crate.

### K7 — Conservative transaction tracking covers locking queries

D4 said `db-core` combines `StatementKind` with the driver's `transaction_state()`. In practice the
core only set its flag for `Dml`/`PlSqlBlock`/`Other`, so `SELECT … FOR UPDATE`, `LOCK TABLE` and
`SET TRANSACTION` left `has_possibly_active_transaction()` **false** on a driver with exact
transaction state — and closing the worksheet discarded row locks and an open transaction in silence.
No `StatementKind` can distinguish a locking query from a plain one; a driver has no honest
classification for `SELECT … FOR UPDATE` other than `Query`.

The rule is now stated as a rule: **no statement kind clears the flag, and every successful statement
sets it unless a driver with `exact_transaction_state` reports `Inactive` immediately afterwards.**
`Dml` and `PlSqlBlock` set it unconditionally. Over-prompting is acceptable and a missed prompt is
not, which is the same trade-off D4 already makes for `TransactionState::Unknown`.

### K8 — A session has a `Closed` state, and its handles are session-scoped

Two smaller model corrections:

- `SessionState` describes a *connection* and has no way to say "closed on purpose", so a closed
  session reported `Usable` — telling callers they could still submit work. `db-core` now has its own
  `SessionLifecycle { Usable, NeedsValidation, Lost, Closed }`. `Lost` takes precedence over
  `Closed`, because why a session ended matters more than that it ended. `SessionState` in the
  contract is unchanged.
- `ResultId` and `LobHandle` are scoped to the session that issued them, so a handle used on another
  session is rejected as *belongs to another session* rather than being mistaken for one that was
  closed. This is what supersedes the narrower "assert `connection_id` in `read_lob_chunk`" fix: a
  handle can no longer cross sessions at all.

The fail-fast error a lost session gives every later command now keeps the original `ErrorKind`,
native code and text, statement position and cause chain, instead of flattening them into a sentence.
A UI cannot tell a network loss from a killed session from a driver bug if every later error says
`Connection`.

### K9 — Bounded damage: the queue is unbounded, the resources are not

Stated explicitly because it was previously only implied. The per-session command channel is
**unbounded on purpose**: a bounded one would make `execute` block the *calling* thread once it
filled, which `SPEC.md` §11/§19 forbids, and the queue's real bound is that one session belongs to
one worksheet. What is bounded instead is what a session can accumulate — `SessionLimits`
(`max_open_results`, `max_lob_chunk_bytes`), with a clear `ErrorKind::Resource` failure rather than an
unbounded allocation.

### Deferred, and deliberately not built now

Recorded so the design stays unblocked:

- **Asynchronous `open_session`.** `connect` runs on the worker thread, but the caller still blocks
  on the reply that the connection is ready. Making that a completion like every other request is
  Phase 1 FFI work; nothing in the current shape prevents it, since the worker already exists before
  the connection does.
- **A per-session outbound completion/event queue for the FFI adapter.** D1 describes it; today each
  request carries its own oneshot reply channel. Both shapes coexist — the adapter would drain one
  queue instead of holding many `Completion`s — so this is additive.
- **`Arc<DatabaseSession>` ergonomics: done, because it was trivial.** `close` takes `&self`, and the
  session is now `Sync` as its documentation already claimed (a `std::sync::mpsc::Sender` is `Send`
  but not `Sync`, so the claim had been false). `Completion::poll` and the new
  `Completion::wait_timeout` consume the completion and hand it back on `Err`, which is what makes
  them non-lossy: the reply exists exactly once and is not `Clone`, so a method that could both
  return it and leave a `Completion` behind would have to fabricate something for the second caller.

## Amendment: the connect-time warning channel (2026-09-20, contract gap C-6)

Owner decision (`phase-0-spike-results.md` §9 item 13, approved 2026-09-19; sequenced after pull
request #5, which has landed). Numbering: `W`.

### W1 — `DatabaseConnection::take_connect_warnings`

`DatabaseDriver::connect` returns `DbResult<Box<dyn DatabaseConnection>>`: a connection or an error,
with nothing in between. `Warning` — this contract's own type for "non-fatal, worth showing" —
travelled only on an `ExecutionOutcome`. So a driver that noticed something while *opening* a
session had exactly two answers, and for one real case both were wrong. The U-14 guard is that case:
`SSL_SERVER_CERT_DN` is a refusal, because the session would be weaker than the profile configured,
but `SSL_SERVER_DN_MATCH` deliberately is **not** — the session that opens verifies the server more
strictly than the parameter asked for — and the caller still has to be told the parameter did
nothing, or an imported profile goes on believing it configured something.

The oracle-thin driver worked around it by holding the findings on the connection and attaching them
to the **first statement that succeeded**. That reached the user through the existing path, but it
coupled a connect-time fact to an unrelated statement's outcome and lost it entirely for a session
that is opened, pinged and closed without executing anything.

One additive, vendor-neutral, defaulted method closes it:

```rust
pub trait DatabaseConnection: Send {
    /// Non-fatal findings produced while this connection was being opened.
    fn take_connect_warnings(&mut self) -> Vec<Warning> {
        Vec::new()
    }
}
```

- **Defaulted**, so a driver with nothing to say needs no change, and **object-safe**, so
  `Box<dyn DatabaseConnection>` is unaffected — the two properties that make this additive rather
  than a contract break.
- **Taken, not borrowed**, so each finding is reported once and the connection keeps nothing.
- Cheap and non-blocking: it reports what `connect` already discovered and must not issue a round
  trip.
- The dividing line is stated on the method: anything that makes the session *worse* than the caller
  asked for stays an error from `connect`. This channel is for "you configured something that did
  nothing", never for "your session is less safe than you think". That is what keeps the U-14 split
  — refuse the pin, report the match — a principle rather than a judgement call.

No Oracle vocabulary enters the crate, and `db-driver-api` still has zero production dependencies.

### W2 — `db-core` collects once, and reports it with the session

`worker.rs` calls it exactly once, on the session's worker thread, immediately after
`driver.connect` returns and before the session is reported ready — the only moment at which
"somebody asks" is defined. The findings travel out with the successful open and are reported by
`DatabaseSession::connect_warnings() -> &[Warning]`.

Three alternatives were considered and rejected:

- **Widening `SessionManager::open_session`'s return type** to a tuple. It would make every caller
  that does not care about warnings destructure one, for a fact that belongs to the session it just
  opened.
- **An event queue.** The per-session outbound completion/event queue is real and is named in
  "Deferred, and deliberately not built now"; it is Phase 1 FFI work, and inventing a one-message
  version of it here would prejudge its shape.
- **Mixing them into the first `ExecuteOutcome::warnings`.** That is the mechanism being replaced.

`connect_warnings` **borrows** rather than takes, which is deliberately the opposite of the driver
method. The driver must hand its findings over once, because keeping them is what caused the
duplicate-delivery problem; the core's copy is fixed for the session's lifetime, so reading it late —
after the UI has a window to show it in — must not be the same as losing it. It is never mixed into
`ExecuteOutcome::warnings`, which belongs to one statement.

A driver that panics inside `take_connect_warnings` is contained like any other driver call (K6): the
connection is *torn*, so it is dropped rather than closed, and the session does not open.

*Evidence:* `a_driver_with_nothing_to_say_about_connecting_needs_no_code_at_all` in
`db-driver-api`'s `session.rs` (a stub connection that implements only the required methods);
`connect_warnings_are_taken_once_and_default_to_none` in `crates/drivers/mock/tests/contract.rs`;
`crates/db-core/tests/connect_warnings.rs` (reaches the owner with no statement run, is not mixed
into a statement's warnings, and is empty for a driver with nothing to say); and live against the
TCPS listener, `a_descriptor_that_sets_dn_matching_opens_a_session_and_reports_the_parameter_as_inert`
and `a_connect_time_finding_survives_a_session_that_never_executes_a_statement` in
`crates/drivers/oracle-thin/tests/s8_tcps.rs` — 13 passed, 0 failed, 49.1 s.

## Amendment: a connection is *created* on a helper thread (2026-09-20, contract gap C-5)

Numbering: `H`. Prompted by the independent review of the C-5 work, which found that the code and
D1/D2 no longer said the same thing.

### H1 — D2's "one worker thread, for the connection's whole life" starts at adoption, not at birth

D2 says a `DatabaseConnection` "is moved to its owning worker thread and never touched from anywhere
else", and the C-5 work (results file §7, `SPEC.md` §8) makes that literally untrue for one instant:
the Oracle driver now runs `oracledb::connect` on a **helper thread**, because upstream cannot bound
a connect and the only way to stop waiting for one is to stop waiting on a different thread (U-15).
The connection is therefore created on a thread that is not its owner and then moved.

The rule is amended to say what it has always meant:

> A connection is used by exactly one thread at a time, and by exactly one thread for its whole
> useful life. It may be **constructed** on another thread and moved to its owner before any
> statement is issued.

This is the ordinary Rust meaning of `Send`, which is what `DatabaseConnection` requires and all
D2 ever needed. What D2 forbids — two threads holding it, or a second thread touching it after the
worker has it — is unchanged and unweakened.

### H2 — why the move is a move, and not a race

The handover is a single rendezvous under one mutex (`connect_timeout::Handoff`), which admits
exactly one of two outcomes and has no window between them:

- the caller collects the connection **before** its limit passes, and from that moment the helper
  thread holds nothing and never refers to it again; or
- the limit passes (or the waiting frame unwinds), the rendezvous is marked abandoned *under the
  lock*, and every later delivery hands the connection straight back to the helper thread, which
  closes it there.

So a late connection is never adopted, and a connection that is adopted was released by the helper
thread first. Two threads never hold it, and the thread that closes an abandoned one is the same
thread that opened it — which is the property D2 cares about. The cost, stated plainly because it is
real: one thread per abandoned attempt, alive until upstream's connect finally returns, since
nothing can interrupt it (U-15).

`db-core` is unaffected. It still receives one connection, moves it to one worker thread, and knows
nothing about how it was made — the driver contract is unchanged, and no other driver has to do
anything.

*Evidence:* `crates/drivers/oracle-thin/src/connect_timeout.rs` unit tests — in particular
`a_handoff_that_expired_hands_a_late_value_back_instead_of_adopting_it`,
`a_session_that_arrives_after_the_limit_is_closed_on_its_own_thread` and
`a_waiter_that_unwinds_still_leaves_nothing_adopted_or_leaked`, the last two driving the shipped
`within` rather than a copy of it.

## Amendment: a batch's column storage is readable, not only indexable (2026-09-20, from M1.3)

Numbering: `I`. Items I1–I2 were prompted by building `crates/ffi` (ADR-0003), which needs to hand C
the *same* buffers the driver filled, not copies of them; I3–I4 by the independent review of that
work the same day.

### I1 — `Column`, `TextColumn`, `BytesColumn` and `NullMask` expose their layout

This ADR's result contract already *documents* the layout — "one contiguous buffer plus a `Vec<usize>`
of offsets", "a null bit per row, LSB-first within each `u64`" — because that layout is the reason
`RowBatch` is column-oriented at all (D5, S2). But the types only offered per-cell access
(`Column::value(row)`, `Column::is_null(row)`), so the one consumer the layout exists for could not
reach it. The following read-only accessors are added; no field is made public and nothing gains a
way to mutate a batch:

```rust
impl NullMask    { pub fn words(&self) -> &[u64]; }
impl TextColumn  { pub fn buffer(&self) -> &str;  pub fn offsets(&self) -> &[usize]; }
impl BytesColumn { pub fn buffer(&self) -> &[u8]; pub fn offsets(&self) -> &[usize]; }
impl Column      { pub const fn data(&self) -> &ColumnData; pub const fn nulls(&self) -> &NullMask; }
```

Additive and non-breaking: every existing method keeps its behaviour, and the unit test
`a_column_exposes_the_layout_it_documents` asserts the accessors and `value`/`is_null` describe the
same cells, so the two views cannot drift.

### I2 — what this commits the contract to, and what it does not

It commits a driver to the *documented* representation: a text column really is one UTF-8 buffer
with `row_count + 1` offsets into it, and a null mask really is bits LSB-first within `u64` words.
That was already the contract; it is now observable, which means a driver cannot quietly satisfy the
per-cell API with some other arrangement. A future storage change — dictionary encoding, a chunked
buffer, an arena shared between columns — is now a breaking change to `db-driver-api` rather than an
internal one, and would have to be carried through `ReldexColumnView` (ADR-0003 D4). That is the
intended trade: the million-row path has no marshalling layer precisely because the shape is fixed.

It does **not** commit anything about lifetime or ownership beyond Rust's own borrow rules. The
borrow ends with the `RowBatch`, and the FFI keeps the batch alive for exactly as long as the
adapter holds the `ReldexBatch*` it was handed.

### I3 — `LobStream` requires `Sync` as well as `Send` (2026-09-20, from the M1.3 review)

A fetched `RowBatch` can hold parked LOB locators, so `RowBatch` was `!Sync` and `&RowBatch` could
not cross a thread boundary at all. That blocked a guarantee the FFI boundary needs: ADR-0003 A12
documents concurrent read-only access to one fetched batch as sound, and without this bound that
promise would have been a false statement about `&RowBatch` rather than a supported one.

`LobStream: Send + Sync`. The bound costs an implementor nothing it was not already doing — every
method that changes a stream takes `&mut self`, and all four implementors in this workspace
(`EmptyLob`, `SliceLob`, `MockLobStream`, `OracleLobStream`) satisfied it with no change at all.
What it forbids is a stream built on `Rc` or `Cell`, which a `Send` type mostly cannot use anyway.

It does **not** weaken D1/D2's single-thread rule: `db-core` still touches a stream only on the
worker thread that owns its connection. `Sync` says a shared reference may cross a thread
boundary; it does not say a read may happen from two places, and nothing in `db-core` or the FFI
reads a locator off its owning thread. A driver whose locator genuinely cannot tolerate a shared
reference existing elsewhere should say so here, because this bound is now part of the contract.

### I4 — `ExecuteOutcome` is `#[non_exhaustive]`

M1.3 added a field (`columns`, see ADR-0003 A6) and M2.5's event queue will want at least one more.
Nothing outside `db-core` constructs an `ExecuteOutcome` — the worker thread is its only producer —
so the attribute costs nothing today and makes the next field an additive change rather than a
breaking one.

## Amendment: the `SqlDialect` descriptor (2026-09-20, M2.4)

Numbering: `J`. `docs/exec-plans/active/phase-1.md` §B4 named this as "additive, in `db-core` rather
than the contract" ahead of implementation; this amendment records where it actually landed and why,
which is one crate further out than that sketch, for reasons specific to how M2.4 was carried out
(see below) and, independently, to dependency direction.

### J1 — `SqlDialect` lives in the new `reldex-sql-text` crate, not in `db-core` or `db-driver-api`

`SPEC.md` §15 forbids splitting a script by every semicolon and requires understanding SQL/PL-SQL
block boundaries and SQL\*Plus's `/`. That parsing is vendor-neutral logic parameterized by
vendor-specific facts — which keywords open a block, which quoting forms exist, whether `/` matters
— and D8 already listed "script/statement-boundary parsing" as out of scope for *this contract*.
M2.4 keeps it out of the contract and gives it its own crate, `crates/sql-text`
(`reldex-sql-text`), with:

```rust
pub struct SqlDialect {
    pub statement_terminators: &'static [char],
    pub slash_terminates_block: bool,
    pub block_may_end_without_slash: bool,
    pub block_starters: &'static [BlockStarter],
    pub block_body_opener: &'static str,
    pub block_nesting_openers: &'static [&'static str],
    pub block_end_keyword: &'static str,
    pub quoting: QuotingRules,
    pub comments: CommentRules,
    pub bind_variables: bool,
    pub substitution_variables: bool,
    pub keywords: &'static [&'static str],
}
```

built entirely from `'static` data (so it is `Copy`, no allocation), consumed by
`reldex_sql_text::{tokenize, tokenize_block, split_statements, statement_at}`. Oracle's own value is
built by the driver: `reldex_driver_oracle_thin::sql_dialect() -> SqlDialect`, a plain function (not
a trait method — see J2), added to that crate without touching `db-driver-api` at all.

**This is the shape as of the original M2.4 implementation.** §J4 below records the fields an
adversarial review added the same day the type first landed; see that section for the current
complete field list rather than relying on this snapshot.

Two reasons, not one:

1. **Dependency direction.** `SqlDialect` is the parameter of `reldex-sql-text`'s own public API. A
   type a crate's signature names should not live one layer further down the dependency graph than
   the crate itself, or every caller of the lexer/splitter — including a future consumer that is
   neither `db-core` nor a driver, such as a standalone formatter — would have to depend on `db-core`
   (sessions, transactions, the worker/registry machinery) or on `db-driver-api` (which D8 keeps
   deliberately small) just to name the parameter type. `reldex-sql-text` depends on nothing; a
   driver depends on it (as `oracle-thin` already depends on `db-driver-api`) to build its
   descriptor; `db-core` and the FFI/UI layer may depend on it directly.
2. **Isolation during implementation.** M2.4 and M2.5/M2.6 (the event-queue and session-registry
   `db-core` refactor) were carried out concurrently in separate worktrees specifically so neither
   blocked the other (`docs/exec-plans/active/phase-1.md` §C.2, "Parallelism"). Adding a type to
   `db-core` from the M2.4 worktree would have collided with that work for no benefit to either side.
   This constraint made the `db-core` placement impractical regardless of J1's dependency-direction
   argument, which would have argued against it anyway.

`ARCHITECTURE.md` §13 item 12 is updated from an open question to resolved, pointing here.

### J2 — no change to the `DatabaseDriver`/`DatabaseConnection` traits

Unlike the `take_connect_warnings`, server-output and metadata-catalog amendments above, this one
adds **no** trait method. `sql_dialect()` is a plain associated function on the concrete
`OracleThinDriver`-adjacent module, not `DatabaseDriver::sql_dialect()`, because nothing calls it
through a `Box<dyn DatabaseDriver>`: `db-core`'s worker never needs a script's dialect (it executes
one statement at a time, handed to it already split), and the UI/FFI layer, which does need it to
drive the editor, already knows which driver it is talking to and can call the concrete function
directly. Adding a defaulted trait method for this would be additive and therefore possible, but
would commit every future driver to "supplying a dialect" as a contract obligation before a second
driver exists to say whether that shape is right — the same restraint D8 already applies elsewhere.
If a second driver arrives and the UI needs to select a dialect without knowing the concrete driver
type, that is the moment to revisit this as a genuine amendment, not before.

### J3 — what stays out of scope, and why

Following `AGENTS.md` "Scope discipline" and `SPEC.md` §15's own "Future SQL\*Plus-like commands may
be added progressively": `reldex-sql-text` implements SQL\*Plus's `REM`/`REMARK` line comment as
machinery (`CommentRules::sqlplus_line_comment_words`, since renamed — see J4), and `StatementKind` is
`#[non_exhaustive]` to leave room for a `SqlPlusCommand` variant later, but Oracle's Phase 1
`SqlDialect` leaves that word list empty and no `SqlPlusCommand` variant exists yet — there is nothing
in Phase 1's scope that needs either.

One splitting shape remains a documented, `#[ignore]`d-test limitation rather than a silently-wrong
answer: Oracle 12c's `WITH FUNCTION … SELECT …` inline PL/SQL, which does not start with a
block-starter keyword at all and would need scanning past arbitrary `WITH`-clause syntax to
recognize. (Two others originally listed here — a `CREATE TRIGGER … CALL proc(…);` body with no
`BEGIN`/`END`, and a `CREATE TYPE … AS OBJECT (…);` spec with no `BEGIN`/`END` — were fixed by J4's
`pending_bodies`/`BlockKind::ParenDelimited` model and are no longer limitations.) This one does not
silently mis-execute a statement either: `WITH FUNCTION`'s inline body's own `;`s are read as
ordinary plain-statement terminators, over-splitting the script into several statements that each
fail to parse alone — never an executable fragment carved out of the middle of one.

*Evidence:* `crates/sql-text/tests/corpus.rs` — see J4 for the post-review test count and the fuzz
evidence, which supersedes the counts originally recorded here. `tests/corpus/*.sql` (a PL/SQL
package with nested `BEGIN`/`CASE`/`IF`/`LOOP`, a trigger using the `:NEW`/`:OLD` shape
`oracle-thin::rewrite` also recognizes, Thai text in strings/comments/quoted identifiers, every
`q'...'` delimiter form, and — added by J4 — labelled/nested blocks, compound triggers, `JAVA SOURCE`,
and `$IF` directives) is checked under both LF and CRLF against two invariants: line-by-line
`tokenize_block` (state carried, as `QSyntaxHighlighter` would drive it) equals whole-document
`tokenize`; and a script's statement spans plus the gaps between them reproduce the input exactly. A
5&nbsp;MB repeated-statement script is included to keep the splitter linear (not timed —
`AGENTS.md`/this task: "no timing upper bounds in tests").

### J4 — adversarial review: a panic, unsafe mis-splits, and the splitter's safety principles

An independent adversarial review of J1's implementation (commit `e78ab58`, the same day it landed)
approved the architecture but found one guaranteed panic and several mis-splits serious enough to
require a same-day fix before this ADR could be considered settled, because a `StatementSpan` is not
a highlighting range — it is what gets executed against a real database. Both classes of defect, and
the fix, are recorded here rather than in a second ADR because they change no boundary this ADR did
not already own: `SqlDialect`'s field list and the splitter's algorithm, both introduced by J1.

**The panic.** `statement_at` clamped an out-of-range offset to `text.len()` but never floored it to a
UTF-8 character boundary, so an offset landing inside a multi-byte character (e.g. a byte-order mark)
panicked on the first `&text[..offset]` slice. Fixed with a hand-rolled `floor_char_boundary` (the
standard library's own is not yet stable) applied before any slicing.

**The mis-splits**, all in the depth-tracking algorithm: a labelled block (`<<outer>> BEGIN ... END
outer;`) was not recognized as a block at all, because leading-keyword matching stopped at the `<`
operator; a subprogram nested inside another subprogram's declare section could be mistaken for the
outer's own body opener/closer; a compound trigger's timing-point sections (`BEFORE STATEMENT IS
BEGIN ... END BEFORE STATEMENT;`) were not understood as owning their own body, so the trigger's
depth count never returned to the level needed to recognize its own close; and a package/type body
with an initialization section *after* its members closed at the wrong point. Each of these could
produce either a `StatementKind::Plain` span carved out of the middle of a block (executable on its
own, and wrong) or a block that ran unterminated to end of input, silently swallowing every statement
after it.

**The fix** replaces the single `absorbs_body_opener: bool` per `BlockStarter` with one unified
model, `crates/sql-text/src/splitter.rs`'s module docs describe in full:

- A `pending_bodies: u32` counter, incremented when a word matching the new
  `SqlDialect::subprogram_header_keywords` (`PROCEDURE`/`FUNCTION`) or — once
  `SqlDialect::compound_trigger_marker` (`COMPOUND TRIGGER`) has been seen —
  `SqlDialect::compound_trigger_timing_starters` (`BEFORE`/`AFTER`/`INSTEAD`) is found to owe a body
  (i.e. its header reaches `SqlDialect::body_intro_keywords` (`IS`/`AS`) before a statement
  terminator, and is not immediately followed by a `SqlDialect::call_spec_keywords`
  (`LANGUAGE`/`EXTERNAL`) call-spec).
- A `BEGIN` fulfills a pending body if one is owed; otherwise it is absorbed as the *current nesting
  frame's own* body exactly once (covering a lone `PROCEDURE`/`DECLARE`/`BEGIN` statement and a
  package/type body's optional init section with the same rule); any other `BEGIN` nests.
- `SqlDialect::label_delimiters` (`<<`/`>>`) are skipped before leading-keyword matching.
- `SqlDialect::body_less_markers` (`CALL`) recognizes a trigger whose body is a bare `CALL`, owing no
  body at all.
- A new `BlockKind` on `BlockStarter` (`Structured`, `OpaqueSource`, `ParenDelimited`) replaces the
  boolean: `ParenDelimited` (Oracle: `CREATE [OR REPLACE] TYPE` without `BODY`) tracks balanced
  parens to the first depth-`0` terminator, with no `BEGIN`/`END` concept at all; `OpaqueSource`
  (Oracle: `CREATE ... JAVA SOURCE ...`) is not PL/SQL and ends only at a lone `/` line or end of
  input, its own `;` characters being ordinary body content.
- `SqlDialect::directive_prefix` (`$`) makes the lexer read `$IF`/`$THEN`/`$ELSIF`/`$ELSE`/`$END` and
  `$$PLSQL_UNIT`-shaped inquiry directives as one new `TokenKind::Directive` token, so `$END`'s `END`
  can never be read as the block's own closing keyword.

**The safety principle (new, and the reason a class of *undiscovered* future bugs in the above is
already bounded):** a lone `/` line is now checked on *every* token of *every* scan in the module,
overriding nesting depth or any pending body the moment it is seen
(`SqlDialect::slash_terminates_block`/`slash_terminates_plain`, the latter new — S1 in the module
docs). SQL\*Plus itself never sends a block to the server until `/` is typed, so this is not a new
behavior invented for safety's sake; it is the same convention scripts already rely on, now enforced
by the splitter rather than assumed. Its effect: even a block-structure miscount this review did not
find can produce at most one over-large statement, never an executable fragment carved from the
middle of one (S3 in the module docs) — the dangerous failure mode is structurally unreachable, not
merely untested.

**`StatementSpan::ended_by: EndedBy`** (`#[non_exhaustive]`: `Terminator`, `SlashLine`,
`InferredBlockEnd`, `EndOfInput`) is new, so a caller — in particular M4.3's script executor — can
tell *why* a span ended where it did, not only whether `terminated` is `true`. `docs/exec-plans/active/phase-1.md`'s
M4.3 row is updated to say execution must consult it.

**Vendor-neutrality fixes**, found by the same review: `lexer.rs` had `"REM"`/`"REMARK"`,
`"Q"`/`"NQ"`, hard-coded as string literals rather than sourced from `SqlDialect` data, contradicting
this crate's own stated invariant. `CommentRules::sqlplus_rem: bool` is now
`sqlplus_line_comment_words: &'static [&'static str]`, and `QuotingRules::alternative_quoting`/
`national_prefix: bool` are now `alternative_quote_prefixes`/`national_string_prefixes: &'static
[&'static str]` — the words themselves are driver-supplied data, matching how every other keyword in
this descriptor already worked.

*Evidence:* `crates/sql-text/tests/corpus.rs` grew to 43 passing tests plus 1 remaining `#[ignore]`d
limitation (`WITH FUNCTION`, J3), including one test per named reproducer above, a
`statement_at`-never-panics sweep over every byte offset (including out-of-range ones) of Thai/emoji/
BOM text, and a new deterministic fuzz test (a dependency-free xorshift32 PRNG — this crate stays at
zero dependencies — combining grammar-aware fragments across 4 fixed seeds, 200,000 cases each in
release mode, 800,000 total) checking: no panics; every span's boundaries land on a character
boundary and are non-overlapping; spans plus gaps reproduce the input exactly; the
line-by-line-vs-whole-document `tokenize` invariant holds under both LF and CRLF; and a new
"known-good blocks joined by `/`" round-trip property (independently-valid blocks concatenated, split,
and confirmed to come back as exactly one span each, all terminated).

### J5 — round-2 adversarial review: a rewrite that fixed J4 but introduced a worse, harder-to-see bug

A second, independent adversarial review of J4's fix rejected it: every J4 finding was confirmed
fixed, and 6,000,000 random-fragment fuzz cases (an extension of J4's own fuzz test) found no
gap/overlap violation — but random fragment concatenation essentially never produces a *balanced*,
deeply nested structure, so it could not see a new defect that only manifests once nesting is
balanced. Four MUST-FIX defects were found by direct code reading; a fifth, worse one was found only
after this review commissioned the grammar-based differential test J5 itself required (below) — the
review's own point that fuzzing fragments and generating whole valid structures catch different bug
classes, proven in the same round.

**MUST-FIX #1 — a `BEGIN` starter's own first nested block was mistaken for its body.**
`BlockStarter` gained `opens_body: bool` (`true` only for the bare `BEGIN` starter, `false` for every
other shape): the depth-tracking scan now starts as though one body has already been absorbed exactly
when the starter's own matched keywords *were* that body's opener, rather than always starting fresh
and letting the first `BEGIN` found — nested or not — claim the role by an implicit, previously
untested assumption. Pre-fix, `BEGIN BEGIN NULL; END; END;` split into 2 spans (an orphaned `END`);
with sibling inner blocks, an *N*-sibling script produced *N + 1* spans, the middle ones complete,
independently runnable statements carved from a single one.

**MUST-FIX #2 — a lone `/` line with nothing pending panicked `StatementSpan::content`.**
`end_at_slash_line` computed `content_end` by trimming trailing whitespace from the whole document up
to the `/`'s own position, with no floor — so a `/` line immediately following an already-terminated
statement (nothing but whitespace between them) could compute a `content_end` for a new, near-empty
span that landed *before* that span's own `content_start`. Lead decision, recorded here because it
changes what a script author's habit means: **a lone `/` line with no pending statement text produces
no span at all** — not an empty statement, not a re-submission of the previous one (SQL\*Plus's own
"re-run the buffer" semantics for this case are themselves unverified against a real desktop-editor
use, so this crate does not attempt to reproduce them). `split_statements` now checks this before a
span's `content_start` is even computed; `end_at_slash_line` additionally clamps `content_end` to
never fall below the `content_start` its caller is building, as defense in depth.

**MUST-FIX #3 — a variable literally named `language`/`external` was read as a call-spec.**
`SqlDialect::call_spec_keywords: &[&str]` (single words) is now `call_spec_phrases: &[Phrase]` (full
phrases: Oracle ships `LANGUAGE JAVA`/`LANGUAGE C`/`LANGUAGE JAVASCRIPT`/`EXTERNAL LIBRARY`/`EXTERNAL
NAME`), matched as whole phrases in both `scan_header` and `scan_structured`'s own outer-header check.
The fail-safe direction only goes one way: a missed call-spec merely swallows extra, syntactically
inert text up to the next `/`/EOF, while a *false* one carves a real body out from under the statement
that owns it — so recognizing a call-spec now requires a full phrase match, never a single word that a
legal identifier could collide with.

**MUST-FIX #4 — a header scan was parenthesis-blind.** Neither `scan_header` (a nested member's own
header) nor `scan_structured`'s own outer-header check tracked `(`/`)` depth, so a parameter default's
own `AS`/`IS`/`CASE … END` — `PROCEDURE p2(a NUMBER DEFAULT CAST(1 AS NUMBER))`, a forward
declaration — was read as the header's real body intro, corrupting `pending_bodies` and, in turn,
consuming the container's own real `BEGIN` as fulfilling a phantom debt instead of being absorbed as
the container's own body. Both scans now track paren depth and only treat `IS`/`AS`/a terminator as
significant at depth `0`. A constructor method's `RETURN SELF AS RESULT IS ...` has the same shape at
depth `0` (the `AS` in `SELF AS RESULT` is not a body intro) — a new `SqlDialect::body_intro_exceptions:
&[Phrase]` field (Oracle: `[SELF AS]`) lets a dialect declare such phrases as data, checked by looking
at the word(s) immediately *preceding* a candidate body-intro match.

**An additional, more severe bug found only by the new differential test** (see below), not among the
four the code-reading pass found: `scan_structured`'s `BEGIN` dispatch treated `pending_bodies > 0`
alone as "this `BEGIN` fulfills a header's debt", without also requiring `depth == 0`. A two-level-deep
nested subprogram (an inner member declared inside a *middle* member's own declare section, where the
inner member's own body itself contained a further nested `BEGIN ... END`) had that innermost `BEGIN`
wrongly consumed as fulfilling the *middle* member's still-outstanding debt, because it was reached
while `pending_bodies` was still nonzero from that outer debt — even though the scan was already
nested one level deeper (`depth == 1`) by the time it got there. The fix adds the missing
`depth == 0` requirement: once already inside a body a previous `BEGIN` opened, any further `BEGIN` is
unambiguously a nested block, never a fulfillment, regardless of how many debts remain further up the
stack. Random-fragment fuzzing (both the J4 fuzz test and this round's 6,000,000-case run) could not
find this either, for the same structural reason it missed MUST-FIX #1: two-plus levels of nested,
still-pending subprogram declarations essentially never arise from concatenating independent
fragments.

**The acceptance gate this round added: a grammar-based differential test**
(`crates/sql-text/tests/differential.rs`, modeled on but not copied from the reviewer's own reference
implementation). Rather than concatenating independent fragments, it generates whole,
structurally-valid, self-contained PL/SQL "units" — anonymous blocks (bare/labelled/`DECLARE`,
nested and sibling inner blocks, an exception handler containing a block), subprograms with
two-level-deep nested subprograms and `CAST`/`CASE`-bearing parameter defaults, cursors and
`TYPE ... IS RECORD|TABLE OF|REF CURSOR`/`SUBTYPE ... IS` declarations, package specs (forward
declarations, a call-spec member) and bodies (members, an init section with an `EXCEPTION` handler),
type specs (object/varray/table-of/incomplete) and a type body (`MEMBER`/`STATIC`/`CONSTRUCTOR ...
RETURN SELF AS RESULT`), simple/compound/`INSTEAD OF`/`CALL` triggers and `WHEN (... IS NULL)`, and
plain SQL with `CASE` expressions and subquery `AS` aliases — concatenates 3–8 of them three ways
(always a `/` line between units, never one, or an independent coin flip per boundary, the last of
which also exercises MUST-FIX #2's rule by sometimes placing a `/` after a *plain* unit too), under
both LF and CRLF, and asserts `split_statements` recovers **exactly** the generated units: the same
count, in order, each one's `StatementSpan::content` equal (after stripping the unit's own trailing
`;`, its only defined trimming rule) to what was generated, the expected `StatementKind`/`EndedBy`,
and every span invariant (`content_start <= content_end <= full_end`, char-boundary offsets,
`.content()`/`.full()` never panicking). 8 fixed seeds (including the reviewer's own 1/42/3/987/2024),
2,000 base scripts per seed in release mode × 3 join modes × 2 line endings = 96,000 scripts checked;
40 base scripts per seed in debug. The random-fragment fuzz test's own vocabulary was extended with
round-2 fragments (nested/sibling `BEGIN` blocks, a `language`/`external`-named variable, the `CAST`
forward declaration, a constructor's `RETURN SELF AS RESULT`) and its assertions now call
`.content()`/`.full()` on every span, not only check offset ordering.

**Renames (naming only; no vendor literal ever lived in `splitter.rs`'s logic, confirmed by this
review):** `CommentRules::sqlplus_line_comment_words` → `line_comment_words`;
`SqlDialect::compound_trigger_marker` → `sectioned_body_marker`;
`SqlDialect::compound_trigger_timing_starters` → `section_header_starters` — the mechanism (a marker
phrase announcing independently-scoped sections, and the words that start one) is not itself an
Oracle-specific idea, only Oracle's *instance* of it (`COMPOUND TRIGGER`/`BEFORE`/`AFTER`/`INSTEAD`) is.

**Documented, not changed:** when safety principle S1 rescues a miscounted scan at a `/` line, its
span's content boundaries are already identical to a clean `EndedBy::SlashLine` close — both paths
share `end_at_slash_line`. A `/` line followed by a same-line comment (`/ -- note`) is *not* currently
treated as a lone `/` line by `is_lone_slash_line` (it requires the rest of the line to be
whitespace-only); this is consistent across every scan that calls it, but unverified against real
SQL\*Plus/SQLcl behavior, which may or may not tolerate a trailing comment there.

*Evidence:* `crates/sql-text/tests/corpus.rs` grew to 64 passing tests (43 named reproducers/round-1
regressions, 12 new round-2 MUST-FIX reproducers named `must_fix_1_*`/`must_fix_3_*`/`must_fix_4_*`
covering every starter shape and identifier-collision case listed above, 1 named
`additional_bug_*` for the fifth defect, plus the pre-existing corpus/fuzz suite) plus 1 remaining
`#[ignore]`d limitation (unchanged, J3); a new `crates/sql-text/tests/differential.rs` (1 test, the
grammar-based property above). `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
-- -D warnings`, `cargo test --workspace`, and `cargo test -p reldex-sql-text --release`
(63 `corpus.rs` tests including the 800,000-case extended fuzz run, 1 `differential.rs` test covering
96,000 generated scripts, both green) all pass; `RUSTDOCFLAGS="-D warnings" cargo doc -p
reldex-sql-text --no-deps` is clean.

### J6 — round-3 adversarial review: named gaps in the differential grammar, one bug found and fixed

A third review **approved** J5's fixes outright (0 failures in 550,000 of the reviewer's own
differential iterations and 6,000,000 fuzz cases, the differential oracle confirmed independent of
the splitter, S1 checks pass, complexity linear) but, before merge, listed specific shapes the J5
grammar did not generate — meaning a regression in any of them would not be caught by the in-repo
gate even though the gate was green. `crates/sql-text/tests/differential.rs`'s grammar was extended
with: `$IF`/`$THEN`/`$ELSIF`/`$ELSE`/`$END` directives wrapped around a statement, a whole nested
block, only the outer `BEGIN`, and only the closing `END`; an opaque-source (`CREATE ... AND COMPILE
JAVA SOURCE ...`) unit with Java text containing `;`/`{}`/`BEGIN`/`END`, generated only immediately
before a forced `/` line, plus a dedicated standalone test confirming that without a following `/` it
runs to `EndedBy::EndOfInput`/`terminated: false`; case-randomization applied to every keyword the
generator emits (`UPPER`/`lower`/`Title`/`MiXeD`, chosen per occurrence) plus keyword-named
identifiers (`Language`, `External`, `Before`, `After`, …) both quoted and as unquoted column
aliases; a forward declaration with a `CAST(... AS ...)`/`CASE ... END`/`x IS NULL` parameter default
nested inside *another subprogram's own* declare section (MUST-FIX #4's exact shape — previously only
generated inside a package spec, never inside a sibling subprogram's declare section ahead of its own
`BEGIN`); a guaranteed double-labelled nested-sibling-block shape; and a dedicated `gen_deep_body`
generator that always recurses (unlike the general one, which bottoms out at a leaf 2 times out of 3)
to force depth 3–5 nesting on every call, addressing the reviewer's note that deep nesting was
under-represented.

**One bug found while adding grammar coverage** (not by running the new grammar arms themselves, but
while reasoning through how to construct the "directive around only the outer `BEGIN`" case — found
and fixed before it could cause an in-repo gate failure): `collect_leading_words` — which walks a
statement's leading tokens looking for a `BlockStarter` match — treated a conditional-compilation
directive as neither a word nor trivia, so it stopped immediately at a directive appearing *before* a
block's own leading keyword, without matching what came after. `$IF $$flag $THEN\nBEGIN\n$END\n
NULL;\nEND;` was misread as three unrelated `Plain` statements instead of one `Block`. The same root
cause defeated `finish_with_maybe_slash`'s search for a following lone `/` line whenever a directive
sat between a block's closing terminator and that `/` (`END;\n$END\n/` stopped the search at `$END`,
producing `InferredBlockEnd` plus a stray extra span instead of one `SlashLine`-terminated block).

The first fix attempt widened the single, shared `is_trivial`/`skip_trivial` predicate (used at eight
call sites) to also treat a directive as trivial. This fixed both manifestations above but broke a
third thing no test yet exercised: `split_statements`'s own top-level loop uses that same predicate to
find where the *next* statement's `content_start` begins — and a directive there is real source text
belonging to that next statement, not gap content to be skipped past like whitespace. Widening it
silently moved `content_start` for any statement beginning with a directive to *after* that directive,
discovered only once the new grammar's "directive around only the outer `BEGIN`" units were actually
joined with a preceding statement in the differential test (which asserts exact content recovery,
unlike any test written for the original bug). The corrected fix keeps `is_trivial`/`skip_trivial` at
their original, narrower scope and adds a second, explicitly-named predicate,
`is_trivial_or_directive`, used *only* by `collect_leading_words` and `finish_with_maybe_slash` — the
two call sites that actually needed it — leaving the other six untouched. This is itself a small
instance of the round-2/round-3 pattern: a shared abstraction covering more call sites than a fix
actually needs is a wider blast radius than the bug it closes.

**A related, purely-in-test correctness note:** the "directive around only the closing `END`" grammar
shape places its own `$END` directive *after* the `END;` it wraps, so — same reasoning as the
opaque-source unit — which statement that trailing directive belongs to is undecidable from raw text
alone once no `/` disambiguates it (the same kind of ambiguity a stray comment between two statements'
terminators already has). The test forces a `/` immediately after this shape too, sidestepping the
ambiguity rather than asserting an arbitrary answer to an ill-posed question; this is a property of
the *test's* construction, not a splitter defect, and `StatementSpan::content` for this shape is
unaffected either way (it already stopped, correctly, right after `END`, before both its own `;` and
the trailing directive).

**A second, deliberately non-Oracle-shaped dialect** (`differential.rs::minimal_non_oracle`: no
`block_starters` at all, `;` the only terminator, no `/` handling) was added so vendor neutrality is
exercised *behaviorally* — `BEGIN`/`COMMIT`/`END` read as ordinary `Plain` statements when the dialect
supplies no block-starter data for them — rather than only by grepping `splitter.rs`/`lexer.rs` for
vendor literals.

**Open questions for M4.3 (script/statement execution), carried forward from J5's "Documented, not
changed" note above and now made explicit rather than left implicit:** two behaviors remain
unverified against a real SQL\*Plus/SQLcl client, and should be checked against one during M4.3's
integration tests before execution semantics are finalized:

1. A `/` line followed by a same-line comment (`/ -- note`) is not treated as an authoritative lone
   `/` line by `is_lone_slash_line` (it requires the rest of the line to be whitespace-only). Real
   SQL\*Plus/SQLcl may or may not tolerate a trailing comment there; this crate's current behavior is
   a reasonable, conservative reading, not a confirmed match.
2. A statement terminator (`;`) followed by a lone `/` line yields exactly one statement and does not
   re-run it (MUST-FIX #2, J5) — a deliberate product decision for a desktop editor, not a verified
   match for SQL\*Plus's own "re-run the buffer on a bare `/`" semantics, which are ambiguous for this
   case in the first place (re-run the previous statement? treat it as inert?).

Neither is a defect in the sense this review process looks for (both fail safely: at most an
over-conservative "not authoritative" reading, never an executable fragment carved from the middle of
a statement), but both are assumptions this crate makes without a real client to check them against,
and M4.3's script executor is the first consumer positioned to do so.

*Evidence:* `crates/sql-text/tests/corpus.rs`: 66 passing (3 new `dollar_if_*`/`additional_bug_a_leading_*`
regression tests for the `is_trivial_or_directive` fix, confirmed genuine by reverting the fix and
observing both directive-order-dependent tests fail with the exact predicted wrong span sequences)
plus 1 unchanged `#[ignore]`d limitation (J3). `crates/sql-text/tests/differential.rs`: 3 tests —
the extended grammar-based property (8 seeds, release mode 5,000 base scripts/seed × 3 join modes ×
2 line endings = 240,000 scripts; 60 base scripts/seed in debug), the dedicated opaque-source-without-
a-slash test, and the second-dialect behavioral test (4 seeds × 2,000 scripts = 8,000 more in
release). All three `differential.rs` tests together: 8.04s wall time in release. `cargo fmt --all --
check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, and
`cargo test -p reldex-sql-text --release` all pass; `RUSTDOCFLAGS="-D warnings" cargo doc -p
reldex-sql-text --no-deps` is clean.

## Amendment: the event reply path (2026-09-20, task M2.5)

The "Deferred, and deliberately not built now" section above promised that a per-session outbound
queue for the FFI adapter was additive: "both shapes coexist — the adapter would drain one queue
instead of holding many `Completion`s". This amendment is that work, built to
`docs/exec-plans/active/phase-1.md` §B2. **Nothing about the driver contract changes**; what changes
is where a `db-core` request's answer goes. Numbering: `E` = event path.

### E1 — a request's answer is a `ReplyTo`, and `Completion` is one of its two arms

The worker's per-request `Sender<DbResult<T>>` became `ReplyTo<T>`, which is either that same
oneshot or `{ session, request, sink }`. Every command, every error path and every resource rule is
shared between the two arms by construction, which is what stops the paths diverging (§B5's last
row). `Completion<T>`'s API and behaviour are unchanged, including the `worker_vanished` error a
dropped worker still produces, and its tests were not touched.

The core's public surface grows by: `RequestId`, `Waker`, `EventCaps`, `EventSink`, `EventQueue`,
`SessionEvent`, `CompletedOperation`, `event_channel`, `DatabaseSession::bind_events`, eleven
`DatabaseSession::submit_*` methods, `DatabaseSession::outstanding_requests`,
`SessionLimits::{with_,}max_outstanding_requests` and `EventQueue`'s own accessors
(`next`, `drain_into`, `wait_timeout`, `len`, `is_empty`, `set_waker`, `dropped_unsolicited`,
`pending_dropped_lines`, `waker_panics`, `caps`). `#![forbid(unsafe_code)]` was added to the crate
root while doing it, because the claim was being made in prose and nowhere else.

### E2 — exactly one reply per accepted request is structural, not bookkept

`ReplyTo::answer` consumes the channel, so "never two" is a type error. "Never zero" is `Drop`: a
reply channel dropped unanswered emits the session's terminal failure as it goes. That single
mechanism covers the worker exiting with commands still queued, a submit whose command could not be
delivered because the worker had already gone, and an unwind through the worker itself — and it
replaces the "set of requests we still owe" that `crates/ffi`'s interim pump has to maintain by hand
(ADR-0003 A14). The synthesised failure keeps the *shape* a success would have had, result id and
large-object handle included, so an adapter routing on those never special-cases failures
(ADR-0003 A21, applied to the core).

The one thing that can refuse a request is `SessionLimits::max_outstanding_requests` (default
1,024), checked before the reply channel exists. A refusal accepts nothing and produces no event,
because an event reporting that the queue is full would be circular. The per-session command channel
stays unbounded (K9): what is bounded is the reply *events*.

A request's slot is released when the **consumer takes its reply out of the queue**, not when the
worker produces it. Releasing on production was the first implementation and it bounded nothing: the
worker answers a `ping` in microseconds, so a submitter that retried on `Resource` could push a
queue nobody was draining to any length it liked (measured at 5,000 events with the counter reading
zero). The counter is therefore shared between the session and the queue, which carries an `Arc` of
it on the queued reply itself; the release is a single atomic performed under the queue's own mutex
during a pop — no second lock, no ordering edge, and no per-session lookup on the hot path. Putting
the slot on the event rather than in a per-session tally also makes the accounting hard to get
wrong: a reply cannot release a slot it was never holding, and a slot cannot outlive its event
whichever way the queue ends. Dropping the `EventQueue` ends the stream: everything in it is discarded, every
slot it held is released, and later events are discarded on arrival rather than accumulating where
nobody can read them. A session that outlives its consumer keeps working (it may still have a close
to run) and is bounded, from then on, by what its worker has genuinely not reached yet.

`submit_close` is the **one exemption**: it reserves against `max_outstanding_requests + 1`. Refusing
the single request that *shrinks* a session's footprint, on the grounds that the session has too much
outstanding, is backwards — and while nothing deadlocks without the exemption (`DatabaseSession::close`
and `Drop` answer through a `Completion` and reserve nothing), an event-driven adapter would have been
told it cannot ask a session to end. The exemption is exactly one: a second close while the first is
still undrained is refused like any other request, so the class grows the bound by one event per
session and no further. Per session, the queue therefore holds at most
`2 × max_outstanding_requests + max_unsolicited_per_session + 3` events — `R + 1` replies, `R`
`Executing`s, `U + 1` unsolicited and one `Terminal`.

Closing is idempotent, and stays idempotent when closes race. Only one of several concurrent closes
finds a worker to run; the rest reach a session that has already ended and are answered by `Drop`.
They asked the same question and the true answer is the same — the session is closed — so they
report `Ok`, exactly as `DatabaseSession::close` does on the completion path. A session that is
`Lost` still reports the failure: idempotency is not a licence to claim a clean close that never
happened (`SPEC.md` §10).

### E3 — `Terminal` is emitted at the transition, once, after everything accepted before it

`SessionShared` gained a one-shot flag. The worker emits `SessionEvent::Terminal` when the session
reaches `Lost` or `Closed`, and before doing so it answers every command already in its queue —
those requests were accepted before the transition, so rule 3 puts their replies ahead of the
announcement. A `Close` found in that queue is *run* rather than failed, because failing it would
leave the worker looping while `DatabaseSession::close` waited to join it.

What this does **not** promise, stated because the difference is real: a request submitted
concurrently with the transition may be answered after `Terminal`. That is rule 3's second sentence,
and it is the only honest guarantee available without blocking submission — which `SPEC.md` §11/§19
forbids. Nor is rule 1 a promise about *acceptance* order: failures a submit synthesises on its own
thread interleave with the failures the worker is producing as it drains, so a consumer routes
strictly by `RequestId` and retires a session's state on `Terminal` — never on "every earlier
request looks answered". `Lost` wins over `Closed` in the announcement, as K8 already had it.

Because there is one announcement and no second one, `bind_events` is **refused** (`Resource`) on a
session that has already emitted its `Terminal`. Binding late would hand a consumer a stream that
never terminates: every submit failing one by one with nothing to say the session is gone.

### E4 — a panic in the consumer's waker is caught, counted and survived

The waker is edge-triggered on empty → non-empty, is invoked with no queue or session lock held, and
is never invoked from inside a pop. `EventQueue::set_waker` takes the registration lock exclusively,
so it cannot return while a wake is running — the use-after-free ADR-0003 D5 rule 2 and spike
criterion K5 are about. Two consequences are part of the contract rather than accidents: a waker
must never call `set_waker` (read lock inside write lock is a self-deadlock), and a waker can run on
the thread that **submitted** a request, because a submit whose command cannot be delivered
synthesises its own failure event and may be the one that fills an empty queue. M2.11's re-entrancy
guard has to tolerate that. The mechanism now lives here, in `db-core`, rather than only in
`crates/ffi`; M2.11 deletes the FFI's copy and delegates.

A waker that panics is caught and counted (`EventQueue::waker_panics`), and the queue keeps working
with that waker still registered. The alternatives were rejected: letting it unwind would eventually
cross an FFI frame, which is undefined behaviour; poisoning the queue would turn a consumer's bug
into a stalled session; unregistering it silently would leave a UI that never hears about another
row and cannot tell why.

### E5 — the drop policy, and what may never be dropped

A reply event, `Executing` and `Terminal` are never dropped and never coalesced — they are bounded
instead, by E2's slot accounting (`R + 1` replies, counting the close exemption, and `R` `Executing`s
per session). Only the unsolicited classes are capped, per session, at
`EventCaps::max_unsolicited_per_session` (256):

* `TransactionStateChanged` reports a *state*, so an undelivered one for that session is updated in
  place to the newer value, and when the session has none queued the new one is admitted **even at
  the cap**. Both halves are needed: coalescing alone still loses the first change after a burst of
  `ServerOutput` has used up the session's allowance, and "a transaction is open" is exactly the
  fact that must never be lost — a UI that misses it shows Commit and Rollback disabled over a live
  transaction. The class therefore costs at most one event per session above the cap. Nothing is
  lost and no drop is counted; the cost is that a coalesced value is delivered at the earlier of the
  two positions. It is advisory — `close` still re-decides on the worker (K4) and
  `has_possibly_active_transaction()` is authoritative — so paying position to never lose the state
  is the right way round.
* `ServerOutput` (whose producer is M2.7) is dropped at the cap, and the drop is reported rather
  than hidden: the incoming event is the one refused, so what is already queued survives, and its
  line count is added to that session's pending count and delivered as `dropped` on the next
  `ServerOutput` that session does get through. A session that goes quiet or ends with lines still
  owed has no next one, so the count also stays readable through
  `EventQueue::pending_dropped_lines(session)`. `EventQueue::dropped_unsolicited` counts refused
  events for diagnostics and never resets.

Dropping the *oldest* queued event instead was considered and rejected: it is O(n) in a shared FIFO,
and it discards the beginning of a PL/SQL run, which is where the error usually is.

### E6 — what the event path costs

Measured on the dev machine, release, against the mock driver; information only, not a claim.
A `ping` round trip through the event path costs **432–537 ns** with one producer session and
**351–391 ns/event** with eight, against **2,043–2,215 ns** and **328–410 ns** for the same round
trip through `Completion` — the single-session case improves because the submitter no longer parks
on a reply. A counting global allocator in its own test binary
(`crates/ffi/tests/core_event_cost.rs`, hosted there because `crates/ffi/tests/fences.rs` restricts
the `unsafe_code` opt-out to the FFI boundary) measures **0.033 allocations per event** once warm,
all of it the command channel's own block amortisation: carrying an event through the queue
allocates nothing beyond the event. The full numbers, and the 1M-row FFI figures either side of the
refactor, are in `docs/exec-plans/active/phase-1.md` beside M2.5.

Those per-event figures **predate the reply-slot accounting** added in review round 1 (see E2). With
the slot on the queued event, independent runs on a quiet machine measure 412–563 ns at one producer
and 408–436 ns at eight — the eight-producer case ~10% above the number above, which is what the
slot costs: one `Arc` clone on the push, one `Arc` drop and one `fetch_update` on the pop. Runs on a
loaded machine came out proportionally higher across *every* benchmark, including ones M2.5 never
touched, so treat all of these as shape rather than as figures to compare across machines.
Allocations per event are unchanged at 0.033. Still information only, still not a claim.

### E7 — an idempotent close reports *how* the session ended, not merely that it has

Closing an already-closed session succeeds (K2's spirit, applied to the session rather than to a
cursor). That rule was implemented as "a close has run, therefore report success", stored as one
boolean on `SessionShared` and read by `DatabaseSession::submit_close` — and "a close has run" is
not "a close succeeded". The worker sets that flag on **both** of its ends: the clean one, and the
one where the connection was already gone and the close truthfully reported the loss. So a close
submitted after a *lost* session had already been closed once could answer `Ok(())`, telling the
caller their `Commit` had happened when nothing had been committed. Intermittent — it needed the
worker to reach the flag between two submits on the caller's thread — and found by CI on
`event_terminal.rs::a_close_after_a_lost_session_still_reports_the_loss` (3/1000 on the M2.5
baseline). It is a latent M2.5 defect, not an M2.6 regression, and it is the loss `SPEC.md` §10
exists to prevent, told backwards.

**Decided: there is one record of how a session ended, and every "this session is already over"
answer is derived from it.** `SessionShared` stores `EndedAs`, written once on the worker thread at
the point the session ends, first writer wins:

- `Cleanly` — an explicit `close` reached the end of the close path on a live connection. Getting
  there means the caller's transaction was resolved as asked or there was none to resolve: the only
  other ways out of the disposition step are `DecisionRequired`, `CommitFailed` and
  `RollbackFailed`, and all three leave the session **open**. A failure from
  `DatabaseConnection::close` itself does not change the classification — it is a report about
  releasing the connection, not about the user's data, and it was already delivered to the close
  that caused it.
- `Unresolved` — everything else: the session was already lost when the close ran, it was abandoned
  (which resolves nothing, by design, K5), or its worker ended without any close at all.

`SessionShared::settled_close()` turns that into the answer, and all three call sites use it —
`submit_close`'s short-circuit, `DatabaseSession::close`'s "there is no worker left to ask" paths,
and `CloseReplyTo`'s `Drop`. **Idempotency answers "this session is already over", never "your
commit happened."**

This changes one documented behaviour: closing a **lost** session a second time now reports the
loss again — with the original error's kind and native code — instead of returning `Ok(())`. The
first close already said so; saying it once and then claiming success is worse than either
consistent answer. Nothing runs on the second call either way, so the close is still idempotent in
the sense that matters: it changes nothing, and it never reaches the driver. Closing a session that
a close really did end cleanly still succeeds, however many times it is asked.

*Evidence:* `event_terminal.rs::a_close_after_a_lost_session_still_reports_the_loss` and its
deterministic twin `…::a_second_close_after_a_lost_session_reports_the_loss_deterministically`
(which forces the interleaving by waiting for the first close's reply rather than hoping for it),
against `…::concurrent_closes_all_report_success_on_a_cleanly_closed_session` for the other
direction; both looped 3,000 times with no failures. `close_disposition.rs::close_is_idempotent` and
`…::a_failing_connection_close_is_reported_but_the_session_is_gone` pin the clean side on both reply
paths; `session_loss.rs` pins the lost side.

## Amendment: the session registry, a non-blocking open, and `abandon` (2026-09-21, task M2.6)

Built to `docs/exec-plans/active/phase-1.md` §B3, on top of the event path (E1–E6). **Nothing about
the driver contract changes** — no new method, no new bound, no new rule for a driver implementer.
What changes is who waits for a `connect`, and what "give up on one" means. Numbering: `R` =
registry.

### R1 — `spawn` stops waiting, and the connect's outcome becomes a `ReplyTo`

`worker::spawn` used to create the thread and then block the caller on a ready channel; it now
returns the instant the thread exists, carrying only the two things that exist before a connection
does (the command channel, the thread handle). The connect's outcome is delivered through a
`ReportOpen` callback the caller supplies, which runs **on the worker thread** the moment `connect`
returns and answers one question: is this session adopted, or was it given up on?

- `SessionManager::open_session` passes a callback that sends down a channel it is parked on. Its
  behaviour, its errors and its tests are unchanged, including "the caller gave up, so close the
  connection cleanly" — that case is now spelled `Adoption::Abandoned` instead of a failed send.
- `SessionRegistry` passes one that hands the outcome to the registry.

`SessionShared` is now built **by the caller** and passed in, already bound to its `EventSink`
(`phase-1-m2-5-event-queue.md` §6 asked for exactly this). That is what lets the registry emit a
session's `Opened`/`OpenFailed`/`Terminal` through the same per-session emit lock as everything
else, before any worker exists, so ordering rule 1 holds for a session's very first events.

The connect reply is an ordinary `ReplyPayload` (`OpenedSession`), so the open is a request like any
other: it reserves a slot, `answer` consumes its channel, and a channel dropped unanswered emits
`OpenFailed` from the same `Drop` that gives every other request its "never zero replies" (E2).
`abandon` needs no bespoke path — it drops the reply.

### R2 — `SessionRegistry` is the one place open, abandon, close and a finishing connect are ordered

Four states, one mutex: `Opening` (the worker is inside `connect`; no handle exists, so `get`
answers `None` and nothing can be submitted), `Open` (`get` hands out the `Arc<DatabaseSession>`),
`Ending` (abandoned; the handle is kept so that nothing is *dropped*, and therefore nothing waited
on, inside `abandon`) and `Ended` (announced, no handle — the tombstone a late connect finds).

Two properties make it reviewable rather than merely tested:

- **No event is emitted and no driver call is made while the lock is held.** Every entry point
  decides under the lock, releases it, and only then emits or calls the driver — the discipline
  M2.5 established for `SessionShared` (E3/E4). Lock order is registry → per-session emit → queue,
  and never the other way.
- **The lock *is* held across `thread::Builder::spawn`**, deliberately. A connect can finish before
  `spawn` has returned, and the entry that owns its reply must already be recorded when it looks.
  The alternative — a placeholder entry filled in afterwards — reintroduces the race it was meant
  to remove. It is not free: `spawn` is a syscall, so `open` serialises on it and a concurrent
  `state()` waits behind it. Measured (2026-09-21 review, Windows debug build): at a burst of 400
  opens the worst `open` took **48.9 ms** and a concurrent `state()` waited **61 ms**; at the rate
  an application actually opens sessions — one per worksheet — both are microseconds. Accepted at
  that price, and worth revisiting only if a workload ever opens sessions in bursts.

`open` therefore **never blocks and never fails synchronously**: it returns a `SessionId`, and even
"the thread could not be spawned" is reported as that session's `OpenFailed` plus its `Terminal`. A
caller holding an id needs one answer, not two shapes of answer.

### R3 — every session the registry names produces exactly one `Terminal`

Including one that never opened. A connect that failed — or a worker thread that would not spawn —
announces `Lost` with that error as the cause (kind, native code and cause chain intact, K8); an
open that was abandoned announces `Closed`, because the abandon won and nothing failed. Only a
deliberate end reports `Closed`. Both follow that session's single `OpenFailed`, which is ordering
rule 3 applied to the one request such a session ever had.

That ordering is why the worker emits **nothing** when its report comes back `Abandoned`: whoever
refused the session has already produced the `OpenFailed`/`Terminal` pair, and a "backstop"
announcement on the worker thread would race it and could deliver `Terminal` first.

That completes the rule an adapter needs: **retire per-session state on `Terminal`, and on nothing
else**, for every session, not merely for the ones that got far enough to open.

### R4 — `abandon` answers now and closes late; it never waits and it is never refused

A `connect` cannot be interrupted — that is what forced the driver's own helper thread (H1/H2,
spike U-15) — so abandoning one cannot mean waiting for it:

1. The open's one reply is produced **immediately**, as `OpenFailed { ErrorKind::Cancelled }`.
   `Cancelled` and not `Connection`: nothing failed and nothing was ever connected, and a UI that
   cannot tell "I gave up" from "the listener is down" will say the wrong thing.
2. The session's `Terminal { Closed }` follows it.
3. The worker thread is **detached, never joined** (ADR-0003 A17 applied to the connect). When the
   connect finally returns it finds the tombstone, and — this is the part that must not be skipped
   — **closes the connection on its own thread**, the only thread allowed to touch it (D1/D2, H2).
   A late success therefore never produces an `Opened` after an `OpenFailed`, and never leaves a
   live database session on the server.

On a session that is already **open**, `abandon` is `Drop`'s abandon without `Drop`'s wait: request
the cancel, queue `CloseIntent::Abandon`, return. It **never commits and never rolls back
explicitly**; the server's own rollback-on-disconnect resolves whatever transaction the session
held, which is what makes "abandoning cannot commit" structural rather than a promise (K5). That
*is* a transaction loss, and `SPEC.md` §10 forbids hiding it — which R6 is about. `abandon` returns
`Abandoned::Open { transaction_possibly_lost }` so a caller can warn **immediately**, but that value
is a documented **lower bound**, not the verdict: it is `has_possibly_active_transaction() ||
outstanding_requests() > 0 || a driver call is in flight`, widened to those three because any of
them can open a transaction after the snapshot is taken. The verdict is on `Terminal` (R6). §B3
sketches `abandon` as returning unit; it returns this instead, because a caller that cannot see
which case it hit cannot report the one that costs the user something.

**`abandon` reserves no request slot, so it can never be refused for lack of one.** Neither event it
produces is a new request's reply: the `OpenFailed` answers the open, whose slot `open` reserved,
and `Terminal` is not a reply. The alternative the M2.5 note allowed — reserving above the limit,
like `submit_close` — was not needed and would have grown the published bound for nothing. The
bound is unchanged at **`2R + U + 3` events per session** (`R + 1` replies counting the close
exemption, `R` `Executing`s, `U + 1` unsolicited, one `Terminal`), with the open sitting *inside*
`R`: it reserves an ordinary slot on a session that has nothing outstanding, so it always fits.
Stated in `events.rs`, `session.rs`, §B2 and here, and probed by
`registry_abandon.rs::abandon_is_never_refused_at_the_request_cap` (a limit of one, consumed by the
open itself, on a session that has not even connected) and
`…::abandon_is_never_refused_when_an_open_sessions_replies_are_undrained`.

Dropping the registry does all of the above for every session it still holds: connecting ones are
answered, announced and detached; open ones are told to abandon *first*, all of them, and then
waited for against **one deadline shared by the whole teardown**.

That bound is `DROP_SHUTDOWN_TIMEOUT` **in total**, not per session, however many sessions are
stuck. Issuing every abandon before waiting for any of them is only half of it: the wait itself has
to happen in the teardown, against the shared deadline, and take each session's worker handle with
it, so the `DatabaseSession::drop` that follows has nothing left to wait for. (The first
implementation issued the abandons and then dropped the sessions, which parked `Drop` on its own
fresh timeout each time — measured by the 2026-09-21 review at 509 ms for one stuck session,
1.016 s for two and 2.015 s for four, exactly the serial cost this claim denied.) The honest
residual, because A17 says to state it: a worker inside an uninterruptible driver call is
**detached** at the deadline, and keeps its connection until that call returns.

### R5 — the initial transaction state is seeded silently

`db-core` seeds the driver's real `transaction_state()` right after `connect`, so closing an
untouched session does not demand a disposition it cannot need. On the event path that seeding used
to emit a `TransactionStateChanged` — **before** the session's own `Opened`, because the registry
binds the sink before the worker exists. `TransactionStateChanged` reports a *flip* of
`has_possibly_active_transaction()` (§B2), and a session that did not exist a moment ago has not
flipped anything, so the seed is now silent.

A consumer takes the initial value from `has_possibly_active_transaction()` when it sees `Opened` —
which is authoritative anyway — and the stream reports every change from there. Assuming the
conservative default until it asks costs at most an extra close prompt, which K7 already accepts;
the opposite mistake is a silent commit, which it does not. The `Completion` path is unaffected: it
has no sink bound at that moment, so nothing was ever emitted there.

### R6 — `Terminal` carries `transaction_possibly_lost`, decided on the worker thread

`SessionEvent::Terminal` gains a fourth field: whether the session ended while it may still have
held an unresolved transaction. **It is the authoritative answer and a consumer must surface it**
(`SPEC.md` §10: never silently commit or hide transaction loss).

It has to live here, and nowhere else, because it is the only place that can be right. K4 already
established that `close` decides on the worker thread *after* every command queued ahead of it has
run; the same reasoning applies to every other way a session can end, and there are five of them —
`abandon`, `retire` of an open session, the registry's teardown, `DatabaseSession::drop`, and a
connection that died — of which four said nothing at all before this. Any answer a control thread
reads is a snapshot that a statement already in the queue can invalidate. The review's repro:
a statement parked in the driver, an `INSERT` queued behind it, `abandon` on the caller's thread
reads "no transaction", the worker then runs the `INSERT`, reaches the abandon, and the server
rolls it back. The loss was real and nobody was told.

The value, at the point the session ends:

| How it ended | `transaction_possibly_lost` |
| --- | --- |
| `close(Commit)` that succeeded | `false` — the user decided and it happened |
| `close(Rollback)` that succeeded | `false` — a rollback the user *chose* is a decision, not a loss |
| `close` with no transaction to resolve | `false` |
| `close` whose disposition failed (`CommitFailed`/`RollbackFailed`) | no `Terminal` — the session stays open (K4) |
| `close(None)` with a transaction open | no `Terminal` — `DecisionRequired`, the session stays open |
| `abandon` of an open session | `true` if a transaction may be open when the worker reaches it |
| `retire` of an open session, and `DatabaseSession::drop` | same — they are the same lossy drop (K5) |
| the registry's teardown | same |
| connection lost while idle (found by the revalidating ping) | `false` when nothing was open — the common dropped connection must not invent a loss |
| connection lost **by** a driver call (execute, rollback-to-savepoint, commit, rollback) | `true`: the statement may have reached the server, so the driver's cached state is not trusted and `Unknown` is recorded instead |
| any of the above on a driver with `exact_transaction_state == false` | `true`, because `Unknown` reads as "may be open" (K7) |
| a session that never opened (connect failed, spawn failed, open abandoned) | `false` — nothing was connected |

Two asymmetries hold this together, and both are deliberate.

"The disposition succeeded" is recorded as a fact on the worker when it happens, not re-derived from
the driver afterwards: a driver that reports `Unknown` would otherwise say "may be open" forever,
and turn a clean `close(Commit)` into a reported loss.

And `DatabaseConnection::transaction_state` is a cache the driver last refreshed on a call that
*returned*, so the three arms that re-read it after a failure must not trust it when the failure
carries `SessionState::Lost`: an `INSERT` that reached the server and then lost its connection
leaves an exact driver still reporting `Inactive`, which would report "no loss" for work the server
rolled back (found by the 2026-09-21 delta review). Those arms record `TransactionState::Unknown`
instead. This is **not** "lost implies a lost transaction": the revalidating-ping path never touches
the driver's transaction state, so a session lost while idle with nothing open still reports
`false`. Nor is it classified by statement kind — the kind lives on an `ExecuteOutcome` a failed
call never produced, so `Unknown` is the honest answer the core actually has. The conservative
default stands everywhere else: an extra warning costs a sentence, a missed one costs the user's
work.

### R7 — `request_cancel` after `close` must be a harmless no-op

A doc-only addition to the `CancelHandle` contract, and the one that made it necessary: the
registry's teardown asks a session to abandon, and a session may already have ended — its
`DatabaseConnection::close` already called — by the time it does. `db-core` now skips the cancel
when it can see the session has ended, but that check is a race narrowed, not closed, in exactly the
way K3 describes for cancels generally. So the contract says it outright: **`request_cancel` on a
handle whose connection has been closed must return without panicking and without touching the
closed connection.** Returning an error is allowed; `Ok(CancelOutcome::Requested)` is allowed; doing
nothing is expected. A driver that cannot make that safe must keep whatever state the handle needs
alive independently of the connection (the handle is already `Arc`-shared and outlives it).

*Evidence:* `crates/db-core/tests/registry_open.rs` (10 tests: success with connection id, cancel
kind and connect warnings; a driver failure with its native code preserved; a `connect_timeout`
expiry classified `Connection`; a panicking `connect` contained as `DriverInternal`; the open's
slot; no handle while connecting; `open` returning with the connect still parked; 32 concurrent
opens with per-session order; retirement; and the adversarial probe that submits the instant `get()`
answers, which still cannot get a reply ahead of `Opened`), `crates/db-core/tests/registry_abandon.rs`
(18 tests, every interleaving forced with the mock's connect gate rather than hoped for: abandon
before a late success and before a late failure, both orders of the race looped 60 times under real
contention with alternating spawn order and a floor that fails a run which only ever saw one order,
abandon after open, abandon twice, abandon at the request cap, abandon of a busy worker, the
registry dropped mid-connect and with open sessions, the one-deadline teardown with eight stuck
sessions, 200 abandoned opens that nobody retires leaving an empty map, the queue dropped
mid-connect, and retirement of a connecting session) and
`crates/db-core/tests/registry_transaction_loss.rs` (12 tests: the whole R6 table, including the
review's parked-statement repro). The whole `db-core` event and registry set: 30 solo runs and
2 concurrent loops, no failures. `reldex.h` is byte-identical (`gen-header.sh --check`), because the
C ABI is M2.11's.

## Amendment: server output (2026-09-24, task M2.7)

Built to `docs/exec-plans/active/phase-1.md` §B4.2, which lists server output as one of the three
additive contract items Phase 1 needs. **The driver contract grows by one capability flag and two
defaulted methods**, so existing drivers need no code at all. `db-core` grows by one request, one
event field and one reply. Numbering: `T` = text output. §B4.2's own interpretation notes ("B4.2 as
implemented") quote the plan sentence each decision below departs from.

### T1 — the contract: a typed setting, a bounded read, and `Unsupported` by default

```rust
// db-driver-api::server_output
pub enum ServerOutputBuffer { Unlimited, Bytes(NonZeroU32) }
pub enum ServerOutputSetting { Disabled /* default */, Enabled(ServerOutputBuffer) }
pub struct ServerOutputChunk { /* lines: Vec<Box<str>>, drained: bool */ }

// db-driver-api::session
Capabilities::server_output() -> bool                       // `with_server_output(bool)`; false in `none()`
DatabaseConnection::set_server_output(&mut self, ServerOutputSetting)
    -> DbResult<ServerOutputSetting>                         // default: Err(Unsupported)
DatabaseConnection::take_server_output(&mut self, max_lines: NonZeroUsize, max_bytes: NonZeroUsize)
    -> DbResult<ServerOutputChunk>                           // default: Err(Unsupported)
```

§B4.2 sketched `set_server_output(enabled, buffer)` and `take_server_output() -> Vec<Box<str>>`.
Both were changed:

* **One enum instead of a pair.** "Off, with a buffer of 0 bytes" is representable as a pair and
  meaningless. `Bytes` is `NonZeroU32` because a zero-byte buffer is not a setting.
* **`set` returns the setting in force.** Oracle's `ENABLE(n)` clamps `n` into
  2,000..=1,000,000 bytes and says nothing (measured on 19.3: `ENABLE(100)` overflows at 2,000
  bytes, `ENABLE(2_000_000)` at 1,000,000). A UI that shows the size it asked for would show the
  wrong one.
* **`take` is bounded and says whether it emptied the buffer.** An unlimited buffer read into one
  `Vec` would have no bound at all. `ServerOutputChunk::is_drained` lets the caller stop without
  an extra call when the driver knows the buffer is empty.

Rules for an implementer, stated on the trait:

* Each call is **one round trip**.
* Neither call is a user statement, so neither changes the connection's transaction tracking.
* A chunk never splits a line.
* When any line is buffered, a chunk returns **at least one**, even when that line alone exceeds
  `max_bytes`. So `max_bytes` is a target, and a caller always makes progress.
* A chunk may hold one line beyond `max_bytes`: the line that did not fit. A driver may learn a
  line's size only by taking it from the server, and a taken line cannot be put back. A caller
  bounding memory therefore allows `max_bytes` plus one maximum-length line.
* An empty chunk means drained.
* Only complete lines are returned. A partial line (a `PUT` with no line end) is not invented.
* An empty line is an empty string.

Vendor SQL lives only in the driver (invariant 2).

### T2 — Oracle: one packed block per round trip, because `GET_LINES` is out of reach

`crates/drivers/oracle-thin/src/server_output.rs` holds every `DBMS_OUTPUT` statement. Here is
what was tried, in order:

1. **`DBMS_OUTPUT.GET_LINES` with an array OUT bind: not available.** `GET_LINES` returns a
   `DBMS_OUTPUT.CHARARR`, a PL/SQL index-by table, and `oracledb` `=26.0.0-beta.3` has no
   collection binds. `Metadata::is_array` is set only for DML `RETURNING`, and
   `BindParameters::Slice` is the `executemany` row array.
2. **`GET_LINE` once per call: rejected.** It costs one round trip per line, so 10,000 lines
   need 10,001 round trips. §B4.2 and the brief rule it out.
3. **Pack many lines into one `VARCHAR` OUT bind: rejected.** A pure OUT `VARCHAR` bind is sized
   from the type's 4,000-character default. The server refuses more than 16,000 bytes into it
   (`ORA-06502`), which is less than one legal `DBMS_OUTPUT` line (32,767 bytes).
4. **Chosen: the same block with `LONG` OUT binds.** A `LONG` OUT bind has no client-side ceiling
   below PL/SQL's 32,767 bytes. The block loops `GET_LINE` **on the server** and appends each
   line to a `VARCHAR2(32767)`, prefixed by its length as five ASCII digits. The length is
   `LENGTH4`, in code points, which is what a Rust `char` counts. Framing is by length, never by a
   delimiter, because a line can contain anything, including `\n`, NUL or digits. A line the
   server has handed out cannot be put back, so the one line that does not fit comes back
   **unframed in a second bind**, and the block stops. Every call therefore makes progress, and a
   32,767-byte line always fits. The block also returns the line count and whether `GET_LINE`
   reported the buffer empty.

   The Rust side validates the framing strictly: five digits, then exactly that many characters,
   and exactly the count the server reported. Any mismatch is `ErrorKind::DataConversion` on a
   still-usable session. The lines of that one read are lost and reported (T5). Nothing is
   guessed.

   Every call names the package `SYS.DBMS_OUTPUT`. An unqualified name resolves through the
   user's schema first, so an object called `DBMS_OUTPUT` there would capture the call. This is
   deliberately **not** tested on the shared test database.

   **Fixed by M2.12 (2026-09-24).** The packed value used to be decoded as a whole by the
   crate's strict UTF-8 conversion, so a single line that was not valid UTF-8 failed the entire
   read, up to 4,096 good lines with it — reproduced live with
   `PUT_LINE(UTL_RAW.CAST_TO_VARCHAR2('41FF42'))` between two good lines, which lost all three
   to `DataConversion`. The framing also assumed that `LENGTH4` equalled Rust's `char` count,
   true only for a Unicode database character set; on a single-byte one a chunk could exceed
   `max_bytes` by about 3× once decoded.

   `crates/drivers/oracle-thin/src/server_output.rs` now packs `RAW`, not `VARCHAR2`. Each line
   is converted to UTF-8 bytes on the server **before** it is measured or packed —
   `SYS.UTL_I18N.STRING_TO_RAW(line, 'AL32UTF8')` — so the five-digit prefix that follows is
   always a byte count in UTF-8, on any database character set; `LENGTH4` is gone. Both binds
   (packed buffer and tail) are `LONG RAW` rather than `LONG`, for the same "no client-side
   ceiling below 32,767 bytes" reason `LONG` was chosen in the first place —
   `DB_TYPE_LONG_RAW`'s `buffer_size_factor` is the same `2147483647` as `DB_TYPE_LONG`'s. This
   also sidesteps the defect directly: `oracledb`'s decode path for `RAW`/`LONG RAW`
   (`ORA_TYPE_NUM_RAW | ORA_TYPE_NUM_LONG_RAW` in `db_value.rs`) is a plain byte copy with no
   UTF-8 validation at all, unlike the `LONG`/`VARCHAR2` path it replaces. Decoding — and
   recovering from an invalid line — is now this module's own job, one line at a time: a line
   whose bytes are not valid UTF-8 is still delivered, with the invalid sequences replaced by
   U+FFFD, and counted in the new `ServerOutputChunk::invalid_utf8_lines()` rather than
   silently accepted or dropped. `ServerOutputChunk` gained that one field, additively
   (`with_invalid_utf8_lines`, defaulting to zero). `db-core`'s own fix round (this branch) carries
   the count the rest of the way rather than discarding it at that boundary: it is now a field on
   both `SessionEvent::ServerOutput` (per event) and `ServerOutputLog` (cumulative across the
   completion-path log, counted in full even for lines the log's bound later refuses) — see
   `phase-1-m2-5-event-queue.md` §7.5 for what remains, which is only M2.11 mapping the field
   across the C ABI.

   **Verified against the live test database, not assumed.** `SYS.UTL_I18N.STRING_TO_RAW`
   exists and is executable by `RELDEX_TEST` on 19c, and passes an already-malformed
   AL32UTF8 byte sequence straight through unchanged rather than raising or repairing it —
   `STRING_TO_RAW(UTL_RAW.CAST_TO_VARCHAR2('41FF42'), 'AL32UTF8')` returns `41FF42` — which is
   exactly what lets the invalid line reach Rust for per-line recovery instead of being caught
   (or silently fixed) on the server. The review's exact reproduction now returns all three
   lines, with the middle one `A\u{FFFD}B` and `invalid_utf8_lines() == 1`, and the session
   stays usable. `NLS_CHARACTERSET` on that database is `AL32UTF8`, confirmed by query, which
   is this module's tested and documented configuration (see the residual limit below).

   **Residual limit, accepted rather than solved.** A PL/SQL `RAW` local variable is capped at
   32,767 bytes, the same ceiling `DBMS_OUTPUT` imposes on one line. On `AL32UTF8` that is a
   non-issue: the database already stores UTF-8, so `STRING_TO_RAW` is a byte-identical no-op
   and a line already ≤ 32,767 bytes stays that size. On a single-byte database character set, a
   line whose non-ASCII repertoire fills that limit can expand past it once converted to UTF-8;
   the block then fails with a PL/SQL numeric/value error, surfaced as this call's `DbError` —
   reported, not a silent truncation and not a process abort, but also not solved. Solving it
   would mean streaming the conversion through a LOB rather than a scalar `RAW`, out of scope
   for M2.12.

   The round-trip cost is unchanged: the same 10,000-line fixture still drains in 5 round trips
   at db-core's default chunk sizes, measured on both the M2.7 and the M2.12 test files against
   the same database. `max_bytes` is now honoured in exact UTF-8 bytes rather than
   database-charset bytes or characters: measured with 40 lines of 750 UTF-8 bytes each (Thai)
   at `max_bytes = 2,000`, every chunk's packed lines stayed within budget, and only a chunk's
   own trailing unframed line — never more than one line — was allowed to exceed it
   (`tests/m2_12_server_output_raw.rs`).

**Measured** (`tests/m2_7_server_output.rs`, round trips counted by the server in `v$mystat`
"SQL*Net roundtrips to/from client", net of the measuring query):

* 10,000 short lines (138,894 packed bytes) drain in **5 round trips** at the core's default
  chunk sizes (4,096 lines / 32 KiB). Every `take_server_output` call is exactly one round trip.
* During the design probe, 10,003 lines including a 32,767-byte line and an empty line took
  **6 round trips**.
* A drain costs ⌈packed bytes / ~32 KiB⌉ round trips, plus one when the last chunk ended on its
  line or byte limit, or on a tail line. That extra call is needed because the block cannot learn
  that the buffer is empty without calling `GET_LINE` once more.

**`DBMS_OUTPUT` behaviour, measured on 19.3 (AL32UTF8) and asserted by those tests:**

* Thai, emoji (including ZWJ sequences and flags), combining marks, and two 32,767-byte lines
  (three-byte Thai and four-byte emoji) come back byte for byte at chunk sizes (4096, 32 KiB),
  (3, 64) and (1, 1).
* `PUT_LINE('')`, `PUT_LINE(NULL)` and `NEW_LINE` are each one empty line.
* A `PUT` with no line end is never returned. Once a read has happened, `DBMS_OUTPUT` discards the
  unfinished part at the next `PUT`, as in SQL*Plus. So a statement that ends with an unfinished
  `PUT` loses it, and this is reported here rather than worked around.
* Overflow is `ORA-20000` with `ORU-10027` in the message. It is **the statement's error**, with
  its native code preserved and the session usable. The lines written before it are still
  buffered, and they drain.
* A block that raises keeps what it printed.
* `DISABLE` purges the buffer.
* Output written while output is off is discarded by the server, not buffered.
* Enable and disable are one round trip each.
* Output a function writes while rows are fetched stays buffered and comes back ahead of the next
  statement's own lines.

### T3 — the core: a per-session switch, set by an ordinary request

The worker holds a `ServerOutputSetting`, initially `Disabled`. `DatabaseSession::submit_set_server_output(request, setting)`
and `set_server_output(setting) -> Completion<ServerOutputSetting>` are the two shapes of one worker
command. `both_paths!` tests them alike. The event-path reply is
`SessionEvent::ServerOutputConfigured { session, request, result }`, which is an ordinary reply
with an ordinary slot. The effects:

* One driver call, so one round trip.
* On a driver without the capability, `Unsupported` with **no** driver call.
* On success the worker stores the setting **in force** that the driver returned.
* A failed call leaves the setting unchanged and goes through the loss path every driver call
  takes (`note_error`, then `note_transaction_state_after`).

### T4 — the drain rule: after every `execute`, before its reply

While the setting is enabled, the worker reads after **every `execute`, whether it succeeded or
failed**. It reads in chunks of `SessionLimits::server_output_chunk_lines` (4,096) and
`server_output_chunk_bytes` (32 KiB) until a chunk is drained or empty. Each chunk is delivered as
it arrives, and only then is the statement's reply sent. For one event-path execute the stream is:

`Executing` → `TransactionStateChanged` (if the state flipped) → `ServerOutput`* → `Executed`

Checked against the five ordering guarantees of `phase-1.md` §B2 (E1–E3):

1. *Production order per session.* The reads run on the session's one worker thread, between the
   statement and its reply, so no other request's events can interleave.
2. *Exactly one reply per request.* This is the reason for **before**. A read that fails is
   reported while the reply is still unsent, on the output stream (T5), so it needs neither a
   second reply nor a change to the first one. Reading *after* the reply would leave a failed read
   with nowhere to go except a second answer, or silence.
3. *`Terminal` exactly once, after the queued replies.* A read that loses the connection sets the
   lifecycle to `Lost`, the reply still follows, and `Terminal` comes after it as for any loss.
4. *`Executing` precedes `Executed`.* Unchanged. The output sits strictly between them.
5. *No cross-session order.* Unchanged.

The second reason for **before** is attribution. A `ServerOutput` carries no request id, and needs
none: every `ServerOutput` lies inside exactly one execute's `Executing`…`Executed` window, and
normally belongs to that execute. There are **two exceptions**, listed below: output written during
a fetch, and output of a statement that left the session needing validation. In both, the lines
arrive inside the *next* execute's window, ahead of that execute's own lines. They are delayed,
and they are misattributed, but they are not lost. On the completion path, "before" means the
output is already in the log when `Completion::wait` returns.

**Not read** after a fetch, commit, rollback, savepoint, rollback-to-savepoint, ping or close:

* None of those runs user PL/SQL, except a fetch that calls a printing function. That output stays
  buffered and arrives inside the next execute's window (measured, T2).
* A read after every fetch batch would add a round trip to the hot path of every large result,
  for output that is almost never there.

Also **not read** when:

* the session is not `Usable`. A `Lost` session has no connection, and a `NeedsValidation` session
  must be pinged before its next use, which that next command does. This is the **second
  exception**: a statement that fails with a `NeedsValidation` error (for example `Timeout`)
  leaves its output on the server. That output is read after the next execute, inside its window
  (`tests/server_output.rs::output_of_a_statement_that_needs_validation_arrives_in_the_next_executes_window`).
  Reading at once was rejected, because it would need a ping first, which holds back the error
  reply on a connection that may be dead. On Oracle today the case is mostly moot: a call timeout
  during a blocked call loses the session, and the output goes with it;
* an abandon has been requested. `begin_abandon` sets a flag **before** it requests the cancel,
  and the drain checks it before every read. A read already in flight completes, because it cannot
  be interrupted, and no further read starts.

### T5 — failure semantics

* **A failed read never masks, delays or replaces the statement's result.** On the event path it
  is `ServerOutput { lines: [], failure: Some(err) }`, and the drain stops. On the completion path
  it is `ServerOutputLog::failure` (the first failure) and `failures` (all of them). Then the
  statement's own reply goes out unchanged, success or failure.
* **A read that finds the session lost takes M2.6's path.** `note_error` moves the lifecycle to
  `Lost`, and `note_transaction_state_after(Some(err))` records `Unknown` (R6). The reply follows,
  and then `Terminal { Lost, transaction_possibly_lost }`.

  **This deliberately over-reports on an exact-state driver.** A take cannot change the
  transaction (T1). So after, say, a `SELECT` on a driver whose transaction state is exact and
  `Inactive`, a lost read still ends in `transaction_possibly_lost: true`, a false positive. The
  code is kept this way on purpose. It is the same rule every driver call follows (R6: a call that
  lost the connection is not trusted to have left the cached state true), and the error only runs
  in the safe direction: a user told "possibly lost" who had nothing open loses nothing, while the
  opposite mistake hides a loss that `SPEC.md` §10 forbids hiding. The Oracle driver is not exact
  (`exact_transaction_state: false`), so it reports `Unknown` after any statement anyway.
* **A failing statement's output is still read.** For the user, that output is usually the
  diagnosis.
* **Overflow is not a read failure.** ORU-10027 is the statement's own error, native code 20000,
  and the lines before it are read as usual.
* **What a failed read leaves behind is not guaranteed.** The lines that read had taken are lost.
  The lines the server still held are read after the next statement, unless that statement prints
  first, in which case `DBMS_OUTPUT` discards them. The `failure` is what tells the user the pane
  is incomplete.
* **An invalid-UTF-8 line is not a failed read (M2.12).** Before M2.12, one line that was not
  valid UTF-8 was indistinguishable from a framing defect: both failed the whole read with
  `DataConversion` and lost every other line in it. That is no longer true. A *framing* error —
  a malformed length prefix, a frame shorter than it claims, the server's own line count and the
  decoded count disagreeing — is still exactly this: `DataConversion`, the read's lines lost, `T2`
  unchanged. A line whose *content* fails to decode as UTF-8 is not a framing error: it is
  delivered like any other line, with U+FFFD in place of the invalid bytes, and counted in
  `ServerOutputChunk::invalid_utf8_lines()`. It costs nothing beyond itself, and it never turns a
  read into a failure.

### T6 — bounded memory, and the queue bound becomes `3R + U + 3`

* **The worker holds at most one chunk.** A chunk is up to 4,096 lines: a 32 KiB target plus at
  most one line of up to 32,767 bytes, so under 64 KiB of text on Oracle.
* **It reads to the end even when the consumer is slow, because leftovers are not safely
  delayed.** Measured on 19.3 (`tests/m2_7_server_output.rs::lines_a_read_leaves_behind_are_purged_by_the_next_put_not_overflowed`):
  * The first `PUT` after a read **purges** whatever that read left, so leftovers never overflow
    a sized buffer later. With `ENABLE(2000)`, printing 19 × 100 bytes, reading 1 line, then
    printing 19 × 100 bytes again does not overflow. The 18 leftovers are simply gone, and
    nothing counts them.
  * A statement that prints nothing leaves them in place, and they are read after it, inside its
    window.

  Reading to the end is what makes every line either delivered or dropped **and counted** (the
  bullet below).
* **What that costs, and what is deferred.** The drain has no total bound. On a driver that
  cannot interrupt a call it cannot be cancelled either; only an abandon stops it, between reads.
  And the statement's `Executed` waits until it finishes: 10 million lines is about 2,500 round
  trips. A per-statement bound is recorded as follow-up **M2.13** in `phase-1.md`. It would be a
  total cap reported through `dropped`/`failure`, or a cancel flag checked between reads like the
  abandon flag. Either is safe *because* the next `PUT` purges what is left.
* **The event path reuses E5 unchanged.** At the per-session cap the incoming `ServerOutput` is
  refused, and its lines are counted and carried on the next delivered one, or in
  `pending_dropped_lines`. There is no second mechanism. A test at the cap asserts **delivered +
  dropped == produced** exactly.
* **One addition.** A `ServerOutput` that carries a failure is never refused: past the cap it is
  admitted without lines, and any lines it had are counted as dropped. Silently losing "your
  output is incomplete" would be worse than one event over the cap. Each execute produces at most
  one such event, and it is queued ahead of that execute's reply, which holds a slot. So these
  events add at most `R`, and the published bound becomes **`3R + U + 3`** (E2/E5, updated in
  `events.rs`, `session.rs` and `phase-1.md` §B2). A session that never enabled output cannot
  produce one, and stays at `2R + U + 3`.
* **The completion path has a stream-less shape.** `DatabaseSession::take_server_output() ->
  ServerOutputLog` hands over a per-session log. The log keeps at most 10,000 lines and 1 MiB
  between takes and counts the rest in `dropped`. It is always a **prefix** of the output: once
  one line is refused, every later line is refused until the next take, even one small enough to
  fit. The log is per session, not per statement. Completions that are pipelined and taken once
  get their output mixed, and `failure` is then not attributed to any of them. Taking it costs no
  round trip.
  Putting output on `ExecuteOutcome` instead was rejected, because a failed statement has no
  outcome and its output is the output that matters most.

### T7 — lifecycle

* **The setting belongs to the worker, and dies with the session.** A new session, including the
  new session a reconnect creates, starts `Disabled`. Nothing re-enables output on it.
* **A session is never replaced silently (§5), so no path carries the setting over.** Tested with
  two sessions on both paths.
* **No read is attempted after an abandon has been requested, or once the session has ended.**

### T8 — "no round trip when off" is structural, and tested twice

* **By construction.**
  * The worker's drain returns before any driver call when the setting is `Disabled`, which is
    its initial value.
  * The Oracle driver issues `DBMS_OUTPUT` only from `set_server_output` and `take_server_output`.
    Every `DBMS_OUTPUT` statement it has is in `server_output.rs`, and `execute` calls none of
    them.
* **With the mock.** The mock counts `set_server_output` and `take_server_output` calls on
  arrival. On both paths, a session that never enabled output runs a printing block, a failing
  block, an insert, a query and its fetch, a commit and a ping with both counters at **zero**.
  After a disable they stop increasing.
* **On the real database.** A statement that calls `PUT_LINE` on a session that never enabled
  output costs **one** round trip, exactly like `BEGIN NULL; END;`.

### T9 — the C ABI

**Unchanged.** `reldex.h` is byte-identical, and `gen-header.sh --check` is clean. Server
output is not reachable from C yet. The interim pump in `crates/ffi` drives sessions through
`Completion`s and never sees a `SessionEvent`, and the ABI has no entry point that turns output
on. So an FFI caller's sessions stay at the default, off, and cost nothing. M2.11 switches the
adapter to the event path, and at that point it must map `ServerOutput` and
`ServerOutputConfigured` and add the entry point. Both variants are new, so a mapping written
before them would send them to `RELDEX_EVENT_UNKNOWN`. The mapping, the entry point and the
adapter's rules are in `phase-1-m2-5-event-queue.md` §7.5.

*Evidence:*

* `crates/db-core/tests/server_output.rs`: 33 tests, 13 of them on both paths through
  `both_paths!`. They cover:
  * zero calls when never enabled;
  * one call each to enable and disable;
  * refusal without a call when the driver lacks the capability;
  * a failed enable leaving output off;
  * output in place by the time of the reply;
  * a failing statement's output;
  * chunking (1,000 lines in 16 reads of 64);
  * a failed read not masking the result;
  * a lost read following R6;
  * no read after a statement that lost the session;
  * no read while validation is pending, with that output arriving in the next execute's window;
  * per-session lifetime;
  * `Executing` < `ServerOutput` < `Executed`;
  * `TransactionStateChanged` before the output;
  * no read after a fetch;
  * delivered + dropped == produced at the cap;
  * the bounded completion-path log;
  * abandon during a blocked read, driven by blocking hooks with no sleeps and no timing bounds.
* `crates/db-core/src/events.rs`: 2 unit tests showing that a failure-carrying `ServerOutput` is
  admitted at the cap without its lines.
* `crates/db-core/src/shared.rs`: 2 unit tests showing that the completion-path log stays a
  prefix at both of its bounds. The first replays the review's probe: 1 MiB − 10 B, then 100 B,
  then 5 B.
* `crates/db-driver-api`: 5 unit tests.
* `crates/drivers/oracle-thin/src/server_output.rs`: 6 offline tests covering framing
  (Thai/emoji/combining, delimiter-like content, malformed framing as an error) and clamping.
* `crates/drivers/oracle-thin/tests/m2_7_server_output.rs`: 9 real-database tests, behind
  `oracle-it`. The ninth shows that leftovers are purged, not overflowed.
* Every `server_output` test was looped 30 times solo (960 runs), and the whole `db-core` suite
  30 times in parallel, with no failures.
* A mutation check turned off the "off" guard and the abandon check. 9 tests failed, and passed
  again once the checks were restored.

## Amendment: every transaction end releases results (2026-09-25, from ADR-0004's review)

**Status: implemented (M5.2 Stage A, 2026-09-26).** Decided by the lead on 2026-09-25; it landed
with M5.2, before M4.3 (`phase-1.md`, the M5.2 row). No new task id. As built:

- `finish_execute` (`crates/db-core/src/worker.rs`) calls `release_results()` after a successful
  execute of kind `TransactionControl`, in the `else` of the implicit-commit branch, before the
  statement's own cursor (if any) is registered. A failed statement releases nothing.
- The test is `a_typed_transaction_control_statement_releases_results_like_the_commands`
  (`crates/db-core/tests/transactions.rs`) on both reply paths: typed `COMMIT`, `ROLLBACK`,
  `SAVEPOINT` and `SET TRANSACTION` each invalidate the open result and its parked LOBs; a failing
  one does not. Removing the release makes it fail. The Result Store's own tests
  (`crates/db-core/tests/result_store.rs`) and the live test
  `a_typed_commit_ends_an_open_result_and_its_lob_cells`
  (`crates/reldex-core-poc/tests/m5_2_result_store_live.rs`) check the store's side on top.

**Additive batch accessors (same change, no layout change).** The Result Store (ADR-0004 RS1)
takes a fetched batch apart on the worker instead of copying it cell by cell. `db-driver-api`
gains three consuming or read-only accessors, and I1/I2's committed layout is unchanged:
`RowBatch::into_columns`, `Column::into_parts` (the data and its NULL mask), and
`TextColumn::heap_bytes` / `BytesColumn::heap_bytes` (buffer plus offsets capacity, what a
byte-capped consumer counts).

### X1 — a typed `COMMIT` or `ROLLBACK` releases results like the commands do

**What `db-core` does today.** `worker.rs` calls `release_results()`, which closes every open
cursor and clears every parked LOB, in three places:

- after a successful `commit` or `rollback` command (`resolve_transaction`);
- after a successful `rollback_to_savepoint` command;
- after an execute that committed implicitly (`finish_execute`, DDL).

This is D2's rule that cursors and locators are transaction-scoped.

**The gap.** A `COMMIT` or `ROLLBACK` typed into a worksheet reaches the driver as an ordinary
statement of kind `StatementKind::TransactionControl`. `db-core` releases nothing after it, so one
transaction end behaves two ways depending on how the user asked for it.

**The rule.** After a successful execute whose kind is `TransactionControl`, `db-core` releases
results exactly as after the commands, before it registers any cursor the statement returned. The
core must not parse SQL (`shared.rs`, `note_statement`), and the kind cannot tell `COMMIT` from
`SAVEPOINT` or `SET TRANSACTION`. So the release also follows those two, which end nothing.
Closing a result early is the safe error. Keeping a cursor the user believes is transaction-scoped
is not.

**Why now.** ADR-0004 (RS2) ends every open result store at a transaction end, deterministically.
It relies on the worker having released the cursors, so that a later fetch fails as "unknown,
closed or invalidated result handle" rather than succeeding on some paths.

**What stays out of reach.** A commit inside a PL/SQL block is invisible to the core. After one,
the cursor behaves as the server says, and D2 still requires an invalidated cursor to be reported
as an error, never as a short result.

**Test.** Open a result; run a typed `COMMIT`, then a typed `ROLLBACK`. A later fetch on the
first result fails with the invalidated-handle error, and its parked LOBs are gone, exactly as
after `commit()`. A `TransactionControl` statement that fails releases nothing.

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
  it would materially improve `SPEC.md` §10 prompting. **[2026-09-23: still unchanged on `main`
  (commit `6785e95`) — `transaction_in_progress: bool` remains private with no accessor; a new issue
  is drafted, see `phase-0-spike-results.md` §6.]**
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
  **[2026-09-23: this gap is now closed on `main` (commit `04b96be`, 2026-09-14, unreleased
  beta.4-dev) — `ErrorKind::DbError(String)` became `ErrorKind::DbError(DbError)`, and the new
  `DbError` struct exposes `.code()`, `.message()` and `.offset()` populated directly from the wire's
  `error_num`/`error_pos` fields. Not yet in our pinned `=26.0.0-beta.3`; this note's description of
  beta.3 itself is unchanged and the wrapper's parse-from-message workaround stays until the pin
  moves.]**
  `SqlPosition::at_char_offset` therefore has no upstream source on this version, and `SPEC.md`
  §24.14's "highlight the offending token" is achievable for PL/SQL only. `Capabilities::error_position`
  has no finer grain than one boolean; see spike contract note C-3.
- **A panic inside a round trip becomes a process abort.** *New; found by spike S2 (`oracledb` U-4).*
  `impl Drop for StatementHolder` does `self.client_ref.lock().unwrap()`, so a panic that poisoned
  the client mutex panics again during unwinding. **No wrapper can contain an upstream panic**, and
  `catch_unwind` does not help. Everything a driver knows to be a panicking input must therefore be
  refused *before* it reaches the crate. **[2026-09-23: the poisoned-lock half of this is fixed on
  `main` (commit `6785e95`, 2026-09-22) — `StatementHolder` no longer exists as such (folded into
  `Statement` by an unrelated refactor) and its `Drop` is now
  `if let Ok(mut client) = self.client_ref.lock() { ... }`, so a poisoned lock is skipped rather than
  unwrapped. A panic on a still-panicking input (U-2, U-3) therefore no longer *cascades* into a
  second panic during unwinding; whether `catch_unwind` now actually contains it is unverified against
  a live database. This wrapper's refuse-before-it-reaches-the-crate posture is unchanged either way,
  since the underlying panics themselves are not fixed.]**
- **Warnings are a plain `String`** — `last_warning() -> Result<Option<String>, Error>`, with no code
  and no structure. *Confirmed.* See S11.
- **Describe nullability is the server's `nulls_allowed` flag, copied verbatim.** *New; found by
  M2.8's metadata catalog work, confirmed against a live database.* `nullable: (nulls_allowed != 0)`
  reads the wire's own byte with no client-side computation
  (`oracledb-26.0.0-beta.3/src/metadata.rs:120,157`) — this is Oracle's own describe/TTC protocol
  behaviour, not a crate-level heuristic or limitation. Only a bare reference to a `NOT NULL`
  column describes as non-null. Every expression, and every bare column that is nullable at its
  source, describes as nullable. `WHERE` predicates never narrow it. A computed column cannot be
  proven non-null by describe; verify declared non-null contracts against the data.

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
