//! Large objects never cross a thread boundary as driver handles.
//!
//! A driver puts live `LobLocator`s straight into a fetched batch's LOB
//! columns. A locator is a handle derived from the connection, so *using* it
//! off the owning worker thread issues protocol traffic on a connection another
//! thread believes it owns — and so does merely **dropping** it, because a
//! locator's `Drop` runs driver code. With the primary driver's
//! `Arc<Mutex<Client>>` that is a deadlock or worse.
//!
//! So `db-core` takes every locator out of the batch before the batch is sent
//! to the caller, parks it beside the cursors, and hands out an opaque
//! [`LobHandle`]. These tests hold it to that: the caller drops batches and
//! handles on other threads, and the mock must still have seen exactly one
//! thread id (ADR-0002 D1/D2).

mod support;

use std::thread;

use reldex_db_core::{LobHandle, OutValue, Statement, ValueRef};
use reldex_db_driver_api::{ErrorKind, LobKind, SqlType};
use reldex_driver_mock::{Action, ColumnSpec, QueryPlan, QuerySource, ScriptValue};

const SELECT_DOCS: &str = "SELECT doc FROM t";
const CONTENT_A: &str = "the first document, long enough to need several chunks";
const CONTENT_B: &str = "ข้อมูลไทยที่ต้องอ่านเป็นช่วง ๆ";

fn scenario_with_two_lobs() -> std::sync::Arc<reldex_driver_mock::Scenario> {
    let scenario = support::scenario();
    let columns = vec![ColumnSpec::new(
        "DOC",
        SqlType::CharacterLob { national: false },
    )];
    let rows = vec![
        vec![ScriptValue::Lob {
            kind: LobKind::Character,
            bytes: CONTENT_A.as_bytes().to_vec(),
        }],
        vec![ScriptValue::Null],
        vec![ScriptValue::Lob {
            kind: LobKind::Character,
            bytes: CONTENT_B.as_bytes().to_vec(),
        }],
    ];
    scenario.on_sql(
        SELECT_DOCS,
        Action::query(QuerySource::Fixed(QueryPlan::new(columns, rows))),
    );
    scenario
}

fn read_all(session: &support::Session, lob: LobHandle) -> String {
    let mut collected = Vec::new();
    loop {
        let chunk = session
            .read_lob_chunk(lob, support::n(7))
            .wait()
            .expect("reading a chunk should succeed");
        if chunk.is_empty() {
            break;
        }
        collected.extend_from_slice(&chunk);
    }
    String::from_utf8(collected).expect("character LOB chunks are valid UTF-8")
}

fn only_plain_data_crosses_threads_even_when_batches_and_handles_are_dropped_elsewhere(
    path: support::ReplyPath,
) {
    let scenario = scenario_with_two_lobs();
    let session = support::open_on(&scenario, path);
    let connection_id = session.connection_id();

    let outcome = session
        .execute(Statement::new(SELECT_DOCS))
        .wait()
        .expect("execute");
    let result = outcome.result.expect("cursor");
    let batch = session
        .fetch_batch(result, support::n(10))
        .wait()
        .expect("fetch");

    assert_eq!(batch.row_count(), 3);
    let first = batch.lob(0, 0).expect("row 0 holds a LOB");
    let third = batch.lob(2, 0).expect("row 2 holds a LOB");
    assert!(
        batch.lob(1, 0).is_none(),
        "row 1 is SQL NULL, so there is nothing to park"
    );

    // A parked cell is `Taken`, which is *not* NULL — an exporter re-reading
    // the batch must not write an empty cell where the database holds data.
    assert!(matches!(batch.value(0, 0), Some(ValueRef::Taken)));
    assert!(!batch.value(0, 0).expect("cell").is_null());
    assert!(batch.value(1, 0).expect("cell").is_null());

    // Send the batch to another thread and drop it there. Nothing in it is a
    // driver handle any more, so this must not touch the driver at all.
    let moved = thread::spawn(move || {
        drop(batch);
    });
    moved.join().expect("the batch may be dropped anywhere");

    // The handles are still valid, and reading still happens on the worker.
    assert_eq!(read_all(&session, first), CONTENT_A);
    assert_eq!(read_all(&session, third), CONTENT_B);

    // Release one from another thread; the other is left for the session to
    // reclaim. `DatabaseSession` is `Sync`, so this is the documented shape —
    // and it is the *session* that is shared here, not this file's per-path
    // harness, which is single-threaded on purpose.
    let shared: &reldex_db_core::DatabaseSession = &session;
    thread::scope(|scope| {
        scope.spawn(|| {
            shared.close_lob(first).wait().expect("close_lob");
        });
    });

    let seen = scenario.thread_ids_seen(connection_id);
    assert_eq!(
        seen.len(),
        1,
        "exactly one thread may ever run driver code for a connection, however the caller \
         moves batches and handles around: {seen:?}"
    );
    assert_ne!(seen[0], thread::current().id());
}

fn a_lob_handle_dies_with_the_result_it_came_from(path: support::ReplyPath) {
    let scenario = scenario_with_two_lobs();
    let session = support::open_on(&scenario, path);
    let outcome = session
        .execute(Statement::new(SELECT_DOCS))
        .wait()
        .expect("execute");
    let result = outcome.result.expect("cursor");
    let batch = session
        .fetch_batch(result, support::n(10))
        .wait()
        .expect("fetch");
    let lob = batch.lob(0, 0).expect("row 0 holds a LOB");

    session.close_result(result).wait().expect("close_result");

    let error = session
        .read_lob_chunk(lob, support::n(7))
        .wait()
        .expect_err("closing the result released its large objects");
    assert!(error.message().contains("closed"), "{error}");
    // Releasing it again is still the usual no-op.
    session.close_lob(lob).wait().expect("idempotent");
}

fn a_lob_handle_from_another_session_is_rejected_as_foreign(path: support::ReplyPath) {
    // Before handles were core-owned and session-scoped, a locator taken from
    // session A's batch could be handed to session B, which happily ran the
    // read on *its* worker thread — driver code on the wrong connection's
    // thread, which is exactly the hazard the whole design exists to prevent.
    let scenario = scenario_with_two_lobs();
    let owner = support::open_on(&scenario, path);
    let other = support::open_on(&scenario, path);

    let outcome = owner
        .execute(Statement::new(SELECT_DOCS))
        .wait()
        .expect("execute");
    let result = outcome.result.expect("cursor");
    let batch = owner
        .fetch_batch(result, support::n(10))
        .wait()
        .expect("fetch");
    let lob = batch.lob(0, 0).expect("row 0 holds a LOB");
    assert_eq!(lob.owner(), owner.id());

    let error = other
        .read_lob_chunk(lob, support::n(7))
        .wait()
        .expect_err("a handle from another session must not be readable here");
    assert!(
        error.message().contains("belongs to another session"),
        "the message must distinguish a foreign handle from a closed one: {error}"
    );
    assert_eq!(error.kind(), ErrorKind::DriverInternal);

    let error = other
        .fetch_batch(result, support::n(10))
        .wait()
        .expect_err("and the same goes for a result handle");
    assert!(
        error.message().contains("belongs to another session"),
        "{error}"
    );

    // The owning session is untouched.
    assert_eq!(read_all(&owner, lob), CONTENT_A);
}

fn a_large_object_returned_through_an_out_bind_is_also_a_handle(path: support::ReplyPath) {
    let scenario = support::scenario();
    scenario.on_sql(
        "BEGIN :doc := load_doc; END;",
        Action::LobOut {
            name: "doc".to_owned(),
            kind: LobKind::Character,
            bytes: CONTENT_A.as_bytes().to_vec(),
        },
    );
    let session = support::open_on(&scenario, path);
    let connection_id = session.connection_id();

    let outcome = session
        .execute(Statement::new("BEGIN :doc := load_doc; END;"))
        .wait()
        .expect("execute");
    let lob = match outcome.out_values.named("doc") {
        Some(OutValue::Lob(lob)) => *lob,
        other => panic!("expected a parked large object, got {other:?}"),
    };
    assert_eq!(read_all(&session, lob), CONTENT_A);

    let seen = scenario.thread_ids_seen(connection_id);
    assert_eq!(seen.len(), 1);
    assert_ne!(seen[0], thread::current().id());
}

fn a_read_larger_than_the_configured_limit_is_capped_rather_than_allocated(
    path: support::ReplyPath,
) {
    let scenario = scenario_with_two_lobs();
    let session = support::open_on_with_limits(
        &scenario,
        reldex_db_core::SessionLimits::new().with_max_lob_chunk_bytes(support::n(8)),
        path,
    );
    let outcome = session
        .execute(Statement::new(SELECT_DOCS))
        .wait()
        .expect("execute");
    let result = outcome.result.expect("cursor");
    let batch = session
        .fetch_batch(result, support::n(10))
        .wait()
        .expect("fetch");
    let lob = batch.lob(0, 0).expect("row 0 holds a LOB");

    // Asking for a gigabyte must not allocate one.
    let chunk = session
        .read_lob_chunk(lob, support::n(1024 * 1024 * 1024))
        .wait()
        .expect("read");
    assert!(
        chunk.len() <= 8,
        "the configured cap bounds the buffer, however much the caller asks for: {}",
        chunk.len()
    );
    assert_eq!(&chunk, &CONTENT_A.as_bytes()[..chunk.len()]);
}

support::both_paths! {
    only_plain_data_crosses_threads_even_when_batches_and_handles_are_dropped_elsewhere,
    a_lob_handle_dies_with_the_result_it_came_from,
    a_lob_handle_from_another_session_is_rejected_as_foreign,
    a_large_object_returned_through_an_out_bind_is_also_a_handle,
    a_read_larger_than_the_configured_limit_is_capped_rather_than_allocated,
}
