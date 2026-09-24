# M2.5 — `EventQueue` / `EventSink` / `SessionEvent` / `Waker` + the `ReplyTo` refactor

**Status:** implemented in `db-core`; independent review round 1 done, must-fixes applied (§5)
**Date:** 2026-09-20, revised 2026-09-21
**Spec:** [`phase-1.md`](phase-1.md) §B2 · **Decision record:**
[ADR-0002](../../decisions/0002-driver-api-and-concurrency-model.md) amendment E1–E6 ·
**Architecture:** [`ARCHITECTURE.md`](../../architecture/ARCHITECTURE.md) §6

This file holds the two things that would have swamped the milestone table: the performance
numbers either side of the refactor (§2), and the exact `SessionEvent` → `ReldexEvent` mapping
M2.11 has to write (§3). §7 adds the part M2.6 produced — the open events, `abandon`, and what the
FFI switch does with them.

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

> **This table predates the reply-slot accounting** (review round 1, §5): it was taken when a reply
> was released on production and the queue carried no slot. Post-change figures, information only
> and not comparable across machines or load: mine, on a machine running an unrelated build,
> 606 / 629 / 643 ns at one producer and 431 / 442 / 456 ns at eight; the reviewer's four runs, on
> a quiet machine, 412–563 ns at one producer and 408–436 ns at eight. The eight-producer figure is
> ~10% above this table's, which is what the slot costs: one `Arc` clone on the push, one `Arc`
> drop and one `fetch_update` on the pop. Allocations per event are unchanged at 0.033.

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
| `ServerOutput { session, lines, dropped, failure }` | **new** `SERVER_OUTPUT` | `dropped`, `error` from `failure`, plus an accessor for the lines; M2.7 landed it, see §7.5 |
| `ServerOutputConfigured { session, request, result }` | **new** `SERVER_OUTPUT_CONFIGURED` (or `COMPLETED` with an operation value) | a reply; see §7.5 (M2.7) |
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
| `shut_down()` on hub destroy | `EventQueue` drop: the stream ends, everything queued is discarded and every reply slot released; the worker is never joined (ADR-0003 A17 holds) | `event_shutdown.rs`, `event_backpressure.rs::dropping_the_queue_releases_every_slot_it_was_holding` |

M2.11 keeps `crates/ffi`'s registry, its arena, its batch and error ownership and its re-entrancy
guard unchanged; what it deletes is `pump_main`, `pump_body`, `run_command`, `PumpCommand`,
`OwedReply`, `answer_after_panic` and the `Completion` imports, replacing them with one drain of
`EventQueue` on the hub's own waker. The hub's `set_waker` should delegate to
`EventQueue::set_waker` rather than keeping a second copy of the same `RwLock` trick.

---

## 4. Deviations from §B2, and interpretations

Recorded here so a reviewer does not have to diff the spec by eye. Each one is argued in ADR-0002
E1–E6, and all eight were upheld at review round 1.

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
4. **`TransactionStateChanged` is exempt from the cap, not merely coalesced.** §B2 says unsolicited
   events *"use a bounded per-session ring with coalescing"*. Applied to a *state*, coalescing in
   place is strictly better than dropping; the price is that a coalesced value is delivered at the
   earlier of the two positions. Coalescing **alone was not enough**, and review round 1 found the
   hole: with no state change queued to fold into, a session at its cap on `ServerOutput` had its
   next `TransactionStateChanged` dropped, so a UI could show Commit and Rollback disabled over a
   live transaction. The class is now admitted over the cap when there is nothing to fold into,
   which bounds it at one event per session. `ServerOutput` is the only class that can be dropped,
   and it is the only one carrying `dropped`.
5. **A drop refuses the incoming event, not the oldest queued one.** §B2 says "ring", which
   conventionally evicts the oldest. In a shared FIFO that is O(n) and it discards the *start* of a
   PL/SQL run, which is where the error usually is. The conservative reading — keep what is already
   promised, refuse the new one, report the loss on the next event that gets through — is what is
   implemented.
6. **`bind_events` returns `DbResult<()>`, and refuses both a second bind and a session that has
   already ended.** §B3 sketches `fn bind_events(&self, sink: EventSink)` returning unit. §B3 is
   M2.6's section; a silent rebind would split one session's stream across two consumers and make
   rule 1 unenforceable. The second refusal came out of review round 1: `Terminal` is emitted once,
   at the transition, so a queue bound after it would never receive one and the consumer would see
   a stream that only ever fails, request by request.
7. **`max_outstanding_requests` bounds the event path only, and counts *undrained* replies.** §B2
   introduces it to bound *reply events*. Applying it to `Completion` calls would change behaviour
   ADR-0002 K9 fixed on purpose (the command queue is unbounded so `execute` never blocks the
   caller), and no reply event is produced for those. Review round 1 showed the original release
   point — when the worker produced the reply — bounded nothing measurable, so the slot is now held
   until the consumer drains that reply; see §5.
8. **The drop-count tests are unit tests, not integration tests.** `ServerOutput`'s producer is
   M2.7, so the only way to reach the drop policy today is from inside the crate. The tests live in
   `crates/db-core/src/events.rs`; `crates/db-core/tests/event_backpressure.rs` covers the
   integration-visible half (a reply is never dropped, whatever the cap).

---

## 5. Review round 1: what changed

The independent review could not break exactly-once empirically (200 × 91 concurrent requests
racing a close, 300 concurrent double-closes, eight-session fan-in, a panicking waker on an
unwinding worker), found lock ordering clean and reproduced the numbers in §2. It found three
must-fixes and a list of should-fixes; all are applied.

**Must-fix.**

1. **`TransactionStateChanged` could still be dropped.** Coalescing only ran when one was already
   queued for that session; otherwise the generic cap check refused it. Reachable once M2.7 lands:
   256 undrained `ServerOutput` events, then an `INSERT`, and the "a transaction is open" event is
   gone. It is now admitted over the cap when there is nothing to fold into, bounding the class at
   one event per session. Test:
   `events.rs::a_transaction_state_change_is_never_lost_to_a_server_output_burst` — the mixed case
   neither single-class test covered.
2. **`max_outstanding_requests` bounded nothing.** The slot was released when the worker *produced*
   the reply, so a never-draining consumer plus a retry-on-`Resource` submitter reached
   `queue.len() = 5000` with the counter at zero, while four documents claimed the queue was
   bounded. The counter is now shared between the session and the queue, which carries an `Arc` of
   it on the queued reply itself, and it is released on the **pop**, under the queue's own mutex (a
   single atomic — no new lock, no ordering edge). New semantics, exactly: a slot is taken when a submit returns `Ok` and given back when
   the consumer takes that request's reply out of the queue — the slot rides on the queued event
   itself, so the reply path touches no per-session map; `outstanding_requests()` means
   "accepted and not yet drained"; dropping the `EventQueue` discards everything in it, releases
   every slot it held, and discards later events on arrival, so a session that outlives its
   consumer keeps working and is bounded from then on by what its worker has not yet reached.
   Tests: `event_backpressure.rs::a_consumer_that_never_drains_stops_the_submitter_rather_than_the_queue_growing`,
   `…::dropping_the_queue_releases_every_slot_it_was_holding`,
   `…::a_session_that_ends_with_undrained_replies_frees_its_slots_when_they_are_drained`.
   **Delta review follow-up:** `submit_close` reserves against `limit + 1`, because it was being
   refused at the cap like any other request — backwards for the one request that shrinks a
   session's footprint. Nothing deadlocked (`close()` and `Drop` use a `Completion` and reserve
   nothing), but an event-driven adapter could not have ended a full session. The exemption is
   exactly one: a second close while the first is undrained is refused. The published bound is
   therefore `2R + U + 3` — `R + 1` replies, `R` `Executing`s, `U + 1` unsolicited, one `Terminal`
   — updated in `events.rs`, `session.rs`, §B2 and ADR-0002 E2/E5. Test:
   `…::a_close_is_accepted_at_the_cap_and_the_exemption_is_exactly_one`. `RequestSlots::release`
   also gained a `debug_assert!` that a slot was actually held (release builds still saturate:
   panicking there would take out a worker thread over bookkeeping).
3. **A flaky waker test.** The consumer returns from `wait_timeout` on the condvar notify issued
   inside the push, while the producer calls the waker only after releasing the emit lock — which
   is the contract — so asserting the count the instant the drain returned was a race (reproduced
   1 in 40 under contention). It now waits on the counter under the hang guard
   (`support::wait_for`). The audit found no other test asserting a cross-thread side-effect count
   straight after a drain; the assertions on `outstanding_requests()` that followed a drain became
   *more* deterministic under fix 2, because the release now happens on the draining thread.

**Should-fix.** A close that lost a race reported `CloseError::Failed` on a session that had closed
cleanly (measured at 7.7% of 300 concurrent double-closes); `CloseReplyTo`'s `Drop` now answers
`Ok(())` when the session has ended and is not `Lost`
(`event_terminal.rs::concurrent_closes_all_report_success_on_a_cleanly_closed_session`,
`…::a_close_after_a_lost_session_still_reports_the_loss`). `bind_events` after the terminal
transition is refused (`…::binding_a_queue_after_the_session_ended_is_refused`). Rule 1 is
documented as *production* order, with the consequence spelled out for consumers (§B2, `events.rs`,
ADR-0002 E3, and §6 below). A pending `ServerOutput` drop count with no later output to ride on is
readable through `EventQueue::pending_dropped_lines(session)`. The `both_paths!` test shim now fails
on a duplicate reply or a leftover one instead of stashing it silently. The waker's possible
execution on a *submitting* thread, and the self-deadlock of calling `set_waker` from inside a wake,
are on the `Waker` contract. `announce_terminal` propagates the inner close's `Flow` behind a
`debug_assert!` instead of hard-coding `Flow::Exit`. Nits: no empty per-session entry is left behind
by a refused event, `wait_timeout(Duration::MAX)` blocks instead of returning immediately, and
`is_empty` takes one lock.

**Re-measured, with a caveat that matters.** The accounting now runs on the pop path, so §2.2 was
re-taken: **606 / 629 / 643 ns** per event with one producer and **431 / 442 / 456 ns** with eight.
Those are *not* comparable to §2.2's columns, because the machine was running an unrelated build
throughout: the one benchmark M2.5 never touched — the 1M-row FFI stream of §2.1, still on the
interim pump and `Completion<T>` — measured **3.43 s** against the 1.35 s / 1.61 s recorded there,
so everything timed in this window is roughly twice its earlier figure. Read the numbers as "the
same shape, on a machine half as fast", and re-take §2.2 on a quiet machine if the figure is ever
load-bearing. What is *not* load-dependent is unchanged: **0.033 allocations per event**.

One comparison inside that window is like for like and did drive a change. The first implementation
of the slot accounting kept a per-session tally in the queue's `sessions` map, which put a hash
lookup on both the push and the pop of every reply — the hot path — and measured 589 / 594 / 612 ns
at eight producers. Carrying the slot **on the queued event itself** instead measured 431 / 442 /
456 ns in the same window, and is also the safer shape: a reply cannot release a slot it was not
holding, and the slot cannot outlive its event whichever way the queue ends. The map is back to
serving only the unsolicited drop policy. Net cost of the bound on the reply path: one `Arc` clone
on push, one `Arc` drop and one `fetch_update` on pop.

---

## 5b. Review round 2 (2026-09-21, found by CI during M2.6): the idempotent close

One latent defect survived round 1 and was found by `test (ubuntu-latest)` on PR #22, failing
`event_terminal.rs::a_close_after_a_lost_session_still_reports_the_loss` intermittently — 3/1000 on
the M2.5 baseline (2b4426d), so **not an M2.6 regression**, and a real semantic bug rather than a
flaky test.

**What it was.** "Closing an already-closed session succeeds" was implemented as one boolean —
`SessionShared::ended`, meaning *a close has run* — set by the worker on **both** of its ends: the
clean one, and the one where the connection was already gone and the close had just reported the
loss. `DatabaseSession::submit_close` short-circuited on that boolean and answered `Ok(())`. So the
second, third or fourth close of a lost session could report that the caller's `Commit` had
happened when nothing had been committed. Whether it did depended on the worker reaching the flag
between two submits on the caller's thread, which is why it was intermittent; waiting for the first
close's reply makes it happen every time.

**The fix.** `SessionShared` now records *how* the session ended (`EndedAs::Cleanly` /
`EndedAs::Unresolved`), written once on the worker thread at the point it ends, and
`settled_close()` is the single place that turns that record into a close's answer. All three
idempotent-close sites read it: `submit_close`, `DatabaseSession::close`'s "no worker left to ask"
paths (which each returned a bare `Ok(())` before, including after a registry teardown detached the
worker at its deadline), and `CloseReplyTo`'s `Drop`. Idempotency now answers "this session is
already over", never "your commit happened". ADR-0002 amendment **E7** has the classification rule
and the one behaviour it changes: a second close on a *lost* session reports the loss again, with
the original kind and native code, instead of succeeding.

**Proof.** The lost and clean directions looped 3,000 runs each, 0 failures, plus a deterministic
regression test that forces the interleaving instead of hoping for it.

## 6. Locked in for M2.6 and M2.11

Decided here, while the reasons are in front of us, so neither task re-litigates them.

### M2.6 — registry, non-blocking open, abandon

* `worker::spawn` should take a **pre-built, already-sink-bound `Arc<SessionShared>`** rather than
  building one itself. The registry can then emit `Opened` / `OpenFailed` through the same
  per-session emit lock as everything else, before a worker exists, which is what keeps ordering
  rule 1 true for a session's very first events. `SessionShared::new` already takes the `SessionId`,
  and `bind_events` is already independent of the worker, so the change is a parameter swap. It was
  **not** done in M2.5: `spawn` still blocks on its ready channel, and splitting that is M2.6's
  actual work, so moving the signature now would be a change with no test to hold it.
* **HARD requirement: `abandon` must never be refused for lack of a slot.** It is the registry's
  teardown path and the only way to stop a session that is still connecting, so it follows
  `submit_close`'s exemption — reserve above the limit, or do not reserve at all and account for the
  one `OpenFailed` it produces. Whichever M2.6 picks, say so where the bound is published
  (`events.rs` module docs, `session.rs`, §B2, ADR-0002 E2/E5) and keep the arithmetic exact; the
  bound is currently `2R + U + 3` per session (`3R + U + 3` with server output on, since M2.7: §7.5)
  and a reviewer checks it against an adversarial probe.
* Give the connect reply the `ReplyTo` treatment. `abandon` then yields exactly one
  `OpenFailed { Cancelled }` from the same `Drop` mechanism as every other request, instead of a
  second bespoke path — and reserve the open request through `reserve_request` so it is bounded
  like the rest.
* Define `Terminal` for a session abandoned **before** it opened. The flag lives on
  `SessionShared`, so the registry can emit it without a worker; what needs deciding is the
  `lifecycle` it carries (`Closed` if the abandon won, `Lost` if the connect failed first).
* Anything that emits a `SessionEvent::is_reply()` event must go through `SessionShared::emit_reply`,
  or the slot accounting drifts. A `debug_assert!` on `EventSink::push_reply` catches it in tests.

### M2.11 — the FFI switch

* Delete the hub's own waker `RwLock` and delegate to `EventQueue::set_waker`; it is the same
  mechanism, and one copy cannot drift from the other.
* **Never call `set_waker` from inside `wake()`** — read lock inside write lock, self-deadlock. It
  is on the `Waker` contract.
* A budgeted `drain_into` that stops early gets **no further wake**: the queue is not empty, so
  there is no edge. The adapter must re-post its own drain, exactly as ADR-0003 D5 says.
* The waker may fire **on the thread that submitted a request** (a submit that cannot deliver its
  command synthesises its own failure event). The re-entrancy guard has to tolerate that, including
  the case where that thread is the UI thread.
* `RequestId` is caller-chosen and unchecked by the core. The FFI allocates one per hub and never
  reuses a live one.
* Route strictly by `RequestId`; retire a session's state on `Terminal` only. Delivery is production
  order, not acceptance order, so "every earlier request looks answered" is not a fact.

---

## 7. `Opened` / `OpenFailed` / `abandon`, for M2.11 (added by M2.6)

M2.6 landed `SessionRegistry` (`phase-1.md` §B3, ADR-0002 R1–R7) and is the producer of the two
rows §3 left to it. The C ABI is **still unchanged** — `reldex.h` is byte-identical — because
M2.11 owns it. What M2.11 has to know:

**The short version, as rules.** Route on `Opened`; never poll `get()`. Hold exactly one
`Arc<DatabaseSession>` per adapter session slot, and drop it before calling `retire`. Tear a session
down with `abandon` and then `retire`, and call `retire` **only on `Terminal`** — but call it for
every session, always, or the registry's map grows (`SessionRegistry::counts()` is there to make
that visible). `SessionId` is safe as an FFI key: it is monotonic and never reused. `get() == None`
means `INVALID_STATE` — accept nothing and emit nothing (ADR-0003 A18) — and that includes the whole
`Opening` window. `transaction_possibly_lost` on `Terminal` is the authoritative transaction-loss
answer and **must** reach the user; the value `abandon` returns is a lower bound for warning early.
The whole shutdown budget is one `DROP_SHUTDOWN_TIMEOUT`, not one per session.

### 7.1 The two open events

| `SessionEvent` | `ReldexEventKind` | fields to fill |
| --- | --- | --- |
| `Opened { session, request, connection, cancel_kind, warnings }` | `OPENED` (1) | `session`, `request`, `connection_id`, `cancel_kind`, `warning_count = warnings.len()`. The warnings themselves stay behind `reldex_session_connect_warnings`, which reads `DatabaseSession::connect_warnings()` — the handle keeps its own copy, so a UI that asks late still gets them |
| `OpenFailed { session, request, error }` | `OPENED` (1) with `error` | `session`, `request`, `error`, `session_state = LOST` |

`Opened` is a session's **first** event. Nothing else for that session precedes it — M2.6 had to
silence the connect-time transaction-state seed to keep that true (R5) — so the adapter creates its
per-session state there and retires it on `TERMINAL`.

### 7.2 Replacing the interim pump's open

`reldex_hub_open_session` currently spawns a pump thread that performs the blocking
`SessionManager::open_session` and reports through it. It becomes:

* `SessionRegistry::open(driver, params, request)` → return the `SessionId` **immediately**, with
  `RELDEX_STATUS_OK` and no pump thread. The hub keeps its `RequestId` allocator (`RequestId` is
  caller-chosen and unchecked by the core).
* Every "the pump is still inside `open_session`, nothing may be submitted yet" state disappears:
  `SessionRegistry::get` answers `None` until the session is open, which is the same refusal
  ADR-0003 A18 already permits (`INVALID_STATE`, nothing accepted, no event follows).
* `reldex_session_close`/destroy paths that today tear a pump down map onto
  `SessionRegistry::abandon` + `SessionRegistry::retire`.

### 7.3 `abandon`, and what the adapter must surface

`abandon(id) -> Abandoned` is the teardown path, and the only way to stop a session that is still
connecting. It never blocks and is never refused.

* `Abandoned::Connecting` — the open gets one `OpenFailed` with `ErrorKind::Cancelled`, then
  `Terminal { Closed }`. A connection that arrives afterwards is closed on the worker thread; the
  adapter sees nothing further.
* `Abandoned::Open { transaction_possibly_lost }` — abandoning an open session never commits, so
  the server rolls back whatever transaction it held. The flag here is a **lower bound**, meant for
  warning the user immediately: `true` means "say so now", `false` means only "nothing visible from
  this thread says otherwise yet". `Terminal` follows when the worker reaches the abandon, which on
  a blocked statement is when that statement returns, and it carries the real answer.
* `Abandoned::Ending` / `Abandoned::Unknown` — idempotent no-ops; nothing is produced.

Exposing the `Abandoned` hint across the ABI is M2.11's call: an out-parameter on the abandon entry
point is the obvious shape, and it is additive.

### 7.3a `Terminal` needs a new field, and this is the cheap moment

`SessionEvent::Terminal` is now `{ session, lifecycle, cause, transaction_possibly_lost }`. The last
field is the authoritative answer to "did this session take the user's work with it?", computed on
the worker thread at the point the session ends (ADR-0002 R6 has the full table). **It must reach
the user** — `SPEC.md` §10 forbids hiding a transaction loss — so the C `TERMINAL` event needs a
field for it, e.g. a `uint8_t transaction_possibly_lost` beside the existing lifecycle and error.
The ABI has not switched to the core event path yet, so adding it costs nothing now and would be a
breaking change later.

Five paths produce `true`, and four of them had no way to report anything before M2.6: `abandon` of
an open session, `retire` of an open session, the registry's teardown, `DatabaseSession::drop`, and
a connection that was lost. A `close` whose disposition succeeded — commit *or* a rollback the user
chose — is `false`, and so is a session that never opened.

For a lost connection the answer is finer than "lost, therefore warn": a session lost **by** a
driver call reports `true`, because the statement that died may have reached the server (the core
records `TransactionState::Unknown` rather than trusting the driver's pre-call cache); a session
lost while **idle**, with nothing open, reports `false`. So the adapter can show the warning
whenever the flag is set without training the user to ignore it on every dropped connection.

### 7.4 Retirement

`SessionRegistry::retire(id)` is what lets the registry drop its handle, and `Terminal` is the only
event that says it may. The adapter's own per-session state and the registry's entry retire
together, on the same event. A `SessionId` that has been retired answers `Unknown` to a later
abandon and `None` to `get`, so a double-retire is harmless.

Two things to get right:

* **Retire only on `Terminal`.** Retiring a session that is still **open** is a blocking, lossy
  path: it drops the handle, which runs `DatabaseSession::drop` — up to `DROP_SHUTDOWN_TIMEOUT` on
  the calling thread, and it resolves nothing, so the server rolls back whatever the session held
  (reported afterwards on that session's `Terminal`). Neither belongs on the UI thread. `abandon`
  is the immediate, non-blocking half; `retire` on `Terminal` is the clean-up, where there is
  nothing left to wait for.
* **Retire every session, always**, and drop the adapter's own `Arc<DatabaseSession>` first — the
  registry can only release *its* hold. An open session's entry is released by nothing else. (Two
  cases release themselves, because the registry owns nothing for them: an open that failed, and an
  abandoned open once its late connect has arrived. Those answer `false` to `retire`, which means
  only "there was nothing left to release".) `SessionRegistry::len()` / `counts()` are the
  diagnostic that makes a leaking adapter visible.

### 7.5 Server output, for M2.11 (added by M2.7)

**M2.11 guidance.** M2.7 (`phase-1.md` §B4.2 as implemented, ADR-0002 T1–T9) produces the
`ServerOutput` row of §3 and adds one reply. The C ABI is **still unchanged**: `reldex.h` is
byte-identical and `gen-header.sh --check` is clean. Until M2.11 maps them, both events arrive at
the interim pump as `RELDEX_EVENT_UNKNOWN`. What M2.11 has to add:

| `SessionEvent` | `ReldexEventKind` | fields to fill |
| --- | --- | --- |
| `ServerOutput { session, lines, dropped, failure }` | **new** `SERVER_OUTPUT` | `line_count`; the lines behind an accessor or an arena as `(pointer, length)` UTF-8, **never** NUL-terminated, because a line may contain `\n` or NUL and an empty line is a real line; `dropped` (lines refused before this event, so "output truncated"); `error` from `failure` (a failed read, so "output incomplete, because …"). There is no `request`: a `ServerOutput` between an execute's `EXECUTING` and its `EXECUTED` belongs to that execute, except in the two cases under "Ordering" below. |
| `ServerOutputConfigured { session, request, result }` | a reply: **new** `SERVER_OUTPUT_CONFIGURED`, or `COMPLETED` with a new `completed_operation` value | `request`, `error` when `Err`, and the setting **in force** — enabled, unlimited, and buffer bytes. Oracle clamps a requested size into 2,000..=1,000,000, and the pane must show the real one. |

Entry point: one `reldex_session_set_server_output(session, request, mode, buffer_bytes)` onto
`DatabaseSession::submit_set_server_output`. Keep it typed across the C line too: a mode enum
(`OFF`, `UNLIMITED`, `BYTES`) plus a size that is read only for `BYTES`. A `(bool, size)` pair is
exactly what the Rust side replaced (§B4.2 item 1).

Rules the adapter must keep:

* **Off by default, and only the user turns it on.** While on, every statement pays at least one
  extra round trip. A reconnect is a new session, and a new session starts with output **off**:
  the core never carries the setting over. If the pane stays open across a reconnect, re-enabling
  is a visible request the adapter issues, never an implicit one.
* **Ordering.** For one execute: `EXECUTING`, then `TRANSACTION_STATE` if it changed, then zero or
  more `SERVER_OUTPUT`, then `EXECUTED`. A read that loses the connection gives `SERVER_OUTPUT`
  with an error, then the statement's `EXECUTED` (its own result, untouched), then `TERMINAL`
  (`Lost`, `transaction_possibly_lost`). **Two exceptions** land inside the **next** execute's
  window, ahead of that execute's own lines. The first is output a function wrote during a
  *fetch*, because a fetch is not followed by a read. The second is output of a statement whose
  `EXECUTED` carried an error that left the session needing validation (e.g. a timeout), because
  no read is made until the next command's ping. The pane must not attribute either more
  precisely than "printed before this statement's own output". The second is mostly moot on
  Oracle today, where a timeout in a blocked call loses the session. The adapter can still mark
  the next window as possibly carrying the failed statement's lines.
* **Budget.** One event carries at most `SessionLimits::server_output_chunk_lines` (4,096) lines
  and `server_output_chunk_bytes` (32 KiB) of text plus at most one more line (up to 32,767 bytes).
  With output on, the per-session queue bound is `3R + U + 3` (§B2), and a chatty PL/SQL run fills
  `U` with `SERVER_OUTPUT`. The adapter's drain budget should count lines or bytes, not only
  events.
* **Drops are reported, never hidden.** A non-zero `dropped` and an error both mean the pane must
  say so. Lines still owed when the session ends are read with
  `EventQueue::pending_dropped_lines(session)` on `TERMINAL`.
* **The completion path has no stream.** Anything that still runs a statement through a
  `Completion` (the interim pump) must call `DatabaseSession::take_server_output()` after it. That
  is a bounded log (10,000 lines / 1 MiB, with `dropped` and `failure`/`failures`). It keeps a
  prefix of the output and costs no round trip. It is per session, not per statement, so take it
  after each `wait()`. Completions pipelined and taken once get their output mixed, with
  `failure` not attributed to any of them.
* **`SessionEvent::ServerOutput` is `#[non_exhaustive]`.** Match it with `..`, because its fields
  are expected to grow (follow-up M2.13 may add a per-statement truncation report).
* **`ServerOutputChunk` already grew one field (M2.12) that this mapping does not carry yet.**
  `crates/db-driver-api/src/server_output.rs::ServerOutputChunk::invalid_utf8_lines()` — how many
  of a chunk's lines were not valid UTF-8 on the wire and were delivered with U+FFFD in place of
  the bad bytes rather than dropped (ADR-0002 amendment T, M2.12 update). Today
  `db-core::worker::drain_server_output` reads it and calls `chunk.into_lines()`, which silently
  discards the count along with `drained` — there is nowhere in `SessionEvent::ServerOutput` or
  `ServerOutputLog` for it to go yet. When this table's `ServerOutput` row is implemented, add
  `invalid_utf8_lines: u32` next to `dropped` (`SessionEvent` is already `#[non_exhaustive]` for
  exactly this) and a matching field on `ServerOutputLog` for the completion path, and thread both
  through `drain_server_output`/`deliver_server_output`/`collect_server_output` rather than
  dropping the count at the `db-core` boundary the way it is dropped today. The pane's reason to
  care: a line reported this way is not corrupt data lost to a bug, it is exactly what the server
  held, and the UI should be able to mark it rather than show mojibake with no explanation.
