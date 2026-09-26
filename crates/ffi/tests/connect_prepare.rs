//! M3.3 (ABI 3.3): preparing a connect on the workspace's service thread, and
//! handing what it prepared to `reldex_hub_open_session` as an Oracle session.
//!
//! Everything here runs with no database: the workspace is in memory with the
//! memory credential store, and the only Oracle sessions opened aim at a port
//! on the loopback interface that nothing listens on, so they fail to connect
//! the way an unreachable server does. The same paths against the real test
//! database are `m3_3_connect_live.rs`'s (feature `oracle-it`).
//!
//! No assertion here is about speed; waits are bounded only by the support
//! module's hang guard.

#![allow(
    unsafe_code,
    reason = "these tests drive the crate's own C ABI, which is what they exist to prove"
)]

mod support;

use std::mem::offset_of;

use reldex_ffi::{
    RELDEX_ABI_VERSION_MINOR, ReldexAbandonOutcome, ReldexAuthKind, ReldexCredentialStoreKind,
    ReldexDriverKind, ReldexErrorKind, ReldexEventKind, ReldexOpenOptions,
    ReldexPasswordSourceKind, ReldexPasswordStorageKind, ReldexPromptReasonKind, ReldexStatus,
    ReldexWorkspaceReplyKind, reldex_abi_version, reldex_connect_summary_release,
    reldex_hub_open_session, reldex_last_error_take, reldex_secret_expose,
    reldex_secret_from_utf8, reldex_secret_release, reldex_workspace_credential_store_kind,
};

use support::workspace::{TestWorkspace, profile_details, str_of, summary_view};
use support::{Harness, take_error, take_error_pointer};

/// A loopback port nothing listens on: a connect there is refused at once.
const REFUSED_PORT: u16 = 1;

#[test]
fn the_abi_3_3_values_are_pinned() {
    assert_eq!(RELDEX_ABI_VERSION_MINOR, 3);
    assert_eq!(reldex_abi_version() & 0xffff, 3);
    assert_eq!(ReldexDriverKind::Mock as i32, 1);
    assert_eq!(ReldexDriverKind::Oracle as i32, 2);
    assert_eq!(ReldexWorkspaceReplyKind::ConnectPrepared as i32, 22);
    assert_eq!(ReldexPasswordSourceKind::Supplied as i32, 4);
    // `connect` is the trailing field, so a 3.2-sized struct stops right
    // before it and still reads as a complete 3.2 struct.
    assert_eq!(
        offset_of!(ReldexOpenOptions, connect) + size_of::<*const u8>(),
        size_of::<ReldexOpenOptions>()
    );
}

#[test]
fn a_prompt_each_time_profile_asks_and_hands_out_nothing() {
    let workspace = TestWorkspace::open();
    let mut details = profile_details("db.example.invalid", 1521, "ORCL", "app_owner");
    details.password_storage = ReldexPasswordStorageKind::PromptEachTime as i32;
    let id = workspace.create_profile(&details);

    let reply = workspace.prepare_connect(&id, std::ptr::null());
    assert_eq!(reply.kind, ReldexWorkspaceReplyKind::ConnectPrepared as i32);
    assert!(reply.error.is_null());
    assert_eq!(reply.id, id, "the reply names the profile it prepared");
    assert_eq!(
        reply.password_source_kind,
        ReldexPasswordSourceKind::PromptRequired as i32
    );
    assert_eq!(
        reply.prompt_reason,
        ReldexPromptReasonKind::PromptEachTime as i32
    );
    assert!(reply.connect.is_null(), "nothing to open with yet");
    assert!(reply.secret.is_null(), "a password never crosses out");
}

#[test]
fn a_profile_with_no_stored_password_asks_and_says_why() {
    let workspace = TestWorkspace::open();
    let id = workspace.create_profile(&profile_details(
        "db.example.invalid",
        1521,
        "ORCL",
        "app_owner",
    ));

    let reply = workspace.prepare_connect(&id, std::ptr::null());
    assert!(reply.error.is_null());
    assert_eq!(
        reply.password_source_kind,
        ReldexPasswordSourceKind::PromptRequired as i32
    );
    assert_eq!(
        reply.prompt_reason,
        ReldexPromptReasonKind::NotStored as i32
    );
    assert!(reply.connect.is_null());
}

/// ADR-0007 S3, Consequences: with no credential store (every platform but
/// Windows today), every connect of a stored-password profile prompts, and
/// says that is why. Runs where there is no store; on Windows the platform
/// store is the real Credential Manager, which no test here may touch.
#[cfg_attr(not(windows), test)]
#[cfg_attr(
    windows,
    expect(dead_code, reason = "runs only where the platform has no credential store")
)]
fn with_no_credential_store_every_connect_prompts() {
    let workspace = TestWorkspace::open_with_platform_store();
    // SAFETY: the handle is live; the open reply was taken, so the kind is set.
    let kind = unsafe { reldex_workspace_credential_store_kind(workspace.handle()) };
    assert_eq!(kind, ReldexCredentialStoreKind::Absent as i32);
    let id = workspace.create_profile(&profile_details(
        "db.example.invalid",
        1521,
        "ORCL",
        "app_owner",
    ));

    for _ in 0..2 {
        let reply = workspace.prepare_connect(&id, std::ptr::null());
        assert!(reply.error.is_null());
        assert_eq!(
            reply.password_source_kind,
            ReldexPasswordSourceKind::PromptRequired as i32
        );
        assert_eq!(
            reply.prompt_reason,
            ReldexPromptReasonKind::StoreUnavailable as i32
        );
        assert!(reply.connect.is_null());
        assert!(reply.secret.is_null());
    }
}

#[test]
fn a_stored_password_travels_only_inside_the_summary() {
    let workspace = TestWorkspace::open();
    let id = workspace.create_profile(&profile_details(
        "db.example.invalid",
        1521,
        "ORCL",
        "app_owner",
    ));
    workspace.put_password(&id, "stored-marker-51c7");

    let reply = workspace.prepare_connect(&id, std::ptr::null());
    assert!(reply.error.is_null());
    assert_eq!(
        reply.password_source_kind,
        ReldexPasswordSourceKind::FromStore as i32
    );
    assert!(
        reply.secret.is_null(),
        "the stored password is not handed out"
    );
    assert!(!reply.connect.is_null());
    let view = summary_view(reply.connect);
    // SAFETY: the view borrows from `reply.connect`, still live.
    assert_eq!(unsafe { view.host.as_str() }, Some("db.example.invalid"));
    assert_eq!(view.port, 1521);
    // SAFETY: owned by this test, released once.
    unsafe { reldex_connect_summary_release(reply.connect) };
}

#[test]
fn a_typed_password_wins_over_the_store_and_is_not_consumed() {
    let workspace = TestWorkspace::open();
    let id = workspace.create_profile(&profile_details(
        "db.example.invalid",
        1521,
        "ORCL",
        "app_owner",
    ));
    workspace.put_password(&id, "stored-marker-51c7");
    let typed_text = "typed-marker-8e02";
    // SAFETY: `typed_text` is valid UTF-8, alive for the call.
    let typed = unsafe { reldex_secret_from_utf8(str_of(typed_text)) };
    assert!(!typed.is_null());

    let reply = workspace.prepare_connect(&id, typed);
    assert!(reply.error.is_null());
    assert_eq!(
        reply.password_source_kind,
        ReldexPasswordSourceKind::Supplied as i32
    );
    assert!(!reply.connect.is_null());

    // Still the caller's: it is saved only after the connect succeeds.
    // SAFETY: `typed` is live and owned by this test.
    let exposed = unsafe { reldex_secret_expose(typed) };
    // SAFETY: `exposed` borrows from `typed`, still live.
    let bytes = unsafe { std::slice::from_raw_parts(exposed.ptr, exposed.len) };
    assert_eq!(bytes, typed_text.as_bytes());
    // SAFETY: both owned by this test, released once.
    unsafe {
        reldex_secret_release(typed);
        reldex_connect_summary_release(reply.connect);
    }
}

#[test]
fn external_authentication_needs_no_password() {
    let workspace = TestWorkspace::open();
    let mut details = profile_details("db.example.invalid", 1521, "ORCL", "app_owner");
    details.auth_kind = ReldexAuthKind::External as i32;
    let id = workspace.create_profile(&details);

    let reply = workspace.prepare_connect(&id, std::ptr::null());
    assert!(reply.error.is_null());
    assert_eq!(
        reply.password_source_kind,
        ReldexPasswordSourceKind::NotNeeded as i32
    );
    assert!(!reply.connect.is_null());
    // SAFETY: owned by this test, released once.
    unsafe { reldex_connect_summary_release(reply.connect) };
}

#[test]
fn the_profile_connect_limit_reaches_the_summary() {
    let workspace = TestWorkspace::open();
    let mut details = profile_details("db.example.invalid", 1521, "ORCL", "app_owner");
    details.auth_kind = ReldexAuthKind::External as i32;
    let id = workspace.create_profile(&details);

    let reply = workspace.prepare_connect(&id, std::ptr::null());
    let view = summary_view(reply.connect);
    assert!(view.has_connect_timeout);
    assert_eq!(view.connect_timeout_seconds, 15, "the built-in default");
    // SAFETY: owned by this test, released once.
    unsafe { reldex_connect_summary_release(reply.connect) };

    workspace.set_profile_connect_timeout(&id, 3);
    let reply = workspace.prepare_connect(&id, std::ptr::null());
    let view = summary_view(reply.connect);
    assert!(view.has_connect_timeout);
    assert_eq!(view.connect_timeout_seconds, 3, "the profile's own limit");
    assert!(!view.connect_without_limit);
    // SAFETY: owned by this test, released once.
    unsafe { reldex_connect_summary_release(reply.connect) };
}

#[test]
fn an_unknown_profile_is_an_error_not_a_prompt() {
    let workspace = TestWorkspace::open();
    let unknown = [0x42_u8; 16];
    let reply = workspace.prepare_connect(&unknown, std::ptr::null());
    assert_eq!(reply.kind, ReldexWorkspaceReplyKind::ConnectPrepared as i32);
    assert!(reply.connect.is_null());
    let error = take_error_pointer(reply.error).expect("a missing profile explains itself");
    assert!(
        error.message.contains("prepare_connect"),
        "{}",
        error.message
    );
}

#[test]
fn an_oracle_session_needs_the_prepared_parameters() {
    let harness = Harness::new();
    let options = ReldexOpenOptions {
        driver: ReldexDriverKind::Oracle as i32,
        ..ReldexOpenOptions::default()
    };
    let mut session = 0_u64;
    // SAFETY: the hub is live; `options` and `session` are real locals.
    let status = unsafe {
        reldex_hub_open_session(
            harness.hub(),
            std::ptr::from_ref(&options),
            1,
            std::ptr::from_mut(&mut session),
        )
    };
    assert_eq!(status, ReldexStatus::InvalidArgument);
    assert_eq!(session, 0, "no session was opened");
    assert_eq!(harness.session_count(), 0);
    let error = take_error_pointer(reldex_last_error_take()).expect("the refusal explains itself");
    assert_eq!(error.kind, ReldexErrorKind::Configuration as i32);
    assert!(error.message.contains("connect"), "{}", error.message);
}

#[test]
fn a_3_2_sized_open_options_still_opens_the_mock() {
    let harness = Harness::new();
    let options = ReldexOpenOptions {
        struct_size: u32::try_from(offset_of!(ReldexOpenOptions, connect)).expect("fits"),
        ..ReldexOpenOptions::default()
    };
    let mut session = 0_u64;
    // SAFETY: the hub is live; `options` is a real local whose declared size
    // is smaller than the struct, which is what an ABI 3.2 caller passes.
    let status = unsafe {
        reldex_hub_open_session(
            harness.hub(),
            std::ptr::from_ref(&options),
            1,
            std::ptr::from_mut(&mut session),
        )
    };
    assert_eq!(status, ReldexStatus::Ok);
    let opened = harness.next_event();
    assert_eq!(opened.kind, ReldexEventKind::Opened as i32);
    assert!(opened.error.is_null(), "the mock opens as it did in 3.2");
    let (status, outcome, _) = harness.abandon(session);
    assert_eq!(status, ReldexStatus::Ok);
    assert_eq!(outcome, ReldexAbandonOutcome::Open as i32);
    let terminal = harness.next_event();
    assert_eq!(terminal.kind, ReldexEventKind::Terminal as i32);
    assert!(terminal.abandoned);
}

/// Prepares a connect to a loopback port nothing listens on and opens it as
/// an Oracle session (request 7) on `harness`; returns the session id.
fn open_refused_oracle_session(harness: &Harness) -> u64 {
    let workspace = TestWorkspace::open();
    let mut details = profile_details("127.0.0.1", REFUSED_PORT, "RELDEX", "reldex_test");
    details.password_storage = ReldexPasswordStorageKind::PromptEachTime as i32;
    let id = workspace.create_profile(&details);
    workspace.set_profile_connect_timeout(&id, 5);
    // SAFETY: the text is valid UTF-8, alive for the call.
    let typed = unsafe { reldex_secret_from_utf8(str_of("not-a-real-password-3d1f")) };
    let reply = workspace.prepare_connect(&id, typed);
    // SAFETY: owned by this test, released once; the summary holds its own
    // copy.
    unsafe { reldex_secret_release(typed) };
    assert!(reply.error.is_null());
    assert!(!reply.connect.is_null());

    let options = ReldexOpenOptions {
        driver: ReldexDriverKind::Oracle as i32,
        connect: reply.connect,
        ..ReldexOpenOptions::default()
    };
    let mut session = 0_u64;
    // SAFETY: the hub is live; `options` borrows a live summary for the call.
    let status = unsafe {
        reldex_hub_open_session(
            harness.hub(),
            std::ptr::from_ref(&options),
            7,
            std::ptr::from_mut(&mut session),
        )
    };
    // Borrowed for the call only: the session keeps its own copy.
    // SAFETY: owned by this test, released once, after the call returned.
    unsafe { reldex_connect_summary_release(reply.connect) };
    assert_eq!(status, ReldexStatus::Ok);
    assert_ne!(session, 0);
    session
}

#[test]
fn an_unreachable_oracle_endpoint_fails_its_open_and_the_session_ends() {
    let harness = Harness::new();
    let session = open_refused_oracle_session(&harness);

    let opened = harness.next_event();
    assert_eq!(opened.kind, ReldexEventKind::Opened as i32);
    assert_eq!(opened.request, 7);
    assert_eq!(opened.session, session);
    let error = take_error(&opened).expect("a refused connect explains itself");
    assert_eq!(
        error.kind,
        ReldexErrorKind::Connection as i32,
        "{}",
        error.message
    );
    let terminal = harness.next_event();
    assert_eq!(terminal.kind, ReldexEventKind::Terminal as i32);
    assert!(!terminal.abandoned);
    take_error(&terminal);
    assert_eq!(harness.session_count(), 0, "the failed session is retired");
}

#[test]
fn an_abandoned_oracle_connect_ends_at_once_and_nothing_is_adopted() {
    let harness = Harness::new();
    let session = open_refused_oracle_session(&harness);

    // Either the refusal already arrived (`ALREADY_ENDED`) or the connect is
    // still in flight (`CONNECTING`); both must end the same clean way.
    let (status, outcome, lost) = harness.abandon(session);
    assert_eq!(status, ReldexStatus::Ok);
    assert!(!lost, "nothing opened, so no transaction to lose");
    assert!(
        outcome == ReldexAbandonOutcome::Connecting as i32
            || outcome == ReldexAbandonOutcome::AlreadyEnded as i32,
        "unexpected abandon outcome {outcome}"
    );
    let mut saw_terminal = false;
    while !saw_terminal {
        let event = harness.next_event();
        assert_eq!(event.session, session);
        if event.kind == ReldexEventKind::Opened as i32 {
            assert!(
                !event.error.is_null(),
                "an abandoned connect is never reported as opened"
            );
        } else {
            assert_eq!(event.kind, ReldexEventKind::Terminal as i32);
            saw_terminal = true;
        }
        take_error(&event);
    }
    assert_eq!(harness.session_count(), 0);
}
