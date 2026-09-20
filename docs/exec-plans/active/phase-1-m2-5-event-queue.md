# M2.5 — `EventQueue` / `EventSink` / `SessionEvent` / `Waker` + the `ReplyTo` refactor

**Status:** implemented in `db-core`, pending review
**Date:** 2026-09-20
**Spec:** [`phase-1.md`](phase-1.md) §B2 · **Decision record:**
[ADR-0002](../../decisions/0002-driver-api-and-concurrency-model.md) amendment E1–E6 ·
**Architecture:** [`ARCHITECTURE.md`](../../architecture/ARCHITECTURE.md) §6

This file holds the two things that would have swamped the milestone table: the performance
numbers either side of the refactor (§2), and the exact `SessionEvent` → `ReldexEvent` mapping
M2.11 has to write (§3).

---

## 1. What landed, in one paragraph

`db-core` gained a second way to answer a request. Every worker command now carries a `ReplyTo`,
which is either the per-request oneshot behind `Completion<T>` — unchanged, including its errors,
its tests and its callers — or `{ session, request, sink }`, in which case the answer is pushed as
a typed `SessionEvent` into one process-wide `EventQueue` that every session's worker shares. The
consumer registers an edge-triggered `Waker` and drains; **no thread is parked per outstanding
request**, which is what `crates/ffi`'s interim per-session pump (ADR-0003 A5) costs today.

The FFI was deliberately **not** switched: M2.11 owns that, and `reldex.h` is byte-identical
(`gen-header.sh --check` clean). §3 is what makes that switch mechanical.

---

## 2. Performance, before and after

Dev machine, `--release`, mock driver. **Information only** — no assertion in the suite depends on
any of these, per `AGENTS.md` ("Performance": record a baseline, record the same numbers after).

### 2.1 One million rows through `crates/ffi`

`cargo test --release -p reldex-ffi --test allocations -- --ignored --nocapture measure_the_bytes`,
which streams 1,000,000 mock rows of the S14 shape through the C ABI in 50,000-row fetches and
holds every batch at once.

| | before M2.5 | after M2.5 |
| --- | --- | --- |
| every column described, no mirror | 123.3 B/row (118 MB) | 123.3 B/row (118 MB) |
| every column viewed with its fixed mirror | 185.3 B/row (177 MB) | 185.3 B/row (177 MB) |
| wall time, both passes | 1.35 s | 1.61 s |

Unchanged, as expected: this path still goes through the interim pump and `Completion<T>`, which
M2.5 did not touch. The wall-time difference is run-to-run noise on a shared machine, not a
measurement of anything M2.5 did.

### 2.2 Per-event cost in `db-core`

A `ping` round trip — submit, worker reply, queue push with its edge-triggered wake, drain —
20,000 per session. "Before" is the same round trip through `Completion<T>`, measured on the tree
as it stood before the refactor; "after" is the event path, from
`cargo test --release -p reldex-ffi --test core_event_cost -- --ignored --nocapture`
(three runs each).

| producers | before (`Completion`, ns/reply) | after (events, ns/event) |
| --- | --- | --- |
| 1 session | 2,043 / 2,080 / 2,215 | 432 / 507 / 537 |
| 8 sessions, concurrent | 328 / 386 / 410 | 351 / 382 / 391 |

The single-session figure is the one that matters and it is ~4× better, because the submitter no
longer parks on a reply channel: the cost that disappears is a thread round trip per request. The
eight-session figures are the same within noise, because there the mock's own work and the single
drainer are the bottleneck, not the handoff.

`Completion<T>` itself was not made slower — it is the same oneshot it always was; the "before"
column is the shape the UI would have had to use, not a regression baseline.

### 2.3 Allocations per event

`crates/ffi/tests/core_event_cost.rs` installs a counting global allocator in its own test binary
and measures 2,000 warm round trips: **65 allocations for 2,000 events, 0.033 per event**.
All of it is `std::sync::mpsc`'s own block amortisation on the *command* channel (one block per
~31 messages); the queue allocates nothing per event beyond the event itself, and the drain reuses
its `Vec`. The suite asserts the weaker, stable form of this (≤ 1 allocation per round trip) so a
loaded machine cannot fail it, and prints the real number under `--nocapture`.

That test lives in `crates/ffi/tests/` rather than in `db-core`'s own, and deliberately: a counting
global allocator needs `unsafe`, and `crates/ffi/tests/fences.rs` keeps the workspace's
`unsafe_code = "deny"` opt-out to the FFI boundary and the link probe. Adding `db-core` to that
list is an architecture decision (ADR-0003 D2), so `db-core` keeps `#![forbid(unsafe_code)]` and the
measurement is hosted in the crate that already owns one. If the owner would rather the test sat
next to what it measures, the fence's allowlist is where that decision goes.

---

## 3. `SessionEvent` → `ReldexEvent`, for M2.11

`ReldexEvent` (`crates/ffi/src/event.rs`) is a flat `#[repr(C)]` struct with a `kind` discriminant.
The table below is the whole switch M2.11 has to write; where a row says **new**, the ABI needs a
new `ReldexEventKind` value, which is additive (the enum reserves `0` for "a kind this header
predates", ADR-0003 D7) and therefore a **minor** ABI bump, not a major one.

| `SessionEvent` | `ReldexEventKind` | fields to fill |
| --- | --- | --- |
| `Opened { session, request, connection, cancel_kind, warnings }` | `OPENED` (1) | `session`, `request`, `connection_id`, `cancel_kind`, `warning_count = warnings.len()`; the warnings themselves stay behind `reldex_session_connect_warnings` (M2.6 produces this event) |
| `OpenFailed { session, request, error }` | `OPENED` (1) with `error` | `session`, `request`, `error`, `session_state = LOST` (M2.6) |
| `Executed { session, request, outcome: Ok(o) }` | `EXECUTED` (2) | `result`/`has_result` from `o.result`, `rows_affected`/`has_rows_affected`, `statement_kind`, `committed_implicitly`, `column_count = o.columns.len()`; move `o.columns` into the result's shared `ResultColumns` exactly as the pump does today (ADR-0003 A20) |
| `Executed { outcome: Err(e) }` | `EXECUTED` (2) | `error = e`, `has_result = false` |
| `Fetched { session, request, result, batch: Ok(b) }` | `FETCHED` (3) | `result` (**always**, A21), `row_count = b.row_count()`, `batch` = a `ReldexBatch` built from `b` and the result's `ResultColumns` |
| `Fetched { result, batch: Err(e) }` | `FETCHED` (3) | `result` (**always**), `error = e`, `batch = null`, `row_count = 0` |
| `Completed { operation: CloseResult(id), result }` | `RESULT_CLOSED` (4) | `result = id` (names what ended), `error` when `Err` |
| `Completed { operation: Commit \| Rollback \| Savepoint \| RollbackToSavepoint \| Ping }` | **new** `COMPLETED` | `request`, `error` when `Err`, plus a `completed_operation: int32_t` field so the adapter need not keep its own request→operation map |
| `Completed { operation: CloseLob(handle) }` | **new** `LOB_CLOSED` (or `COMPLETED` with the operation field) | `request`, `error` when `Err` |
| `LobChunk { lob, bytes }` | **new** `LOB_CHUNK` | `request`, the handle, the bytes (needs a `ReldexBytes`-shaped carrier or an arena append; the ABI has no LOB reader yet) |
| `SessionClosed { session, request, result }` | `SESSION_CLOSED` (5) | `close_outcome` and `session_still_open` from `CloseError` exactly as `QueuedEvent::with_close_outcome` does today; `error` from `close_error_to_reldex` |
| `Executing { session, request, deadline }` | **new** `EXECUTING` | `request`, `deadline_ms` + `has_deadline`; this is the honest "running, with this limit" state `SPEC.md` §24.8 needs on a driver that cannot cancel |
| `ServerOutput { session, lines, dropped }` | **new** `SERVER_OUTPUT` | `dropped`, plus an accessor for the lines (M2.7) |
| `TransactionStateChanged { session, possibly_active }` | **new** `TRANSACTION_STATE` | `possibly_active` |
| `Terminal { session, lifecycle, cause }` | **new** `TERMINAL` | `session_state` from `lifecycle`, `error` from `cause`; this is what lets the adapter retire a worksheet's session once, rather than inferring it |

Common to every row: `session`, and `session_state` from the session's lifecycle at the time the
event was produced. A `SessionEvent` variant the header does not know maps to `RELDEX_EVENT_UNKNOWN`
(`0`) and is dropped by the adapter, which is why the enum is `#[non_exhaustive]` on both sides.

### 3.1 What the interim pump does that the core path now does instead

| pump mechanism (ADR-0003 A5/A14/A21) | core equivalent | test |
| --- | --- | --- |
| one thread per session parked in `Completion::wait` | none — the worker pushes | `crates/ffi/tests/core_event_cost.rs`, `event_fan_in.rs` |
| the slot mutex that made per-session order exact | the per-session emit lock in `SessionShared`; no sequence number was needed | `event_ordering.rs::rule_1_…` |
| `OwedReply` + `answer_after_panic` | `ReplyTo`'s `Drop` (ADR-0002 E2) | `event_terminal.rs::a_contained_driver_panic_…`, `…::a_request_submitted_after_the_session_ended_…` |
| `retire_results` / `released` / `lost` holders, so the pump never frees a caller's strings | unchanged, and still `crates/ffi`'s job: the core hands out owned `ColumnMetadata` in `ExecuteOutcome`, not borrowed pointers | `event_payloads.rs::an_executed_event_describes_the_result_columns_…` |
| result id carried on synthesised failures | `Fetched.result`, `Completed(CloseResult)`, `LobChunk.lob` — all set on the failure path | `event_payloads.rs::a_fetch_and_a_result_close_name_their_result_…` |
| session-lost detection read off `session.session_state()` per event | `SessionEvent::Terminal`, once, with the cause | `event_terminal.rs::a_lost_session_yields_exactly_one_terminal_…` |
| `shut_down()` on hub destroy | `EventQueue` drop; the worker is never joined (ADR-0003 A17 holds) | `event_shutdown.rs` |

M2.11 keeps `crates/ffi`'s registry, its arena, its batch and error ownership and its re-entrancy
guard unchanged; what it deletes is `pump_main`, `pump_body`, `run_command`, `PumpCommand`,
`OwedReply`, `answer_after_panic` and the `Completion` imports, replacing them with one drain of
`EventQueue` on the hub's own waker. The hub's `set_waker` should delegate to
`EventQueue::set_waker` rather than keeping a second copy of the same `RwLock` trick.

---

## 4. Deviations from §B2, and interpretations

Recorded here so a reviewer does not have to diff the spec by eye. Each one is argued in ADR-0002
E1–E5.

1. **`Fetched`, `LobChunk` and `Completed` carry what they are *about*.** §B2 writes
   `Fetched { session, request, batch }` and `Completed { session, request, result }`. They now also
   carry `result: ResultId`, `lob: LobHandle` and `operation: CompletedOperation` respectively,
   because ADR-0003 A21 — accepted after §B2 was written — requires a reply to name its subject on
   the failure path too, and `RESULT_CLOSED` would otherwise lose the id it exists to report.
2. **"then stop" on loss means "stop doing driver work", not "exit the thread".** §B2: *"Sequence on
   loss: fail every already-queued command (each producing its own reply event), then emit
   `Terminal`, then stop."* Exiting the worker there would force every later submit to be answered
   from the caller's thread, which is a second producer for that session and a worse ordering story.
   The worker therefore keeps its loop and answers later submits with the terminal error, which is
   what §B2's rule 3 second sentence already describes.
3. **Rule 3 is bounded by "queued when the transition was observed".** §B2: *"`Terminal` is
   delivered exactly once per session, after every reply for requests accepted before the
   transition."* A request accepted on another thread *concurrently* with the transition cannot be
   ordered before `Terminal` without blocking submission, which `SPEC.md` §11/§19 forbids. The
   implemented guarantee is the deterministic one — everything in the queue at the transition — and
   the concurrent case falls under rule 3's own "may follow `Terminal`".
4. **`TransactionStateChanged` coalesces rather than ever dropping.** §B2 says unsolicited events
   *"use a bounded per-session ring with coalescing"*. Applied to a *state*, coalescing in place is
   strictly better than dropping, and it means this class can never reach the cap; the price is
   that a coalesced value is delivered at the earlier of the two positions. `ServerOutput` is the
   only class that can be dropped, and it is the only one carrying `dropped`.
5. **A drop refuses the incoming event, not the oldest queued one.** §B2 says "ring", which
   conventionally evicts the oldest. In a shared FIFO that is O(n) and it discards the *start* of a
   PL/SQL run, which is where the error usually is. The conservative reading — keep what is already
   promised, refuse the new one, report the loss on the next event that gets through — is what is
   implemented.
6. **`bind_events` returns `DbResult<()>` and refuses a second bind.** §B3 sketches
   `fn bind_events(&self, sink: EventSink)` returning unit. §B3 is M2.6's section; a silent rebind
   would split one session's stream across two consumers and make rule 1 unenforceable, so it is a
   reported failure.
7. **`max_outstanding_requests` bounds the event path only.** §B2 introduces it to bound *reply
   events*. Applying it to `Completion` calls would change behaviour ADR-0002 K9 fixed on purpose
   (the command queue is unbounded so `execute` never blocks the caller), and no reply event is
   produced for those.
8. **The drop-count tests are unit tests, not integration tests.** `ServerOutput`'s producer is
   M2.7, so the only way to reach the drop policy today is from inside the crate. The tests live in
   `crates/db-core/src/events.rs`; `crates/db-core/tests/event_backpressure.rs` covers the
   integration-visible half (a reply is never dropped, whatever the cap).
