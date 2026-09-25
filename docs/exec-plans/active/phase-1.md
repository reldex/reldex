# Execution Plan — Phase 1 Desktop MVP

**Status:** Active — spike S15 complete 2026-09-20 (M1.8 done); ADR-0003 stays Proposed, awaiting an
owner ruling on three open questions (M1.9 in progress). M2 core work proceeds under the owner's
standing instruction to continue through phases, since nothing in M2 depends on the ADR's wording and
nothing there would be wasted by a re-open.
**Date:** 2026-09-20
**Depends on:** [ADR-0001](../../decisions/0001-database-driver-strategy.md) (database driver strategy),
[ADR-0002](../../decisions/0002-driver-api-and-concurrency-model.md) (driver API and concurrency model),
[ADR-0003](../../decisions/0003-qt-rust-integration.md) (Qt ↔ Rust integration — **Proposed**, spike S15
complete 2026-09-20, owner ruling requested), and [`phase-0.md`](phase-0.md) (Phase 0 exit assessment —
the owner gave the **GO for Phase 1 on 2026-09-19**).

**Purpose.** This is the Phase 1 (Desktop MVP) execution plan produced immediately after the Phase 1 GO:
the core (`db-core`) changes needed before/with the UI (§B), the milestone plan M1–M6 with owner/inputs/
outputs/dependencies/acceptance criteria per task (§C), and the risk register (§D). ADR-0003 is not
accepted yet: spike S15 is complete and recorded (`docs/exec-plans/active/phase-1-s15-ffi-spike.md`),
no criterion's failure is located in the boundary, and both the measurement author and an independent
reviewer recommend acceptance — but the lead does not accept it unilaterally and does not re-open the
design; three rulings are requested from the owner instead (M1.9, and see ADR-0003's own "S15 result"
section). The owner decisions in §C.3 carry their
current status: #1–#9 were approved as recommended on 2026-09-20; #10, #11 and #15 are still open. Status per task uses the same legend as `TASKS.md`: `[x]` done, `[~]` in progress,
`[ ]` todo, `[!]` blocked.

---

## Preface — assumptions and repository-vs-brief notes

Stated first because the repository wins over any brief when the two disagree; the notes below are the
ones that remain true as of this plan's date.

1. The brief that produced this plan said the workspace **forbids** `unsafe`. The repository
   (`Cargo.toml`) sets `[workspace.lints.rust] unsafe_code = "deny"`, with the comment *"FFI crate(s)
   will opt out of this once the FFI boundary is implemented"*. There is already a precedent:
   `crates/mobile-link-check/Cargo.toml` documents that cargo rejects overriding `workspace.lints` in a
   manifest, so the opt-out is a crate-level `#![allow(unsafe_code)]`. ADR-0003 uses that mechanism, not
   a manifest override.
2. **Phase 0's exit assessment is no longer a draft.** The owner gave the GO for Phase 1 on 2026-09-19
   (recorded in `phase-0.md` and `TASKS.md`); the criteria verdicts in `phase-0.md` are unchanged by the
   GO (a GO is not the same as every criterion being met — criterion 4, Cancel, stays **not met**,
   accepted as a limitation). `phase-0.md` also lists open items that are Phase-0 leftovers, not new
   Phase-1 work: driver `connect_timeout` (C-5), trigger auto-rewrite (U-18), connect-warning channel
   (C-6), upstream issues F/G, and physical-device mobile validation. C-5/U-18/C-6 are carried into
   Phase 1 Milestone 2 as explicit tasks (M2.1–M2.3) rather than assumed to have landed; per the owner's
   2026-09-19 decision they were **done as Phase 0 carry-over work (branch
   `phase-0/driver-carryover`, merged 2026-09-20, independently reviewed)** — M2 consumes that result rather than
   duplicating it as fresh Phase 1 work. Upstream issues F and G are resolved, not pending: the owner
   decided on 2026-09-19 **not** to submit them for now, keeping the drafts for tracking only (results
   file §6; see §C.3 #13 below). Mobile device work stays out of Phase 1 scope (`SPEC.md` §25
   unchanged); the owner separately provided an Android arm64 phone (OPPO CPH2399) on 2026-09-19 and a
   physical-device validation run is in progress on branch `phase-0/android-device` (not finished, no
   results to cite yet) — this is a Phase 0 tail, not Phase 1 work (see §C.3 #14).
3. `SessionManager::open_session` already runs `connect` on the worker thread — what blocks is only the
   *caller's wait for the reply*. Section B below solves the wait, not the I/O placement.
4. `Number` carries **40** significant digits (ADR-0002 amendment S6), not 38.
5. Dev machine: MSVC 2022 Community is installed and `cl.exe` is on PATH, but **CMake and Ninja are not**
   and there is no `C:\Qt`. The install list in §C.0 reflects that, and M1.1 stays blocked on owner
   approval of that install (owner decision C.3 #1) — Qt is not installed and installing it needs the
   owner's explicit approval.

---

## B. Core changes needed before/with the UI

Everything here is vendor-neutral, UI-independent, and **additive**: the existing `Completion<T>` API keeps working for `reldex-core-poc`, tests, and any blocking caller. ADR-0002's "Deferred" section already blessed both shapes coexisting; this section is the ADR-0002 amendment it anticipated.

### B1 — What stays exactly as it is

One worker thread per session owning the connection; the unbounded FIFO command channel (K9); out-of-band `cancel` that never queues (D2, K3); conservative transaction tracking including locking queries (K7); `close` deciding on the worker after the queue drains and never costing a transaction on a failed disposition (K4); `Drop` bounded at `DROP_SHUTDOWN_TIMEOUT` then detaching, never committing (K5); session-scoped `ResultId`/`LobHandle` and the "only plain data crosses threads" invariant (K1, K8); `SessionLimits`; panic containment (K6). `Completion<T>`'s consuming `poll`/`wait_timeout` shape is kept and is what the spike uses before B2 lands.

### B2 — Event queue and sink

```rust
/// Caller-chosen correlation id. Opaque to the core; the adapter uses it to
/// find the QObject that asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(pub u64);

/// Producer side: cheap to clone, `Send + Sync`. Held by workers.
#[derive(Clone)]
pub struct EventSink { /* … */ }

/// Consumer side: not `Clone`. One per application (or one per adapter).
pub struct EventQueue { /* … */ }

pub trait Waker: Send + Sync {
    /// Called when the queue goes empty -> non-empty. Edge-triggered and
    /// coalesced: a burst produces one call. MUST return promptly, MUST NOT
    /// block, and MUST NOT call back into `db-core`.
    fn wake(&self);
}

pub fn event_channel(caps: EventCaps) -> (EventSink, EventQueue);

impl EventQueue {
    /// Replacing or clearing the waker does not return while a `wake` is in
    /// progress, so a consumer can be destroyed safely.
    pub fn set_waker(&self, waker: Option<Arc<dyn Waker>>);
    pub fn next(&self) -> Option<SessionEvent>;                    // never blocks
    pub fn drain_into(&self, max: usize, out: &mut Vec<SessionEvent>) -> usize;
    pub fn wait_timeout(&self, d: Duration) -> Option<SessionEvent>; // headless tests
}

#[non_exhaustive]
pub enum SessionEvent {
    // --- exactly one of these per accepted request, ever ---
    Opened       { session: SessionId, request: RequestId, connection: ConnectionId,
                   cancel_kind: CancelKind, warnings: Vec<Warning> },
    OpenFailed   { session: SessionId, request: RequestId, error: DbError },
    Executed     { session: SessionId, request: RequestId, outcome: DbResult<ExecuteOutcome> },
    Fetched      { session: SessionId, request: RequestId, batch:   DbResult<FetchedBatch> },
    LobChunk     { session: SessionId, request: RequestId, bytes:   DbResult<Vec<u8>> },
    Completed    { session: SessionId, request: RequestId, result:  DbResult<()> },   // commit/rollback/savepoint/ping/close_result/close_lob
    SessionClosed{ session: SessionId, request: RequestId, result:  Result<(), CloseError> },

    // --- progress / unsolicited ---
    /// The worker has *started* this statement (it is no longer queued), with
    /// the deadline actually armed. This is what the honest no-Cancel UI shows.
    Executing    { session: SessionId, request: RequestId, deadline: Option<Duration> },
    ServerOutput { session: SessionId, lines: Vec<Box<str>>, dropped: u32 },
    TransactionStateChanged { session: SessionId, possibly_active: bool },

    /// Exactly once per session, at the transition. Never twice, never absent.
    Terminal     { session: SessionId, lifecycle: SessionLifecycle, cause: Option<DbError> },
}
```

**Thread safety.** `EventSink: Send + Sync + Clone`; `EventQueue: Send`, not `Sync` — one consumer. `SessionEvent: Send`, and by construction carries only plain data plus core-owned handles (K1 is preserved: a `FetchedBatch` has already had its locators parked).

**Ordering guarantees (the contract the adapter may rely on):**

1. Per session, events are delivered in the order they were **produced**. Production order is not acceptance order, and M2.5 made that explicit: at a terminal transition a submit can synthesise its own failure on the caller's thread while the worker is draining and failing the commands it already had, so those failures interleave. A consumer must route strictly by `RequestId` and must **not** infer "every earlier request of this session is answered" from a reply; session state is retired on `Terminal` only.
2. Every accepted request produces **exactly one** reply event for its `RequestId` — never zero, never two — including when the session is already `Lost` or `Closed` (the reply is then the failure, carrying the original kind and native code per K8).
3. `Terminal` is delivered **exactly once** per session, after every reply for requests accepted before the transition. Requests submitted after it still get their one failure reply, which may follow `Terminal`.
4. `Executing` precedes the matching `Executed` and follows any earlier request's reply on that session.
5. **No ordering is promised across sessions.** The hub interleaves freely.

**Back-pressure.** Reply events are bounded by outstanding requests, which is bounded by a new `SessionLimits::max_outstanding_requests` (default 1,024). Exceeding it is the **one synchronous failure** in the submit API — `Err(DbError)` with `ErrorKind::Resource`, no event — because producing an event for it would be circular. Unsolicited events (`ServerOutput`, `TransactionStateChanged`) use a bounded per-session ring with coalescing; drops are *counted and reported* on the next event (`dropped`) so the UI can say "output truncated" rather than silently lying. This mirrors K9: the command queue stays unbounded, the resources do not.

**Back-pressure, as implemented (M2.5, after review).** Four things above needed pinning down, because the first implementation was bounded only on paper:

* A request's slot is released when the **consumer drains its reply**, not when the worker produces it. Releasing on production bounds nothing — the worker answers a `ping` in microseconds, so a submitter retrying on `Resource` grew an undrained queue to 5,000 events with the counter reading zero. `DatabaseSession::outstanding_requests()` therefore means "accepted and not yet drained".
* Dropping the `EventQueue` ends the stream: everything in it is discarded, every slot it held is released, and later events are discarded on arrival. Sessions keep working (one may have a close to run) and are then bounded by what their worker has not yet reached.
* `TransactionStateChanged` is not subject to the cap at all: it coalesces in place when one is queued and is **admitted over the cap** when none is, so the class costs at most one event per session and a state change can never be lost behind a `ServerOutput` burst. `ServerOutput` is the only class that is dropped; its line count rides out on the next delivered `ServerOutput`, or is read with `EventQueue::pending_dropped_lines(session)` when there is no next one.

* `submit_close` reserves against **one more** than the limit. It was being refused at the cap like anything else, which is backwards: it is the one request that shrinks a session's footprint. Nothing deadlocked (`close()` and `Drop` go through a `Completion` and reserve nothing), but an event-driven adapter would have been unable to ask a full session to end. The exemption is exactly one — a second close while the first is undrained is refused — so the class costs one event per session.

Per session the queue therefore holds at most `2 × max_outstanding_requests + max_unsolicited_per_session + 3` events (`R + 1` replies, `R` `Executing`s, `U + 1` unsolicited, one `Terminal`), whatever a producer does.

**M2.7 raises that to `3R + U + 3` for a session with server output on.** A `ServerOutput` that carries a failed read is never dropped — past the cap it is admitted without its lines, which are counted as dropped — and a session produces at most one per `execute`, queued ahead of that execute's reply, which still holds its slot. That is at most `R` more events. A session that never turned output on produces none and stays at `2R + U + 3`. See §B4.2 as implemented, below, and ADR-0002 T6.

**M2.6 does not change that arithmetic.** `SessionRegistry::open` reserves an ordinary slot for the open, on a session that has nothing outstanding — so it is never refused and it sits *inside* `R`, not above it. `SessionRegistry::abandon` reserves **nothing at all**: neither event it can produce is a new request's reply (the `OpenFailed` answers the open, whose slot `open` already took, and `Terminal` is not a reply), which is what makes "abandon is never refused for lack of a slot" structural rather than an exemption to maintain. The `transaction_possibly_lost` field M2.6 adds to `Terminal` is a field on an event that was already counted, not a new event. See §B3's notes and ADR-0002 R4/R6.

### B3 — Non-blocking open and the session registry

```rust
pub struct SessionRegistry { /* Mutex<HashMap<SessionId, Arc<DatabaseSession>>> + EventSink */ }

impl SessionRegistry {
    pub fn new(manager: SessionManager, events: EventSink) -> Self;

    /// Never blocks and never fails synchronously. The worker thread is spawned
    /// here; `connect` runs on it and its outcome arrives as `Opened`/`OpenFailed`.
    pub fn open(&self, driver: Arc<dyn DatabaseDriver>, params: ConnectionParams,
                request: RequestId) -> SessionId;

    pub fn get(&self, id: SessionId) -> Option<Arc<DatabaseSession>>;

    /// Give up on a pending connect, or drop an open session without committing.
    /// If the connect has not completed, the caller gets `OpenFailed{Cancelled}`
    /// immediately, and the eventual connection is closed on arrival — exactly
    /// one reply, and nothing is adopted late.
    pub fn abandon(&self, id: SessionId);
}

impl SessionManager {
    pub fn open_session(&self, driver, params) -> DbResult<DatabaseSession>; // unchanged, blocking
}
```

`DatabaseSession` gains event-routed submission alongside the existing `Completion` methods. Implementation is one internal change — the worker's `Reply<T>` becomes `enum ReplyTo<T> { OneShot(Sender<DbResult<T>>), Event { sink: EventSink, request: RequestId } }` with a per-command wrapper into `SessionEvent` — so every correctness property already tested stays where it is:

```rust
impl DatabaseSession {
    pub fn bind_events(&self, sink: EventSink);                 // once, at open

    pub fn submit_execute(&self, request: RequestId, statement: Statement) -> DbResult<()>;
    pub fn submit_fetch(&self, request: RequestId, result: ResultId, max_rows: NonZeroUsize) -> DbResult<()>;
    pub fn submit_commit(&self, request: RequestId) -> DbResult<()>;
    pub fn submit_rollback(&self, request: RequestId) -> DbResult<()>;
    pub fn submit_savepoint(&self, request: RequestId, name: SavepointName) -> DbResult<()>;
    pub fn submit_rollback_to_savepoint(&self, request: RequestId, name: SavepointName) -> DbResult<()>;
    pub fn submit_ping(&self, request: RequestId) -> DbResult<()>;
    pub fn submit_read_lob_chunk(&self, request: RequestId, lob: LobHandle, max: NonZeroUsize) -> DbResult<()>;
    pub fn submit_close_result(&self, request: RequestId, result: ResultId) -> DbResult<()>;
    pub fn submit_close_lob(&self, request: RequestId, lob: LobHandle) -> DbResult<()>;
    pub fn submit_close(&self, request: RequestId, disposition: Option<CloseDisposition>) -> DbResult<()>;

    // unchanged, synchronous, out-of-band:
    pub fn cancel(&self) -> DbResult<CancelOutcome>;
    pub fn cancel_kind(&self) -> CancelKind;
    pub fn has_possibly_active_transaction(&self) -> bool;
    pub fn session_state(&self) -> SessionLifecycle;
}
```

**How the deadline and cancel state surface.** The armed deadline is a property of the `Statement` the caller built (`Statement::with_deadline`), and the resolved three-level setting is applied by the UI layer *before* submitting. The core echoes what was actually armed in `Executing { deadline }`, so the worksheet shows the real limit rather than what it hoped for. A fired deadline arrives as `ErrorKind::Timeout` (never relabelled `Cancelled`, ADR-0002 M5), typically with `SessionState::NeedsValidation` or `Lost`, and the resulting `Terminal` is the honest "the session did not survive the limit" the spec demands. `cancel_kind` is constant per session and is what the UI asks before offering anything.

**How transaction state surfaces.** `TransactionStateChanged` is emitted whenever `has_possibly_active_transaction()` flips, so a worksheet's Commit/Rollback affordances and its close-prompt do not poll. It is advisory: `close` still re-decides on the worker after the queue drains (K4) and can still answer `CloseError::DecisionRequired`.

**How `Lost`/`Closed` are delivered exactly once.** The worker already computes the transition in `SessionShared::note_error` / `mark_closed`. The change is that the transition — not each observation of it — emits `Terminal`, guarded by a one-shot flag in `SessionShared`. Sequence on loss: fail every already-queued command (each producing its own reply event), then emit `Terminal`, then stop. The session *handle* remains valid until the registry is told to release it; terminal is not free.

### B3 as implemented (M2.6, 2026-09-21)

Landed on branch `phase-1/m2-6-session-registry`. The decision record is [ADR-0002](../../decisions/0002-driver-api-and-concurrency-model.md) amendment R1–R7; the `Opened`/`OpenFailed`/abandon mapping M2.11 needs is in [`phase-1-m2-5-event-queue.md`](phase-1-m2-5-event-queue.md) §7. Every place this section had to be interpreted, with the sentence it interprets:

1. **`abandon` returns a value, and `Terminal` gains a field.** §B3 writes *"`pub fn abandon(&self, id: SessionId);`"*. It returns `Abandoned` instead — `Unknown` / `Connecting` / `Open { transaction_possibly_lost }` / `Ending`. The reason is one case: abandoning an **open** session releases the connection without committing, so the server rolls back whatever transaction it held, and `SPEC.md` §10 forbids hiding that. A caller that cannot see which case it hit cannot report it. The returned flag is a documented **lower bound** (`has_possibly_active_transaction() || outstanding_requests() > 0 || a driver call is in flight`), because anything a control thread reads is a snapshot a queued statement can invalidate. The authoritative answer is `transaction_possibly_lost` on `SessionEvent::Terminal`, computed on the worker thread at the point the session ends — the same place ADR-0002 K4 already decides for `close`, now applied to every lossy path: abandon, `retire` of an open session, the registry's teardown, `DatabaseSession::drop` and a lost connection. Four of those five said nothing at all before the M2.6 review. ADR-0002 R6 has the full table.

2. **What `abandon` means for an open session with a possibly-active transaction.** §B3: *"Give up on a pending connect, or drop an open session without committing."* Implemented as exactly `Drop`'s `CloseIntent::Abandon` **without `Drop`'s bounded wait**: request the cancel, queue the abandon, return. It never commits and never rolls back explicitly — the server's own rollback-on-disconnect resolves the transaction, which is what makes "abandoning cannot commit" structural (ADR-0002 K5). The session's `Terminal { Closed }` follows when the worker reaches the abandon, which on a busy worker is when its current driver call returns.

3. **Submits during `Opening` are impossible, not refused.** §B3 gives `open` a `SessionId` and `get` an `Option<Arc<DatabaseSession>>`; it says nothing about submitting before the connect finishes. `get` answers `None` until the session is `Open`, so there is no handle to submit through and no state in which a request could be accepted against a connection that does not exist. That is the stricter of ADR-0003 A18's two permitted behaviours (*"either refused with `INVALID_STATE` … or accepted and answered normally"*), and it needs no new error.

4. **`open` cannot fail synchronously, including when the thread will not spawn.** §B3: *"Never blocks and never fails synchronously."* Taken literally: the return type has no `Result`, so a failed `thread::Builder::spawn` is reported as that session's one `OpenFailed` plus its `Terminal`, exactly like a failed connect. A caller that already holds a `SessionId` gets one answer, not two shapes of answer.

5. **The registry also needs a `retire`.** §B2: *"The session handle remains valid until the registry is told to release it; terminal is not free."* §B3's sketch has no method for telling it. `retire(id) -> bool` is that method, and the rule is the same one the adapter follows: retire on `Terminal`, never on anything else. Retiring a session that is still `Opening` abandons it first, so nothing is ever dropped with a request unanswered.

6. **A fourth state, and a name for it.** §B3 sketches the registry as `Mutex<HashMap<SessionId, Arc<DatabaseSession>>>`. A map of handles cannot hold a session that has no handle yet (`Opening`) or one that never got one (`Ended`, the tombstone a late connect must find), so the value is a four-state entry: `Opening` / `Open` / `Ending` / `Ended`. `RegisteredSession` exposes it read-only, for diagnostics and so the state machine is observable rather than implied.

7. **`Terminal`'s lifecycle for a session that never opened.** Not in §B3; decided in `phase-1-m2-5-event-queue.md` §6 and implemented as it says — `Closed` when the abandon won, `Lost` (with the connect's own error as `cause`) when the connect failed first.

8. **Abandon reserves no slot.** The M2.5 note's HARD requirement allowed either "reserve above the limit or do not reserve". Not reserving is what was implemented, because neither event abandon produces is a new request's reply; see §B2's addition above and ADR-0002 R4.

9. **The initial transaction state is seeded silently.** §B2: *"`TransactionStateChanged` is emitted whenever `has_possibly_active_transaction()` flips."* Seeding the driver's real state right after `connect` is not a flip — the session did not exist a moment before — and announcing it put an unsolicited event **ahead of that session's own `Opened`**, which was found by the first run of the fan-in test. It is now silent; a consumer reads the initial value from `has_possibly_active_transaction()` when it sees `Opened`. ADR-0002 R5.

10. **`worker::spawn` no longer blocks, and takes a pre-built `SessionShared`.** Anticipated by `phase-1-m2-5-event-queue.md` §6 and done as described. `SessionManager::open_session` is unchanged in behaviour and its tests were not touched; it now parks on a channel the worker's report callback sends down instead of on `spawn` itself.

11. **The registry forgets a session as soon as it owns nothing for it.** Not in §B3, and decided after the review measured 200 abandoned opens accumulating with nothing to release them. An open that failed, and an abandoned open once its late connect has arrived and been refused, keep **no entry**: their `Terminal` has already been emitted and there is nothing left to hold, so a tombstone would only be a leak waiting for a consumer that forgets to retire. `state(id)` then answers `None` and `retire(id)` `false` — "the registry forgot it" and "it never existed" are deliberately indistinguishable, which is safe because `Terminal`, not a poll, is what a consumer routes on (ADR-0003 A18). Sessions that really opened are different: the registry holds a handle, so only `retire` releases them, and `SessionRegistry::len` / `counts` exist so a consumer that forgets is visible rather than slowly leaking.

12. **Dropping the registry costs one `DROP_SHUTDOWN_TIMEOUT` in total.** §B3 says nothing about teardown; the first implementation issued every abandon before waiting, which the review measured at 500 ms *per stuck session* anyway, because each session's `Drop` then started its own fresh timeout. The teardown now takes one deadline for the whole operation and takes each worker handle with it, so the `Drop` that follows has nothing to wait for. ADR-0002 R4 carries the numbers.

### B4 — Additive `db-driver-api` items Phase 1 needs (each an ADR-0002 amendment)

Three, and no more — each one is data or a capability flag, not new execution machinery:

1. **Connect-time warnings (C-6, already approved in principle).** `DatabaseConnection::take_connect_warnings() -> Vec<Warning>`, collected once by `db-core` after connect and delivered in `Opened { warnings }`. This is how TCPS `SSL_SERVER_DN_MATCH` and the trigger-rewrite notice reach the user.
2. **Server output (DBMS_OUTPUT).** `Capabilities::server_output`, `DatabaseConnection::set_server_output(enabled, buffer)` and `take_server_output() -> Vec<Box<str>>`. The vendor SQL (`DBMS_OUTPUT.ENABLE`/`GET_LINES`) stays inside the Oracle driver; the core polls only when a worksheet enabled the pane, because it costs a round trip per statement.
3. **Metadata catalog — the light shape.** `DatabaseDriver::metadata_catalog() -> &dyn MetadataCatalog`, where the *driver* returns a prepared `Statement` plus a declared column contract for each vendor-neutral metadata query (`Schemas`, `ObjectsOfKind{schema, kind, name_filter, limit}`, `ColumnsOf{schema, table}`). `db-core` executes it through the ordinary path and gets an ordinary `RowBatch`. Vendor dictionary SQL stays in the vendor (invariant 2) and no new result plumbing, paging or caching is invented. A `MetadataProvider` trait with its own result types was considered and rejected for Phase 1: it duplicates the fetch path for no MVP benefit, and the SQLite metadata cache (`ARCHITECTURE.md` §13 item 8) is P2 anyway.

   *As implemented (M2.8):* a single `MetadataCatalog::prepare(request: MetadataRequest)` covers all three request shapes (one method, not three) — `MetadataRequest` is `#[non_exhaustive]` so this stays extensible. `ColumnsOf`'s declared contract deliberately omits a `default`/`DATA_DEFAULT` column: on Oracle that column is a `LONG`, and spike S12 already found that fetching a `LONG` through the pinned `oracledb` crate can abort the whole process (U-4); leaving it out of the Phase 1 contract was the brief's own explicit fallback for this case. `ColumnsOf` orders by `COLUMN_ID` (table definition order), not by name — "deterministic `ORDER BY name`" is honored for `Schemas`/`ObjectsOfKind`, but column order is the one place name ordering would be actively wrong; it also excludes `INVISIBLE` columns (`COLUMN_ID IS NULL`) so the declared `position` contract's non-nullability is never falsified in the *data*. A live-DB diagnostic found that Oracle's own describe protocol reports the server's `nulls_allowed` flag verbatim (the pinned `oracledb` crate does no client-side computation of its own — ADR-0002, "Notes for driver implementers"), narrowing to non-null only for a bare reference to a `NOT NULL` column: every computed expression describes as nullable regardless of provable non-nullity (even `SELECT 1 FROM DUAL`), and a `WHERE` predicate never narrows a bare column's own declared nullability. `type_name`/`nullable` are computed (`CASE`/`DECODE`); `position` is a bare `COLUMN_ID` reference, but that column is itself declared nullable in `ALL_TAB_COLUMNS`'s own definition. None of the three can be verified non-nullable via describe through this driver; the integration test checks their declared non-nullability against the fetched data directly instead (a live `INVISIBLE`-column fixture, not just a SQL-text unit test, proves the exclusion). `type_name` composes Oracle's precision/scale/length qualifiers onto bare `DATA_TYPE` for `NUMBER`/`FLOAT`/`VARCHAR2`/`CHAR`/`NCHAR`/`NVARCHAR2`/`RAW`, the families where `DATA_TYPE` alone loses information. Permission reclassification is a `MetadataErrorClassifier` function pointer (`fn(&DbError) -> Option<DbError>`) carried on the returned `PreparedMetadataQuery` itself, not a separate trait method a caller could forget to call; on Oracle it corrects `ORA-00942`/`ORA-01039` (both "not visible" from a statement naming a dictionary object the driver chose) — `ORA-01031` is already unconditionally `Permission` and needs no correction, and `ORA-00990` is a `GRANT`-statement syntax error unrelated to this catalog's `SELECT`s, so it correctly stays `Syntax`. `type_name` also distinguishes `CHAR(n CHAR)`/`CHAR(n BYTE)` the same way `VARCHAR2` does, via `CHAR_USED` — `NCHAR`/`NVARCHAR2` never need the unit, since a national-charset column is always char-length semantics. Known open items, deliberately left untested/unimplemented: a materialized view's container table may list under `Tables` (the test account lacks `CREATE MATERIALIZED VIEW` to confirm either way); `type_name` shows a bare `REF`/user-type/`XMLTYPE` without its referenced type or owning schema, and a bare `UROWID` without its declared length — none are among the nine object groups' everyday column types.

Also additive, and outside the contract: a `SqlDialect` **descriptor** (keywords, quote and `q'[…]'` rules, block starters, terminator handling) that the driver supplies as data and `reldex-sql-text` consumes as the parameter to its lexer/splitter. Landed in M2.4 in `reldex-sql-text` itself, not `db-core` as first sketched here — see ADR-0002 amendment "The `SqlDialect` descriptor" for why (dependency direction, plus M2.4/M2.5 running as parallel worktrees). See §C M2.4.

### B4.2 as implemented (M2.7, 2026-09-24)

Built on branch `phase-1/m2-7-server-output`. The decision record is [ADR-0002](../../decisions/0002-driver-api-and-concurrency-model.md) amendment T1–T9. The mapping M2.11 needs is in [`phase-1-m2-5-event-queue.md`](phase-1-m2-5-event-queue.md) §7. Each place this section had to be interpreted, with the sentence it interprets:

1. **A typed setting, not `(enabled, buffer)`.** §B4.2 writes *"`set_server_output(enabled, buffer)`"*. Implemented as `set_server_output(ServerOutputSetting) -> DbResult<ServerOutputSetting>`, with `ServerOutputSetting::{Disabled, Enabled(ServerOutputBuffer::{Unlimited, Bytes(NonZeroU32)})}`. A pair can say "off, with a buffer of 0 bytes"; the enum cannot. The call returns the setting **in force**, because Oracle clamps a requested size into 2,000..=1,000,000 bytes without saying so, and a pane that shows the size it asked for would be wrong.
2. **`take_server_output` is bounded.** §B4.2: *"`take_server_output() -> Vec<Box<str>>`"*. An unlimited buffer read into one `Vec` is unbounded by construction. Implemented as `take_server_output(max_lines, max_bytes) -> ServerOutputChunk { lines, drained }`: one round trip per call; a line is never split; every call returns at least one line when any is buffered.
3. **Not `GET_LINES`.** §B4.2: *"(`DBMS_OUTPUT.ENABLE`/`GET_LINES`)"*. `GET_LINES` needs a PL/SQL collection OUT bind, which `oracledb` 26.0.0-beta.3 does not have. The driver instead runs one anonymous block per round trip. The block loops `GET_LINE` on the server and packs the lines, length-prefixed, into one 32 KiB value; the line that does not fit comes back in a second bind. Measured: **10,000 lines in 5 round trips** at the core's default chunk sizes, where `GET_LINE` per line takes 10,001. ADR-0002 T2 records what was tried.
4. **What "a round trip per statement" means.** §B4.2: *"because it costs a round trip per statement"*. While output is on, a statement that printed nothing costs **one** extra round trip (the read that finds the buffer empty). A statement that printed costs one per 32 KiB of packed output, plus one when the output ends exactly on a chunk boundary. While output is off it costs **nothing**. The worker returns before any driver call, and the driver's `execute` issues no `DBMS_OUTPUT` call at all. Both are proven: the mock counts zero calls, and the real database counts one round trip for a printing statement on a session that never turned output on, the same as for `BEGIN NULL; END;`.
5. **Where the lines go.** Event path: `ServerOutput` events between the statement's `Executing` and its `Executed` (and after its `TransactionStateChanged`), so a consumer attributes them by position. Completion path: a bounded per-session log, `DatabaseSession::take_server_output() -> ServerOutputLog`, capped at 10,000 lines / 1 MiB. It keeps the oldest lines as a strict prefix, so once one line is refused every later one is refused and counted until the next take. It is per session, not per statement: completions that are pipelined and taken once get their output mixed. The log is not put on `ExecuteOutcome`, because a failed statement has no outcome, and its output is the output that matters most.
6. **Failures travel with the output, never instead of the reply.** `SessionEvent::ServerOutput` gains `failure: Option<DbError>`, and `ServerOutputLog` gains `failure`/`failures`. A read that fails ends that drain and is reported on the output stream. The statement's own reply follows unchanged. A read that loses the connection ends the session through M2.6's loss path (`Terminal { Lost, transaction_possibly_lost }`). The queue bound this costs is in §B2 above.
7. **Turning it on is a request.** `submit_set_server_output` plus `set_server_output` (a `Completion`), answered by `ServerOutputConfigured { result }`. It costs one round trip, and on a driver without the capability it is refused `Unsupported` with no call. A failed enable leaves output off. The setting lives on the session's worker and dies with it. A new session starts off, and nothing ever re-enables output on another session.
8. **Knobs.** `SessionLimits::{with_,}server_output_chunk_lines` (default 4,096) and `{with_,}server_output_chunk_bytes` (default 32 KiB) bound what the worker holds at once.
9. **What is not read.** Nothing is read after a fetch, commit, rollback, savepoint, ping or close. Nothing is read on a session that needs validation or is lost, or once an abandon has been requested. Output that a function writes while rows are fetched is kept by the server and arrives with the next statement's read, ahead of that statement's own lines. This is measured on the real database. The same delay applies to the output of a statement that failed and left the session needing validation (for example a timeout). No read is made until the next command's ping, so those lines arrive with the next execute's read. This is the second documented exception (ADR-0002 T4), chosen over pinging at once, which would hold back the error reply on a possibly dead connection. On Oracle today a call timeout loses the session instead. The drain always reads to the end, because the next `PUT` after a partial read would purge the leftovers with nothing counted. Bounding that drain is follow-up M2.13.

### B5 — Risks in this design, and what is done about them

| Risk | Consequence | Mitigation |
| --- | --- | --- |
| Worker blocked forever on a black-holed link with "no limit" | Every later command on that session queues forever; `close` cannot run | Default limit is 600 s and the "no limit" UI states the consequence; `abandon()` detaches within `DROP_SHUTDOWN_TIMEOUT` and the UI closes the worksheet; a process-level counter of detached workers is exposed in diagnostics so the leak is visible, not silent |
| Unbounded connect attempts (U-15: a connect cannot be bounded upstream; C-5's helper thread is the fix) | Thread and socket accumulation on a bad network | C-5 must land (M2.1) *before* the connection manager ships; cap concurrent pending connects per process; `abandon` is answered immediately and the late connection is closed on arrival |
| Waker use-after-free when the adapter is destroyed | Crash on exit or on window close | `set_waker` blocks until any in-flight wake returns; 10,000-iteration ASan teardown test is spike kill criterion K5 |
| Re-entrancy from the waker into the core | Deadlock inside a session lock | Contract forbids it; debug thread-local guard returns `RELDEX_STATUS_REENTRANT`; the C++ trampoline only posts |
| Event storm from `ServerOutput` on a chatty PL/SQL run | UI starves | Bounded per-session ring, coalescing, drop-count reported; drain budget in the adapter |
| `Completion` and event paths diverge over time | Two semantics for the same operation | Both are the same `Command` with a different `ReplyTo`; the existing `db-core` test suite runs against both paths (parameterized) |
| A request submitted after `Terminal` | Adapter confusion about lifetime | Uniform rule: it still gets exactly one failure reply; `Terminal` is a state announcement, not a queue close |

---

## C. Phase 1 plan — Desktop MVP

## C.0 — Toolchain to install (needs owner approval before M1 starts)

Already present on the dev machine: Rust stable MSVC, **Visual Studio 2022 Community with the MSVC 14.44 toolset**. Missing: CMake, Ninja, Qt.

| Item | Version | What for | Size class |
| --- | --- | --- | --- |
| **Qt 6.8 LTS**, kit `msvc2022_64` | latest patch still published to open-source users | The UI | ~1.5–2.5 GB downloaded, ~4–5 GB on disk |
| — module `qtbase` | | Core, Gui, Network(off if unused), `windeployqt` | included |
| — module `qtdeclarative` | | Qt Quick, QML, Qt Quick Controls 2, `qmlcachegen` | included |
| — module `qtshadertools` | | required by Quick | included |
| — module `qtsvg` | | icons | small |
| — module `qttools` | | `lupdate`/`lrelease`, Linguist (Thai/English) | small |
| — **Qt Creator** | optional | convenience only; agents build from CLI | ~1 GB — recommend **skip** |
| **CMake** ≥ 3.24 | | build driver (Corrosion needs ≥3.22; Qt6 needs ≥3.16, ≥3.21 for qml modules) | ~110 MB |
| **Ninja** | 1.12+ | generator | ~1 MB |
| `cbindgen` | latest | header generation (`cargo install`) | negligible |

**Explicitly NOT installed, and not to be used:** Qt Charts, Qt Graphs / Data Visualization, Qt Virtual Keyboard, Qt Quick 3D (and Quick 3D Physics), Qt WebEngine, Qt Wayland *compositor*, qt5compat. The first group is published **GPLv3-only** in the open-source offering and would force Reldex itself to be GPL; WebEngine adds Chromium's bulk and its own licence tangle; qt5compat exists to drag Qt5 APIs forward and we have none.

**Licence position (LGPLv3, for a possibly closed-source desktop app) — what we must do:**

- **Link Qt dynamically.** Ship Qt as DLLs via `windeployqt`; never `-static`. Static linking under LGPLv3 obliges us to distribute relinkable object files of *our* application, which is incompatible with a closed-source Pro build.
- **Ship the licence texts and attribution** (LGPLv3 + GPLv3 reference + Qt's third-party notices) alongside our own generated notices file (`cargo about` for the Rust graph — already a P0 task).
- **State which Qt version we ship and where its source is**, plus any patches (we plan none). A written offer or a link to the exact upstream tarball satisfies this.
- **Do not prevent the user replacing the Qt DLLs.** This is why the installer must not verify or lock the bundle contents.
- Our own code, the QML, and the Rust core are unaffected: moc/rcc/uic output is covered by The Qt Company's GPL exception, and LGPL applies to the Qt libraries we distribute, not to our sources.
- **Flagged now, decided later:** iOS distribution (Phase 4/5) requires static linking in practice and App Store DRM sits badly with LGPLv3 §4's installation-information requirement. That is the usual point where projects buy a commercial Qt licence. It is not a Phase 1 blocker but it is a Phase 1 *decision input* — see owner decision 2.
- **Verify the per-module licence table for the exact Qt version before first distribution.** Qt's module licensing has changed between releases; treating the list above as final without checking would be exactly the kind of unverified claim the repository's own standards reject.

## C.1 — Scope: in and out

**IN (Phase 1 = Desktop MVP, Windows x64 first):** app shell and workspace layout; connection profiles with three-level settings and Windows Credential Manager; connect with a bounded timeout and honest TCPS wording; multiple independent worksheet sessions; SQL/PL-SQL editor with highlighting, line numbers, search/replace, bracket matching, Thai/IME/high-DPI, themes and fonts; statement splitting and run current/selection/script; bind-variable dialog; auto-commit OFF with explicit Commit/Rollback, close-with-pending-transaction dialog, savepoints; the honest no-Cancel UX with the three-level time limit; virtualized result grid with NULL visualization, row numbers, column resize/reorder, copy (cell/row/range, with headers), search-in-results, type-aware formatting, CLOB/BLOB viewers; DBMS_OUTPUT pane; error pane with PL/SQL position highlighting only; query history; minimal lazy object browser; workspace persistence; logging/diagnostics without secrets; Windows packaging; CI with headless QML tests on three OS; the fetch-batch benchmark and its shipped default; third-party notices; Thai+English UI baseline and an accessibility baseline.

**OUT of Phase 1 — the defer list, with reasons:**

| Deferred | To | Reason |
| --- | --- | --- |
| Streaming CSV/TSV/JSON/SQL export | P2 | `TASKS.md` already places it in P2; it needs the Result Store's streaming path settled (ADR-0004) and is not needed to prove the MVP workflow |
| Explain Plan tree/text UI | P2 | `TASKS.md` P2. `SPEC.md` §24.15 is a **V1 DoD** item, not an MVP item; the driver side already passes (spike S12) |
| PL/SQL object editor, compile, compile-error mapping, Table Inspector, DDL viewer | P2 | `TASKS.md` P2; each is its own vertical |
| In-grid sorting and filtering | P2 | Client-side sort over a partially-fetched million-row result is a lie about completeness; the honest forms (re-execute with `ORDER BY`, or a full materialization) need ADR-0004 first |
| Code folding, and files > 5 MB in the editor | P2 | `QQuickTextEdit` lays out the whole document; a fold model plus a virtualized document is a second editor project. MVP measures the limit and refuses beyond it honestly |
| Autocomplete, signatures, diagnostics, go-to-definition | P2 | `SPEC.md` §14 requires only that the architecture *allows* them — the `reldex-sql-text` crate is that allowance |
| SQLite metadata cache, 100k-object browsing | P2 | `ARCHITECTURE.md` §13 item 8 is unresolved; MVP browses server-side with a filter and a row cap |
| macOS/Linux packaging | P2 | `TASKS.md` P2. Phase 1 keeps them **building and testing** in CI so they never rot |
| Arrow evaluation, result spill/eviction | P3 | Benchmark-gated by `SPEC.md` §12 |
| Mobile anything | P4/P5 | `SPEC.md` §25 needs devices the owner has not provided for Phase 1 (an Android phone was provided 2026-09-19 for the Phase 0 validation tail; iOS still needs a Mac + Apple Developer account + device) |
| Pro features, entitlements service, gateway, AI | P6 | `SPEC.md` §22/§23 |
| On-demand Cancel | blocked | ADR-0001 owner decision; upstream issue #24 |

## C.2 — Milestones

Six milestones. M1 is the de-risking gate and nothing downstream starts until it passes. ★ marks tasks where **independent review is mandatory** (per `AGENTS.md` and the `reldex-development` review checklist): architecture, FFI/`unsafe`, concurrency, security, and correctness-critical transaction paths. Status legend matches `TASKS.md`: `[x]` done, `[~]` in progress, `[ ]` todo, `[!]` blocked.

---

### M1 — De-risk: toolchain, ADR-0003, and a real virtualized table

**Goal.** Prove the whole Qt↔Rust path end to end at scale before any product feature is built, and accept or kill ADR-0003 on evidence.

**Exit gate (demonstrable).** A QML window shows a `TableView` scrolling 1,000,000 mock rows fed through the real `reldex-ffi` and a real `QAbstractTableModel`; the S15 measurement report records frame time (p50/p99), first-row latency, RSS, and per-batch boundary cost against the K1–K7 thresholds; ADR-0003's status is updated on that evidence; CI builds the CMake+Corrosion+Qt project and runs offscreen tests on windows/ubuntu/macos.

**M1 exit-gate result (2026-09-20).** The demonstrable part is done: S15's report has a measured number and a verdict for every K1–K7 threshold, and CI is green on all three OS. The status update is not "moved to Accepted, or re-opened with the owner" as originally framed — no criterion's failure is located in the boundary, so re-opening the design is not warranted, but the lead does not accept ADR-0003 unilaterally either. ADR-0003 stays **Proposed**, now carrying the S15 evidence and three rulings requested from the owner (M1.9).

| ID | Status | Title | Owner | Inputs | Outputs | Deps | Acceptance | Size |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| M1.1 | `[x]` done 2026-09-20 — owner approved; Qt 6.8.3, CMake, Ninja, cbindgen installed (`phase-1-toolchain.md`) | Owner approval + toolchain install (Qt, CMake, Ninja, cbindgen) | owner + `sonnet` | §C.0 list | Installed toolchain; `docs/exec-plans/active/phase-1-toolchain.md` recording exact versions and paths | — | `cmake --version`, `ninja --version`, `qmake -query` all report; a stock Qt Quick hello-world builds and runs | S |
| M1.2 ★ | `[x]` done — ADR-0003 drafted as **Proposed** by this change | Draft ADR-0003 (this section A) | `opus` | §A | `docs/decisions/0003-qt-rust-integration.md` (Proposed) | — | Reviewed by a second `opus`; alternatives and kill criteria present | M |
| M1.3 ★ | `[x]` done 2026-09-20 — `reldex-ffi`, ABI 3, two independent reviews | `crates/ffi` skeleton: hub, session open/execute/fetch, batch views, errors, waker | `opus` | ADR-0003 D2–D7; `db-core` public API | `crates/ffi` + `crates/ffi/include/reldex.h` | M1.2 | `cbindgen --verify` clean; clippy `-D warnings`; every `unsafe` has SAFETY; Miri green on the crate's tests | L |
| M1.4 | `[x]` done 2026-09-20 — C11/C++17 harness, ASan/UBSan on Linux CI, `ui.yml` on three OS | C smoke harness (`ui/tests/ffi_smoke`), no Qt, mock driver, ASan on Linux | `sonnet` | M1.3 header | A C program exercising open→execute→fetch→close | M1.3 | Runs green on all 3 CI OS; ASan/UBSan clean on Linux | M |
| M1.5 | `[x]` done 2026-09-20 — `ui/` skeleton, Corrosion v0.6.1 pinned | CMake + Corrosion + Qt project skeleton; QML module for the adapter | `sonnet` | M1.1, M1.3 | `ui/CMakeLists.txt`, `ui/adapter`, `ui/app` | M1.1, M1.3 | One-command build on Windows; `QT_QPA_PLATFORM=offscreen` test target runs | M |
| M1.6 ★ | `[x]` done 2026-09-20 — Bridge / SessionController / ResultTableModel, independently reviewed | `ResultTableModel : QAbstractTableModel` over borrowed batch views; `Bridge` waker→`invokeMethod` drain | `opus` | ADR-0003 D4/D5 | Adapter classes + `QAbstractItemModelTester` suite | M1.5 | Model tester green; K5 teardown test (10k iterations, ASan) green; no FFI call inside `data()` beyond pointer reads | L |
| M1.7 | `[x]` done 2026-09-20 — `GeneratedQuerySpec`, 1M rows in ~0.55 s | Mock driver: 1M-row generator of the S14 shape with controllable latency and a 10 s blocking statement | `sonnet` | `crates/drivers/mock` | New `Scenario` cases | — (parallel with M1.3–M1.6) | Deterministic; DB-free; used by M1.4 and M1.8 | S |
| M1.8 ★ | `[x]` done 2026-09-20 | Spike S15 measurement run + report | `opus` | M1.6, M1.7 | `docs/exec-plans/active/phase-1-s15-ffi-spike.md` with method, environment, numbers | M1.6, M1.7 | Every K1–K7 threshold has a measured number and a verdict; method recorded per `AGENTS.md` "Performance" — met: K1/K3/K4/K5/K6/K7 pass (K1/K5 with a named gap); **K2 fails as written** (cold first paint 903.55 ms vs 150 ms; warm path 15.85 ms passes by ~9×), cause outside the boundary | M |
| M1.9 | `[~]` in progress — S15 recorded; awaiting owner ruling | Accept or re-open ADR-0003; update `ARCHITECTURE.md` §13 items 2/3/10, `TASKS.md`, `Task.html` | `sonnet` | M1.8 | Updated docs | M1.8 | Status changed with evidence links; dashboard not stale — done: ADR-0003, `ARCHITECTURE.md`, `TASKS.md`, `Task.html` updated with the S15 result and the three open rulings; not done: the owner has not yet ruled, so ADR-0003 stays Proposed rather than Accepted | S |

**Parallelism.** M1.7 runs alongside M1.3–M1.6. M1.4 and M1.6 can run in parallel once M1.3's header is stable. M1.2 must land before M1.3 begins coding.

---

### M2 — Core readiness: events, async open, settings, SQL text, driver leftovers

**Goal.** Make `db-core` and the Oracle driver Phase-1-complete so every UI milestone consumes a finished contract.

**Exit gate.** `db-core`'s existing test suite passes on *both* the `Completion` and event paths; a headless test opens 8 sessions asynchronously, runs statements concurrently, loses one, and observes exactly one `Terminal` each and exactly one reply per request; `connect_timeout` is honoured against a black-holed address; `CREATE TRIGGER … :NEW` succeeds with a reported rewrite; the three-level settings resolver has a truth-table test; the splitter passes a corpus of Oracle scripts.

| ID | Status | Title | Owner | Inputs | Outputs | Deps | Acceptance | Size |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| M2.1 ★ | `[x]` done 2026-09-20 — carried over from Phase 0 (independently reviewed); M2 consumes the result | Driver: honour `connect_timeout` on a helper thread (C-5/U-15), default 15 s, "no limit" supported | `opus` | `phase-0-spike-results.md` §7 C-5 | `crates/drivers/oracle-thin` change + live test | — | A connect into a black hole returns at the limit; no session is adopted after it; abandoned attempt closed on arrival | M |
| M2.2 | `[x]` done 2026-09-20 — carried over from Phase 0 (independently reviewed); M2 consumes the result | Driver: `CREATE TRIGGER` `:NEW`/`:OLD` auto-rewrite (U-18), on by default, reported as a warning with the submitted text, per-connection off switch | `sonnet` | SPEC §8 owner decision | Driver change + live test + canary update | — | Rewrite works; warning carries the exact statement sent; off switch restores the explanatory refusal | M |
| M2.3 ★ | `[x]` done 2026-09-20 — carried over from Phase 0 (independently reviewed); M2 consumes the result | Contract: `take_connect_warnings` (C-6) + ADR-0002 amendment | `opus` | `phase-0-spike-results.md` §7 C-6 | `db-driver-api` + `db-core` + mock + oracle-thin | M2.1 | Warnings collected once after connect; TCPS `SSL_SERVER_DN_MATCH` warning reaches the caller | S |
| M2.4 | `[x]` done 2026-09-20; round-1 adversarial-review safety fixes 2026-09-21 (panic + mis-split MUST-FIXes, ADR-0002 amendment J4); round-2 adversarial review 2026-09-21 rejected that fix for a deeper depth-tracking defect + 3 more MUST-FIXes, now fixed with a grammar-based differential test as the new acceptance gate (ADR-0002 amendment J5) | `crates/sql-text`: lexer + statement splitter driven by a `SqlDialect` descriptor the driver supplies | `sonnet` | SPEC §15; ADR-0002 D8 | New crate (`reldex-sql-text`) + Oracle dialect descriptor in `oracle-thin` | — | A corpus of Oracle scripts (PL/SQL blocks, nested BEGIN, `/`, `q'[…]'`, comments, strings, Thai text) splits correctly; `tokenize_block(text, in_state) -> (tokens, out_state)` shaped for `QSyntaxHighlighter`; a lone `/` line is authoritative regardless of nesting depth (S1); `statement_at` never panics at any byte offset; **round-2 additions:** a lone `/` line with no statement text pending produces no span at all, never an empty or re-run one (ADR-0002 J5); a call-spec (`LANGUAGE`/`EXTERNAL`) is recognized only on a full phrase match, never a single word, because a missed call-spec merely over-swallows text while a false one carves a real body out from under the statement that owns it — the fail-safe direction always favors the larger span | L |
| M2.5 ★ | `[x]` done 2026-09-21 — reviewed twice; see `phase-1-m2-5-event-queue.md` | `EventQueue`/`EventSink`/`SessionEvent`/`Waker` + `ReplyTo` refactor of the worker | `opus` | §B2 | `db-core` change | — | Ordering rules 1–5 each have a test; exactly-once `Terminal` test; drop-count reporting test; existing suite runs on both paths | L |
| M2.6 ★ | `[x]` done 2026-09-21 — independently reviewed twice, 3 must-fix fixed (ADR-0002 amendment R) | `SessionRegistry` + non-blocking `open`, `abandon` semantics | `opus` | §B3 | `db-core` change | M2.5 | `open` returns without blocking; abandon-before-open yields exactly one `OpenFailed{Cancelled}` and closes the late connection; ADR-0002 amendment recorded — met: ADR-0002 R1–R5, §B3 "as implemented" above, `registry_open.rs` (9) + `registry_abandon.rs` (16), 30 solo and 2 × 15 concurrent runs clean, `reldex.h` byte-identical | M |
| M2.7 ★ | `[x]` done 2026-09-24 — reviewed (no must-fix); ADR-0002 amendment T; follow-ups M2.12/M2.13 | Server output capability (DBMS_OUTPUT) in contract + driver + core polling when enabled | `opus` | §B4.2 | Contract + driver + core | M2.5 | Thai text survives byte-exact (S5 precedent); no round trip when the pane is off | M |
| M2.8 | `[x]` done 2026-09-24 — reviewed twice (2 must-fix fixed); see §B4 item 3 "As implemented" | Metadata catalog descriptor (`MetadataCatalog`) + Oracle dictionary SQL for the 9 object groups | `sonnet` | SPEC §16; spike S12 | Contract + driver | M2.3 | Each group returns a declared column contract; server-side name filter and row cap; permission failures classify as `ErrorKind::Permission`, not driver failure | M |
| M2.9 ★ | `[x]` done 2026-09-25 — reviewed (accept with follow-ups, all landed); ADR-0006; see "M2.9 notes" | Settings model: three-level resolution with provenance; profile model; SQLite store | `opus` | SPEC §17/§20; owner decisions | `db-core` workspace/settings module + schema | — | `effective = worksheet ?? profile ?? application ?? built-in`, with the source reported; truth-table test; schema migration path; **no secret ever written to SQLite** | L |
| M2.10 ★ | `[ ]` todo | Credential store: `CredentialStore` trait + Windows Credential Manager implementation | `opus` | SPEC §17; ARCHITECTURE §13 item 9 | `crates/secrets` + core wiring | M2.9 | Password round-trips through Credential Manager keyed by profile UUID; absence of a store means **prompt each time**, never a plaintext fallback; nothing secret in logs or `Debug`; licence of every new dep recorded | M |
| M2.11 | `[ ]` todo | FFI surface for M2.5–M2.10 + regenerate and verify header | `sonnet` | M1.3 | `crates/ffi` extension | M2.5–M2.10 | `cbindgen --verify` clean; C smoke harness extended. **M2.9 hand-off:** the production Oracle `DriverBinding` (lift `crates/workspace/tests/support/oracle_binding.rs`) maps extension keys only and wires the driver's `reldex_driver_oracle_thin::sid_endpoint` for SID endpoints; settings cross the ABI by a numeric id, never the storage key; `ProfileId` as 16 bytes (ADR-0006 P4) | M |
| M2.12 | `[x]` done 2026-09-25 — reviewed (1 must-fix landed); live 6/6 + 9/9; fuzz 120k | Server output framing over `RAW`/`LENGTHB` with per-line UTF-8 decoding in Rust, so one invalid line loses only itself | `sonnet` | M2.7 review; ADR-0002 T2 "Known limit" | `oracle-thin` `server_output.rs` change + live test | M2.7 | Today a line that is not valid UTF-8 loses every line of its read (up to 4,096 good lines) through the crate's strict `from_utf8` (`db_value.rs:164`). After this, only that line is lost and it is reported. The `LENGTH4` == Rust char count assumption is gone. Single-byte database character sets no longer exceed `max_bytes` (≈3× today, review N7). | S |
| M2.13 | `[ ]` todo | Per-statement server-output drain bound: a total cap reported through `dropped`/`failure`, or a cancel flag checked between reads like abandon | `opus` | M2.7 review; ADR-0002 T6 | `db-core` worker change + tests | M2.7, M4.7 | Today the drain has no length bound, cannot be cancelled on Oracle, and holds `Executed` back until it finishes (10M lines ≈ 2.5k round trips). After this, a statement's reply is never held longer than the bound, and what was not read is reported, never silent. This is safe because the next `PUT` purges leftovers (measured, T6). | S |

**Parallelism.** M2.1/M2.2 (driver), M2.4 (sql-text), M2.9/M2.10 (settings/secrets) and M2.5/M2.6 (events) are four independent tracks. M2.11 gates on all of them.
**Mandatory review:** M2.1, M2.3, M2.5, M2.6, M2.7, M2.9, M2.10 — concurrency, contract change, and security.

**M2.5 notes.** The implementation notes for the event queue live in
[`phase-1-m2-5-event-queue.md`](phase-1-m2-5-event-queue.md): the before/after performance numbers
(1M rows through `crates/ffi`; per-event cost at 1 and 8 producer sessions; allocations per event),
the exact `SessionEvent` → `ReldexEvent` mapping M2.11 has to write — the C ABI is **unchanged** by
M2.5, and `reldex.h` is byte-identical — and every place the implementation had to interpret §B2.
The decision record is [ADR-0002](../../decisions/0002-driver-api-and-concurrency-model.md)
amendment E1–E6.

**M2.6 notes.** The registry's interpretations of §B3 are listed in "B3 as implemented" above, its
decision record is ADR-0002 amendment R1–R5, and what M2.11 has to do with `Opened`/`OpenFailed`/
`abandon` is [`phase-1-m2-5-event-queue.md`](phase-1-m2-5-event-queue.md) §7. The C ABI is again
**unchanged** and `reldex.h` byte-identical; `crates/ffi` still uses the blocking
`SessionManager::open_session` and its interim pump, which M2.11 replaces.

**M2.9 notes (as implemented).** The decision record is
[ADR-0006](../../decisions/0006-local-persistence-settings-profiles-sqlite.md). Where it departs from
the row: (1) the output is a crate of its own, `crates/workspace` (`reldex-workspace`), not a
`db-core` module, so `db-core` stays free of SQLite and keeps its "`db-driver-api` only" dependency
rule (ADR-0006 P1); (2) the vendor-specific residue of a profile — a SID endpoint and the Oracle
driver's extension keys — goes through a `DriverBinding` the composition root supplies; the SID
descriptor itself is built by the driver (`reldex_driver_oracle_thin::sid_endpoint`, next to its
Easy Connect builder, live-tested), the reference Oracle binding is test code
(`crates/workspace/tests/support/oracle_binding.rs`) that calls it, and **M2.11 lifts it into
`crates/ffi`**, together with the workspace service thread that owns the `Store` and a 16-byte
`ProfileId` in the ABI; (3) no `CredentialStore` trait is defined here — M2.10 owns it; the seam is
`CredentialKey` (the profile UUID); (4) no display setting is registered yet, since none is decided
(they come with M5). The independent review (ACCEPT-WITH-FOLLOW-UPS) added: credential-looking text
in an endpoint is refused with a value-free error and a connect string's `Debug` prints only its
length (P7); a `treat_as_production` flag M3.4 reads instead of the environment enum (P3); the
server-output buffer's lower bound is 2,000 bytes; store hardening — `0700`/`0600` on Unix, typed
`ReadOnly`/`IncompleteHeader`/`SchemaMismatch`, case-insensitive ids, the four-wait open documented
(P5); and a dependency-rule test. The C ABI is unchanged and `reldex.h` byte-identical.

---

### M3 — Connect: shell, connection manager, first real session

**Goal.** A user can create a profile, store its password securely, connect to Oracle 19c, and see the session live — with honest wording for timeouts and TCPS.

**Exit gate.** From a clean machine: create a profile (Easy Connect, service name, and a full descriptor), connect over TCP and over TCPS with a user-supplied CA PEM, see a Production profile's persistent indicator, and have the password survive a restart without ever appearing in the SQLite file or the log.

| ID | Status | Title | Owner | Inputs | Outputs | Deps | Acceptance | Size |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| M3.1 | `[x]` done 2026-09-25 — reviewed (tab-bar palette must-fix landed); 9/9 offscreen tests; DPI 1×/1.5×/2× | App shell: window, docking-free fixed layout (sidebar / worksheet tabs / output panes), light+dark theme, high-DPI | `sonnet` | SPEC §14 | `ui/app` | M1 gate | Renders at 100/150/200% DPI; theme switch has no restart | M |
| M3.2 | `[ ]` todo | Connection manager UI: list, create/edit/delete, environment, test-connect | `sonnet` | M2.9 | QML + `ProfileModel` | M2.11 | All `SPEC.md` §17 fields present; environments Dev/Test/UAT/Staging/Production/Custom. **M2.9 hand-off:** endpoint text that looks like a credential is refused with `ProfileError::CredentialInEndpoint { field, pattern }` — show the field and the pattern, never echo the text; offer the "treat as production" choice for a Custom environment only (ADR-0006 P3/P7) | L |
| M3.3 ★ | `[ ]` todo | Connect flow over the async path, with a bounded timeout and a cancellable "Connecting…" state | `opus` | §B3 | `SessionController` | M2.6, M2.11 | Cancelling a pending connect returns immediately and adopts nothing late; failures show kind + ORA code + cause chain | M |
| M3.4 | `[ ]` todo | Production indicator: persistent, not colour-only (icon + text + tab badge) | `sonnet` | SPEC §17 | QML | M3.2 | Visible in every place a statement can be run; passes a greyscale check. **M2.9 hand-off:** read `Profile::treat_as_production()`, not the `Environment` enum — always on for Production, the user's choice for Custom (ADR-0006 P3) | S |
| M3.5 ★ | `[ ]` todo | TCPS UI described exactly as `SPEC.md` §8: user-supplied CA PEM, verification always on; surfaces the descriptor-guard warnings from C-6 | `opus` | SPEC §8; spike S8; PR #5 | QML + wording | M2.3 | No control implies mTLS, wallet files, OS trust store or revocation; `SSL_SERVER_CERT_DN` refusal explains the opt-out rather than failing blankly | M |
| M3.6 | `[ ]` todo | Settings UI: application defaults, per-profile overrides, provenance shown ("inherited from profile") | `sonnet` | M2.9 | QML | M3.2 | Every default in the product is reachable here (owner rule: every default is user-configurable) | M |
| M3.7 ★ | `[ ]` todo | Logging/diagnostics: `tracing` + rotating file sink, redaction layer, Qt messages forwarded through the FFI | `opus` | AGENTS "do not log secrets" | `crates/ffi` + core | M2.11 | A test asserts no password, PEM, or token reaches the log at any level; SQL text logged only at `debug` behind an explicit opt-in; connection strings redacted. **M2.9 hand-off:** two connect-string echoes remain outside `reldex-workspace` (whose own `Debug` prints only a length): `ConnectionParams`' derived `Debug` (`db-driver-api` `params.rs:228`) and upstream `oracledb`'s `invalid connect string: {connect_string}: {reason}` | M |

**Parallelism.** M3.1/M3.2/M3.6 (UI) run alongside M3.3/M3.5/M3.7 (integration). **Review:** M3.3, M3.5, M3.7.

---

### M4 — Worksheet: editor, execution, transactions, honest limits

**Goal.** The core developer loop: type SQL/PL-SQL, run it against a stable session, see errors and output, and control the transaction explicitly.

**Exit gate.** On the Phase 0 test database: run a statement, a selection, and a multi-statement script including a PL/SQL block with `:NEW` trigger DDL; bind values through the dialog; observe DBMS_OUTPUT including Thai; hit a per-statement time limit and read an honest explanation; close a worksheet with an open transaction and be asked Commit/Rollback/Cancel; have a commit failure leave the session open with the transaction intact.

| ID | Status | Title | Owner | Inputs | Outputs | Deps | Acceptance | Size |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| M4.1 ★ | `[ ]` todo | Editor component decision + implementation: `TextArea` + `QSyntaxHighlighter` on `QQuickTextDocument`, tokens from `reldex-sql-text` over FFI | `opus` | SPEC §14; M2.4 | `ui/adapter/SqlHighlighter`, QML editor | M2.11 | Highlighting is per-block with carry state (maps 1:1 to `previousBlockState`); Thai + non-BMP text renders and edits correctly; IME composition works; 1 MB file loads in < 500 ms; the measured practical file-size limit is recorded and enforced with an honest message | L |
| M4.2 | `[ ]` todo | Editor essentials: line numbers, current-line, bracket matching, indentation, search/replace, font and theme settings | `sonnet` | M4.1 | QML | M4.1 | Keyboard-first; every shortcut discoverable | M |
| M4.3 | `[ ]` todo | Statement detection and run modes: current statement, selection, whole script | `sonnet` | M2.4 | `WorksheetController` | M4.1 | Cursor-in-statement resolution matches the splitter; script runs sequentially on one session, stopping or continuing per a user setting; execution must consult `StatementSpan::ended_by` (not only `terminated`) — e.g. to decide whether an `EndedBy::InferredBlockEnd` span with `terminated: false` should be sent, retried, or surfaced as "not yet submitted"; this milestone's integration tests must also confirm or correct, against a real SQL\*Plus/SQLcl client, the two `reldex-sql-text` splitter behaviors ADR-0002 amendment J6 documents as unverified (a `/` line with a trailing comment not treated as a terminator; a `;` followed by a lone `/` yielding one statement with no re-run) | M |
| M4.4 | `[ ]` todo | Bind-variable dialog: detected placeholders, typed entry, IN/OUT/IN OUT | `sonnet` | ADR-0002 D6 | QML + controller | M4.3 | Re-executable (`Statement` is `Clone`); refuses the two unsafe NUMBER bind shapes with the driver's own explanation (U-1/U-2), never silently | M |
| M4.5 ★ | `[ ]` todo | Transaction UX: auto-commit OFF, Commit/Rollback, savepoints, close-with-pending-transaction dialog mapped to `CloseDisposition` | `opus` | SPEC §10; ADR-0002 K4 | QML + controller | M2.5 | `DecisionRequired` → dialog; `CommitFailed`/`RollbackFailed` → session stays open and the user is told the transaction is unchanged; a lost session's close says nothing was committed; **no path commits without an explicit user action** | L |
| M4.6 ★ | `[ ]` todo | The honest no-Cancel UX: three-level time-limit control, "no limit" with its consequence, no Cancel button, "Disconnect worksheet…" as the only stop | `opus` | SPEC §10 interim note; ADR-0001; `phase-0-spike-results.md` §4 "S4 addendum (2026-09-24)" — wording must distinguish "stopped at the time limit, session intact" (the common CPU-bound case) from "the connection had to be dropped" (suspended server work) | QML + wording | M2.5 | Nothing in the UI is labelled Cancel; the running state shows the *armed* deadline from `Executing`; choosing "no limit" states plainly that only disconnecting can end a hung statement, and that disconnecting loses the transaction | M |
| M4.7 | `[ ]` todo | DBMS_OUTPUT pane: per-worksheet enable, size, clear, truncation notice | `sonnet` | M2.7 | QML | M2.7 | Off by default (it costs a round trip); truncation is reported, never silent. The pane must also say that the user's own `DBMS_OUTPUT` calls override it: an `ENABLE(2000)` in the user's code overflows at 2,000 bytes even under an "unlimited" pane, and a `DISABLE` in the user's code purges what was buffered. | S |
| M4.8 ★ | `[ ]` todo | Error presentation: kind, ORA code, message, cause chain; caret highlighting **only** for PL/SQL positions | `opus` | ADR-0002 D3/S3; SPEC §24.14 | QML + offset mapping | M2.11 | PL/SQL `ORA-06550` line/column maps to the right character in a Thai/non-BMP document via the UTF-16 conversion; a plain SQL error shows no caret and says why position is unavailable | M |
| M4.9 | `[ ]` todo | Multiple independent worksheets: N sessions, per-tab state, one busy tab never blocks another | `sonnet` | SPEC §24.17 | Controller + shell | M3.3 | 8 concurrent sessions, one blocked 20 s, UI stays at frame budget (measured) | M |
| M4.10 | `[ ]` todo | Query history (per profile, SQLite), re-run into the current worksheet | `sonnet` | M2.9 | Store + QML | M2.9 | Bounded size; secrets never captured; text stored verbatim | S |

**Parallelism.** M4.1–M4.4 (editor track) and M4.5–M4.8 (execution/transaction track) are independent after M2; M4.9/M4.10 follow. **Review:** M4.1 (FFI + text correctness), M4.5, M4.6, M4.8.

---

### M5 — Results at scale

**Goal.** A million-row result behaves, and the fetch default is chosen by measurement, not taste.

**Exit gate.** A 1,000,000-row query from the real database scrolls at the M1 frame budget; memory stays within the ADR-0004 policy; the batch-size benchmark report exists and the shipped default is set from it; LOB viewers open a 100 MB CLOB without materializing it.

| ID | Status | Title | Owner | Inputs | Outputs | Deps | Acceptance | Size |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| M5.1 ★ | `[ ]` todo | **ADR-0004 — Result Store representation, paging and bounded-memory policy** | `opus` | ARCHITECTURE §13 item 6; spike S14 | `docs/decisions/0004-result-store.md` | M1 gate | Decides: retain the fetched prefix in core, fetch-on-demand as the view scrolls, configurable row/byte caps with an explicit "fetched N rows (limit reached)" state; spill/eviction and Arrow explicitly deferred to P3 with the benchmark that would reopen it | M |
| M5.2 ★ | `[ ]` todo | Result Store implementation in `db-core` + FFI batch lifetime rules | `opus` | M5.1 | `db-core` + `crates/ffi` | M5.1 | Random access O(1) over the fetched prefix; batch release rules tested; no locator ever crosses a thread (K1 test extended) | L |
| M5.3 | `[ ]` todo | Grid features: row numbers, NULL visualization, column resize/reorder, type-aware formatting via the bulk formatter, search-in-results | `sonnet` | ADR-0003 D4 | QML + model | M5.2 | NULL, `Taken` and `Unsupported` are three *visibly different* states — never all shown as empty | M |
| M5.4 | `[ ]` todo | Copy: cell, row, range, with/without headers | `sonnet` | M5.3 | Controller | M5.3 | Large range copy streams rather than materializing; delimiter configurable | S |
| M5.5 | `[ ]` todo | CLOB/BLOB viewers over `read_lob_chunk`, paged, with a size warning | `sonnet` | `db-core` LOB API | QML | M5.2 | 100 MB CLOB opens with bounded memory (S7 method); NCLOB Thai/non-BMP byte-exact (S11 method) | M |
| M5.6 ★ | `[ ]` todo | **Fetch-batch benchmark** across row shapes and a real network; pick and record the shipped default | `opus` | spike S14 | `docs/exec-plans/active/phase-1-fetch-benchmark.md` | M5.2 | ≥3 row shapes × ≥4 batch sizes × local and a latency-injected link; method and environment recorded; the default is a *setting* with a bounded range, and the number is justified by the data (S14 showed throughput is not monotonic) | M |
| M5.7 | `[ ]` todo | Perf gate re-run on the real database; record against M1's numbers | `sonnet` | M1.8 method | Updated measurement report | M5.3 | Frame time, first-row latency, RSS, fetch throughput all recorded; regressions vs M1 explained | S |
| M5.8 ★ | `[ ]` todo | Scrolling while a result is still streaming drops ≈ 0.3% of frames (GUI-thread bound: drains + view work) — budget the drain per frame / insert coalescing | `opus` | S15 `streamscroll` phase (`phase-1-s15-ffi-spike.md` K6) | `db-core`/adapter change + re-measurement | M5.2 | The worst frame in the `streamscroll` phase (49.53 ms, 2/601 over 33 ms) is reduced to the machine's own background rate (~0.02%, per K1's idle control); boundary's own share of a drain stays under 1% | M |

**Parallelism.** M5.3–M5.5 run in parallel after M5.2; M5.6 runs alongside them; M5.8 follows M5.2. **Review:** M5.1, M5.2, M5.6, M5.8.

---

### M6 — Browse, prove, package

**Goal.** Close the MVP: minimal object browser, workspace persistence, the non-functional obligations, and a Windows installer.

**Exit gate.** A fresh Windows machine installs Reldex from the produced artefact, connects, runs a query, browses objects, restarts with its workspace restored, shows Thai UI correctly, and ships a complete third-party notices file. CI is green on all three OS including offscreen QML tests.

| ID | Status | Title | Owner | Inputs | Outputs | Deps | Acceptance | Size |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| M6.1 | `[ ]` todo | Object browser: lazy tree over the 9 `SPEC.md` §16 groups, server-side filter, row cap, columns of a selected table | `sonnet` | M2.8 | QML + model | M2.8 | Uses its own metadata session (never the worksheet's); a permission failure reads as a permission message, not a driver error; no freeze with a large schema (measured) | L |
| M6.2 | `[ ]` todo | Workspace persistence: open worksheets, text, layout, active profile — **non-transactional state only** | `sonnet` | SPEC §20/§24.16 | Store + shell | M2.9 | Restores after restart; never restores or implies a session or a transaction | M |
| M6.3 | `[ ]` todo | i18n baseline: `qsTr` everywhere, EN + TH catalogues, `lrelease` in the build; Thai rendering test in editor, grid and dialogs | `sonnet` | SPEC §14 | `.ts` files + test | M4.2 | A screenshot test (offscreen) covers Thai and non-BMP in all three surfaces; no clipped or tofu glyphs; font setting documented | M |
| M6.4 | `[ ]` todo | Accessibility baseline: focus order, keyboard-only operation, `Accessible` properties, no colour-only meaning, contrast check | `sonnet` | SPEC §14 | QML + checklist | M3.4 | Every primary flow completable from the keyboard; Windows Narrator smoke pass recorded | M |
| M6.5 | `[ ]` todo | Third-party notices: `cargo about` for the Rust graph + Qt/LGPL attribution, shipped in the installer and an About dialog | `sonnet` | SPEC §22; TASKS P0 | `NOTICES` artefact + build step | M6.6 | Covers the full transitive graph (63 crates in the oracle-thin graph alone) plus Qt and its third-party content; generated by the build, not by hand | M |
| M6.6 ★ | `[ ]` todo | Windows packaging: `windeployqt6`, unsigned installer, first-run layout | `opus` | §C.0 licence position | Installer + `docs/packaging.md` | M6.1–M6.4 | Qt linked dynamically; DLLs replaceable; no GPL-only module present (verified by an inventory step); installs and runs on a machine with no dev tools | M |
| M6.7 | `[x]` done 2026-09-25 — already delivered by `ui.yml` (M1.4/M1.5); evidence + Actions licences recorded | CI: build the Qt project on all three OS; run offscreen QML/QTest and the C smoke harness; cache Qt and cargo | `sonnet` | M1.5 | `.github/workflows/ui.yml` | M1.5 | Cold job under the agreed time budget; `QT_QPA_PLATFORM=offscreen`; keeps the existing fast hermetic Rust job untouched | M |
| M6.8 ★ | `[ ]` todo | Phase 1 DoD review against `SPEC.md` §24, honest status per item; update `TASKS.md`, `phase-1.md`, `Task.html` | `opus` | everything | Exit assessment | all | Every DoD item marked met / met-with-limits / not-met with evidence; §24.8 Cancel stays **not met**; nothing softened | S |
| M6.9 | `[ ]` todo | Cold first paint ≈ 800–900 ms (D3D11 device creation ≈ 250 ms + first delegate-instantiation polish ≈ 551 ms) vs `SPEC.md` §19 startup target — investigate fix candidates named in the S15 report | `sonnet` | S15 K2 diagnosis (`phase-1-s15-ffi-spike.md`) | Adapter/QML startup change + re-measurement | M1 gate | Cold execute → first painted frame materially under 903.55 ms, ideally toward `SPEC.md` §19's <1 s desirable / <2 s acceptable warm-startup target; warm-path number (15.85 ms) unaffected | M |

**Parallelism.** M6.1/M6.2 (features) run alongside M6.3/M6.4 (non-functional), M6.5/M6.7 (build) and M6.9 (startup). **Review:** M6.6, M6.8.

**M6.7 as implemented (branch `phase-1/m6-7-ui-ci`).** `.github/workflows/ui.yml` already carried M6.7's full scope as a byproduct of M1.4/M1.5 — this task's real work was verifying that against the acceptance line, closing the one documentation gap (CI-Actions-dependency licences), and recording current evidence here rather than only in ADR-0003's original spike numbers.

Jobs (all three matrixed `windows-latest`/`ubuntu-latest`/`macos-latest` except `qt-asan`):
- `ffi-smoke` — no Qt install; builds `ui/tests/ffi_smoke` (the C11/C++17 smoke harness) against `reldex-ffi`'s cdylib and runs it via CTest; ASan/UBSan on the ubuntu leg. `timeout-minutes: 25`.
- `qt-build` — real Qt 6.8.3 (LGPL, dynamic) install, then `bash ui/build.sh --test` with `QT_QPA_PLATFORM=offscreen` (the full QML/QTest suite, `ADR-0003 K7`'s job). `timeout-minutes: 25`.
- `qt-asan` (`ubuntu-latest` only) — same as `qt-build` but `bash ui/build.sh --sanitize --test` (ADR-0003 K5's ASan half). `timeout-minutes: 25`.

Cold (ADR-0003 K7's original spike, before `ffi-smoke`/`qt-asan` existed, `qt-build` only): windows 3.0 min, ubuntu 2.1 min, macos 1.6 min (macos **FAILED** once on a build-script race, fixed in M1.4; every run since has been green on all three). Budget: **25 minutes cold**, per ADR-0003 K7 — the only time budget written in this plan/ADR set; `timeout-minutes: 25` on `qt-build`/`qt-asan` enforces it, `ffi-smoke` (no Qt install) gets `timeout-minutes: 15`.

Warm, from two consecutive real `push`-to-`main` runs today (2026-09-25, runs `36079299866` and `36080240322`, both cache hits on cargo and Qt — see below): `qt-build` windows 3m9s/3m11s, ubuntu 2m0s/1m36s, macos 1m58s/2m18s; `ffi-smoke` windows 1m29s/1m25s, ubuntu 26s/59s, macos 36s/39s; `qt-asan` ubuntu 2m2s/1m56s. Worst warm job seen: 3m11s — consistent with ADR-0003's own "worst warm job 4.0 min" and nowhere near the 25-minute budget.

Cache-hit evidence (run `36080240322`, `qt-build`): cargo (`Swatinem/rust-cache`) — `Cache hit for: v0-rust-qt-build-<os>-...` / `Cache restored successfully` on all three OS; Qt (`jurplel/install-qt-action`, `cache: true`) — `Cache hit for: install-qt-action-<os>-...-6.8.3-...-qtshadertools` / `Cache restored successfully` on all three OS. The Qt cache key embeds the pinned version (`6.8.3`) and module list, so bumping either invalidates the cache automatically — no separate cache-key plumbing was needed.

Gaps closed by this task: `jurplel/install-qt-action`'s own licence (MIT) and the other three CI-only Actions' licences were not previously documented anywhere; now in `ui.yml`'s header comment and `ui/README.md`'s new "CI-only dependencies (M6.7)" subsection. No workflow behavior changed — `ffi-smoke`, `qt-build` and `qt-asan` were already green on all three OS (`qt-asan` is ubuntu-only by design, matching `ffi-smoke`'s own ASan/UBSan leg, for the reasons already in `ui.yml`'s comments) and `ci.yml`'s fast hermetic Rust job was not touched.

**M6.9 as implemented / as measured (branch `phase-1/m6-9-cold-first-paint`, 2026-09-25).** No code change; a measurement-only outcome, checkbox left for the lead. Full method and numbers: `phase-1-s15-ffi-spike.md` "M6.9 — cold first paint follow-up" and `ui/README.md` "Startup (M6.9)". Summary: a fresh, unmodified-code baseline (n=12, quiet machine, checked before every batch per this task's own constraint — two other workers were running `cargo`/`rustc` concurrently) measured **576.19 ms median**, not 903.55 ms — a ~36% drop attributable to driver/OS-cache state warmed since S15's 2026-09-20 run, not to any change here; every candidate was judged against this fresh number, per the task's own instruction. `QSG_RENDER_LOOP=basic` (n=8) gave no material gain (594.78 ms median, within baseline noise) beyond a much tighter spread (CoV 4.1% vs 23.1%) and was not adopted (blast radius on K1's frame-pacing numbers, not re-verified). `Loader.asynchronous` and pre-warming the delegate with a throwaway query were reasoned through and partly evidenced (the fresh warm-path number, 16.38 ms vs S15's 15.85 ms, unaffected, is the pre-warm candidate's measured ceiling) but not implemented: `ui/app/Main.qml` (M3.1's app shell) has no `TableView` yet to attach either fix to — the result pane is still `WorksheetArea.qml`'s placeholder. Recommendation for the owner's K2 ruling is unchanged from S15's (rule on the warm path); recommendation for M4.x is concrete and written up (pre-warm the real result grid's delegate with a throwaway query at startup). Acceptance line's literal wording ("materially under 903.55 ms") is technically met by the fresh baseline alone, which is exactly why this note leads with "no code change" rather than a claimed win.

## C.3 — Owner decisions required (numbered; recommendation for each; current status per the 2026-09-20 facts)

1. **Install Qt and the build tools per §C.0?** — *Recommend yes*, Qt 6.8 LTS `msvc2022_64`, modules `qtbase`/`qtdeclarative`/`qtshadertools`/`qtsvg`/`qttools` only, **without Qt Creator**, plus CMake and Ninja. ~2 GB download, ~5 GB on disk. Nothing starts without this.
   **Status:** Approved 2026-09-20 as recommended (no Qt Creator). Install in progress (M1.1).
2. **Licence position.** — *Recommend*: ship Community under **LGPLv3-compliant dynamic linking**, ban GPL-only Qt modules, and treat a commercial Qt licence as a decision deferred to the first of (a) iOS distribution, (b) a closed-source Pro build that needs static linking. Budget implication to be aware of now, not to spend now.
   **Status:** Approved 2026-09-20 as recommended.
3. **Qt version policy.** — *Recommend* pinning one exact Qt version in `phase-1-toolchain.md` and treating an upgrade as a reviewed change (same discipline as the `oracledb` pin). Note that LTS patch releases move to commercial-only after the open-source window; the pinned version must be one we can still legally obtain.
   **Status:** Approved 2026-09-20 as recommended.
4. **Approve the defer list (§C.1).** — *Recommend yes as written.* The sharpest cuts: Explain Plan and export move to P2 even though `SPEC.md` §24 lists them, because §24 is the **V1** Definition of Done, not the MVP. If you want either in Phase 1, say which milestone loses a task to pay for it.
   **Status:** Approved 2026-09-20 as recommended.
5. **App identifier and branding.** — Needs: reverse-DNS id, executable name, display name, installer publisher string, and a placeholder icon. *Recommend* `com.reldex.reldex` / `Reldex.exe` / "Reldex", with `AGENTS.md`'s rule enforced by an automated check that no vendor trademark appears in any of them. Vendor names remain allowed in driver and compatibility text.
   **Status:** Approved 2026-09-20 as recommended.
6. **Code signing for Windows.** — *Recommend* shipping Phase 1 **unsigned** (internal/early users see a SmartScreen warning) and buying a certificate only before public distribution. Signing an unsigned-today build later is cheap; buying early is not.
   **Status:** Approved 2026-09-20 as recommended.
7. **Secrets storage.** — *Recommend* Windows Credential Manager via the `windows` crate (MIT/Apache-2.0) for Phase 1, with the `keyring` crate evaluated for macOS/Linux in P2. Rejected for now: `keyring` on Windows-only, because its Linux path drags in zbus/D-Bus we do not need yet. **No plaintext fallback ever** — if no store is available, Reldex prompts every time.
   **Status:** Approved 2026-09-20 as recommended.
8. **Local store format.** — *Recommend* one SQLite file (rusqlite, bundled SQLite) for profiles, settings, history, workspace: atomic, no half-written config, and it is needed for history regardless. Trade-off accepted: settings are not hand-editable; an export/import to TOML can come later if support needs it.
   **Status:** Approved 2026-09-20 as recommended.
9. **Telemetry and logging.** — *Recommend* no telemetry at all in Phase 1; local rotating log file, default level `info`, SQL text logged only at `debug` behind an explicit opt-in, secrets redacted by construction and asserted by test.
   **Status:** Approved 2026-09-20 as recommended.
10. **Fetch-batch default (M5.6).** — *Recommend* the owner signs off the number the benchmark produces rather than pre-committing one. S14 showed 10,000 rows/batch was ~3.5× *slower* than the best of 100 and 1,000, so intuition is actively wrong here.
    **Status:** Open — not yet decided (deferred to the M5.6 benchmark by design). **Evidence added by S15's fetch-size sweep (2026-09-20, headless + in-app, `phase-1-s15-ffi-spike.md` "Sweeps"):** as information for the owner, not a decision — 1,000 rows/fetch with 2 fetches in flight is the fastest-or-tied-fastest stream, the lowest per-row memory, and the lowest per-batch boundary cost among the sizes that stream well; 100 rows/fetch costs ~20 MB/million rows more; 50,000 rows/fetch costs 30.64 ms of first-row latency; more than 2 fetches in flight buys nothing (n=3 repeats: the four settings' medians span 14 ms against a 16–40 ms per-cell spread). Every number has **zero network latency in it** and must be re-measured on a real network in M5.7, per the report's own recommendation.
11. **Wording sign-off for the no-Cancel UX and "no limit" (M4.6).** — *Recommend* the owner reads and approves the exact strings, because this is the product's honesty commitment in user-visible form. Proposed: Cancel is absent, not disabled; the run bar shows "Time limit 10 min (from profile)"; "no limit" reads *"A statement with no limit can only be ended by disconnecting this worksheet, which loses its transaction."*
    **Status:** Open — not yet decided.
12. **Phase-0 leftovers carried into M2 (C-5, U-18, C-6).** — *Recommend* treating them as Phase 1 M2 tasks (as planned above) rather than blocking the Phase 1 start on them. Confirm.
    **Status:** Resolved — the carry-over approach is confirmed and the work was done as Phase 0 carry-over and merged on 2026-09-20; M2 consumes the result (M2.1–M2.3 above).
13. **Upstream issues F and G** (U-15…U-18) — still awaiting your go-ahead from Phase 0. *Recommend* submitting; F in particular is the connect-timeout gap M2.1 works around locally.
    **Status:** Resolved — the owner decided on 2026-09-19 **not** to submit F and G for now; the drafts are kept for tracking only (results file §6). This is a decision, not a pending recommendation.
14. **Mobile test hardware.** — Not needed for Phase 1, but *recommend* acquiring an Android arm64 device (API 26+) during Phase 1 so P4 is not gated on procurement. iOS additionally needs a Mac and an Apple Developer account.
    **Status:** Resolved for Android — the owner provided an Android arm64 phone (OPPO CPH2399) on 2026-09-19 and the native-binary device run passed 7/7 on 2026-09-20 (`phase-0-android-device.md`); previously reported as in progress on branch `phase-0/android-device` (not finished; no results to cite yet). This validation is Phase-0 scope, not Phase 1. iOS still needs a Mac + Apple Developer account + device — not yet provided.
15. **Community/Pro licensing decision** (open since P0). — Blocks first distribution, not Phase 1 development. *Recommend* deciding before M6.6 so the notices file and About dialog are right the first time.
    **Status:** Open — not yet decided.

---

## D. Risks and unknowns

| # | Risk | Impact | Mitigation | Earliest task that retires it |
| --- | --- | --- | --- | --- |
| R1 | Qt Quick `TableView` + a custom model cannot hold 60 FPS over 1M rows on the dev machine | The whole UI choice is wrong; Phase 1 has no foundation | Measure it first, with the real boundary, before any feature exists; kill criteria K1/K3 stated in advance — **retired by S15 evidence (2026-09-20):** K1 worst frame-production p50 5.72 ms / worst app CPU 5.24 ms against an 8 ms threshold, and 8 of 36,008 vsync-on frames over 33 ms (0.022%); K3 rows cost +172.5–172.7 MB against a 200 MB threshold. Reduced, not fully retired: K1's p99 wording and K3's baseline/metric still need an owner ruling (ADR-0003) | **M1.8** |
| R2 | The waker → `invokeMethod` path has a lifetime or reentrancy defect that only appears under load or on shutdown | Intermittent crashes that are expensive to find later | Contract rules in ADR-0003 D5; `set_waker` blocks on in-flight wakes; 10k-iteration ASan teardown test — **retired by S15 evidence (2026-09-20):** K5, 10,000-iteration flood teardown, no hang/crash/leak in 3 Windows runs, plus ASan+UBSan+LSan clean over the whole adapter suite on Linux CI (job `35508957508`), live counts back to baseline every time | **M1.6 / M1.8 (K5)** |
| R3 | Hand-written FFI introduces UB that tests do not catch | Memory corruption in the most critical layer | One crate, no logic, SAFETY comments, Miri, ASan C harness, mandatory independent review of every FFI change — **reduced by S15 evidence (2026-09-20):** two independent FFI reviews (M1.3's, producing amendments A10–A18; the first-consumers review, producing A19–A25) plus the ASan/UBSan/LSan harness on Linux CI clean over the whole adapter suite; **Miri still not run** (ADR-0003 A9 — no nightly toolchain on the dev machine), so this risk is reduced, not fully retired | **M1.3 + M1.4** |
| R4 | `QQuickTextEdit` is unusable for real SQL files (large documents, Thai shaping, IME) | The editor — the product's core surface — needs a rewrite mid-phase | Measure the limit in M4.1 with Thai and non-BMP corpora before building features on it; documented fallback is a custom `QQuickItem` editor with a Rust-side text model, costed as an L task | **M4.1** |
| R5 | A blocked worker with "no limit" accumulates detached threads and sockets | Slow resource leak that looks like a hang to the user | 600 s default; `abandon` detaches within 500 ms (K5); detached-worker counter surfaced in diagnostics; C-5 bounds the connect half | **M2.1 + M2.6** |
| R6 | The event refactor (`ReplyTo`) regresses one of ADR-0002's hard-won correctness properties (K1, K4, K5, K7, K8) | Silent commit, lost transaction, or a handle on the wrong thread — the failures `SPEC.md` §2 ranks worst | Run the **existing** `db-core` suite parameterized over both reply paths; no new semantics, only a new delivery channel; mandatory independent review | **M2.5** |
| R7 | `oracledb` 26.0.0-beta.3 aborts the process on an unhandled input reaching it (U-4) | A crash no layer can contain, now with a GUI and unsaved editor text in play | Driver keeps refusing known-panicking inputs; **add**: periodic editor-text autosave to the workspace store so an abort never costs the user's SQL | **M6.2** (autosave); canary suite already tracks the defect |
| R8 | Retaining a fetched million-row prefix breaks the bounded-memory promise on wide rows | Memory blowout in normal use | ADR-0004 makes the cap explicit, configurable and *visible* ("fetched N rows, limit reached") instead of implicit | **M5.1 / M5.2** |
| R9 | Qt licence obligations are misjudged (module licensing, static linking, iOS) | Legal exposure, or a forced rewrite before shipping Pro | Module inventory step in the packaging task; verify Qt's per-module licence table for the pinned version before first distribution; owner decision 2 taken with eyes open | **M6.6** |
| R10 | Qt in CI is slow or flaky enough to stop being a gate | Regressions land unnoticed; the three-OS promise rots | Separate workflow from the fast hermetic Rust job; cache Qt and cargo; offscreen only; a time budget the job must meet | **M6.7** (proven in **M1.5**) |
| R11 | Statement splitting disagrees with the server on real scripts | Wrong statement executed — a correctness failure, not a UI bug | Corpus-driven tests in M2.4 including `q'[…]'`, nested/labelled blocks, compound triggers, comments and Thai; a lone `/` line is authoritative regardless of nesting depth (S1, ADR-0002 J4), bounding any undiscovered depth-tracking bug to one over-large span rather than an executable fragment; 800k-case deterministic fuzz test; the splitter never drives transaction policy (classification stays in the driver, ADR-0002 D4/S9). **Round-2 finding (ADR-0002 J5):** random-fragment fuzzing essentially never produces the *balanced*, deeply-nested structures needed to surface a depth-tracking bug, missing a defect 6M cases of it ran clean over — a grammar-based differential test (`tests/differential.rs`, generates whole valid PL/SQL units and asserts exact recovery after joining several) is now a permanent second gate alongside the fragment fuzzer, and is what found the worst of the round-2 defects | **M2.4** |
| R12 | Thai/non-BMP position mapping is wrong, so error carets point at the wrong token | Misleading diagnostics in exactly the market the product targets | Core supplies scalar→UTF-16 conversion; tested against the S11 corpora; fragment offsets added in one place | **M4.8** |
| R13 | Upstream `oracledb` ships a breaking beta while Phase 1 is mid-flight | Unplanned rework in the driver layer | The pin, the canary suite and `oracledb-upgrade-checklist.md` already exist; treat an upgrade as its own reviewed task, never as incidental | already retired (Phase 0) |
| R14 | Phase 0's `SPEC.md` §24.8 Cancel gap becomes a support/marketing problem once real users see it | Trust damage if it is discovered rather than disclosed | The UI states it plainly (M4.6), the README already does, and the DoD review keeps it marked **not met** | **M4.6 / M6.8** |

**Known unknowns, deliberately not guessed:** the practical editor file-size ceiling (M4.1 measures it); the shipped fetch-batch default (M5.6); whether a `ping` between statements is cheap enough to use as a liveness probe in the UI (measure in M5.7 — S1 measured 769 µs median, which suggests yes); and whether Qt's offscreen platform gives frame timings worth gating on in CI (assume not; gate on the dev machine).
