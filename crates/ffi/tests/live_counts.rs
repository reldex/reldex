//! Leak accounting: everything this library hands out must come back.
//!
//! Spike criterion K5 is written in terms of ASan, which is not available on
//! the development machine — and the failure most likely to actually happen at
//! this boundary is not a use-after-free anyway. It is a `ReldexBatch*` or a
//! `ReldexError*` that nobody released, or a session this crate never let go
//! of: invisible to every other test until memory runs out.
//! [`reldex_live_counts`] turns that into an assertion, and these are the
//! paths worth asserting it on — including, since M2.15, on a hub that stays
//! alive: a session stops counting when its `TERMINAL` is drained, however it
//! ended, not when the hub goes away.
//!
//! The counters are process-wide, so the tests in this binary take a lock and
//! run one at a time rather than measuring each other. Nothing here asserts a
//! duration: anything that finishes on another thread is observed with the
//! deadline helper, which fails only when something is genuinely stuck.

#![allow(
    unsafe_code,
    reason = "these tests drive the crate's own C ABI, which is what they exist to prove"
)]

mod support;

use std::sync::{Mutex, MutexGuard};

use reldex_ffi::{
    ReldexAuthKind, ReldexCloseDisposition, ReldexDatabaseType, ReldexEndpointKind,
    ReldexEnvironmentKind, ReldexEventKind, ReldexLiveCounts, ReldexMockScenarioConfig,
    ReldexMockStatement, ReldexPasswordStorageKind, ReldexProfileDetails, ReldexServiceTargetKind,
    ReldexSessionRoleKind, ReldexStatus, ReldexStr, ReldexTransportKind, ReldexWorkspaceReply,
    reldex_hub_pending_events, reldex_live_counts, reldex_workspace_close,
    reldex_workspace_create_profile, reldex_workspace_credential_put, reldex_workspace_next_reply,
    reldex_workspace_open, reldex_workspace_pending_replies, reldex_workspace_resolve_password,
};

use support::{Harness, take_error, wait_until};

/// Serialises the tests in this binary; the counters belong to the process.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn exclusively() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn counts() -> ReldexLiveCounts {
    let mut counts = ReldexLiveCounts::default();
    // SAFETY: `counts` is a real local with `struct_size` set.
    let status = unsafe { reldex_live_counts(std::ptr::from_mut(&mut counts)) };
    assert_eq!(status, ReldexStatus::Ok);
    counts
}

/// Clears this thread's last-error slot, which holds a live `ReldexError`.
fn forget_last_error() {
    support::free_error(reldex_ffi::reldex_last_error_take());
}

/// Waits until every count is back to `baseline`, then asserts it.
///
/// Everything this crate counts is released before `reldex_hub_destroy`
/// returns, but waiting is still the honest shape for a check that runs after
/// other threads have been involved, and the deadline makes a genuine leak
/// fail rather than hang.
fn settles_back_to(baseline: ReldexLiveCounts, what: &str) {
    wait_until(what, || counts() == baseline);
    assert_eq!(counts(), baseline, "{what}");
}

fn config() -> ReldexMockScenarioConfig {
    ReldexMockScenarioConfig {
        rows: 64,
        seed: 5,
        ..ReldexMockScenarioConfig::default()
    }
}

#[test]
fn a_whole_lifecycle_returns_every_count_to_its_baseline() {
    let _guard = exclusively();
    forget_last_error();
    let baseline = counts();

    {
        let harness = Harness::new();
        assert_eq!(counts().hubs, baseline.hubs + 1, "the hub is counted");

        let session = harness.open(config());
        assert_eq!(counts().sessions, baseline.sessions + 1);

        assert_eq!(
            harness.execute(session, 10, ReldexMockStatement::GeneratedQuery),
            ReldexStatus::Ok
        );
        let executed = harness.next_event();
        assert!(executed.error.is_null());
        let result = executed.result;

        assert_eq!(harness.fetch(session, 20, result, 64), ReldexStatus::Ok);
        let fetched = harness.next_event();
        assert_eq!(fetched.kind, ReldexEventKind::Fetched as i32);
        assert!(!fetched.batch.is_null());
        assert_eq!(
            counts().batches,
            baseline.batches + 1,
            "the caller now owns one batch"
        );
        support::release_batch(&fetched);
        assert_eq!(counts().batches, baseline.batches, "and has given it back");

        assert_eq!(harness.close_result(session, 30, result), ReldexStatus::Ok);
        let closed = harness.next_event();
        assert_eq!(closed.kind, ReldexEventKind::ResultClosed as i32);

        assert_eq!(
            harness.close(session, 40, ReldexCloseDisposition::None),
            ReldexStatus::Ok
        );
        let ended = harness.next_event();
        assert_eq!(ended.kind, ReldexEventKind::SessionClosed as i32);
        assert!(ended.error.is_null(), "a SELECT opens no transaction");
    }

    settles_back_to(baseline, "every count to return to its baseline");
}

#[test]
fn destroying_a_hub_with_undrained_events_frees_what_they_hold() {
    let _guard = exclusively();
    forget_last_error();
    let baseline = counts();

    const FETCHES: u64 = 4;
    {
        let harness = Harness::new();
        let session = harness.open(config());
        assert_eq!(
            harness.execute(session, 10, ReldexMockStatement::GeneratedQuery),
            ReldexStatus::Ok
        );
        let executed = harness.next_event();
        let result = executed.result;

        for request in 0..FETCHES {
            assert_eq!(
                harness.fetch(session, 20 + request, result, 16),
                ReldexStatus::Ok
            );
        }
        // Deliberately drain nothing. The rows exist, queued inside
        // `db-core`'s events, and are this library's to free.
        wait_until("every fetch to be answered", || {
            // SAFETY: the hub is live.
            let pending = unsafe { reldex_hub_pending_events(harness.hub()) };
            pending >= FETCHES as usize
        });
        // A `ReldexBatch` is made — and counted — when its event is drained,
        // so what an undrained event holds is `db-core`'s, not a handed-out
        // object; the hub's destroy below drops the queue that holds it.
        assert_eq!(counts().batches, baseline.batches);
        assert_eq!(counts().sessions, baseline.sessions + 1);
    }

    settles_back_to(
        baseline,
        "an undrained queue to be freed with the hub that owns it",
    );
}

#[test]
fn a_contained_driver_panic_leaks_nothing() {
    let _guard = exclusively();
    forget_last_error();
    let baseline = counts();

    {
        let harness = Harness::new();
        let session = harness.open(config());
        // The panicking request, and one queued behind it that must be
        // answered as well.
        assert_eq!(
            harness.execute(session, 10, ReldexMockStatement::Panicking),
            ReldexStatus::Ok
        );
        assert_eq!(
            harness.execute(session, 11, ReldexMockStatement::GeneratedQuery),
            ReldexStatus::Ok
        );

        let first = harness.next_event();
        assert_eq!(first.request, 10);
        assert!(
            take_error(&first).is_some(),
            "the contained panic is reported as a failure"
        );
        // Request 11 and the session's `TERMINAL`, in whichever order the
        // worker reached them.
        let mut answered = false;
        let mut terminal = false;
        while !(answered && terminal) {
            let event = harness.next_event();
            assert!(
                take_error(&event).is_some(),
                "everything after the loss reports why"
            );
            if event.kind == ReldexEventKind::Terminal as i32 {
                terminal = true;
            } else {
                assert_eq!(event.request, 11);
                answered = true;
            }
        }
        assert_eq!(
            counts().errors,
            baseline.errors,
            "every error object was freed"
        );
        assert_eq!(
            counts().sessions,
            baseline.sessions,
            "the lost session stopped counting once its TERMINAL was drained"
        );
    }

    settles_back_to(
        baseline,
        "the session lost to a driver panic to release everything",
    );
}

/// Opens a session on `harness` and returns its id, asserting the counts
/// moved by exactly one session.
fn open_counted(
    harness: &Harness,
    baseline: ReldexLiveCounts,
    config: ReldexMockScenarioConfig,
) -> u64 {
    let session = harness.open(config);
    assert_eq!(counts().sessions, baseline.sessions + 1);
    assert_eq!(harness.session_count(), 1);
    session
}

/// Asserts `session` no longer counts anywhere: not in the process-wide
/// counts, not in the hub's own count or list, and not as a known id.
fn assert_retired(harness: &Harness, baseline: ReldexLiveCounts, session: u64, how: &str) {
    assert_eq!(counts().sessions, baseline.sessions, "{how}: live counts");
    assert_eq!(
        harness.session_count(),
        0,
        "{how}: reldex_hub_session_count"
    );
    // SAFETY: the hub is live; a null `out` asks only for the count.
    let listed =
        unsafe { reldex_ffi::reldex_hub_list_sessions(harness.hub(), std::ptr::null_mut(), 0) };
    assert_eq!(listed, 0, "{how}: reldex_hub_list_sessions");
    assert_eq!(
        harness.execute(session, 999, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::NotFound,
        "{how}: the retired id is not found"
    );
    forget_last_error();
}

#[test]
fn a_closed_session_stops_counting_once_its_terminal_is_drained() {
    // The bug M2.15 fixes: `reldex_hub_session_count()` and
    // `reldex_live_counts().sessions` only ever went down when the hub was
    // destroyed, so a long-lived hub counted every session it had ever
    // opened.
    let _guard = exclusively();
    forget_last_error();
    let baseline = counts();

    let harness = Harness::new();
    let session = open_counted(&harness, baseline, config());
    assert_eq!(
        harness.close(session, 2, ReldexCloseDisposition::None),
        ReldexStatus::Ok
    );
    let closed = harness.next_event();
    assert_eq!(closed.kind, ReldexEventKind::SessionClosed as i32);
    assert!(closed.error.is_null());
    // Still counted: per-session state ends on `TERMINAL`, never before.
    assert_eq!(counts().sessions, baseline.sessions + 1);
    let terminal = harness.next_event();
    assert_eq!(terminal.kind, ReldexEventKind::Terminal as i32);
    assert_retired(&harness, baseline, session, "a clean close");
}

#[test]
fn a_lost_session_stops_counting_once_its_terminal_is_drained() {
    let _guard = exclusively();
    forget_last_error();
    let baseline = counts();

    let harness = Harness::new();
    let session = open_counted(&harness, baseline, config());
    assert_eq!(
        harness.execute(session, 2, ReldexMockStatement::LoseSession),
        ReldexStatus::Ok
    );
    let failed = harness.next_event();
    assert_eq!(failed.request, 2);
    take_error(&failed).expect("the loss explains itself");
    let terminal = harness.next_event();
    assert_eq!(terminal.kind, ReldexEventKind::Terminal as i32);
    take_error(&terminal).expect("a lost session's TERMINAL carries the cause");
    assert_retired(&harness, baseline, session, "a session lost mid-statement");
}

#[test]
fn a_failed_open_stops_counting_once_its_terminal_is_drained() {
    let _guard = exclusively();
    forget_last_error();
    let baseline = counts();

    let harness = Harness::new();
    let (session, opened) = harness.open_raw(
        ReldexMockScenarioConfig {
            connect_failure: reldex_ffi::ReldexMockFailure::Authentication as i32,
            ..config()
        },
        1,
    );
    assert_eq!(opened.kind, ReldexEventKind::Opened as i32);
    take_error(&opened).expect("the failed connect explains itself");
    assert_eq!(counts().sessions, baseline.sessions + 1);
    let terminal = harness.next_event();
    assert_eq!(terminal.kind, ReldexEventKind::Terminal as i32);
    take_error(&terminal).expect("a failed open's TERMINAL carries the cause");
    assert_retired(&harness, baseline, session, "a failed open");
    drop(harness);
    settles_back_to(baseline, "a failed open to release everything");
}

/// A workspace profile shaped enough for `reldex_workspace_credential_put`/
/// `reldex_workspace_resolve_password` to accept it -- the fields the
/// credential path itself reads, not a realistic connection.
fn workspace_profile_details() -> ReldexProfileDetails {
    ReldexProfileDetails {
        struct_size: u32::try_from(size_of::<ReldexProfileDetails>()).expect("fits"),
        name: reldex_str("undrained-reply leak check"),
        database_type: ReldexDatabaseType::Oracle as i32,
        environment: ReldexEnvironmentKind::Test as i32,
        environment_label: ReldexStr::empty(),
        treat_as_production: false,
        endpoint_kind: ReldexEndpointKind::HostPort as i32,
        host: reldex_str("db.example.invalid"),
        port: 1521,
        service_target_kind: ReldexServiceTargetKind::ServiceName as i32,
        service_name_or_sid: reldex_str("ORCL"),
        connect_string: ReldexStr::empty(),
        auth_kind: ReldexAuthKind::Password as i32,
        username: reldex_str("app_owner"),
        password_storage: ReldexPasswordStorageKind::CredentialStore as i32,
        role: ReldexSessionRoleKind::Normal as i32,
        transport: ReldexTransportKind::Plain as i32,
        ca_directory: ReldexStr::empty(),
        allow_unenforced_certificate_pin: false,
    }
}

fn reldex_str(text: &'static str) -> ReldexStr {
    ReldexStr {
        ptr: text.as_ptr(),
        len: text.len(),
    }
}

/// Waits for `request`'s reply and returns it, draining nothing else (every
/// call site below has exactly one request outstanding at a time).
fn drain_workspace_reply(
    workspace: *mut reldex_ffi::ReldexWorkspace,
    request: u64,
) -> ReldexWorkspaceReply {
    let found = std::cell::RefCell::new(None);
    wait_until(
        &format!("the workspace to answer request {request}"),
        || {
            // SAFETY: `workspace` is live for the duration of this test; `out` is
            // a real local, zeroed (a valid all-zero bit pattern for this flat
            // `#[repr(C)]` struct of integers, bools and raw pointers) with
            // `struct_size` set.
            let mut out: ReldexWorkspaceReply = unsafe { std::mem::zeroed() };
            out.struct_size = u32::try_from(size_of::<ReldexWorkspaceReply>()).expect("fits");
            let taken =
                unsafe { reldex_workspace_next_reply(workspace, std::ptr::from_mut(&mut out)) };
            if taken && out.request == request {
                *found.borrow_mut() = Some(out);
                true
            } else {
                taken && out.request != request
            }
        },
    );
    found.into_inner().expect("the awaited reply was captured")
}

#[test]
fn closing_a_workspace_with_an_undrained_reply_frees_what_it_holds() {
    // Should-fix #8 (M2.11 review round 2): `reldex_workspace_close`'s doc
    // comment used to say an undrained reply "must still be drained and
    // released or it leaks". That was wrong -- closing frees the reply
    // queue, and every reply in it, including a `ReldexSecret` nobody ever
    // called `reldex_secret_release` on. This proves it with the same
    // instrument the review used: the process-wide live-object count.
    let _guard = exclusively();
    forget_last_error();
    let baseline = counts();

    {
        let mut handle: *mut reldex_ffi::ReldexWorkspace = std::ptr::null_mut();
        // SAFETY: `handle` is a real local out-pointer; an in-memory store
        // with an in-memory credential store needs no filesystem or OS
        // credential-manager access.
        let status = unsafe {
            reldex_workspace_open(
                ReldexStr::empty(),
                true,
                true,
                1,
                std::ptr::from_mut(&mut handle),
            )
        };
        assert_eq!(status, ReldexStatus::Ok);
        assert!(!handle.is_null());
        let opened = drain_workspace_reply(handle, 1);
        assert!(opened.error.is_null(), "the memory workspace must open");

        let details = workspace_profile_details();
        // SAFETY: `handle` is live; `details` is a real local.
        let status =
            unsafe { reldex_workspace_create_profile(handle, 2, std::ptr::from_ref(&details)) };
        assert_eq!(status, ReldexStatus::Ok);
        let created = drain_workspace_reply(handle, 2);
        assert!(created.error.is_null(), "profile creation must succeed");
        let id = created.id;

        // SAFETY: `handle` is live; `id` is a real local array.
        let status = unsafe {
            reldex_workspace_credential_put(handle, 3, id.as_ptr(), reldex_str("leak-check-pw"))
        };
        assert_eq!(status, ReldexStatus::Ok);
        let put = drain_workspace_reply(handle, 3);
        assert!(put.error.is_null(), "storing the password must succeed");

        // Submitted, and deliberately never drained: the reply -- carrying a
        // live `ReldexSecret` once the service thread answers it -- sits in
        // the queue when this workspace is closed below.
        // SAFETY: as above.
        let status = unsafe { reldex_workspace_resolve_password(handle, 4, id.as_ptr()) };
        assert_eq!(status, ReldexStatus::Ok);
        wait_until("the undrained resolve_password reply to be queued", || {
            // SAFETY: `handle` is live.
            (unsafe { reldex_workspace_pending_replies(handle) }) >= 1
        });
        assert!(
            counts().misc_objects > baseline.misc_objects,
            "the workspace, and now a secret sitting in its queue, are live"
        );

        // SAFETY: `handle` is a live workspace this test alone owns; closing
        // it with an undrained reply still queued is exactly the case under
        // test, not a use-after-free (`reldex_workspace_next_reply` is never
        // called on `handle` again after this).
        unsafe { reldex_workspace_close(handle) };
    }

    settles_back_to(
        baseline,
        "closing the workspace to free the undrained reply and the secret inside it",
    );
}
