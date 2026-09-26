# `reldex-ffi`

The single C ABI boundary between Reldex Core (Rust) and the Qt/C++ adapter (ADR-0003 D2). This
crate is the workspace's one deliberate exception to `unsafe_code = "deny"`
(`crates/ffi/tests/fences.rs` enforces that nothing else opts out). `include/reldex.h` is generated
by `cbindgen` and **committed**; regenerate it with `bash crates/ffi/gen-header.sh` and commit the
result in the same change as any source change that affects it — `gen-header.sh --check` and
`tests/header.rs` both fail on a stale copy.

This file is a map for a reader who already has ADR-0003 (the architecture decision record for this
boundary) open; it does not repeat that reasoning.

## Surface map, by family

| Family | Entry points | Notes |
| --- | --- | --- |
| Hub / sessions | `reldex_hub_create`/`_destroy`/`_set_waker`, `reldex_hub_open_session`, `reldex_session_execute`/`_fetch`/`_close`/`_close_result`/`_abandon`, `reldex_hub_next_event`/`_pending_events`, `reldex_hub_session_count`/`_list_sessions` | One hub per process is typical but not required; a session is scoped to the hub that opened it, and its id is not found once its `TERMINAL` has been drained. |
| Transaction control | `reldex_session_commit`/`_rollback`/`_savepoint`/`_rollback_to_savepoint`/`_ping` | Reply is `RELDEX_EVENT_KIND_COMPLETED`; read `completed_operation` (`ReldexCompletedOperation`) to know which. |
| Server output | `reldex_session_set_server_output`, `reldex_server_output_lines_count`/`_get`/`_release` | Off by default; a reconnect never carries the setting over. Delivered ahead of the reply that follows it (see "Events" below). |
| Statement splitting | `reldex_split_statements` -> `ReldexStatementSpan` | `snprintf`-style capacity contract; honours the caller's `struct_size`. |
| Metadata | `reldex_metadata_prepare` + `ReldexMetadataQuery` (`_sql`/`_bind_count`/`_column_count`/`_column`/`_reclassify_error`/`_release`) | Vendor-concrete: this is one of the two places (with `workspace.rs`) allowed to name `reldex-driver-oracle-thin` directly. |
| Batches / cells | `reldex_batch_*`, `reldex_batch_format_column`, `reldex_text_arena_*` | Zero-copy on the dominant column types; see `batch.rs`/`format.rs` module docs for the column-kind contract. |
| Errors | `ReldexError`, `reldex_error_view`, `reldex_last_error_take`/`_clear` | Per-thread last-error slot; an error attached to an *event* is owned by whoever drains the event instead. |
| Workspace: settings/profiles | `ReldexWorkspace`, `reldex_workspace_open`/`_close`/`_set_waker`/`_pending_replies`/`_next_reply`, `_resolve_setting`/`_set_setting`/`_clear_setting`, `_create_profile`/`_update_profile`/`_delete_profile`/`_get_profile`/`_list_profiles`, `_build_connect_params` | One service thread per `ReldexWorkspace`, independent of the hub's event queue and waker. |
| Workspace: credentials | `reldex_workspace_credential_get`/`_put`/`_delete`, `_resolve_password`, `ReldexSecret` (`reldex_secret_expose`/`_from_utf8`/`_release`), `ReldexCredentialError`, `ReldexCredentialStoreKind` | A password never crosses as a `ReldexStr` the caller could keep; see "Ownership and lifetime" below. |
| Workspace: history/worksheets/layout | `reldex_workspace_record_history`/`_list_history`/`_clear_history`, `_save_worksheet`/`_load_worksheets`/`_delete_worksheet`, `_save_layout`/`_load_layout`, `_new_worksheet_id` | Non-transactional state only (`AGENTS.md`): no session, connection or transaction is ever implied. |
| Diagnostics | `reldex_abi_version`, `reldex_live_counts` | `reldex_live_counts` is a test/diagnostic instrument, not part of the working API — see `counters.rs`'s module doc. |

## Ownership and lifetime

- Every non-opaque struct starts with `uint32_t struct_size` (D7): a call honours the caller's
  declared size, writes `min(declared, this build's size)`, and reports how much it filled in. A
  `struct_size` too small to hold the type's documented minimum is refused, not guessed at.
- Every enum reserves `0` for "a value this header predates" (D6/D7) — an unknown value from a
  future header is never undefined behaviour on an older one.
- **Which fields a library fills is decided by its ABI minor, not by `struct_size`.** A field
  appended in a minor can land inside the previous minor's tail padding, where the returned
  `struct_size` is the same with or without it: 3.1's `completed_operation` (offset 108) sits inside
  3.0's 112-byte `ReldexEvent`, and 3.2's `has_deadline`/`transaction_possibly_active`/`abandoned`
  (145–147) inside 3.1's 152 bytes. An adapter must therefore refuse a library whose minor is below
  its header's (`Bridge::checkAbiVersion`, `Bridge::StartError::AbiMinorTooOld`); a newer minor is
  fine. `struct_size` still tells the *library* which header a caller was built against, which is
  how it avoids handing a caller a kind that header predates (ADR-0003 A35, A38).
- A caller-owned object (`ReldexBatch`, `ReldexError`, `ReldexSecret`, `ReldexProfileList`,
  `ReldexHistoryList`, `ReldexWorksheetList`, `ReldexMetadataQuery`, `ReldexServerOutputLines`, …) is
  released with its own paired `_release`/`_free` function, never `free()`. `reldex_live_counts`
  exists so a test can assert every object handed out has come back.
- `ReldexSecret` zeroizes on release. `reldex_secret_expose` borrows from it and is the one
  documented exception to this boundary's outbound-NUL-termination promise (ADR-0003 A30, A13) —
  compare by length, never by scanning for a NUL. `reldex_secret_from_utf8` builds one from caller
  bytes the caller is responsible for wiping itself.
- Closing a hub or a workspace never blocks the caller and does not require draining the reply
  queue first: whichever of the close call or the owning thread's own exit drops the last reference
  also drops whatever is still queued, releasing everything it owns through ordinary `Drop`. Calling
  a drain function on a handle *after* closing it is a use-after-free, not a way to collect what was
  left — see `reldex_hub_destroy`'s and `reldex_workspace_close`'s own doc comments.

## Threads

- The hub spawns no thread of its own per session: each session's worker thread belongs to
  `db-core`'s `SessionRegistry`, and events are translated on the caller's thread inside
  `reldex_hub_next_event`. The workspace's service thread is exactly one thread, spawned and owned
  by this crate. A request function submits and returns immediately; the answer arrives as an
  event/reply drained after a waker fires. Never do database or filesystem I/O on the caller's
  thread (`AGENTS.md`).
- `reldex_hub_destroy` abandons every open session and returns without waiting; if any session is
  still finishing, the registry's bounded teardown (at most 500 ms in total) runs on a short-lived
  `reldex-ffi-teardown` thread instead of the caller's (ADR-0003 A33).
- A waker callback must not block and must not call any `reldex_*` function (D5) — re-entering from
  inside one is refused (`RELDEX_STATUS_REENTRANT`), not undefined behaviour.
- `Store` (SQLite-backed) and the credential store are both `Send`, not `Sync`, and must live on one
  thread for their whole life; a `ReldexWorkspace`'s settings/profiles/credentials/history/
  worksheets/layout calls all share that one service thread rather than opening three competing
  threads against the same file.
- A panic inside the workspace service thread's command loop is caught (`catch_unwind`): the
  workspace is marked failed, every outstanding and later request gets
  `RELDEX_STATUS_INVALID_STATE` instead of a silent hang, and the process does not abort.
- `reldex_workspace_open`'s service thread (`service_main`) opens a non-in-memory store with
  `Store::open_creating`, not `Store::open`: it creates the parent directory and the store file
  itself (owner-only on Unix) if either is missing, the same rule `Store::open_default` applies to
  the platform-default path (ADR-0006 P5). Fixed 2026-09-26 (M3.2 fix round) — the C++ adapter used
  to reimplement this itself before calling here (on whichever thread called it, sometimes the UI
  thread), which is exactly the filesystem I/O the bullet above says never to do off this thread.
  Internal behaviour only; `reldex_workspace_open`'s signature is unchanged.

## Events (ABI 3.2, M2.15)

The hub drains `db-core`'s `EventQueue` (`docs/exec-plans/active/phase-1.md` §B2/§B3; ADR-0003
A29, A32–A36). What a caller can rely on:

- **Exactly one `RELDEX_EVENT_KIND_TERMINAL` per session**, carrying `transaction_possibly_lost`,
  `abandoned` and (when the session was lost) the cause in `error`, which the caller owns. It follows
  a close that closed, a connect that failed or was abandoned, a contained worker panic — and, since
  3.2, arrives the moment a session is **lost mid-statement**, without waiting for a close. Taking it
  retires the session: later calls naming it report `RELDEX_STATUS_NOT_FOUND`, and
  `reldex_hub_session_count`/`reldex_live_counts().sessions` stop counting it.
- **`reldex_session_abandon`** relays to the registry and never blocks; it reports what it found
  (`ReldexAbandonOutcome`) and whether a transaction may have been lost. The session's `TERMINAL`
  then says `abandoned`.
- **Only a reply carries a request id.** Exactly one event per accepted request has that request's
  id in `request` — the 3.1 contract, unchanged — and every other event has `0`.
- **A caller is never given a kind its header predates.** `reldex_hub_next_event` reads the
  caller's `struct_size`: below 3.2's `ReldexEvent` size, `EXECUTING`, `TRANSACTION_STATE` and
  `FETCHED_SEGMENT` are discarded, not delivered, and no wake is raised for them alone. A 3.1
  adapter sees 3.1's kinds and nothing else; origin/main's 3.1 `smoke.c`, built against the 3.1
  header, passes 457/458 against this library. The one check it fails asserts that the library's
  `sizeof(ReldexEvent)` *equals* the 3.1 header's, which no minor that appends a field can satisfy
  (3.0 → 3.1 grew it from 112 to 152 bytes; 3.2 to 168); the 3.2 harness checks `>=` instead.
- **Progress and notifications**: `EXECUTING` (`request == 0`; the statement's id is in
  `executing_request`, and its deadline in `deadline_ms` when `has_deadline`) precedes that
  statement's `EXECUTED`; `TRANSACTION_STATE` reports `transaction_possibly_active` when it changes
  (coalesced, never dropped); `SERVER_OUTPUT` arrives ahead of the reply that follows it, with
  `server_output_dropped` and `server_output_invalid_utf8_lines`. `FETCHED_SEGMENT` is surfaced as
  an opaque kind; nothing in 3.2 submits one.
- **Unknown kinds**: a kind this header predates arrives as its raw value, or as
  `RELDEX_EVENT_KIND_UNKNOWN` for a `db-core` event this build does not translate. Ignore both (D7).
- **Bounds**: a session holds at most 1,024 accepted-but-undrained replies; past that a submit is
  refused with `RELDEX_ERROR_KIND_RESOURCE`, never blocked. Unsolicited events are capped per session
  (256), with dropped output counted.
- **What the drain costs**: translation now happens inside `reldex_hub_next_event`, on the caller's
  thread, including the one `ReldexBatch` allocation per `FETCHED` that the pump used to make on its
  own thread. Measured before/after in ADR-0003 A37: about **1.1 µs** more per event on the caller
  during the initial stream and about **2.4 µs** more per event under `streamscroll`; the whole
  drain did not move beyond noise. Allocations per event taken: `FETCHED` 1 (the `ReldexBatch`
  box), `EXECUTING`/`TRANSACTION_STATE`/`COMPLETED`/`SERVER_OUTPUT_CONFIGURED`/a DML's `EXECUTED` 0
  — pinned by `allocations.rs`'s `taking_an_event_allocates_what_it_hands_over_and_nothing_else`;
  off the hot path, a query's `EXECUTED` 9 (its column descriptions, once per result),
  `SERVER_OUTPUT` 5 for three lines, `TERMINAL` 1 on a clean close and about 10 when it carries a
  loss's error.

## Known limitation: the fetch-size hint and the result-store settings

`Statement::with_fetch_rows` (`db-driver-api`) is never set by anything in this crate today —
`reldex_session_fetch`'s `max_rows` caps one call's row count but does not become a server-side
fetch-array-size hint. Closing that gap belongs to M5.2 Stage B, not this crate; it is recorded here
so a caller does not assume `max_rows` tunes round-trip count.

Relatedly, M5.2 (landed on `main` after M2.11's branch point) added three settings to
`reldex-workspace`'s registry — `SettingId::ResultsMaxRows`/`ResultsMaxBytes`/
`ResultsCloseCursorAtLimit` — that this crate has not been extended to expose, and M5.6 added a
fourth the same way, `SettingId::ResultsRoundTripBytes`: they resolve on the Rust side but
`ReldexSettingId::from_setting_id` currently maps all four to `Unknown`, so
`reldex_workspace_resolve_setting`/`_set_setting`/`_clear_setting` cannot name them yet. Pinned as
the current, deliberate answer by `every_setting_id_is_pinned_to_its_numeric_abi_id`
(`workspace.rs`), not a bug for this task to fix — exposing them, including assigning
`ResultsRoundTripBytes` a numeric id, is part of the same M5.2 Stage B follow-up (M2.15) as the
fetch-size hint above.

## Testing this crate without a GUI

1. `crates/ffi/tests/*.rs` — Rust integration tests calling this crate's own `extern "C"` functions
   against the mock driver (`mock-driver`, on by default).
2. `ui/tests/ffi_smoke` — a plain C program (built as both C11 and C++17) linking the `cdylib`,
   driving the mock end to end with no Qt at all.
3. `crates/ffi/tests/fences.rs` enforces the `unsafe_code` boundary and a line-count budget on
   `src/` (ADR-0003 D2: a reviewer should be able to read the whole crate in one sitting).
4. `crates/ffi/tests/header.rs` enforces that `include/reldex.h` matches what `cbindgen` generates
   from source, and that every exported function and every `#[repr(C)]`/`#[repr(i32)]` type actually
   reaches it (the latter guards against the class of gap ADR-0003 A27 records).
