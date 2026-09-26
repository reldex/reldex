# 0004 — Result Store: representation, paging and the bounded-memory policy

**Status:** Accepted by the lead (2026-09-25), after one independent review round (PR #39).
**M5.2 Stage A (the core) is implemented** (2026-09-26); see "As implemented: M5.2 Stage A" for
where the code differs from the text and why. Stage B (ABI 4 in `crates/ffi`, RS5) follows.
The owner-review list is at "Owner-review points": (a) the fetch-size sign-off, re-asked with a
corrected diagnosis, (b) whether a 1,000,000-row default cap meets `SPEC.md` §19, (c) no
browsing past the caps in Phase 1, (d) the mobile caps (for information), (e) upstream Issue J /
U-19 (a draft, not posted; owner review pending). The M5.2 review added (f) failed statements
that still end the transaction and (g) the wire array size on Oracle.
The ADR-0003 K1/K2/K3 rulings are still open with the owner too. The decisions below do not wait
for any of these; "Robust to the pending rulings" says what holds under each outcome.
**Date:** 2026-09-25 (drafted and revised after review the same day)
**Task:** M5.1 ★ (`phase-1.md` §C.2)
**Resolves:** `ARCHITECTURE.md` §13 item 6 (result store representation, bounded-memory policy,
spill/eviction, Arrow). It also answers ADR-0002's open "Evidence/benchmarks" item, which said the
batch-shape claim "must be benchmarked before the Result Store design is fixed".

## Context

`SPEC.md` §12 fixes the pipeline — cursor, batch fetch, **Result Store**, virtual table model,
visible cells — and asks the store for typed values, NULL, batches, bounded memory, lazy large
values, streaming export and efficient random access. Arrow may be used internally only where
benchmarks show a benefit, and never in a UI API. §19 targets virtualized handling of 1,000,000+
logical rows at 60 FPS. §2 ranks database and transaction correctness above UI responsiveness,
performance and memory, in that order. The owner's standing rule of 2026-09-19 is that every
default is user-configurable and the UI shows the value in force.

What exists today, and what this ADR has to replace or keep:

- **Batches** (ADR-0002 D6, I1/I2). `RowBatch` is column-oriented. `Number` is 44 bytes, a
  `Timestamp` 16, a `Value` 48. Text and bytes are one buffer plus `usize` offsets, and NULL is one
  bit. The layout is observable, so a storage change in `db-driver-api` is a breaking one (I2).
  `Statement::with_fetch_rows` sizes the driver's array fetch at execute. `fetch_batch(max_rows)`
  bounds each batch.
- **The core** hands each batch out as a `FetchedBatch`, after parking every LOB locator on the
  worker thread (K1). A LOB cell is found by a linear search (`FetchedBatch::lob`).
- **The boundary** (ADR-0003, ABI 3). `FETCHED` transfers an owned `ReldexBatch*` to the adapter.
  Text, `bool`, `f32` and `f64` are zero-copy. `NUMBER` and `TIMESTAMP` are rendered by the bulk
  formatter per 1,024-row window, or mirrored by `reldex_batch_column_fixed` at 46/16 bytes per row
  (A19).
- **The adapter** (M1.6) keeps every batch it was given, as ADR-0003 D4's MVP rule said it would
  until this ADR ("MVP retains the fetched prefix — see §C M5 and the Result-Store ADR it
  produces"). `SessionController` streams the *whole* result with `fetchesInFlight` fetches
  outstanding. `ResultTableModel` stops at `INT_MAX` rows. **Nothing bounds bytes.** Retained
  memory grows with the result, which is `phase-1.md` risk R8.
- **Settings** (ADR-0006 P2). `results.fetch_rows` defaults to 1,000 (1–100,000; application,
  profile, worksheet). `results.fetches_in_flight` defaults to 2 (1–8; application only). The
  owner's sign-off on the number is pending, and M5.6 sets the shipped default. This ADR changes
  what that sign-off should be about (RS2, "Bytes per round trip").
- **Evidence so far.** S14: memory tracks the batch, not the result, when batches are dropped, and
  throughput is not monotonic in batch size. S15, with the S14 shape (`NUMBER`, `VARCHAR2(40)`,
  `DATE`) and every batch retained: +114–119 B/row headless; +172.5–172.7 MB in-app for
  1,000,000 rows from an idle, drawn app; +200.3–200.8 MB from process start. The last reading is
  the K3 number that is marginal and awaiting a ruling. Scrolling while a result streams drops
  0.33% of frames (M5.8). 1,000 rows per fetch with 2 in flight was the best of S15's sweep, which
  ran against the mock driver, so it never exercised the Oracle wire path.
- **Oracle facts the store has to live with.** The cursor is forward-only; scrollable cursors are
  out of scope (ADR-0002 D8), so a row dropped from memory comes back only by running the query
  again. `oracle-thin` executes a query as a describe (`prefetch_rows(0)`), so a query's server
  work runs in its **first** `fetch_batch`. There is no on-demand cancel (ADR-0001;
  `PreArmedDeadline`).
- **What the time limit bounds.** `oracledb`'s call timeout is the socket's **read** timeout
  (`Client::set_call_timeout` → `transport.set_read_timeout`). It bounds each wait for bytes from
  the server. It does not bound the client's decoding between reads, so it bounds neither a fetch
  nor a result. It is also a property of the connection, not of a statement: every later
  `execute` re-arms it for every cursor still open on the connection. While any result set is
  open, `OracleCancelHandle::remaining` therefore declines to name a stop time
  (`crates/drivers/oracle-thin/src/conn.rs`, `OpenResultSets`).
- **What a fetch costs on Oracle 19c.** One fetch costs roughly the square of the bytes it
  carries, because of a behaviour of `oracledb` 26.0.0-beta.3 found by this ADR's review.
  - A server that does not announce end-of-response does not mark where a response ends. The
    client enables it only when the negotiated TNS protocol is ≥ 23 (23ai or newer, per U-19 in
    `phase-0-spike-results.md` §5), and 19c never qualifies. The client learns that more packets
    are needed only by failing to parse (`client/mod.rs`, `receive_response`: on `out_of_data`,
    `receive_packets` returns one more packet).
  - Each time that happens, `Response::add_packets` rebuilds its read buffer from **all** packets
    received so far, and deserialization starts again at byte 0 (`response/mod.rs`).
  - So one fetch costs O(packets²) in client CPU. Measured at the driver level on the 19c container:
    - Each doubling of rows per fetch costs about 4× per batch.
    - One 1,000-row fetch of 16 KB rows takes **23 s**.
    - A 2 MiB SDU changes nothing (Table 3a).
  - This is the "throughput is not monotonic in batch size" that Phase 0's S14 could not explain
    (`phase-0-spike-results.md` S14, and U-19 in §5, which holds the upstream analysis and the
    draft Issue J).

Consumers: M5.2 (implementation and FFI lifetime rules), M5.3–M5.5 (grid, copy, LOB viewers),
M5.6–M5.8 (fetch benchmark, perf re-run, drain budget), M4.3 (run modes that replace a result),
M6.3 (translated status text), and the P2 export and sort/filter work that `phase-1.md` §C.1
deferred "until ADR-0004".

## Decision

### RS1 — The core retains the fetched prefix, as immutable, compacted columnar segments

**Who owns it.** A `db-core` type, `ResultStore`, one per open result, owns every row the view can
show. The adapter no longer owns batches. Caps, byte accounting and the fetch policy (RS2) are
rules, and rules do not belong in C++ or QML (`AGENTS.md`). In the product, the store lives in the
composition root's consumer, `crates/ffi`'s hub, and is touched only from the thread that drains
events: the Qt main thread (ADR-0003 D5 rule 3). Appending a segment there costs a pointer push.
Since M2.15 `crates/ffi` has no pump thread: the hub drains `db-core`'s event queue on the Qt main
thread (ADR-0003 A32). The append happens when the hub **drains** a `FETCHED` event, so no worker
ever touches a store (RS5, "Threads").

**Segments.** One fetched batch becomes one segment, and a segment is never mutated, moved or
evicted once appended. Segments are shared as `Arc` and hold plain data only: locators are parked
on the worker before the reply leaves it (K1). They are therefore `Send + Sync`, so ADR-0003 A12's
"concurrent reads of one batch are sound" still holds.

**Compaction happens on the session's worker thread**, as part of the fetch command whose reply
goes to a store, after LOB parking and before the reply is sent. The UI thread never pays for it,
and the event queue carries compact data. The measured cost is 31–45 µs per 1,000-row batch of the
S14 shape, about 138 µs for 10 `NUMBER` columns and about 185 µs for 5 text + 2 date columns
(Table 2). That is 1–2% of one real 1,000-row fetch even on loopback (Table 3b), and roughly the
size of a whole S15 drain (80–250 µs), which is why it must not run in the drain. The
`Completion`-path `fetch_batch` may keep returning `FetchedBatch` for tools and tests; M5.2 chooses
the call shape.

**Representation per column kind:**

| Kind | Stored as | Per value | Today, as delivered |
| --- | --- | --- | --- |
| `NUMBER`, every non-NULL value in that segment's column exactly `m × 10⁻ˢ` with `s ≤ 18` and `m` in `i64` | `Vec<i64>` + one `u8` scale per segment column | 8 B | 44 B |
| `NUMBER`, otherwise (e.g. a 40-digit quotient) | `Vec<Number>`, unchanged | 44 B | 44 B |
| Text, JSON, `Unsupported` text, bytes | the same one-buffer-plus-`usize`-offsets layout as `TextColumn`/`BytesColumn`, **sized exactly** | payload + 8 B | payload + driver slack + 8 B |
| `TIMESTAMP` (all three types) | `Vec<Timestamp>`, exact capacity | 16 B | 16 B |
| `BOOLEAN`, `BINARY_FLOAT`, `BINARY_DOUBLE` | as delivered, exact capacity | 1 / 4 / 8 B | same |
| LOB | one `u64` LOB id per row; the locator stays parked on the worker (K1) | 8 B + the parked locator | 16 B (an emptied `Option<LobLocator>`) + 32 B (`(row, column, LobHandle)`) + the parked locator |
| NULL | `NullMask`, 1 bit per cell, as delivered | ⅛ B | same |
| Per segment | a small header plus about 100 B per column (measured: +3 B/row for `s14` at 100 rows per segment) | about 0.1 B per row per column at 1,000 rows per segment | about the same |

- **The `NUMBER` re-encoding is lossless by construction.** It is chosen per segment column, so one
  odd value costs only its own segment, and the bench checks every cell back against the original
  `Number` (`verify` in the bench). This is the change that matters: 10 `NUMBER` columns cost
  **118 B/row instead of 443** (Table 1). The measured column of 40-digit quotients falls back to
  44 B, and it alone is 37% of that 118. ADR-0002 foresaw "re-encode `Number` … without changing
  the exponent/digit semantics"; this is that change, made in the store, and the contract type is
  untouched.
- **Formatting must not show the encoding.** The bulk formatter formats `i64` plus scale directly,
  and its output must be **byte-identical** to `Number`'s canonical `Display` (ADR-0002 S6) for
  every value. The scale is chosen per segment column, so without that rule one value could read
  `1.5` in one segment and `1.50` in the next. This is a stated test requirement for M5.2, and
  M5.3 repeats it through the grid. The test is a property test over random `Number`s, formatted
  from both encodings, with scales 0–18 and segments that mix them. Nothing is mirrored and kept
  for a store segment: RS5 says how `NUMBER` and `TIMESTAMP` are read.
- **Text keeps its layout and loses its slack.** The Oracle driver reserves 16 bytes per row and
  grows by doubling (`crates/drivers/oracle-thin/src/value.rs`). With 5 × `VARCHAR2(100)` that
  left about 300 B/row, close to half of each text buffer, unused: 714 B/row retained against
  416 B/row compacted. Offsets stay `usize`: switching to `u32` would save 4 B per row per text
  column (5.5% of the S14 row, 4.8% of the text5date2 row). That is not worth breaking
  `ReldexColumnView` and ADR-0002 I2's committed layout. The prototype copies each text buffer at
  its exact size. M5.2 may instead `shrink_to_fit` the driver's buffer, which the allocator can
  often do in place without a copy; the measured bytes are the same either way.
- **Row-of-values is rejected on the numbers.** One `Box<[Value]>` per row, with a `String` per
  text cell, costs 201–695 B/row accounted and 233–813 B/row in private bytes. The allocator's
  per-allocation overhead shows as +16% on the text shapes. Appending 1,000,000 rows takes 156–561
  ms where compaction takes 31–185 ms (1.8–5.0× slower). It would also need a `Value` that can
  hold a core `LobHandle`, which `Value` cannot (K1).
- **Strings and LOBs.** Inline text is retained whole: it has already crossed the wire, and
  truncating it would lose data that cannot be fetched again (forward-only cursor). A `CLOB`/`NCLOB`
  /`BLOB` is never materialized. `oracle-thin` always fetches locators (`fetch_lobs()`), and the
  store holds an id that the M5.5 viewer reads in chunks. The 32 KiB inline shape in Table 1 stands
  for an extended `VARCHAR2(32767)` or a CLOB converted to text. Nothing in the representation helps
  there (32.8 KB/row in every form), and that is what the byte cap (RS3) is for.
- **Surfaces.** The store's column enum, its `NUMBER` storage enum, `ResultState`, `LimitKind` and
  `MoreRows` are `#[non_exhaustive]` with private fields, and read through typed accessors. A later
  encoding (packed BCD for the fallback, dictionary text, an Arrow segment) is then an addition, not
  a break. On the C side a new kind versions through `ReldexColumnKind` (0 = unknown) and the ABI
  major (ADR-0003 D7).

### RS2 — Fetch on demand as the view scrolls; the view states demand, the core decides fetches

**Who drives.** The view reports **demand**: how many rows it wants resident, typically the last
visible row plus one. `QAbstractItemModel::fetchMore` becomes "demand = rows shown + 1". The core's
fetch policy, inside `ResultStore`, decides whether to submit a fetch and how many rows to ask for.
The adapter no longer counts fetches in flight. `SessionController::submitFetches` moves into the
core, and with it the last piece of fetch logic in C++.

**The policy:**

1. At `EXECUTED` with a result, the first fetch is submitted at once, whatever the demand. That
   fetch is the first page, and on `oracle-thin` it is also where the query's server work runs.
   Its size comes from the describe's declared column widths ("Bytes per round trip" below), not
   from a guess.
2. After that, the store keeps **one fetch of read-ahead**. It submits while
   `retained + rows requested and not yet answered < demand + fetch_rows`, with at most
   `results.fetches_in_flight` requests outstanding **per result**, and only inside the caps (RS3).
   In practice one fetch is outstanding while paging, and two only when demand jumps (a scrollbar
   drag, or Ctrl+End).
3. **Fetch all** (an explicit user action) sets demand to the row cap. It is the only mode that
   streams, and even then it stops at the caps.
4. **Stop fetching** stops *submitting*. It takes effect at once: the fetches already submitted
   (at most `fetches_in_flight`) still run and are kept, because their rows are already paid for.
   It is not a cancel and is never labelled one (`SPEC.md` §10). The worst case is the fetches
   already submitted, run one after another on the worker. Today, at the defaults (1,000 rows,
   2 in flight) on 16 KB rows, that is about **2 × 23 s** (Table 3a). The bound on bytes per round
   trip below is what shrinks it.

**Bytes per round trip.** On Oracle 19c a fetch's cost grows with the **bytes** it carries, roughly
fourfold per doubling (Context, Table 3a). It does not grow with the row count as such, and it
depends on the server version. So no row count is a safe default. 1,000 rows is 4.4 ms for the
S14 shape, 22 ms for 5 texts + 2 dates, and 23 s for 4 × `VARCHAR2(4000)`.

- **The rule.** The store's fetch requests, and the driver's wire array size, are bounded by
  **bytes per round trip**, computed from the describe's declared column widths
  (`ColumnMetadata::max_size_bytes`, which `oracle-thin` fills from the describe; a fixed width
  for `NUMBER`, `DATE` and `TIMESTAMP`; the locator size for a LOB). Rows per round trip is
  `clamp(budget / declared row width, 1, results.fetch_rows)`, so `results.fetch_rows` becomes
  an upper bound, not the size. A declared width can overstate the data badly: an expression is
  commonly described as `VARCHAR2(4000)`. So once the first segment arrives, the observed average
  row width replaces the declared one, but never exceeds it. The first round trip is the only one
  sized blind, and it errs small.
- **The budget.** M5.6 measures it on this server version and on a real network, and the owner
  signs it off (owner-review point (a)). Until then M5.2 uses a placeholder of **256 KiB**. That is
  about 22 ms on the Table 3a curve (5 texts + 2 dates at 1,000 rows carry roughly 0.3 MB) and
  under 70 ms for wide rows (50 rows of 16 KB). A declared width over-estimates sparse text, so the
  bound errs toward more, smaller round trips; M5.6 weighs that against network latency.
- **Applying it needs a driver change.** `oracle-thin` sets the array size at `execute`, before
  the describe, and `oracledb`'s public `Cursor` has no setter. Its fetch message does read the
  size from the statement's options on every fetch (`messages/fetch.rs`), so a setter is a small
  upstream change. M5.2 chooses the mechanism: a setter upstream (asked alongside Issue J, or
  separately, under the same owner review as point (e)), or a describe before the execute. Until
  one of them lands,
  `results.fetch_rows` alone sizes the round trip, and wide rows keep Table 3a's cost (Accepted
  limitation 11).

**Back-pressure.** A result's in-flight count drops only when the consumer **drains** a reply, the
same rule ADR-0002 E2 uses for request slots. A consumer that stops draining therefore stops its
own fetching. Rows not yet admitted are bounded by `fetches_in_flight × fetch_rows` per result,
which also keeps fetches far below `SessionLimits::max_outstanding_requests` (1,024).

**Ordering.** Segments are appended in the order their fetches were submitted. The worker runs a
session's commands FIFO (ADR-0002 D1), and one session's replies arrive in production order
(`phase-1.md` §B2 ordering rule 1, built by ADR-0002 E1–E3), which for a single cursor is the
order submitted. The store checks this: each fetch it submits carries a per-result sequence number
in its request bookkeeping, and a reply out of sequence is a bug. It is asserted in debug and
reported as the result's failure in release, never appended silently. A failure reply synthesized
at a terminal transition (ADR-0002 E3) carries no rows and becomes the result's `Failed` or `Ended`
state.

**Cancellation, abandon and the missing Cancel.** No fetch can be interrupted on request. A
running fetch ends when the client has received and decoded the whole array. The time limit fires
only if the server is silent for longer than the limit on one socket read, because it bounds each
read, not the fetch (Context). The client-side decode between reads, which is the O(packets²) part,
has no bound at all.

- The first fetch carries the query's work, so a long `SELECT` shows as *running* until its first
  segment arrives. The UI shows the limit armed at execute as a limit on waiting for the server,
  never as a countdown or a stop time. It never offers Cancel.
- **Discarding** a result (a new execute in the worksheet, closing its grid) marks the store
  discarded at once and submits `close_result`. In-flight replies still arrive, exactly one per
  request (E2), and are released on arrival. The close queues behind them on the worker, so the
  session stays busy until the running fetches finish. The UI says so without naming a time:
  "waiting for the current fetch to finish". It shows no stop time while any result set is open
  on the connection, following `OpenResultSets`: `OracleCancelHandle::remaining` declines to name
  one then, because every later statement re-arms the one socket timeout for all open cursors.
- **Abandon or loss of the session** (ADR-0002 R4/R6): the store moves to
  `Ended { cause: SessionEnded }`. Its segments stay readable, because they are plain data and are
  never cleared behind the user's back, under a banner saying these N rows are all that was
  fetched. LOB ids die with the session (K1/K8), so LOB cells read "unavailable (session ended)",
  never NULL.

**A transaction end, or a rollback to a savepoint, ends every open store of the session,
deterministically.** This is a lead decision of 2026-09-25, from the review. The events that end
the stores are:

- a successful commit, rollback or rollback-to-savepoint command;
- a successful typed `COMMIT`, `ROLLBACK` or other `TransactionControl` statement;
- a statement that committed implicitly (DDL).

The prefix stays readable. No further fetch is submitted. The state becomes
`Ended { retained, cause: TransactionEnded }`, and every LOB cell in the prefix **is** unavailable
("unavailable (transaction ended)"). "Keep the cursor open" and "Fetch more" (RS3) therefore last
only until the next of these, or until the next execute replaces the result in that worksheet
(M4.3).

- **Why not follow Oracle's cursor rules.** On Oracle an ordinary cursor survives `COMMIT` and
  `ROLLBACK`, and a `SELECT … FOR UPDATE` cursor fails after either with ORA-01002. But `db-core`
  already closes every cursor and clears every parked LOB after a successful commit, rollback or
  rollback-to-savepoint command. `worker.rs` `resolve_transaction` and the
  `RollbackToSavepoint` arm call `release_results()`. `finish_execute` does the same after an
  implicit commit, per ADR-0002 D2, which treats cursors and locators as transaction-scoped. A
  fetch after that fails with `DriverInternal` "unknown, closed or invalidated result handle".
- **The gap being closed.** A typed `COMMIT` or `ROLLBACK` (`StatementKind::TransactionControl`)
  does **not** release results today, so one transaction end would behave two ways. The ADR-0002
  amendment of 2026-09-25 ("every transaction end releases results", X1) records the change. It is a
  small work item that lands with M5.2 or M4.3, whichever comes first. The core must not parse
  SQL, so it releases after every successful `TransactionControl` statement. That includes
  `SAVEPOINT` and `SET TRANSACTION`, which end nothing: ending a result early is the safe error.
- **How the store learns it, in order.** When the hub submits a commit, rollback or
  rollback-to-savepoint command, every open store of that session stops submitting. Fetches
  already submitted run first (FIFO) and their rows are kept. On the command's success reply the
  stores end. On its failure they resume, because the worker releases nothing when the command
  fails. For a statement, the store learns from the `EXECUTED` outcome: `committed_implicitly`,
  or kind `TransactionControl`. Production order delivers that outcome before the reply to any
  fetch submitted after it, so the store maps such a fetch's "invalidated result handle" error
  to the `Ended` it has already entered, never to `Failed`.
- **What the core cannot see.** A commit inside a PL/SQL block is invisible to the core. After one,
  the cursor behaves as Oracle says: an ordinary cursor keeps fetching (and may later fail with
  ORA-01555), and a `FOR UPDATE` cursor fails with ORA-01002 and the store shows
  `Failed { after N rows }`. Either way the result never looks complete: D2 makes a driver report
  an invalidated cursor as an error, never as a short result.
- **A statement that fails can still have ended the transaction.** The core ends the stores only
  on a *successful* reply, because the worker releases nothing when an execute fails
  (`finish_execute`). Oracle can commit anyway, and the core cannot see it. The review of M5.2
  reproduced two cases:
  - **A DDL that fails still commits.** `INSERT`, then `CREATE TABLE` on an existing name
    (ORA-00955), then `ROLLBACK`: the inserted row survived.
  - **A `COMMIT` that fails can still end the transaction.** For example, ORA-02091 (transaction
    rolled back) at commit time.

  In both cases the store stays `Open`, and its cursor behaves as Oracle says. An ordinary cursor
  fetched to `Complete` in the review. A `FOR UPDATE` cursor ended `Failed { ORA-01002 }`. Both are
  honest, but "Keep the cursor open" then outlives the transaction it was scoped to. This is
  Accepted limitation 13 and owner-review point (f).

**The honest state, and how it reaches the UI.** The store exposes a typed `ResultState`, never
prose:

| State | Meaning | What the UI can say (M6.3 translates) |
| --- | --- | --- |
| `Fetching { retained }` | a fetch is outstanding | "Fetching… 3,000 rows" |
| `Open { retained }` | cursor open, idle; more rows may exist | "3,000 rows fetched — scroll for more" |
| `Complete { rows }` | the cursor is exhausted; every row is in the store | "12,345 rows" |
| `LimitReached { retained, limit: Rows \| Bytes, more: Yes \| Unknown }` | stopped at a cap | "Fetched 1,000,000 rows (row limit reached: 1,000,000, from the application default). The result has more rows." |
| `Failed { retained, error }` | a fetch failed after `retained` rows | "Fetched 5,000 rows, then: ORA-01555 … The result is incomplete." |
| `Ended { retained, cause: SessionEnded \| TransactionEnded \| Discarded }` | no further fetch will happen | "Transaction ended — 5,000 rows were fetched before it did. Run the query again for more." |

Every state carries the **caps in force with their provenance**, as ADR-0006 `Resolved<T>` values
(`value`, `source: Level`), so the grid can say where a limit came from, as M3.6 does for settings.
Across the ABI the state is one `struct_size`-versioned POD read with `reldex_result_state` (RS5).
The adapter reads it after every `FETCHED` it drains and after `Terminal`. No new event kind is
needed. QML composes the sentence and formats the numbers for the locale; the core sends numbers
and enums.

### RS3 — Row and byte caps, per result, both user settings; what happens at the cap

Three settings join the ADR-0006 registry. M5.2 adds them; a new setting needs no schema
migration. The registry rows are recorded in ADR-0006's amendment "Result caps (ADR-0004)".

| Setting | Value kind | Desktop default | Mobile default (RS6) | Levels | Bounds | "No limit" |
| --- | --- | --- | --- | --- | --- | --- |
| `results.max_rows` | `EntryLimit` (existing) | **1,000,000** | **100,000** | application, profile, worksheet | 1–2,147,483,647 | yes: the grid's own ceiling of 2,147,483,647 rows (Qt's `int`) |
| `results.max_bytes` | `ByteLimit` (existing) | **512 MiB** | **64 MiB** | application, profile, worksheet | 16 MiB up to 4 GiB − 1 B (`ByteLimit` holds a `u32`) | yes: see below |
| `results.close_cursor_at_limit` | `bool` (existing) | **off** (keep the cursor open) | off | application, profile, worksheet | — | — |

- **Value kinds.** No new kind is needed.
  - `EntryLimit { Count(NonZeroU32) | Unlimited }` already exists for query history, and it
    already says "at most this many, or no limit".
  - The at-limit behaviour is a `bool` named for its non-default action. There are two behaviours
    and no third in sight. A `bool` reuses the storage encoding and the settings UI's toggle,
    where an enum would need a new `ValueKind`, a storage tag and a widget for one binary choice.
    If a third behaviour ("ask each time") is ever wanted, it becomes a new setting id rather than
    a widened kind.
- **Per-target defaults.** The built-in default is chosen by target OS at build time (a `cfg` on
  the static descriptor). Resolution and provenance are unchanged: the UI reports "built-in"
  either way.
- **`results.fetches_in_flight`.** Its meaning changes from the one pipeline a process has
  (ADR-0006 P2) to **per result** (RS2). It stays application-only.

All three take effect at the next statement. Raising a limit for one result without changing the
setting is "Fetch more" below.

**Why these numbers.**

- **1,000,000 rows** is `SPEC.md` §19's figure. S15 proved the grid at that size, and with the
  store the S14 shape costs 73.3 B/row, 73 MB per million rows (Table 1). For every shape up to
  about 537 B/row the row cap is the one that binds, which covers the S14 shape (73 B), 10
  `NUMBER`s (118 B) and 5 texts + 2 dates (416 B). §19 says "1,000,000+". S14-shape rows would
  fit about 7.3 million times in 512 MiB, so the row cap could be raised. Whether it should be is
  owner-review point (b).
- **512 MiB** is the net for wide rows, not the everyday limit. It is set so that it does not bite
  before the row cap for ordinary rows. For 32 KiB rows it stops at about 16,000 rows. It is
  counted in the store's **accounted bytes**, the heap it knows it holds, not RSS. Measured, the
  process's private bytes ran 1.03–1.06× the accounted figure for the store at 1,000,000 rows, and
  1.002× for 32 KiB rows (Table 1). The gap is allocator overhead, and the cap does not try to
  predict it.
- **Keep the cursor open** so that "Fetch more" continues the *same* cursor and the *same*
  read-consistent snapshot. Running the query again would give a new snapshot, repeat any side
  effects of functions in the select list, and without `ORDER BY` a different order. The cursor
  stays open only until the transaction ends or the next execute replaces the result (RS2).

**What happens at the cap.**

1. **Stop submitting fetches.**
   - **The row cap is exact.** The fetch that would reach it asks for `room + 1` rows. If
     `room + 1` come back, the extra row is kept as a lookahead (counted in bytes, not shown) and
     the state is `LimitReached { more: Yes }`. If fewer come back, the result is `Complete` and
     no limit message appears.
   - **The byte cap is checked before each submit**, and requests are shrunk to what fits.
   - **The first fetch has no observed width**, so it is sized by the describe's declared widths
     against the bytes-per-round-trip budget (RS2), and never against the cap alone. Without that,
     a first request at high settings can by itself exceed the cap: 2 in flight × 10,000 rows ×
     32 KiB is 640 MiB, more than the whole 512 MiB. The same bound limits the wire array size
     once the driver change lands, which also caps the O(packets²) cost.
   - **The residual overshoot** is what the fetches in flight carry when the cap is reached. While
     requests are sized by declared width, that is at most `fetches_in_flight × budget` bytes: 512
     KiB at the defaults. Once they are sized by the observed average, a round trip can exceed its
     budget only by rows wider than the average so far, and never beyond declared width × rows.
     Until the driver change lands, one round trip's rows sit in the driver regardless, because
     `results.fetch_rows` is the wire array size: 1,000 rows at the default (32 MiB of 32 KiB
     rows), and a user who raises `fetch_rows` raises this bound. The settings UI says so.
   - For the byte cap `more` is `Unknown`, unless the batch that crossed it was short.
2. **The cursor stays open** by default. "Fetch more" raises this result's caps by one more step
   of the same size, or to "no limit" for this result only. That lasts until the transaction ends
   or the next execute replaces the result (RS2); after that the store is `Ended` and "Run again"
   is the only way on. With `results.close_cursor_at_limit` on, the cursor is closed at the cap.
   Its server cursor, snapshot and any temporary LOBs are freed, "Fetch more" is unavailable, and
   "Run again" re-executes, which the UI states is a new snapshot.
3. **Transactions on Oracle.** Nothing in the store commits or rolls back. Auto-commit stays OFF
   and ADR-0002 K4/K7 are unchanged. An open cursor holds an `OPEN_CURSORS` slot and a
   read-consistent snapshot, and a late fetch can fail with ORA-01555 (RS2). It holds **no row
   locks** of its own. A `SELECT … FOR UPDATE` locked every row of its result at execute, before
   any fetch, and those locks belong to the transaction until commit or rollback. Closing the
   cursor at the cap does not release them, and the UI must not imply it does. Closing the session
   with a result open releases the cursor, and the existing close rules decide the transaction
   (ADR-0002 K4). A commit or rollback ends the store (RS2), and that is when `db-core` closes an
   open cursor.

**"No limit", stated honestly** (ADR-0006 P2 `NoLimitConsequence`; the UI shows it when chosen;
the two new consequences are named in ADR-0006's amendment):

- **Rows, no limit:** "Fetch all" pulls the whole result, up to the byte cap and the grid's ceiling
  of 2,147,483,647 rows. On a large table that holds the cursor, the network and the server for as
  long as it takes.
- **Bytes, no limit:** memory grows with the result until the machine runs out. Reldex cannot
  protect other worksheets or the rest of the application. Windows pages heavily first. When an
  allocation finally fails, the process aborts: that is Rust's behaviour on out-of-memory. The
  abort takes every worksheet's session with it, and the server rolls back their transactions.
  Editor text is protected only by autosave (`phase-1.md` R7, M6.2).
- **Both:** both of the above.

The caps are **per result**. There is no process-wide budget in Phase 1 (Accepted limitations), so
eight worksheets (M4.9's figure) can hold 8 × 512 MiB at worst.

### RS4 — Spill, eviction and Arrow are deferred to P3, with the benchmark that reopens each

**Eviction without spill is ruled out, not deferred.** With a forward-only cursor an evicted row
returns only by re-running the query, on a different snapshot and possibly with side effects, so
"scroll back" would show different data. Phase 1 never evicts. The caps are the bound.

**Spill to disk: deferred to P3.** Reopen it when **either** holds:

- a requirement appears to browse, in the grid, results larger than the byte cap on desktop. That
  is an owner decision, not an engineering one;
- M5.7 or a physical-device run shows the default caps cannot be held on a target machine.

A spill design is accepted only if a benchmark with this ADR's four shapes shows all four of the
following. The shapes are `numbers10` and `s14` at 10,000,000 rows, `text5date2` at 2,000,000 rows,
and 32 KiB inline text at 100,000 rows.

1. Resident store memory ≤ 256 MiB on desktop and ≤ 32 MiB on mobile, whatever the row count.
2. A random 1,024-row window read from spilled segments with p99 ≤ 1 ms.
3. K1's frame cost unchanged (p50 ≤ 8 ms) while jumping across the spilled range.
4. Append throughput at least the best fetch rate measured on the real path at the time. Today
   that is about 300,000 rows/s for `s14` (Table 3b); spilling must never slow fetching.

**Arrow: deferred to P3** (`SPEC.md` §12, `ROADMAP.md` Phase 3). Reopen it when **either** holds:

- an Arrow-backed segment, measured with this ADR's bench at 1,000,000 rows, uses ≥ 20% fewer
  accounted bytes per row, **or** appends ≥ 20% faster, on at least two of the three shapes,
  without making a random cell read more than 10% slower;
- a feature needs Arrow interchange: Arrow IPC or Parquet export (`SPEC.md` §21, Future), or
  compute kernels for P2's sort and filter.

Recorded now so the reopening starts from facts:

- Arrow's `Decimal128` holds 38 digits and Oracle's stored `NUMBER` up to 40. The measured n9
  column does not fit, so Arrow needs `Decimal256` (32 B/cell) or a fallback, and loses to the
  scaled `i64` (8 B) on ordinary numbers.
- Arrow's dates are proleptic Gregorian. Reldex's `Timestamp` follows the historical mixed calendar
  (ADR-0002 M7), so dates before 1582 would need a conversion rule.
- `arrow-rs` would be the largest dependency in the mobile binary, and it must be measured as one.
- Estimated on paper, not measured: Arrow's 4-byte offsets and 8-byte second-precision dates
  would save about 16% on `s14` and 9% on `text5date2`, and its 16-byte `Decimal128` would cost
  more than the scaled `i64` on `numbers10`. That is below the 20% bar on two of the three shapes,
  and both savings are open to the store itself, without Arrow, if they are ever wanted.

### RS5 — The C-ABI shape M5.2 must implement (ABI 4)

ADR-0003 is still Proposed, and its D4 already defers retention to this ADR. The shape below
therefore follows D4's principles and changes only ownership. M5.2 records it as an ADR-0003
amendment and moves the ABI major from 3 to 4, because `FETCHED` changes meaning.

- **Ownership moves to the store.** `FETCHED` no longer transfers a batch. It says "result R has
  appended N rows, or changed state", and carries the result id as A21 made it do.
  `reldex_batch_release` is removed: every `ReldexBatch` is now **borrowed** from a result's store,
  so there is nothing for the adapter to release and no way to free one twice.
- **New calls.** Names are indicative; M5.2 fixes them, and the header states each call's cost, per
  A19's rule.
  - `reldex_result_state(hub, session, result, ReldexResultState* out)` returns RS2's state, the
    retained rows and bytes, `more_rows`, the limit hit, and each cap in force with its level. The
    struct starts with `struct_size`, and its enums reserve 0 for unknown (ADR-0003 D7).
  - `reldex_result_set_demand(hub, session, result, size_t rows)` is RS2's input. It returns at
    once: a submit is a channel send, and no I/O happens on the calling thread.
  - `reldex_result_fetch_more(hub, session, result, …)` raises this result's caps by a step or to
    no limit, and `reldex_result_stop(…)` stops submitting.
  - `reldex_result_segment(hub, session, result, size_t row, const ReldexBatch** out,
    size_t* first_row)` returns the segment that holds `row`. The existing
    `reldex_batch_row_count`, `_column_count`, `_column_info`, `_column` and
    `reldex_batch_format_column` work on it unchanged; `_column_fixed` changes (below).
  - Rows and row counts are `size_t`: A11's rule is that a count of things in this process is
    `size_t`. Ids, including LOB ids, stay `uint64_t`.
- **Threads.**
  - `_segment`, `_state`, `_set_demand`, `_fetch_more` and `_stop` read or change a result's
    segment list and policy. They are called from the **draining thread only**, the Qt main thread
    (ADR-0003 D5 rule 3), and the hub checks this as it checks D5's other rules.
  - Reading a segment returned by `_segment` is sound from **any** thread (A12). A published
    segment is an immutable `Arc`, and compaction on the worker finishes before the segment is
    published, so a reader and the worker never touch the same segment.
  - Appending happens when the hub drains `FETCHED`, on the draining thread (RS1). M2.15 replaced
    `crates/ffi`'s per-session pump with `db-core`'s event queue; this rule did not change.
- **Lifetime.** A segment pointer, and every pointer taken from it, stays valid until the caller
  **submits** `reldex_session_close_result` for that result, submits `reldex_session_close`, or
  calls `reldex_hub_destroy`. That is A20's rule, including "submits, not is answered". The core
  never invalidates one on its own: not at a cap, not on a failure, not at a transaction end, not
  on session loss. On loss the segments move to the `lost` holder, exactly as A20 does for column
  descriptions. No segment is evicted in Phase 1.
- **What is zero-copy, and what is a copy.**
  - Zero-copy, pointer arithmetic per cell: text, bytes, JSON and `Unsupported` (buffer plus
    offsets), the `bool`/`f32`/`f64` arrays, and LOB ids (a `uint64_t` array behind `fixed`, stride
    8, 0 when NULL).
  - `NUMBER`, whether scaled or decimal, and `TIMESTAMP` are never exposed raw in the plain view
    (`fixed == NULL`). They are read through the bulk formatter into a caller-owned arena, one
    1,024-row window per column at a time. A15's "no allocation per cell" is unchanged.
  - **No mirror is cached for a store segment** (lead decision, 2026-09-25). Under ABI 3,
    `reldex_batch_column_fixed` builds a `ReldexNumber` mirror (46 B/row) and keeps it for the
    batch's life (A19). Under the store, that would be the result's life. It would silently undo
    the 8 B scaled `NUMBER`, and the byte cap would not see it. For a store segment, M5.2 therefore
    replaces the cached mirror with an on-demand read. The caller passes a buffer and a row window,
    and the call converts from the scaled `i64` (or the fallback `Number`, or the `Timestamp`) into
    that buffer and keeps nothing. Either it fills a caller-owned buffer or it returns a borrowed
    view valid until the next call: M5.2 chooses and the header states which. No case was found
    that needs a cached mirror. If M5.2 finds one, the mirror is charged to the byte cap and the
    ADR says why.
- **Finding a row.** A row maps to its segment by division when every segment before the last
  holds the same number of rows. The store knows whether that holds. Otherwise the lookup
  binary-searches a start-row index, O(log segments). The store's own policy breaks uniformity:
  the `room + 1` request at the row cap, requests shrunk for the byte cap, the observed-width
  sizing of RS2, and each "Fetch more" can all leave a short segment that is not the last
  (Accepted limitation 10). The adapter caches the last segment it touched. Measured, a random
  single-cell read over a 1,000,000-row prefix is bound by memory latency in every representation:
  medians 50–124 ns in the store, against 55–121 ns for today's retained batches (Table 1 notes).
- **"Virtualized, never one QML object per row" at the boundary** means:
  - no per-row object, no per-row call, and no per-cell call beyond pointer reads cross the ABI;
  - a row count is a number;
  - the model asks for a segment once per segment it touches, and for a formatted window once per
    1,024 rows per column (ADR-0003 D4);
  - a drain appends rows **by count**, with one `beginInsertRows` per drain rather than per batch.
    That is an input to M5.8.
- **Consistency with S15.** The per-cell read path is the one S15 measured (K4: 100–120 ns per warm
  cell including Qt; the boundary under 1% of a drain). Only the segment lookup is new: a division,
  or a binary search when segments differ in size. M5.7 re-measures regardless.

### RS6 — Mobile: the same core and store, with smaller default caps

Android and iOS, phone and tablet, use the same `ResultStore`, the same segments and the same
policy. Only the built-in defaults of RS3 differ: **100,000 rows and 64 MiB**, chosen at build time
by target OS and still user-configurable.

Why smaller:

- A mobile OS does not fail an allocation first. Android's low-memory killer and iOS's jetsam end
  the app. Every session goes with it and the server rolls back their transactions, the worst
  outcome `SPEC.md` §2 ranks.
- Fetching runs over Wi-Fi or cellular.
- A phone shows 15–25 rows, so 100,000 rows is thousands of screens.

With the store, 100,000 rows of the S14 shape is 7.3 MB, and 64 MiB holds about 160,000
text5date2 rows or about 2,000 rows of 32 KiB text. The row cap again binds for rows up to about
670 B.

**These numbers are reasoned, not measured on a device.** Nothing here was built, run or measured
on Android or iOS. `SPEC.md` §25 and `AGENTS.md` require physical-device evidence before any mobile
claim, and the defaults are re-measured when P4/P5 run on a device. `results.fetch_rows` keeps its
default on mobile until then.

### Robust to the pending rulings

- **Fetch size (§C.3 item 10, M5.6).** The question is re-asked (owner-review point (a)). On
  Oracle 19c a row count cannot be a safe default, because the cost of one fetch follows the bytes
  it carries: 1,000 rows is 4.4 ms, 22 ms or 23 s depending on the row (Table 3a). The sign-off
  should be on a **bytes-per-round-trip budget** (RS2) that M5.6 measures, with
  `results.fetch_rows` kept as an upper bound. The store itself is indifferent:
  - Segment size follows rows per round trip, and the store's cost barely moves with it: 76.1 /
    73.3 / 73.1 B/row for `s14` at 100 / 1,000 / 10,000 rows per segment, and 421.6 / 415.5 /
    414.9 for `text5date2`. Today's retention swings 594–721 B/row on the same data (Table 4).
  - **If round trips get small** (a small budget, wide rows), the per-segment header costs about
    3 B/row at 100 rows. If it ever exceeds 10% of a row, M5.2 coalesces small batches into
    segments of at least 1,024 rows on the worker. The first segment is never delayed to coalesce.
  - **If the owner keeps a plain row count**, the store works unchanged. Wide rows then keep Table
    3a's cost, and "Stop fetching" keeps its worst case of `fetches_in_flight` × one fetch (RS2).
  - **On a server that marks end-of-response** (23ai or newer, per U-19), the
    quadratic term does not arise. The budget can then be larger, and M5.6 records which server
    each number came from.
- **K3 (ADR-0003).** The store's per-row cost does not depend on which baseline the owner picks.
  For the S14 shape through the mock path it is 73.1 B/row accounted (Table 2). Scaled by the
  measured private-to-accounted ratio (about 1.05, Table 1), that is about 77 B/row of process
  memory, against 114–119 B/row that S15's retained batches cost headless: a saving of about
  37–42 B/row. That puts the process-start reading, 200.3–200.8 MB today and over 200 × 10⁶ B, at
  roughly **158–164 MB**. **This is an estimate** from two different instruments, and M5.7 measures
  it in-app. Under either reading K3 holds with the store.
- **K2.** The store adds nothing before the first paint. The first fetch is submitted at
  `EXECUTED`, its segment is published on arrival, and compaction costs about 45 µs per 1,000
  rows. If K2 is ruled on the cold path, its remedy stays where M6.9 put it (delegate pre-warm in
  M4.x), not here.
- **K1.** The read path is unchanged. The segment lookup adds a division.

## Consequences

- **M5.2** implements, in `db-core`:
  - `ResultStore`, its segments and `ResultState`;
  - compaction on the worker thread;
  - the fetch policy;
  - the row-cap lookahead and byte accounting.

  It also:
  - adds the three settings (existing value kinds) and per-target built-in defaults in
    `crates/workspace` (ADR-0006 amendment "Result caps");
  - extends the bulk formatter to scaled numbers, and replaces `_column_fixed`'s cached mirror
    with an on-demand read for store segments (RS5);
  - bounds bytes per round trip from the declared widths, and gets the driver mechanism that
    applies it (RS2);
  - makes every transaction end release results, including typed `TransactionControl`
    statements (the ADR-0002 amendment of 2026-09-25), unless M4.3 lands that first;
  - replaces `FetchedBatch::lob`'s linear search with the per-column id array;
  - implements ABI 4 in `crates/ffi`;
  - strips the fetch logic and batch ownership out of `SessionController` and `ResultTableModel`;
  - extends the K1 test.

  Its tests:
  - accounted versus private bytes;
  - exact row cap, lookahead, and the overshoot bound, including a first fetch sized by
    declared widths;
  - sequence checking;
  - discard with fetches in flight;
  - session loss keeping the prefix;
  - **a transaction end ending the store**: after a commit, rollback or rollback-to-savepoint
    command, a typed `COMMIT`/`ROLLBACK`, and a DDL statement, the store is
    `Ended { cause: TransactionEnded }`, the prefix is readable, no fetch is submitted, a fetch
    already in flight is kept, and every LOB cell is unavailable. A failed commit leaves the store
    open;
  - a lossless re-encoding property test over random `Number`s;
  - formatting identity: for every value, the scaled form formats byte-identically to `Number`'s
    canonical `Display`, across segments of different scales (RS1). M5.3 repeats it through the
    grid.
- **M5.3** shows NULL, `Taken`, `Unsupported` and "unavailable (session ended / transaction
  ended)" as different states. It puts RS2's state, the caps in force and their provenance in the
  result's status line.
- **M5.4** copies from segments. A range past the retained prefix is not copyable, and the UI says
  what was copied.
- **M5.5** reads LOBs by id. A LOB cell is charged a nominal **256 B** against the byte cap (the
  core-side structures plus an allowance for the driver's locator) until M5.5 measures the real
  per-locator cost. A LOB that has been *read* keeps `oracle-thin`'s 64 KiB staging buffer
  (`lob.rs`, `STAGING_BYTES`) until it is closed, so the viewer closes what it opened and the
  store charges an open LOB that 64 KiB.
- **M5.6** does not pick a row count off Table 3b's curve. It measures and proposes a
  **bytes-per-round-trip budget** (RS2) on this server version and on a real network, and keeps
  `results.fetch_rows` as the upper bound. Table 3a is its first input. Its probes must make every
  row's values different (`phase-1-m5-1-data/README.md`: TTC compresses repeated values). It
  records the server version with every number, because a server that marks end-of-response does
  not have the quadratic term. The bench here gives the store-side cost per segment size.
- **M5.7** re-runs the perf gate with the store: in-app RSS against accounted bytes, and K3's
  readings.
- **M5.8** is helped by on-demand paging, since only read-ahead arrives while the user scrolls, and
  by appending by count per drain. "Fetch all" while scrolling still needs its drain budget.
- **M4.3.** Re-executing in a worksheet replaces its current result (submit `close_result`, discard
  the store). A script that produces several results gets one capped store each, and M4.3 decides
  how many a run keeps. A `COMMIT`, `ROLLBACK` or DDL later in the same script ends the earlier
  results' stores (RS2).
- **M6.3.** The state strings are composed in QML from typed state, translated, with numbers
  formatted for the locale. No text comes from the core.
- **P2 export** streams from the open cursor after the retained prefix: the prefix first, then
  further batches written and **not retained**, so the caps limit retention and never export. If
  the cursor was closed (at a cap with `results.close_cursor_at_limit`, after a failure, or at a
  transaction end), export offers to run the query again and says it is a new snapshot.
- **P2 sort and filter** is offered on the client only over a `Complete` result, as a row
  permutation (4 B/row) over the segments. Otherwise the query is re-run with `ORDER BY` / `WHERE`
  (`phase-1.md` §C.1's reason for the deferral).
- `ARCHITECTURE.md` §13 item 6 is resolved by this ADR. `SPEC.md` needs no change: the caps
  implement §12's "bounded memory", and whether §19's "1,000,000+" wants a higher default row cap
  is owner-review point (b).
- ADR-0006 gains the registry rows and two `NoLimitConsequence` values (its amendment "Result caps
  (ADR-0004)"). ADR-0002 gains the amendment "every transaction end releases results".
  `phase-0-spike-results.md` S14 gains a note naming the cause of its unexplained slowdown.

## Accepted limitations

1. **Nothing past the caps can be browsed in the grid in Phase 1.** There is no spill and no
   eviction (RS4). Export (P2) streams past them.
2. **The byte cap counts accounted bytes, not RSS.** It excludes allocator overhead (3–6% for the
   store at 1,000,000 rows, 0.2% for 32 KiB rows) and replies not yet drained (at most
   `fetches_in_flight` batches).
3. **The byte cap can be overshot** by what is already requested when it is reached: at most
   `fetches_in_flight` round trips. Sized by declared width, that is `fetches_in_flight × budget`.
   Until the driver can size the wire array (limitation 11), it is `fetches_in_flight × fetch_rows`
   rows, and the bound grows with `fetch_rows`.
4. **There is no process-wide budget.** The worst case is the number of open results × 512 MiB.
5. **LOB cells are charged a nominal amount.** Parked locators live on the worker thread and, for
   temporary LOBs, in the server's temporary tablespace until the result closes. Neither is
   measured here.
6. **An open cursor holds server resources while the user reads**: a cursor slot, a snapshot, and
   the risk of ORA-01555 on a late fetch. They are reported, not prevented. Turning on
   `results.close_cursor_at_limit`, closing a result, or ending the transaction frees them.
7. **A fetch cannot be cancelled, and the time limit does not bound it.** "Stop fetching" only
   stops the next submit. A running fetch ends when the client has decoded the whole array. The
   limit bounds each socket read, not the decode, so no stop time is shown while a result set is
   open. The worst case is `fetches_in_flight` fetches: about 2 × 23 s at today's defaults on
   16 KB rows (RS2, Table 3a; ADR-0001).
8. **Mobile caps are unmeasured** on any device.
9. **One value can cost a whole segment column.** A single `NUMBER` in a segment column that is
   outside `i64` or has more than 18 decimals keeps that column at 44 B per cell for that segment.
10. **Row lookup is O(1) only while every segment but the last holds the same number of rows.**
    Two things break that and turn the lookup into a binary search:
    - a driver that returns a short batch mid-stream;
    - **the store's own policy**: the `room + 1` request at the row cap, requests shrunk to fit
      the byte cap, observed-width sizing, and each "Fetch more" can each leave a short segment
      that is not the last.

    `phase-1.md`'s M5.2 row asks for O(1). This is where it holds, and where it degrades to
    O(log segments): 10–17 steps for 1,000–100,000 segments.
11. **Wide rows are slow to fetch until the byte bound can be applied.** On Oracle 19c one fetch
    costs about the square of its bytes (`oracledb` 26.0.0-beta.3; Table 3a). The fix in this
    ADR, bounding bytes per round trip (RS2), needs a driver mechanism that does not exist yet.
    Until it lands, a 1,000-row fetch of 16 KB rows takes about 23 s. The upstream cause is U-19;
    Issue J is its draft (owner-review point (e)).
12. **On Oracle the wire array stays at the driver's default of 100 rows, and `results.fetch_rows`
    does not size it.** Added with M5.2 Stage A as a lead decision for Stage B. The adapter's
    execute passes no fetch-size hint, so `oracledb`'s default of 100 rows is what each wire round
    trip carries, fixed at execute. `results.fetch_rows` bounds only the rows the store requests
    (RS2).
    - Passing `results.fetch_rows` (1,000) as the hint would multiply a wide row's round-trip cost
      by the square of the ratio (Table 3a). So it is not passed until one of two things happens:
      M5.6 measures a budget, or upstream ships a setter for the array size after execute.
    - The cost is more round trips for narrow rows on a real network, which M5.6 measures.
    - Owner-review point (g).
13. **A statement that fails but still ended the transaction does not end the stores.** A DDL that
    fails still commits on Oracle, and a `COMMIT` can fail after the transaction was rolled back
    (ORA-02091). The worker releases nothing on a failed execute, so the stores stay `Open` over a
    transaction that has ended (RS2, "What the core cannot see").
    - The cursor stays honest either way. It either keeps fetching or fails with ORA-01002; it is
      never presented as complete.
    - Owner-review point (f).

## Owner-review points

The lead accepted this ADR on 2026-09-25. These points go to the owner. None of them blocks M5.2,
because each is either a setting's default or information.

- **(a) The fetch-size sign-off (§C.3 item 10), re-asked with the corrected diagnosis.**
  - The Phase 0 and S15 numbers behind "1,000 rows" did not see the cause. On Oracle 19c,
    `oracledb` 26.0.0-beta.3 costs about the square of the bytes in one fetch (Context,
    Table 3a).
  - The ADR asks the owner to sign off a **bytes-per-round-trip bound** (RS2) instead of a row
    count. M5.6 measures the bound; M5.2 uses a 256 KiB placeholder. `results.fetch_rows` stays
    as the upper bound and the setting.
- **(b) Whether a 1,000,000-row default cap satisfies `SPEC.md` §19's "1,000,000+".** S14-shape
  rows fit about 7.3 million times in 512 MiB, so the row cap could be raised without touching
  the byte cap. The 512 MiB byte cap and keeping the cursor open at the limit (RS3) are part of
  the same question.
- **(c) Phase 1 cannot browse past the caps.** Spill is deferred to P3 and eviction without spill
  is ruled out (RS4). Export streams past the caps (P2). There is no process-wide memory budget
  (Accepted limitation 4).
- **(d) Mobile caps: 100,000 rows and 64 MiB** (RS6). This is for information until the
  physical-device gate: the numbers are reasoned, not measured.
- **(e) Issue J / U-19 (a draft, not posted; owner review pending).** This is the upstream issue
  for the O(packets²) fetch behaviour, in `phase-0-spike-results.md` §5 (U-19) and §6 (the
  draft). It follows the upstream-issue etiquette: dedupe first, and the owner reviews it before
  it is posted. A setter for the fetch array size after execute (RS2) is a separate, smaller ask,
  under the same owner review. This ADR references the draft and does not write it.
- **(f) Failed statements that still end the transaction (Accepted limitation 13), raised by the
  M5.2 review.** Two ways forward:
  - **Accept it as a limitation.** The cursor stays honest, and only "Keep the cursor open"
    outlives the transaction.
  - **Extend the driver contract** so that `committed_implicitly`, or a "transaction ended"
    signal, is also reported on the **error** path of an execute. `db-core` would then release
    and end the stores exactly as after a successful DDL.

  Nothing is implemented until the owner chooses.
- **(g) The wire array size on Oracle (Accepted limitation 12).** This is tied to (a) and (e).
  Stage B keeps `oracledb`'s default of 100 rows and does not pass `results.fetch_rows` as the
  fetch-size hint. It waits until M5.6 measures a budget or upstream ships a setter. The owner
  may want the larger array for narrow rows on a slow network; M5.6's data is the input.

Changing a default later is a registry edit and needs no redesign.

## Alternatives considered

- **Keep the adapter owning the batches (status quo).** Rejected. It leaves the rules in C++,
  leaves bytes unbounded (R8), and keeps the driver's text slack and 44-byte numbers.
- **Row-of-values.** Rejected: 1.7–4.2× the store's bytes on ordinary shapes (1.0× for 32 KiB
  text), 1.8–5.0× its append time, and the allocator's overhead on top (RS1, Tables 1 and 2).
- **Retain batches as delivered, with no compaction.** It is simpler and free to append, but costs
  1.7–3.8× the store's bytes on ordinary shapes. The store keeps this form only as the fallback
  for kinds with no compact form.
- **`u32` text offsets.** They save about 5% of a row, at the cost of breaking `ReldexColumnView`
  and ADR-0002 I2's layout. Not now.
- **Packed BCD (24 B) for every `NUMBER`.** It is ADR-0002's "halves the size". Scaled `i64` covers
  ordinary columns at 8 B; BCD could later replace the 44 B fallback as a new `#[non_exhaustive]`
  variant, which would take `numbers10` from 118 to about 98 B/row.
- **Stream every result to the end, as the S15 harness does.** Rejected as the default. It holds
  the server and the network for rows nobody looks at, and it is the configuration that drops
  frames while scrolling (M5.8). It stays available as "Fetch all".
- **Close the cursor at the cap by default.** Rejected: "Fetch more" is lost and the snapshot with
  it. It is a setting.
- **Evict and re-execute.** Rejected: a different snapshot and possible side effects (RS4).
- **Arrow now; spill now.** Deferred with reopening criteria (RS4).
- **One byte budget across all results.** Deferred. It needs a policy for which result gives way,
  and no evidence yet says the per-result caps are not enough.

## Evidence

**Method.** `crates/db-core/benches/result_store_shapes.rs` is a `harness = false` binary, kept
because it is cheap (about 3 minutes) and is what reopens RS4:
`cargo bench -p reldex-db-core --bench result_store_shapes -- --csv <abs> --mock-csv <abs>`.

- It generates batches the way `oracle-thin` builds them: `min(fetch_rows, 4096)` rows reserved,
  text reserved at 16 B/row and grown by doubling.
- It builds each representation in a **fresh child process**. It records accounted bytes (the
  capacities held), the growth in the process's private bytes (PowerShell `PrivateMemorySize64`,
  sampled before and after), the time spent appending (conversion only, generation excluded), and
  1,000,000 random single-cell reads.
- It then fetches 1,000,000 rows through a real `db-core` session and worker with the mock driver.
- The store is a prototype inside the bench. No production code changed. Every re-encoded
  `NUMBER` is checked against its original.
- The real-database run used a throwaway example, deleted after the run: `SessionManager` over
  `oracle-thin`, one fetch in flight, the local 19c container over loopback. It ran 100,000-row
  `CONNECT BY LEVEL` queries of the same three shapes. S14: `LEVEL`, `RPAD('row '||LEVEL,40,'.')`,
  `DATE '2026-01-01' + MOD(LEVEL,3650)`. `numbers10` is ten computed `NUMBER` columns, among them
  `LEVEL/7` (40 digits) and one NULL every 20th row. `text5date2` is five `RPAD` columns of 20–100
  characters (one NULL every 50th row) and two `DATE`s (one NULL every 30th row). All three
  statements are in `environment.csv`.
- Three repeats per cell.
- Machine: Ryzen 7 5700G, 63.4 GB RAM (37.4 GB free), Windows 11 build 26200, rustc 1.98.1, bench
  profile. No `cargo`/`rustc`/`cl`/`link` process was running when each run started, and CPU load
  was 4–17%. Other workers had been building earlier in the session.
- **The independent review of PR #39** did two things:
  - It re-ran the bench and got identical accounted bytes for all four shapes at 1k, 100k and 1M
    rows (`review-reproduction.csv`).
  - It probed the fetch path at the driver level: a release build calling `oracle-thin` directly,
    with no `db-core` worker or event queue in the path, on the same container. It measured rows
    per fetch from 500 to 10,000 for `s14` and `text5date2`, and one fetch of 50–1,000 wide rows
    (4 × `VARCHAR2(4000)`, a distinct value on every row, about 16 KB per row). It also re-ran
    with a 2 MiB SDU. There is one run per cell, and a `-warm` line precedes each series
    (`driver-fetch-probe.csv`).
- Raw numbers are in `docs/exec-plans/active/phase-1-m5-1-data/`. Its `README.md` says which file
  holds what, and why generated rows must differ from each other.

**Table 1 — retained bytes per row.** Accounted bytes; in brackets, private-bytes growth per row
(min–max of 3) at the largest count. Fetch size 1,000.

| Shape | Rows | `rows` (row of `Value`) | `batches` (today) | `store` (RS1) |
| --- | --- | --- | --- | --- |
| `numbers10` (10 × `NUMBER`, one of them 40-digit) | 1,000 | 496.4 | 442.9 | 118.2 |
|  | 100,000 | 501.0 | 442.8 | 118.1 |
|  | 1,000,000 | 496.8 [531.1–531.3] | 442.8 [444.8–445.0] | **118.1** [123.7–124.0] |
| `text5date2` (5 × `VARCHAR2(100)`, 2 × `DATE`) | 1,000 | 694.8 | 713.8 | 416.1 |
|  | 100,000 | 699.0 | 713.7 | 415.5 |
|  | 1,000,000 | 694.8 [812.2–812.9] | 713.7 [720.1–722.0] | **415.5** [427.8–428.8] |
| `clob32k_inline` (1 × 32 KiB text) | 1,000 | 32,832 | 33,563 | 32,776 |
|  | 10,000 | 32,842 | 33,563 | 32,776 |
|  | 100,000 | 32,837 [33,002–33,007] | 33,563 [33,643–33,649] | **32,776** [32,848–32,861] |
|  | 1,000,000 | not measured: about 32 GiB in every representation | | |
| `s14` (`NUMBER`, `VARCHAR2(40)`, `DATE`) | 1,000 | 201.0 | 132.9 | 73.4 |
|  | 100,000 | 205.6 | 132.8 | 73.3 |
|  | 1,000,000 | 201.4 [233.2–233.3] | 132.8 [135.1–135.2] | **73.3** [77.4–77.8] |

At 1,000,000 rows that is 118 MB against 443 MB (`numbers10`), 416 against 714 (`text5date2`), and
73 against 133 (`s14`). Private-bytes growth at 1,000 rows is page-granular noise and is left out;
it is in the CSV. A small result shows the per-segment cost. At 50 rows of `s14` the store holds
4,046 B in total, where today's batch holds 84,568 B, because the driver reserves 1,000 rows of
capacity (`review-reproduction.csv`). There is no fixed per-segment cost worth counting.

The synthetic `batches` figure for `s14` (132.8) is above S15's measured 114–119 B/row. The
bench's text is longer (every 10th row Thai, every 25th an emoji) and reserved the way
`oracle-thin` reserves it, while S15 measured RSS through the mock driver. Compare within a table,
not across the two.

Random single-cell reads at 1,000,000 rows, medians in ns for rows / batches / store:
`numbers10` 51.0 / 55.1 / 49.6, `s14` 66.9 / 78.6 / 76.2, `text5date2` 64.8 / 121.1 / 124.3. The
bench reads a text cell's length, which the row form keeps beside the cell and the other two do
not. All three are bound by memory latency.

**Table 2 — appending 1,000,000 rows.** Conversion only; median of 3 in ms, with the range.

| Shape | `rows` | `batches` | `store` | `store` per 1,000-row batch |
| --- | --- | --- | --- | --- |
| `numbers10` | 249.9 (248.9–250.1) | 0.06 | 137.9 (132.9–140.3) | ≈ 138 µs |
| `text5date2` | 561.3 (546.6–568.1) | 0.07 | 184.8 (184.1–217.8) | ≈ 185 µs |
| `s14` | 156.3 (156.1–174.3) | 0.05 | 31.1 (30.6–31.8) | ≈ 31 µs |
| `clob32k_inline`, 100,000 rows | 1,701 (1,637–1,946) | 0.03 | 489.8 (476.5–494.1) | ≈ 4.9 ms |

Through `db-core`'s worker and the mock driver, 1,000,000 rows at 1,000 rows per fetch, compacting
serially on the consumer thread:

| Shape | Kept as batches | Kept as store | Compaction | Store bytes |
| --- | --- | --- | --- | --- |
| `s14` | 487–493 ms total | 501–559 ms | 44.4–45.9 ms | 73.1 B/row |
| 10 × `NUMBER` ids | 714–758 ms | 718–766 ms | 149.4–156.0 ms | 82.1 B/row |

The in-process fetch itself takes 0.44–0.71 ms per batch at p50.

**Table 3a — the cost of one fetch against its size, at the driver level** (the review's probe;
Oracle 19c on loopback; `driver-fetch-probe.csv`). The payload column is estimated from each
statement's column values (about 52 B per `s14` row, 315 B per `text5date2` row, 16,000 B per wide
row). It is an estimate, not a measured wire size.

| Shape | Rows per fetch | ≈ Payload per fetch | Batch p50 | Client CPU / wall |
| --- | --- | --- | --- | --- |
| `s14` | 500 | 26 KB | 2.4 ms | 0.33 |
| `s14` | 1,000 | 52 KB | 4.4 ms | 0.61 |
| `s14` | 2,000 | 0.1 MB | 10.2 ms | 0.88 |
| `s14` | 4,000 | 0.2 MB | 23.4 ms | 0.89 |
| `s14` | 7,000 | 0.36 MB | 56.4 ms | 0.95 |
| `s14` | 10,000 | 0.52 MB | 129.1 ms | 0.99 |
| `text5date2` | 500 | 0.16 MB | 7.8 ms | 0.59 |
| `text5date2` | 1,000 | 0.32 MB | 22.4 ms | 0.99 |
| `text5date2` | 2,000 | 0.63 MB | 85.8 ms | 0.97 |
| `text5date2` | 4,000 | 1.3 MB | 359.9 ms | 0.96 |
| `text5date2` | 7,000 | 2.2 MB | 1,228 ms | 0.98 |
| `text5date2` | 10,000 | 3.2 MB | 2,921 ms | 0.94 |
| wide (4 × `VARCHAR2(4000)`) | 50 | 0.8 MB | 70 ms | 0.77 |
| wide | 100 | 1.6 MB | 227 ms | 0.88 |
| wide | 250 | 4 MB | 1,503 ms | 0.94 |
| wide | 500 | 8 MB | 4,939 ms | 0.92 |
| wide | 1,000 | 16 MB | **22,979 ms** | 0.94 |

- **Rows are not the variable.** At the same row count the time differs by more than 1,000× between
  shapes. At similar payloads it is within about 2×: 1.3 MB of `text5date2` takes 360 ms and
  1.6 MB of wide rows takes 227 ms.
- **The cost is quadratic in the payload.** Each doubling costs about 4×.
- **The client is the bottleneck.** Client CPU over wall time reaches 0.9–0.99 as fetches grow.
  The server and the loopback network are not.
- **A 2 MiB SDU changes nothing.** With it, `s14` measured 3.8 ms at 1,000 rows and 161.9 ms at
  10,000, and `text5date2` 24.4 ms and 3,168 ms. The O(packets²) re-parse explains this (Context).

**Table 3b — the same effect through `db-core`** (the M5.1 run: 100,000 rows through `db-core`'s
worker and `oracle-thin`, one fetch in flight; ranges over 3 runs; `real-db-fetch-latency.csv`).
Bold marks the current `results.fetch_rows` default, not the best result. For `text5date2`, 100
rows per fetch was slightly faster. The slowdown at 10,000 rows is Table 3a's quadratic term, not a
property of the row count.

| Shape | Rows/fetch | Batch p50 (ms) | First batch (ms) | Total (ms) | Rows/s |
| --- | --- | --- | --- | --- | --- |
| `s14` | 100 | 0.97–1.04 | 1.06–3.36 | 1,042–1,121 | 89k–96k |
| `s14` | **1,000** | 3.18–3.48 | 3.84–4.18 | **336–368** | **272k–297k** |
| `s14` | 10,000 | 88.5–102.3 | 111.7–138.4 | 949–1,018 | 98k–105k |
| `numbers10` | 100 | 1.17–1.18 | 1.25–1.32 | 1,212–1,228 | 81k–83k |
| `numbers10` | **1,000** | 7.11–7.71 | 7.26–7.89 | **749–807** | **124k–133k** |
| `numbers10` | 10,000 | 377–450 | 410–454 | 3,914–4,333 | 23k–26k |
| `text5date2` | 100 | 1.68–1.71 | 1.83–4.11 | 1,722–1,778 | 56k–58k |
| `text5date2` | **1,000** | 18.4–18.6 | 19.4–22.5 | **1,882–1,919** | **52k–53k** |
| `text5date2` | 10,000 | 1,983–2,052 | 1,876–2,581 | 20,452–21,299 | 4.7k–4.9k |

Execute, which is a describe on `oracle-thin`, took 1.4–2.6 ms once a statement text had been
parsed. The first run of each text took 7.7–58 ms. Loopback means **zero network latency**: every
figure here is a floor, and M5.6/M5.7 own the real-network numbers. Both tables come from one
server version (19c). A server that marks end-of-response (23ai or newer) would not show the
quadratic term.

**Table 4 — retained bytes per row against fetch size** (100,000 rows; accounted bytes)

| Shape | Form | 100 rows/fetch | 1,000 | 10,000 |
| --- | --- | --- | --- | --- |
| `s14` | `batches` | 137.0 | 132.8 | 154.4 |
| `s14` | `store` | 76.1 | 73.3 | 73.1 |
| `text5date2` | `batches` | 721.4 | 713.7 | 594.1 |
| `text5date2` | `store` | 421.6 | 415.5 | 414.9 |

**Not measured.**

- 1,000,000 rows of 32 KiB text (32 GiB).
- The driver-side cost of a parked LOB locator, and server temporary-LOB usage.
- A real network.
- Any mobile device.
- The store in the running app: in-app RSS, and K1/K2/K4 with the store in place. That is M5.7's.
- A spill or Arrow prototype. The Arrow figures in RS4 are arithmetic.
- The fetch cost on a server that marks end-of-response (23ai or newer), or with the bytes
  bound applied (RS2). The latter needs the driver change first.
- The actual wire size of a fetch. Table 3a's payload column is an estimate.

## As implemented: M5.2 Stage A, the core (2026-09-26)

Stage A is `db-core`, the settings registry, the tests and the bench re-run. Stage B, ABI 4 in
`crates/ffi` and the adapter changes (RS5), comes after it. Where the code differs from the text
above, this section says how and why. The text above is left as decided.

**Where it lives.** `crates/db-core/src/store/`:

- `segment.rs`: `ResultSegment`, `SegmentColumn`, `SegmentData`, `CellValue`.
- `scaled.rs`: `ScaledNumber`, `NumberValue`.
- `policy.rs`: `Cap`, `CapSource`, `Sourced`, `ResultCaps`, `ResultPolicy`, and the declared row
  width.
- `result_store.rs`: `ResultStore`, `ResultState`, `ResultPhase` and the fetch policy.
- `session_results.rs`: `SessionResults` and `Observed`.

The worker compacts in a new command, and its reply is `SessionEvent::FetchedSegment` (an
additive variant) or the `Completion` of `DatabaseSession::fetch_segment`. `fetch_batch` and
`FetchedBatch` are unchanged. That is RS1's "M5.2 chooses the call shape".

**RS1, segments.**

- **Compaction copies to size; it does not shrink in place.** This corrects RS1's "the measured
  bytes are the same either way" (under "Text keeps its layout and loses its slack"). The *accounted* bytes are the same. The
  process's *private* bytes are not.
  - With `shrink_to_fit`, each driver buffer's spare tail stayed behind as a free fragment
    between retained segments. On `text5date2` at 1,000,000 rows that cost private bytes 58%
    over the accounted ones.
  - The store now copies each column that has spare capacity into an exact allocation while the
    whole batch is still alive, and then frees the batch. That measures +3.2%, the prototype's
    figure (`phase-1-m5-2-data/`).
  - A column with no spare capacity is moved, not copied.
- **What a segment is charged.** The segment struct, the `Arc`'s two counters, one header per
  column and every heap capacity. The store adds its own index: the segment list and each
  segment's first row.
- **A LOB cell is charged 320 B, not 256 B.**
  - The M5.2 review measured about **288 B** of client memory per parked temporary CLOB. It was
    one rough run: 5,000 CLOBs on 19c.
  - The 256 B nominal was about 11% under that. The charge is the measurement plus about 10% for
    a single run's uncertainty: the 8 B id and 312 B for the parked entry and the driver's
    locator.
  - The declared width of a LOB column uses the same figure.
  - M5.5 measures the real cost and replaces it.
- **The worker gives parked-LOB storage back.**
  - The review found about 800 KB still allocated after 5,000 temporary CLOBs, a `COMMIT` and a
    close: about 160 B per LOB that no byte cap sees. The cause was that clearing a map keeps its
    capacity.
  - A transaction end now drops the parked-LOB and cursor maps and starts fresh ones. Closing one
    result shrinks the LOB map once it is less than half full.
  - How much client memory LOBs take, and how to bound it, is M5.5's to measure (`phase-1.md`
    M5.5 row).
- **A compaction that fails closes the cursor.** The store records such a result as `Failed`
  with its cursor closed. The worker now does the same, releasing the cursor and its parked LOBs
  exactly as after a failed fetch.
- **`FetchedBatch::lob`'s linear search is not replaced.** The batch path stays as it is for its
  current consumer, the adapter, until Stage B moves the adapter to segments. Segments already
  hold the per-column id array Consequences asks for.

**RS2, the fetch policy.**

- **The store does no I/O.** `ResultStore::pump` hands each action that is due (`Fetch` or
  `CloseResult`) to a closure. `submit_events` is the same onto a session's event path. If a
  submit fails, the action stays due and nothing is recorded.
- **One blind request at a time.** The first fetch is due at once, but no second one is
  submitted until it is answered. Otherwise the second request is also sized by declared width
  before any row is seen.
- **Read-ahead** keeps `retained + requested < demand + rows per round trip`. RS2 wrote
  `demand + fetch_rows`; rows per round trip is the byte-bounded figure RS2 defines, so this is
  the same rule once the byte bound applies. Rows per round trip is
  `clamp(256 KiB / row width, 1, fetch_rows)`.
- **Declared widths.** `NUMBER` 44 B, `DATE`/`TIMESTAMP` 16 B, a LOB 256 B, text
  `max_size_bytes + 8` (4,000 + 8 when the describe gives no size), plus one NULL bit per column.
  The observed width replaces the declared one once rows arrive, and never exceeds it.
- **"Fetch all"** is a flag that streams to the caps. **"Stop fetching"** (`stop`) clears it and
  stops submitting at once. `resume()` is added: it undoes a stop so that fetching follows the
  demand again. RS2 named no way back except "Fetch all".
- **"Fetch all" ends with empty fetches.** A driver that does not know it has read the last row
  answers the fetches still in flight with empty segments. With two in flight that is two extra
  requests: the live 5,000-row test made 7 requests for 5 segments. Each costs a round trip.
  Tuning this, for example one fetch in flight once a reply comes back short, is for Stage B or
  M5.6.
- **Sequence.** Each fetch carries a `FetchTicket` (result, sequence). A reply out of sequence
  panics in a debug build. In a release build it fails the result and is never appended
  (`Fetched::OutOfSequence`). A reply that arrives after the store failed, ended or was discarded
  is counted off and dropped (`Fetched::Stale`). This includes the "invalidated handle" error of
  a fetch submitted behind a typed `COMMIT`.
- **Routing.** `SessionResults::observe` applies the transaction-end and session-end rules to a
  session's events.
  - Commands must be announced with `transaction_end_submitted()`, so the stores stop submitting
    until the reply.
  - `Executed` carries a statement's kind and `committed_implicitly`.
  - `Completed` answers a command: success ends the stores, failure lets them resume.
  - `Terminal` ends the session's stores.
  - A store opened while a commit command is pending inherits the pause.
- **What an end does to a store that has already stopped.**
  - The session's end leaves `Complete` and `Failed` as they are, because they say more than
    "ended". Their LOB cells become unavailable (`SessionEnded`).
  - **A fetch that loses the session** (lead decision, M5.2 review) leaves the store
    `Failed { after, error }`, with the loss's classification (`NetworkLost`, native code) in
    `error`.
    - Its LOB reason is `SessionEnded`, not `ResultClosed`, whenever
      `error.session_state()` is `Lost`: the session took every LOB, not only the cursor.
    - The replies queued behind the failure are `Stale`. The `Terminal` that follows changes
      neither the phase nor the reason.
    - A fetch that fails without losing the session keeps `ResultClosed`.
    - Tests: `a_fetch_that_loses_the_session_fails_the_result_and_its_lobs_end_with_the_session`
      (unit) and `losing_the_session_mid_fetch_fails_the_result_and_ends_its_lobs_with_the_session`
      (mock, through `SessionResults`).
  - At a transaction end, an `AtLimit` store whose cursor was already closed at the cap stays
    `AtLimit`, so it still says which cap stopped it. One whose cursor was open becomes
    `Ended { TransactionEnded }`.

**RS3, the caps.**

- **The row cap is exact**, with a one-row lookahead. `LimitKind::RowCeiling` is added: with the
  row cap at "no limit", or above 2,147,483,647, the store stops at the grid's ceiling and says
  so.
- **The byte cap.** Requests shrink so that retained bytes plus the estimate for what is in
  flight stay within the cap. The store stops at `AtLimit { Bytes, Unknown }` when one more row
  of the current width would cross it. The first request always asks for at least one row, even
  under a cap smaller than one declared row.
- **The overshoot bound assumes the declared widths are true.** The observed row width is capped
  at the declared width, so the estimate for a request is never wider than the describe says.
  Some columns declare too little:
  - a type the driver renders as text (`Unsupported`, charged a 64 B floor);
  - JSON;
  - a column whose describe understates its size.

  For those, a request can carry more bytes than estimated. The overshoot is then bounded by the
  rows requested times their *actual* width, not by the budget. The row cap still bounds rows.
- **Provenance.** Each cap is a `Sourced<Cap>`: a value and a `CapSource`. `CapSource` mirrors
  ADR-0006's `Level` (`BuiltIn`, `Application`, `Profile`, `Worksheet`) and adds `FetchMore` for
  a cap that "Fetch more" raised. `db-core` does not depend on `crates/workspace`, so the adapter
  maps each `Resolved<T>` to a `Sourced` in Stage B. RS2's table said the state carries
  `Resolved<T>` itself.
- **"Fetch more".** `FetchMore::Step` raises each capped limit by its setting's value, and
  `FetchMore::Unlimited` lifts both. It applies only at a row or byte cap with the cursor open. At
  `AtLimit { RowCeiling }` no setting can raise anything, so it returns `false` and changes
  nothing.
- **Provenance stays in `db-core`** (lead decision, M5.2 review). `Sourced` and `CapSource` are
  the store's own types. Stage B maps each ADR-0006 `Level` to a `CapSource`, with a test that
  pins every level.
- **One default, two crates.** `results.fetch_rows` and `results.fetches_in_flight` have
  built-in defaults that `db-core` falls back on as `DEFAULT_FETCH_ROWS` and
  `DEFAULT_FETCHES_IN_FLIGHT`. `crates/workspace/tests/result_pipeline_defaults.rs` pins each pair
  equal.
- **`results.close_cursor_at_limit`.** At a cap, the store makes `CloseResult` due. Once that is
  submitted, `cursor_open` is false, LOB cells read unavailable (`ResultClosed`), and `fetch_more`
  refuses.

**The state's shape.** `ResultState` is a view with these accessors:

- `phase()`: `Fetching`, `Open`, `Complete`, `AtLimit { limit, more, cursor_open }`,
  `Failed { after, error }` or `Ended { cause }`;
- `rows()` (the rows shown), `retained_rows()` (the lookahead included) and `retained_bytes()`;
- `caps()`, `fetches_in_flight()`, `stopped()` and `lobs_unavailable()`.

RS2's table carried `retained` inside each state; the counts are separate accessors here.
`LimitReached` is named `AtLimit`.

**Reads.**

- `ResultStore::value(row, column)` returns a `CellValue` and does not allocate. A LOB cell
  reads `LobUnavailable(reason)` once its transaction, session or cursor has ended, and never
  NULL. `ResultStore::lob` returns a `LobCell`.
- A scaled `NUMBER` formats byte-identically to `Number`'s `Display`. Four checks hold it to
  that:
  - a 40,000-value property test;
  - 1.5 held at scale 1 and at scale 2, in two segments of one store;
  - the bench, over every generated `NUMBER`;
  - the live test on 19c.
- **Lookup (limitation 10), with one change.**
  - A row is found by division while every segment **between the first and the last** holds the
    same number of rows, and by binary search over the first rows otherwise.
    `lookup_is_constant_time()` says which.
  - The first segment is exempt. It is sized blind, by declared widths, so it commonly differs:
    63 rows against 1,000 on the live 100,000-row query. Without the exemption the common case
    would have been a binary search.
  - Requests sized by observed width still vary on wide rows (16–17 rows live), and there the
    lookup is a binary search, as limitation 10 says.

**Registry.** The three settings and two `NoLimitConsequence` values are in (ADR-0006 amendment
"Result caps", now implemented).

**Mock driver.** A mock column can declare a byte width. The generated `NAME` column declares 160
bytes (`VARCHAR2(40 CHAR)` in AL32UTF8), the width Table 1's S14 row assumes.

**Tests.**

- **Unit tests** (`store/result_store/tests.rs`, `scaled.rs`, `segment.rs`, `policy.rs`): every
  transition of the state machine.
  - Caps: the first request sized by declared widths, then observed width; the exact row cap and
    its lookahead; the byte cap's shrinking requests and bounded overshoot; a byte cap below one
    row; "Fetch more" by a step and without limit; closing the cursor at the cap.
  - Stop and resume; discard with fetches in flight; a transaction end by command (success and
    failure) and by statement kind.
  - A fetch failing after the end is stale; the session's end; a failed fetch; a reply for
    another result; out of sequence.
  - Division against binary search; formatting identity across scales; accounted bytes; LOB
    cells unavailable, never NULL.
- **Mock integration tests** (`tests/result_store.rs`), on both reply paths:
  - demand fetch to the end, with one driver thread (K1);
  - the exact row cap and "Fetch more" on the same cursor;
  - closing the cursor at the cap;
  - a failed fetch keeping its prefix;
  - **K1 extended**: segments and a whole store dropped on other threads, handles still read on
    the worker, one thread id;
  - LOB cells unavailable after a commit.
- **Mock integration tests** through `SessionResults` on the event path:
  - every transaction-end trigger ends every open store and keeps its prefix: commit, rollback
    and rollback-to-savepoint commands, a typed `COMMIT`, DDL;
  - fetches submitted behind a typed `COMMIT` are stale;
  - a commit command waits for the fetches ahead of it;
  - a failed commit keeps the store open;
  - session loss keeps the prefix;
  - discard with fetches in flight.
- **Live tests on 19c** (`crates/reldex-core-poc/tests/m5_2_result_store_live.rs`, feature
  `oracle-it`; they skip when the environment names no database):
  - 100,000 `CONNECT BY` rows whose every column varies, checked cell by cell;
  - four `VARCHAR2(4000)` columns under a 16 MiB byte cap;
  - `NUMBER`s that are exact at two scales across segments and inexact (`LEVEL/7`);
  - a CLOB column read through handles;
  - a typed `COMMIT` ending an open result and its LOB cells.

  All five pass against the local 19c container (`phase-1-m5-2-data/live-19c.csv`).
  - The 100,000 rows cost 67.6 B/row accounted, with a constant-time lookup.
  - The wide rows stop at `AtLimit { Bytes, Unknown }` after 1,080 rows and 16,770,028 bytes,
    less than one row under the 16 MiB cap, in requests of 16–17 rows.

**Bench (re-run, `phase-1-m5-2-data/`).** The bench's `store` now measures the implemented
segments. It checks every cell, `NUMBER` formatting included, after the measurements.

- **Accounted bytes per row match Table 1** within +0.66%, which is the worst case (`s14` at 100
  rows per segment). At 1,000 rows per segment the change is +0.00–0.14%: `numbers10` 118.2,
  `text5date2` 415.6, `s14` 73.4 and `clob32k_inline` 32,776.3 B/row. No regression reaches the
  5% threshold.
- **Private bytes over accounted, at 1,000,000 rows:** +5.0% (`numbers10`), +3.2%
  (`text5date2`), +3.6% (`s14`), +0.2% (`clob32k_inline` at 100,000). Limitation 2's 3–6%
  holds.
- **Times.** Append and random-read times show no regression. The machine was shared, and the
  unchanged representations moved by up to 30% between runs.
- **Through a real session**, 1,000,000 mock rows with compaction on the worker take as long as
  retaining the batches: 551 against 552 ms (`s14`) and 745 against 811 ms (`ids10`).

**Accepted limitation 11, precisely.** No driver mechanism was chosen in M5.2, so the limitation
stands. This is what the store bounds now, and what it still does not.

- **Bounded by the 256 KiB budget:**
  - each request, and so each segment;
  - the rows one reply hands the consumer;
  - the byte cap's overshoot: at most `fetches_in_flight` requests of rows at declared width, plus
    the store's index.
- **Not bounded:**
  - The driver's wire array is `Statement::fetch_rows`, fixed at execute, before the describe. A
    request smaller than it is served from rows the driver already holds. So each wire round trip
    still carries `fetch_rows` rows and costs what Table 3a says they cost.
  - Those rows wait in the driver, outside the byte cap, until the store asks for them.
  - The time to the first row of a wide result and the latency of "Stop fetching" keep that cost.
- **What the product passes today.** The adapter's execute (`crates/ffi`) sets no fetch-size hint.
  So on the product path the wire array is `oracledb`'s own default of **100 rows**, not
  `results.fetch_rows`.
  - At 100 rows, a round trip of all-distinct 16 KB rows carries about 1.6 MB, not 16 MB.
  - The live wide-row test ran this way. Its first page, 33 rows, took 93 ms, with two of its four
    columns compressed by TTC.
  - Stage B does **not** pass `results.fetch_rows` as the hint (lead decision after the M5.2
    review; Accepted limitation 12, owner-review point (g)). Passing it would restore Table 3a's
    cost for wide rows. It would also cut round trips for narrow rows on a real network, a
    trade-off M5.6 measures.
- **The mechanism is not chosen.**
  - An upstream setter for the array size after execute is the recommendation. `oracledb`
    26.0.0-beta.3 has none. It goes to the owner with Issue J (owner-review point (e)).
  - A describe before every execute would add a round trip to every query, to help the wide ones.
    It was not built.
  - M5.6 measures the budget either way.

## Status

**Accepted by the lead (2026-09-25),** after one independent review round on PR #39. That review
reproduced Table 1 exactly and returned doc-only findings, all of which are addressed in this
revision:

- the fetch-cost diagnosis and Table 3a;
- what the time limit bounds;
- every transaction end ends the store;
- RS5's thread rules and no mirror cache;
- formatting identity;
- a first fetch sized by declared widths;
- the registry entries;
- the lookup limitation.

The owner-review points (a)–(g) are open. RS3's and RS6's defaults stand until the owner answers,
and changing one is a registry edit.

ADR-0003's status is independent of this ADR. Its D4 already defers retention here, and the ABI 4
change in RS5 is recorded as an ADR-0003 amendment when M5.2 lands.
