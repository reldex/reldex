//! `reldex-ffi` — the stable C ABI between the C++/Qt adapter and `db-core`
//! (ADR-0003, `docs/decisions/0003-qt-rust-integration.md`).
//!
//! This crate is the **only** place in the workspace where `unsafe` is
//! allowed. It contains no business logic: every exported function is
//! *validate arguments → call `db-core` → marshal the answer*. If you find
//! yourself wanting to decide something here, it belongs in `db-core`.
//!
//! # What crosses the boundary
//!
//! | Object | Crossing form | Owner | Released by |
//! | --- | --- | --- | --- |
//! | Application hub | `ReldexHub*` (opaque) | Rust | [`reldex_hub_destroy`] |
//! | Session | `uint64_t` id | Rust registry | [`reldex_session_close`] |
//! | Result set | `uint64_t` id (session-scoped) | Rust registry | [`reldex_session_close_result`], or session close |
//! | Fetched batch | `ReldexBatch*` (opaque) | **caller**, from the moment [`reldex_hub_next_event`] hands it out | [`reldex_batch_release`] |
//! | Error | `ReldexError*` (opaque) + [`ReldexErrorView`] | **caller**, likewise | [`reldex_error_free`] |
//! | Text arena | `ReldexTextArena*` (opaque) | caller | [`reldex_text_arena_release`] |
//! | Metadata query (M2.11) | `ReldexMetadataQuery*` (opaque) | caller | [`reldex_metadata_query_release`] |
//! | Server output lines (M2.11) | `ReldexServerOutputLines*` (opaque) | caller | [`reldex_server_output_lines_release`] |
//! | Workspace handle (M2.11) | `ReldexWorkspace*` (opaque) | Rust, own service thread | [`reldex_workspace_close`] |
//! | Profile list (M2.11) | `ReldexProfileList*` (opaque) | caller | [`reldex_profile_list_release`] |
//! | Connect params summary (M2.11) | `ReldexConnectSummary*` (opaque) | caller | [`reldex_connect_summary_release`] |
//! | Resolved/fetched password (M2.11) | `ReldexSecret*` (opaque) | caller | [`reldex_secret_release`] |
//! | History page (M2.11) | `ReldexHistoryList*` (opaque) | caller | [`reldex_history_list_release`] |
//! | Worksheet list (M2.11) | `ReldexWorksheetList*` (opaque) | caller | [`reldex_worksheet_list_release`] |
//!
//! Integer ids, not pointers, wherever the core already has a scoped id: a
//! stale or foreign id is *reported* (`RELDEX_STATUS_NOT_FOUND`), whereas a
//! stale pointer would be undefined behaviour (ADR-0003 D3). Pointers are used
//! only where the lifetime is unambiguous and the access is hot.
//!
//! # The contract the adapter must keep (ADR-0003 D5)
//!
//! 1. The waker **must not block and must not call back into `reldex_*`**.
//!    Any FFI entry made from inside a waker returns
//!    `RELDEX_STATUS_REENTRANT` instead of deadlocking.
//! 2. [`reldex_hub_set_waker`] with a null function does not return while a
//!    wake is in flight, so the C++ `Bridge` can be destroyed safely
//!    (spike S15 kill criterion K5).
//! 3. Every `reldex_*` call except [`reldex_session_request_cancel`] is made
//!    from one thread — the Qt main thread. Cancel is deliberately callable
//!    from anywhere and returns promptly.
//! 4. A `ReldexBatch*` borrows *into* its batch. Every pointer
//!    [`reldex_batch_column`] hands out stays valid until
//!    [`reldex_batch_release`] and not one instruction longer.
//! 5. Exactly one reply event is delivered for every *accepted* request, and
//!    events for one session are delivered in the order that session produced
//!    them. A request that is **rejected** (a non-`RELDEX_STATUS_OK` return
//!    from the submitting call) was never accepted and produces no event.
//!    This holds even when the library itself fails: a panic in a session's
//!    pump thread is caught, the session is marked lost, and every request
//!    still owed a reply — the one in flight and everything queued behind it —
//!    gets one failure event.
//!
//! # Errors
//!
//! Every function that returns a non-`RELDEX_STATUS_OK` status sets the
//! thread-local last error **before** returning, so
//! [`reldex_last_error_take`] immediately after a failure always describes
//! *that* failure and never a stale one from an earlier call. A successful
//! call leaves the slot alone; a caller that wants to be certain can
//! [`reldex_last_error_clear`] first.
//!
//! The slot is **per thread**: an error recorded by a call on the Qt main
//! thread is not visible to a worker thread that called
//! [`reldex_session_request_cancel`], and vice versa. Take the error on the
//! thread that made the failing call.
//!
//! An error attached to an *event* is different: it is owned, it crosses on
//! the queue, and it belongs to whoever drains the event.
//!
//! # What is interim here
//!
//! `db-core` today offers `Completion<T>` and a blocking `open_session`.
//! `EventQueue`/`EventSink`/`Waker`/`SessionRegistry`
//! (`docs/exec-plans/active/phase-1.md` §B2/§B3) **have** landed, as tasks
//! M2.5/M2.6 (2026-09-21) — but this crate has not switched the FFI pump onto
//! them yet. That switch is a separate task, **M2.15** (`docs/exec-plans/
//! active/phase-1.md`, after M2.14; ADR-0003 A29 records why it was split out
//! of M2.11 rather than attempted alongside a review-round fix pass), not
//! something M2.11 does. So the event pump still lives *here*, as one thread
//! per session that owns that session's pending `Completion`s and turns them
//! into events (see the module docs in `src/session.rs`). It is deliberately
//! small and deliberately temporary: when M2.15 lands, `session.rs` loses the
//! pump and forwards a `db-core` `SessionEvent` instead, and nothing in the C
//! ABI has to change.
//!
//! **What the interim pump cannot deliver today, until M2.15:** a genuinely
//! unsolicited, mid-statement [`ReldexEventKind::Terminal`] — this build only
//! ever produces one on a close that actually closes, a failed open, or a
//! contained panic, never on a loss the pump discovers between requests;
//! `abandon` is not relayed through the real `SessionRegistry`; there are no
//! `EXECUTING`/`TRANSACTION_STATE` events; and [`ReldexEventKind::ServerOutput`]
//! is delivered only on the completion path (drained after a reply while
//! output is on), never as a truly unsolicited event ahead of it. Each of
//! these is also stated next to the relevant type's own doc comment. The
//! interim pump blocks; it never polls, so no polling interval can distort
//! the S15 measurements.
//!
//! Not yet exported, and out of scope for M1.3: binds and LOB reads. Commit /
//! rollback / savepoint / ping, server output control, statement splitting,
//! metadata, the `Terminal` session event, and settings/profiles/
//! credentials/history/worksheets/layout landed in M2.11 (see below) — each
//! a small, deliberate addition, purely additive to the ABI major version
//! (`RELDEX_ABI_VERSION_MINOR` moved `0` → `1`, not a major bump; see
//! [`RELDEX_ABI_VERSION_MINOR`]'s doc comment for why that is a deliberate
//! deviation from the M2.11 brief's request for a major bump).
//!
//! # M2.11: settings, profiles, credentials, history, worksheets and layout
//!
//! [`reldex_workspace_open`] opens `reldex-workspace`'s local SQLite-backed
//! `Store` and a `reldex-secrets` credential store together, on **one
//! service thread this crate spawns and owns** — `Store` is `Send`, not
//! `Sync`, and must live on exactly one thread for its whole life, so
//! settings, profiles, credentials, history, worksheets and layout all share
//! it rather than opening three competing threads against the same SQLite
//! file. [`ReldexWorkspace`] is submit-now/reply-later, exactly like a
//! session: a request function sends a command and returns immediately, and
//! the answer arrives as a [`ReldexWorkspaceReply`] drained with
//! [`reldex_workspace_next_reply`] after its own, independent waker fires —
//! it does not share the hub's event queue or waker. `crates/ffi/src/
//! workspace.rs` is also this crate's second composition root (alongside
//! `metadata.rs`/`splitter.rs`): it is the one place that names
//! `reldex-driver-oracle-thin` concretely, to map a profile's settings into
//! real connection parameters. A resolved or fetched password never crosses
//! as a plain string the adapter could copy and keep — see
//! [`ReldexSecret`]/[`reldex_secret_expose`]'s doc comment for the one,
//! deliberate exception to this crate's outbound-NUL-termination promise.
//!
//! # The header
//!
//! `include/reldex.h` is generated by `cbindgen` and **committed**: the adapter
//! builds against the checked-in copy, so nothing in the C++ build depends on
//! having `cbindgen` installed. Regenerate it with `crates/ffi/gen-header.sh`
//! and commit the result in the same change; `gen-header.sh --check` and
//! `tests/header.rs` both fail on a stale copy. `cbindgen` is a command-line
//! tool, not a build-dependency: the header does not change per build, and a
//! parser of our own source has no business running inside every build of the
//! most correctness-critical crate.
//!
//! # Features
//!
//! `mock-driver` (**on by default**) links `reldex-driver-mock`, the only
//! driver this crate can currently open a session against. It is how M1.3 and
//! spike S15 drive the boundary with no database.
//!
//! **A cargo feature never changes the ABI.** The exported symbols and the
//! generated header are identical either way, so one `reldex.h` describes every
//! build. Without the feature, [`reldex_hub_open_session`] reports
//! `RELDEX_STATUS_INVALID_ARGUMENT` — there is no driver to open — and
//! [`reldex_mock_release_block`] does nothing. That is the shape a shipping
//! build should have until a real driver feature replaces it (M1.8): the mock's
//! *behaviour* is compiled out, and the adapter is not built against a
//! different ABI than the one it was tested on.

// The workspace denies `unsafe_code` (root `Cargo.toml`) and ADR-0003 D2 makes
// this crate its single exception. Cargo rejects overriding `workspace.lints`
// in a manifest — the precedent and reasoning are recorded in
// `crates/mobile-link-check/Cargo.toml` — so the opt-out is this crate-level
// attribute, fenced by the three lints below, by `catch_unwind` on every
// `extern "C"` body, and by `crates/ffi/tests/fences.rs`, which asserts no
// other crate in the workspace gains the same allowance.
#![allow(
    unsafe_code,
    reason = "ADR-0003 D2: this crate is the workspace's single FFI boundary"
)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::missing_safety_doc)]
#![warn(clippy::undocumented_unsafe_blocks)]

mod batch;
mod counters;
mod error;
mod event;
mod format;
mod hub;
mod metadata;
mod mock;
mod session;
mod splitter;
mod status;
mod strings;
mod workspace;

pub use batch::{
    RELDEX_NUMBER_MAX_DIGITS, ReldexBatch, ReldexColumnInfo, ReldexColumnKind, ReldexColumnView,
    ReldexNullable, ReldexNumber, ReldexTimestamp, reldex_batch_column, reldex_batch_column_count,
    reldex_batch_column_fixed, reldex_batch_column_info, reldex_batch_release,
    reldex_batch_row_count,
};
pub use counters::{ReldexLiveCounts, reldex_live_counts};
pub use error::{
    ReldexError, ReldexErrorKind, ReldexErrorView, ReldexSessionState, reldex_error_free,
    reldex_error_view, reldex_last_error_clear, reldex_last_error_take,
};
pub use event::{
    ReldexCloseOutcome, ReldexCompletedOperation, ReldexEvent, ReldexEventKind,
    ReldexServerOutputLines, ReldexServerOutputMode, ReldexStatementKind,
    reldex_server_output_lines_count, reldex_server_output_lines_get,
    reldex_server_output_lines_release,
};
pub use format::{
    ReldexArenaView, ReldexBytesStyle, ReldexFormatOptions, ReldexTextArena, ReldexTimestampStyle,
    reldex_batch_format_column, reldex_text_arena_clear, reldex_text_arena_count,
    reldex_text_arena_create, reldex_text_arena_release, reldex_text_arena_view,
};
pub use hub::{
    ReldexHub, ReldexWakeFn, reldex_hub_create, reldex_hub_destroy, reldex_hub_list_sessions,
    reldex_hub_next_event, reldex_hub_pending_events, reldex_hub_session_count,
    reldex_hub_set_waker,
};
pub use metadata::{
    ReldexMetadataObjectKind, ReldexMetadataQuery, ReldexMetadataRequest,
    ReldexMetadataRequestKind, reldex_metadata_prepare, reldex_metadata_query_bind_count,
    reldex_metadata_query_column, reldex_metadata_query_column_count,
    reldex_metadata_query_reclassify_error, reldex_metadata_query_release,
    reldex_metadata_query_sql,
};
pub use mock::{
    ReldexDriverKind, ReldexMockScenario, ReldexMockScenarioConfig, ReldexMockStatement,
    reldex_mock_release_block, reldex_mock_statement,
};
pub use session::{
    ReldexCancelKind, ReldexCancelOutcome, ReldexCloseDisposition, ReldexOpenOptions,
    ReldexRequestId, ReldexResultId, ReldexSessionId, reldex_hub_open_session,
    reldex_session_close, reldex_session_close_result, reldex_session_commit,
    reldex_session_connect_warnings, reldex_session_execute, reldex_session_fetch,
    reldex_session_ping, reldex_session_request_cancel, reldex_session_result_column,
    reldex_session_result_column_count, reldex_session_rollback,
    reldex_session_rollback_to_savepoint, reldex_session_savepoint,
    reldex_session_set_server_output,
};
pub use splitter::{ReldexEndedBy, ReldexSplitKind, ReldexStatementSpan, reldex_split_statements};
pub use status::ReldexStatus;
pub use strings::{RELDEX_UTF16_OFFSET_INVALID, ReldexStr, reldex_utf16_offset};
pub use workspace::{
    ReldexAuthKind, ReldexConnectSummary, ReldexConnectSummaryView, ReldexCredentialError,
    ReldexCredentialStoreKind, ReldexDatabaseType, ReldexEndpointKind, ReldexEnvironmentKind,
    ReldexHistoryList, ReldexHistoryOutcomeKind, ReldexHistoryRecordView, ReldexLayout,
    ReldexPasswordSourceKind, ReldexPasswordStorageKind, ReldexProfileDetails, ReldexProfileList,
    ReldexProfileView, ReldexPromptReasonKind, ReldexSecret, ReldexServiceTargetKind,
    ReldexSessionRoleKind, ReldexSettingError, ReldexSettingId, ReldexSettingLevel,
    ReldexSettingValue, ReldexTransportKind, ReldexValueKind, ReldexWorksheetList,
    ReldexWorksheetView, ReldexWorkspace, ReldexWorkspaceReply, ReldexWorkspaceReplyKind,
    ReldexWorkspaceWakeFn, reldex_connect_summary_release, reldex_connect_summary_view,
    reldex_credential_store_kind_can_store, reldex_history_list_count, reldex_history_list_get,
    reldex_history_list_release, reldex_profile_list_count, reldex_profile_list_get,
    reldex_profile_list_release, reldex_secret_expose, reldex_secret_from_utf8,
    reldex_secret_release, reldex_worksheet_list_count, reldex_worksheet_list_get,
    reldex_worksheet_list_release, reldex_workspace_build_connect_params,
    reldex_workspace_clear_history, reldex_workspace_clear_setting, reldex_workspace_close,
    reldex_workspace_create_profile, reldex_workspace_credential_delete,
    reldex_workspace_credential_get, reldex_workspace_credential_put,
    reldex_workspace_credential_store_kind, reldex_workspace_delete_profile,
    reldex_workspace_delete_worksheet, reldex_workspace_get_profile, reldex_workspace_list_history,
    reldex_workspace_list_profiles, reldex_workspace_load_layout, reldex_workspace_load_worksheets,
    reldex_workspace_new_worksheet_id, reldex_workspace_next_reply, reldex_workspace_open,
    reldex_workspace_pending_replies, reldex_workspace_record_history,
    reldex_workspace_resolve_password, reldex_workspace_resolve_setting,
    reldex_workspace_save_layout, reldex_workspace_save_worksheet, reldex_workspace_set_setting,
    reldex_workspace_set_waker, reldex_workspace_update_profile,
};

/// Major part of the ABI version reported by [`reldex_abi_version`].
///
/// Bumped when an existing symbol's meaning, an existing struct field's
/// meaning or **layout**, or an existing enum value changes. The adapter
/// refuses to start on a mismatch (ADR-0003 D7).
///
/// `3` because the first two consumers — the Qt adapter (M1.6) and the C smoke
/// harness (M1.4) — found the boundary charging for work neither wanted:
/// `reldex_batch_column` no longer builds the `NUMBER`/`TIMESTAMP` mirror, so
/// `fixed` is now NULL there and the new `reldex_batch_column_fixed` is the
/// only way to get one (ADR-0003 amendment A19). Existing code compiles and
/// reads NULL, which is exactly the kind of silent change a major bump exists
/// for. Versions 1 and 2 were never accepted and never shipped — ADR-0003 is
/// still Proposed — so the number moves rather than pretending a recompiled
/// adapter would still be compatible.
pub const RELDEX_ABI_VERSION_MAJOR: u32 = 3;

/// Minor part of the ABI version reported by [`reldex_abi_version`].
///
/// Bumped when a symbol, a *trailing* struct field, or an enum value is added.
/// An older adapter keeps working: every non-opaque struct starts with
/// `struct_size`, and every enum reserves `0` for "a value this header
/// predates".
///
/// `1` because M2.11 adds the events/registry/server-output-control,
/// statement-splitting, metadata, and settings/profiles/credentials/history/
/// worksheets/layout families purely additively — new symbols, new opaque
/// types, new enum values, and new *trailing* fields on existing structs
/// (`ReldexEvent`, `ReldexLiveCounts`), each guarded by the `struct_size`
/// prefix rule so an adapter built against `3.0` still links and runs against
/// this build. The brief for M2.11 asked for a major bump (`3` to `4`); this
/// is recorded as a deliberate deviation, not an oversight — see the PR
/// description's "deviations" section and ADR-0003's M2.11 amendment.
pub const RELDEX_ABI_VERSION_MINOR: u32 = 1;

/// The ABI version this library implements: `(major << 16) | minor`.
///
/// Compare against `RELDEX_ABI_VERSION` in `reldex.h`. Its real value is
/// catching a stale header in a developer's build, not distribution: core and
/// UI ship in one binary (ADR-0003 D7).
#[unsafe(no_mangle)]
pub extern "C" fn reldex_abi_version() -> u32 {
    // Through the same wrapper as every other export, even though nothing here
    // can panic or re-enter: "every `extern "C"` body is wrapped" is only a
    // useful claim if it has no exceptions for a reviewer to check.
    status::entry_value(0, || {
        (RELDEX_ABI_VERSION_MAJOR << 16) | RELDEX_ABI_VERSION_MINOR
    })
}

#[cfg(test)]
mod tests {
    use super::{RELDEX_ABI_VERSION_MAJOR, RELDEX_ABI_VERSION_MINOR, reldex_abi_version};

    #[test]
    fn the_abi_version_packs_major_and_minor() {
        let version = reldex_abi_version();
        assert_eq!(version >> 16, RELDEX_ABI_VERSION_MAJOR);
        assert_eq!(version & 0xFFFF, RELDEX_ABI_VERSION_MINOR);
    }
}
