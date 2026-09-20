//! What happens when things go wrong at the boundary: a failing statement, a
//! panicking driver, a stale id, a mis-sized struct, and a close that must not
//! commit anything (ADR-0003 D2/D3/D6/D7).

#![allow(
    unsafe_code,
    reason = "these tests drive the crate's own C ABI, which is what they exist to prove"
)]

mod support;

use reldex_ffi::{
    ReldexCloseDisposition, ReldexCloseOutcome, ReldexErrorKind, ReldexEvent, ReldexEventKind,
    ReldexFormatOptions, ReldexMockScenarioConfig, ReldexMockStatement, ReldexOpenOptions,
    ReldexSessionState, ReldexStatus, reldex_batch_format_column, reldex_hub_next_event,
    reldex_last_error_take, reldex_text_arena_count, reldex_text_arena_create,
    reldex_text_arena_release,
};

use support::{Harness, OwnedBatch, free_error, take_error};

fn config() -> ReldexMockScenarioConfig {
    ReldexMockScenarioConfig {
        rows: 20,
        ..ReldexMockScenarioConfig::default()
    }
}

#[test]
fn a_failing_statement_crosses_with_its_native_code_and_position() {
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.execute(session, 5, ReldexMockStatement::Failing),
        ReldexStatus::Ok
    );
    let event = harness.next_event();
    assert_eq!(event.kind, ReldexEventKind::Executed as i32);
    assert_eq!(
        event.request, 5,
        "the reply carries the caller's request id"
    );
    let error = take_error(&event).expect("a failing statement produces an error");
    assert_eq!(error.kind, ReldexErrorKind::Syntax as i32);
    assert_eq!(error.native_code, Some(942));
    assert_eq!(error.line_column, Some((1, 15)));
    assert!(
        error.native_message.contains("ORA-00942"),
        "the vendor's own text survives: {}",
        error.native_message
    );
    assert_eq!(
        error.session_state,
        ReldexSessionState::Usable as i32,
        "a rejected statement does not disturb the session"
    );

    // The session is still usable, which is the point of a `Usable` state.
    assert_eq!(
        harness.execute(session, 6, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let next = harness.next_event();
    assert!(next.error.is_null());
    assert_eq!(next.request, 6);
}

#[test]
fn a_driver_panic_comes_back_as_an_error_event_not_an_abort() {
    // `db-core` contains a driver panic (ADR-0002 K6) and this boundary must
    // deliver it as an ordinary failure. The *process surviving this test* is
    // half the assertion.
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.execute(session, 7, ReldexMockStatement::Panicking),
        ReldexStatus::Ok
    );
    let event = harness.next_event();
    assert_eq!(event.kind, ReldexEventKind::Executed as i32);
    assert_eq!(event.request, 7);
    let error = take_error(&event).expect("a contained panic is reported");
    assert_eq!(error.kind, ReldexErrorKind::DriverInternal as i32);
    assert!(
        error.message.contains("panic"),
        "the panic's message survives: {}",
        error.message
    );
    assert_eq!(
        error.session_state,
        ReldexSessionState::Lost as i32,
        "a torn connection is not pretended to be usable"
    );
    assert_eq!(
        event.session_state,
        ReldexSessionState::Lost as i32,
        "the event reports the session's own lifecycle"
    );
}

#[test]
fn a_stale_or_foreign_result_id_is_reported_not_undefined() {
    let harness = Harness::new();
    let first = harness.open(config());
    let second = harness.open(config());

    assert_eq!(
        harness.execute(first, 1, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let executed = harness.next_event();
    let result = executed.result;
    assert!(executed.has_result);

    // An id this session never had.
    assert_eq!(
        harness.fetch(first, 2, result + 4_000, 10),
        ReldexStatus::NotFound
    );
    let error = reldex_last_error_take();
    assert!(!error.is_null(), "a refusal says why");
    free_error(error);

    // The same id on the *other* session: also not found there, which is the
    // whole reason ids are session-scoped integers rather than pointers.
    assert_eq!(harness.fetch(second, 3, result, 10), ReldexStatus::NotFound);

    // A rejected request produces no event at all — only the accepted one
    // below does.
    assert_eq!(harness.fetch(first, 4, result, 10), ReldexStatus::Ok);
    let fetched = harness.next_event();
    assert_eq!(fetched.request, 4, "no phantom replies for refused calls");
    OwnedBatch(fetched.batch);

    // After the result is closed its id is stale, and stays reported.
    assert_eq!(harness.close_result(first, 5, result), ReldexStatus::Ok);
    let closed = harness.next_event();
    assert!(closed.error.is_null());
    assert_eq!(harness.fetch(first, 6, result, 10), ReldexStatus::NotFound);

    // So is a session id that was never issued.
    assert_eq!(
        harness.execute(9_999, 7, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::NotFound
    );
}

#[test]
fn a_close_with_a_possibly_open_transaction_refuses_to_decide_for_the_user() {
    // `SPEC.md` §10: Reldex never silently commits, and a close that cannot be
    // resolved leaves the session open so the user can be asked.
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.execute(session, 1, ReldexMockStatement::Dml),
        ReldexStatus::Ok
    );
    let executed = harness.next_event();
    assert!(executed.error.is_null());
    assert!(executed.has_rows_affected);
    assert_eq!(executed.rows_affected, 1);

    assert_eq!(
        harness.close(session, 2, ReldexCloseDisposition::None),
        ReldexStatus::Ok
    );
    let refused = harness.next_event();
    assert_eq!(refused.kind, ReldexEventKind::SessionClosed as i32);
    assert_eq!(
        refused.close_outcome,
        ReldexCloseOutcome::DecisionRequired as i32
    );
    assert!(
        refused.session_still_open,
        "a close that did not happen must leave the session usable"
    );
    let error = take_error(&refused).expect("the refusal explains itself");
    assert!(error.message.contains("disposition"), "{}", error.message);

    // The session really is still usable, and closing it with an explicit
    // decision works.
    assert_eq!(
        harness.execute(session, 3, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    assert!(harness.next_event().error.is_null());
    assert_eq!(
        harness.close(session, 4, ReldexCloseDisposition::Rollback),
        ReldexStatus::Ok
    );
    let closed = harness.next_event();
    assert_eq!(closed.close_outcome, ReldexCloseOutcome::Closed as i32);
    assert!(!closed.session_still_open);
    assert_eq!(closed.session_state, ReldexSessionState::Closed as i32);

    // Nothing more is accepted, and nothing more is answered.
    assert_eq!(
        harness.execute(session, 5, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::InvalidState
    );
    assert!(harness.poll_event().is_none());
}

/// Waits for the next event into a caller-shaped struct, so a test can use a
/// deliberately mis-sized one.
fn take_into(harness: &Harness, event: &mut ReldexEvent) {
    let mut seen = harness.signal.wakes();
    for _ in 0..4 {
        // SAFETY: the hub is live and `event` is a real local.
        if unsafe { reldex_hub_next_event(harness.hub(), std::ptr::from_mut(event)) } {
            return;
        }
        seen = harness.signal.wait_past(seen);
    }
    panic!("no event arrived within the hang guard");
}

#[test]
fn an_undersized_event_struct_is_refused_without_consuming_the_event() {
    // Losing an event here would leak the batch it carries, so the rule is
    // "refuse, do not truncate" (ADR-0003 D7).
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.execute(session, 2, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let executed = harness.next_event();
    assert_eq!(executed.request, 2);

    assert_eq!(
        harness.fetch(session, 5, executed.result, 5),
        ReldexStatus::Ok
    );
    // Filled with a recognisable value so the refusal can be shown to leave it
    // alone: a `false` return must mean "`*out` untouched", not "partly
    // overwritten with an empty event".
    let mut small = ReldexEvent {
        struct_size: 8,
        request: 0xDEAD_BEEF,
        session: 0xFEED_FACE,
        ..ReldexEvent::default()
    };
    // Give the pump time to produce the event first, so the refusal below is
    // about the struct and not about an empty queue.
    let mut waited = ReldexEvent::default();
    take_into(&harness, &mut waited);
    assert_eq!(waited.request, 5);
    OwnedBatch(waited.batch);

    assert_eq!(
        harness.fetch(session, 6, executed.result, 5),
        ReldexStatus::Ok
    );
    let seen = harness.signal.wakes();
    harness.signal.wait_past(seen.saturating_sub(1));
    // SAFETY: `small` is a real `ReldexEvent`; only its declared size lies,
    // which is exactly what this asserts is caught.
    let taken = unsafe { reldex_hub_next_event(harness.hub(), std::ptr::from_mut(&mut small)) };
    assert!(!taken, "a struct too small to hold the event is refused");
    assert_eq!(
        (small.struct_size, small.request, small.session),
        (8, 0xDEAD_BEEF, 0xFEED_FACE),
        "a refused call must not write to `out` at all"
    );
    let error = reldex_last_error_take();
    assert!(!error.is_null(), "the refusal says why");
    free_error(error);

    // The event was not consumed: a correctly sized call still gets it.
    let event = harness.next_event();
    assert_eq!(event.request, 6);
    OwnedBatch(event.batch);

    // A caller from a *newer* header is accepted, and told how much of its
    // struct this build filled in.
    assert_eq!(
        harness.fetch(session, 7, executed.result, 5),
        ReldexStatus::Ok
    );
    let mut large = ReldexEvent {
        struct_size: u32::try_from(size_of::<ReldexEvent>() + 64).expect("fits"),
        ..ReldexEvent::default()
    };
    take_into(&harness, &mut large);
    assert_eq!(large.request, 7);
    assert_eq!(
        large.struct_size as usize,
        size_of::<ReldexEvent>(),
        "the caller is told how much of its struct is valid"
    );
    OwnedBatch(large.batch);

    // And `false` means "untouched" when the queue is simply empty, too.
    let mut probe = ReldexEvent {
        request: 0x1234_5678,
        ..ReldexEvent::default()
    };
    // SAFETY: `probe` is a real, correctly sized `ReldexEvent`; the queue is
    // empty because everything submitted above has been taken.
    let taken = unsafe { reldex_hub_next_event(harness.hub(), std::ptr::from_mut(&mut probe)) };
    assert!(!taken, "nothing is queued");
    assert_eq!(
        probe.request, 0x1234_5678,
        "an empty queue must leave `out` untouched"
    );
}

#[test]
fn an_options_struct_from_an_older_header_gets_the_documented_defaults() {
    // Only `struct_size` and `driver` supplied: everything else must read as
    // its zero default rather than as garbage.
    let harness = Harness::new();
    let options = ReldexOpenOptions {
        struct_size: u32::try_from(size_of::<u32>() + size_of::<i32>()).expect("fits"),
        ..ReldexOpenOptions::default()
    };
    let mut session = 0_u64;
    // SAFETY: `options` is a real struct whose declared size is the minimum
    // this build accepts.
    let status = unsafe {
        reldex_ffi::reldex_hub_open_session(
            harness.hub(),
            std::ptr::from_ref(&options),
            1,
            std::ptr::from_mut(&mut session),
        )
    };
    assert_eq!(status, ReldexStatus::Ok);
    let opened = harness.next_event();
    assert!(opened.error.is_null(), "the default world must open");

    // A `struct_size` this build cannot honour is refused, not guessed at.
    let broken = ReldexOpenOptions {
        struct_size: 4,
        ..ReldexOpenOptions::default()
    };
    // SAFETY: as above; the declared size is deliberately too small.
    let status = unsafe {
        reldex_ffi::reldex_hub_open_session(
            harness.hub(),
            std::ptr::from_ref(&broken),
            2,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(status, ReldexStatus::InvalidArgument);
    free_error(reldex_last_error_take());

    // The same rule for the formatter's options, which default to "as stored".
    assert_eq!(
        harness.execute(session, 3, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let executed = harness.next_event();
    assert_eq!(
        harness.fetch(session, 4, executed.result, 2),
        ReldexStatus::Ok
    );
    let fetched = harness.next_event();
    let batch = OwnedBatch(fetched.batch);
    let arena = reldex_text_arena_create();
    let minimal = ReldexFormatOptions {
        struct_size: u32::try_from(size_of::<u32>() * 2).expect("fits"),
        ..ReldexFormatOptions::default()
    };
    // SAFETY: the batch and arena are live; `minimal` declares the smallest
    // size this build accepts.
    let status = unsafe {
        reldex_batch_format_column(batch.0, 0, 0, 2, std::ptr::from_ref(&minimal), arena)
    };
    assert_eq!(status, ReldexStatus::Ok);
    // SAFETY: the arena is live.
    assert_eq!(unsafe { reldex_text_arena_count(arena) }, 2);

    // Null options are the documented defaults too.
    // SAFETY: a null `options` is explicitly allowed.
    let status = unsafe { reldex_batch_format_column(batch.0, 0, 0, 2, std::ptr::null(), arena) };
    assert_eq!(status, ReldexStatus::Ok);
    // SAFETY: the arena is live and released exactly once.
    unsafe { reldex_text_arena_release(arena) };
}

#[test]
fn null_and_unaligned_arguments_are_refused_rather_than_dereferenced() {
    let harness = Harness::new();
    // SAFETY: passing null is exactly what these calls must survive.
    unsafe {
        assert_eq!(reldex_ffi::reldex_batch_row_count(std::ptr::null()), 0);
        assert_eq!(reldex_ffi::reldex_batch_column_count(std::ptr::null()), 0);
        assert!(!reldex_hub_next_event(harness.hub(), std::ptr::null_mut()));
        assert_eq!(
            reldex_ffi::reldex_session_request_cancel(
                std::ptr::null_mut(),
                1,
                std::ptr::null_mut()
            ),
            ReldexStatus::InvalidArgument
        );
        reldex_ffi::reldex_batch_release(std::ptr::null_mut());
        reldex_ffi::reldex_error_free(std::ptr::null_mut());
        reldex_ffi::reldex_text_arena_release(std::ptr::null_mut());
        reldex_ffi::reldex_hub_destroy(std::ptr::null_mut());
    }
    free_error(reldex_last_error_take());
}

#[test]
fn a_request_submitted_before_the_opened_event_is_either_refused_or_answered() {
    // The header used to say "nothing may be submitted until OPENED arrives",
    // while the code marked the session open *before* pushing that event. Both
    // outcomes are correct; what would not be is a third one where a request is
    // accepted and never answered. This pins the two:
    //
    //   * INVALID_STATE  -> nothing accepted, no event follows;
    //   * OK             -> accepted, and its EXECUTED event arrives.
    //
    // The race is real but not controllable, so the test accepts either and
    // checks the consequence, rather than sleeping to force one.
    let harness = Harness::without_waker();
    let options = ReldexOpenOptions {
        mock: ReldexMockScenarioConfig {
            rows: 4,
            ..ReldexMockScenarioConfig::default()
        },
        ..ReldexOpenOptions::default()
    };
    let mut session = 0_u64;
    // SAFETY: the hub is live and the locals are real.
    assert_eq!(
        unsafe {
            reldex_ffi::reldex_hub_open_session(
                harness.hub(),
                std::ptr::from_ref(&options),
                1,
                std::ptr::from_mut(&mut session),
            )
        },
        ReldexStatus::Ok
    );

    // Submitted immediately, with no wait for OPENED at all.
    let early = harness.execute(session, 2, ReldexMockStatement::GeneratedQuery);
    let accepted = match early {
        ReldexStatus::Ok => true,
        ReldexStatus::InvalidState => {
            // The refusal must say why rather than leave a stale error.
            let error = reldex_ffi::reldex_last_error_take();
            assert!(!error.is_null(), "a refusal records why");
            support::free_error(error);
            false
        }
        other => panic!("unexpected status {other:?}"),
    };

    // Drain until the session has answered everything it owes: the OPENED
    // event always, plus the execute if it was accepted.
    let expected = 1 + usize::from(accepted);
    let mut requests = Vec::new();
    support::wait_until("the session to answer everything it accepted", || {
        while let Some(event) = harness.poll_event() {
            support::release_batch(&event);
            if !event.error.is_null() {
                // SAFETY: the error came from the event and is freed once.
                unsafe { reldex_ffi::reldex_error_free(event.error) };
            }
            requests.push((event.kind, event.request));
        }
        requests.len() >= expected
    });

    assert_eq!(requests[0], (ReldexEventKind::Opened as i32, 1));
    if accepted {
        assert_eq!(
            requests[1],
            (ReldexEventKind::Executed as i32, 2),
            "an accepted early request must still get its one reply"
        );
    }
    assert_eq!(
        requests.len(),
        expected,
        "and nothing beyond what was accepted: {requests:?}"
    );
}
