//! Direct tests of `reldex-driver-mock` against the `reldex-db-driver-api`
//! contract, independent of `reldex-db-core`. These exist so a mock bug is
//! caught here rather than showing up as a confusing `db-core` test failure.

use std::num::NonZeroUsize;
use std::thread;
use std::time::Duration;

use reldex_db_driver_api::{
    CancelKind, CancelOutcome, Capabilities, ConnectionParams, Credentials, DatabaseConnection,
    DatabaseDriver, Endpoint, ErrorKind, LobKind, SavepointName, SessionState, SqlType, Statement,
    StatementKind, Timestamp, TransactionState,
};
use reldex_driver_mock::{
    Action, BlockGate, BlockSpec, ColumnSpec, MockDriver, QueryPlan, QuerySource, Scenario,
    ScriptValue, ScriptedError,
};

fn connect(scenario: &std::sync::Arc<Scenario>) -> Box<dyn DatabaseConnection> {
    let driver = MockDriver::new(std::sync::Arc::clone(scenario));
    let params = ConnectionParams::new(
        Endpoint::ConnectString("mock".to_owned()),
        Credentials::External,
    );
    driver.connect(&params).expect("connect should succeed")
}

fn one(max: usize) -> NonZeroUsize {
    NonZeroUsize::new(max).expect("non-zero")
}

#[test]
fn connect_can_be_scripted_to_fail() {
    let scenario = Scenario::new();
    scenario.fail_connect(ScriptedError::new(
        ErrorKind::Authentication,
        "bad password",
    ));
    let driver = MockDriver::new(scenario);
    let params = ConnectionParams::new(
        Endpoint::ConnectString("mock".to_owned()),
        Credentials::External,
    );
    let error = match driver.connect(&params) {
        Ok(_) => panic!("connect should fail"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ErrorKind::Authentication);
}

#[test]
fn ping_can_be_scripted_to_fail_and_recover() {
    let scenario = Scenario::new();
    let mut connection = connect(&scenario);
    connection.ping().expect("ping succeeds by default");

    scenario.fail_ping(ScriptedError::new(ErrorKind::NetworkLost, "gone"));
    let error = connection.ping().expect_err("ping should fail");
    assert_eq!(error.kind(), ErrorKind::NetworkLost);
    assert_eq!(error.session_state(), SessionState::Lost);

    scenario.allow_ping();
    connection.ping().expect("ping recovers");
}

#[test]
fn multi_batch_fetch_with_nulls_number_text_bytes_timestamp_unsupported_and_lob() {
    let scenario = Scenario::new();
    let ts = Timestamp::date(2026, 9, 19).expect("valid date");
    let columns = vec![
        ColumnSpec::new("N", SqlType::Number),
        ColumnSpec::new("T", SqlType::VARCHAR),
        ColumnSpec::new("B", SqlType::Raw),
        ColumnSpec::new("D", SqlType::Timestamp),
        ColumnSpec::new("U", SqlType::Unsupported).with_native_type_name("INTERVAL"),
        ColumnSpec::new("L", SqlType::CharacterLob { national: false }),
    ];
    let rows = vec![
        vec![
            ScriptValue::from(1_i64),
            ScriptValue::from("alpha"),
            ScriptValue::from(vec![1_u8, 2, 3]),
            ScriptValue::from(ts),
            ScriptValue::Unsupported("+01 02:03:04".to_owned()),
            ScriptValue::Lob {
                kind: LobKind::Character,
                bytes: b"hello lob".to_vec(),
            },
        ],
        vec![
            ScriptValue::Null,
            ScriptValue::Null,
            ScriptValue::Null,
            ScriptValue::Null,
            ScriptValue::Null,
            ScriptValue::Null,
        ],
        vec![
            ScriptValue::from(3_i64),
            ScriptValue::from("gamma"),
            ScriptValue::from(vec![9_u8]),
            ScriptValue::from(ts),
            ScriptValue::Unsupported("+03 00:00:00".to_owned()),
            ScriptValue::Lob {
                kind: LobKind::Character,
                bytes: b"row three".to_vec(),
            },
        ],
    ];
    scenario.on_sql(
        "SELECT * FROM t",
        Action::query(QuerySource::Fixed(QueryPlan::new(columns, rows))),
    );

    let mut connection = connect(&scenario);
    let mut outcome = connection
        .execute(&Statement::new("SELECT * FROM t"))
        .expect("execute");
    assert_eq!(outcome.statement_kind(), StatementKind::Query);
    let mut cursor = outcome.take_cursor().expect("has a cursor");

    let batch1 = cursor.fetch_batch(one(2)).expect("first batch");
    assert_eq!(batch1.row_count(), 2);
    assert_eq!(
        batch1
            .value(0, 0)
            .and_then(|v| v.as_number().and_then(|n| n.to_i64())),
        Some(1)
    );
    assert!(batch1.value(1, 0).expect("cell").is_null());
    assert_eq!(batch1.value(0, 1).and_then(|v| v.as_str()), Some("alpha"));
    assert_eq!(
        batch1.value(0, 2).and_then(|v| v.as_bytes()),
        Some(&[1_u8, 2, 3][..])
    );
    assert_eq!(
        batch1.value(0, 4).and_then(|v| v.as_unsupported_text()),
        Some("+01 02:03:04")
    );
    assert!(!batch1.value(0, 4).expect("cell").is_null());

    assert!(
        batch1
            .column(5)
            .and_then(|c| c.value(0))
            .is_some_and(|v| v.as_lob().is_some())
    );
    // Take the LOB out to read it (mirrors how `db-core` would).
    let mut batch1 = batch1;
    let mut locator = batch1
        .column_mut(5)
        .and_then(|c| c.take_lob(0))
        .expect("lob present on row 0");
    let mut buf = [0_u8; 4];
    let mut collected = Vec::new();
    loop {
        let n = locator.read_chunk(&mut buf).expect("read chunk");
        if n == 0 {
            break;
        }
        collected.extend_from_slice(&buf[..n]);
    }
    assert_eq!(collected, b"hello lob");

    let batch2 = cursor.fetch_batch(one(2)).expect("second batch");
    assert_eq!(batch2.row_count(), 1);
    assert_eq!(batch2.value(0, 1).and_then(|v| v.as_str()), Some("gamma"));

    let batch3 = cursor.fetch_batch(one(2)).expect("exhausted batch");
    assert!(batch3.is_empty(), "an empty batch means exhausted");
    assert!(cursor.is_exhausted());
}

#[test]
fn fault_injection_fails_the_nth_batch() {
    let scenario = Scenario::new();
    let columns = vec![ColumnSpec::new("N", SqlType::Number)];
    let rows: Vec<Vec<ScriptValue>> = (0..5).map(|n| vec![ScriptValue::from(n as i64)]).collect();
    let plan = QueryPlan::new(columns, rows).with_fail_on_batch(
        2,
        ScriptedError::new(ErrorKind::NetworkLost, "connection reset mid-fetch"),
    );
    scenario.on_sql("SELECT * FROM big", Action::query(QuerySource::Fixed(plan)));

    let mut connection = connect(&scenario);
    let mut outcome = connection
        .execute(&Statement::new("SELECT * FROM big"))
        .expect("execute");
    let mut cursor = outcome.take_cursor().expect("cursor");

    let first = cursor.fetch_batch(one(2)).expect("first batch ok");
    assert_eq!(first.row_count(), 2);

    let error = cursor.fetch_batch(one(2)).expect_err("second batch fails");
    assert_eq!(error.kind(), ErrorKind::NetworkLost);
    assert_eq!(error.session_state(), SessionState::Lost);
}

#[test]
fn table_store_is_transactional_and_isolated_per_connection() {
    let scenario = Scenario::new();
    let columns = vec![ColumnSpec::new("NAME", SqlType::VARCHAR)];
    scenario.on_sql(
        "INSERT INTO t VALUES ('a')",
        Action::Dml {
            rows_affected: 1,
            insert: Some(("t".to_owned(), vec![ScriptValue::from("a")])),
        },
    );
    scenario.on_sql(
        "SELECT * FROM t",
        Action::query(QuerySource::Table {
            table: "t".to_owned(),
            columns: columns.clone(),
        }),
    );

    let mut owner = connect(&scenario);
    let mut other = connect(&scenario);

    owner
        .execute(&Statement::new("INSERT INTO t VALUES ('a')"))
        .expect("insert");

    // Uncommitted: visible to the owning connection, not to another one.
    let mut outcome = owner
        .execute(&Statement::new("SELECT * FROM t"))
        .expect("select on owner");
    let mut cursor = outcome.take_cursor().expect("cursor");
    let batch = cursor.fetch_batch(one(10)).expect("fetch");
    assert_eq!(
        batch.row_count(),
        1,
        "owner sees its own uncommitted insert"
    );

    let mut outcome = other
        .execute(&Statement::new("SELECT * FROM t"))
        .expect("select on other");
    let mut cursor = outcome.take_cursor().expect("cursor");
    let batch = cursor.fetch_batch(one(10)).expect("fetch");
    assert_eq!(
        batch.row_count(),
        0,
        "a second connection must not see uncommitted state"
    );

    owner.commit().expect("commit");

    let mut outcome = other
        .execute(&Statement::new("SELECT * FROM t"))
        .expect("select on other after commit");
    let mut cursor = outcome.take_cursor().expect("cursor");
    let batch = cursor.fetch_batch(one(10)).expect("fetch");
    assert_eq!(
        batch.row_count(),
        1,
        "commit makes the row visible to everyone"
    );
}

#[test]
fn rollback_discards_uncommitted_inserts() {
    let scenario = Scenario::new();
    scenario.on_sql(
        "INSERT INTO t VALUES ('a')",
        Action::Dml {
            rows_affected: 1,
            insert: Some(("t".to_owned(), vec![ScriptValue::from("a")])),
        },
    );
    let mut connection = connect(&scenario);
    connection
        .execute(&Statement::new("INSERT INTO t VALUES ('a')"))
        .expect("insert");
    connection.rollback().expect("rollback");
    assert!(scenario.committed_rows("t").is_empty());
}

#[test]
fn savepoint_and_rollback_to_savepoint_undo_only_later_ops() {
    let scenario = Scenario::new();
    for value in ["a", "b", "c"] {
        scenario.on_sql(
            format!("INSERT INTO t VALUES ('{value}')"),
            Action::Dml {
                rows_affected: 1,
                insert: Some(("t".to_owned(), vec![ScriptValue::from(value)])),
            },
        );
    }
    let mut connection = connect(&scenario);
    connection
        .execute(&Statement::new("INSERT INTO t VALUES ('a')"))
        .expect("insert a");
    let sp = SavepointName::new("sp1").expect("valid name");
    connection.savepoint(&sp).expect("savepoint");
    connection
        .execute(&Statement::new("INSERT INTO t VALUES ('b')"))
        .expect("insert b");
    connection
        .execute(&Statement::new("INSERT INTO t VALUES ('c')"))
        .expect("insert c");
    connection
        .rollback_to_savepoint(&sp)
        .expect("rollback to savepoint");
    connection.commit().expect("commit");

    let rows = scenario.committed_rows("t");
    assert_eq!(rows, vec![vec![ScriptValue::from("a")]]);
}

#[test]
fn ddl_reports_implicit_commit_and_transaction_state_tracking() {
    let scenario = Scenario::new();
    scenario.on_sql(
        "INSERT INTO t VALUES ('a')",
        Action::Dml {
            rows_affected: 1,
            insert: Some(("t".to_owned(), vec![ScriptValue::from("a")])),
        },
    );
    scenario.on_sql("CREATE TABLE u (x NUMBER)", Action::Ddl);

    let mut connection = connect(&scenario);
    assert_eq!(connection.transaction_state(), TransactionState::Inactive);

    connection
        .execute(&Statement::new("INSERT INTO t VALUES ('a')"))
        .expect("insert");
    assert_eq!(connection.transaction_state(), TransactionState::Active);

    let outcome = connection
        .execute(&Statement::new("CREATE TABLE u (x NUMBER)"))
        .expect("ddl");
    assert_eq!(outcome.statement_kind(), StatementKind::Ddl);
    assert!(outcome.committed_implicitly());
    assert_eq!(connection.transaction_state(), TransactionState::Inactive);
    // DDL commits around itself: the pending insert is now durable.
    assert_eq!(scenario.committed_rows("t").len(), 1);
}

#[test]
fn imprecise_driver_reports_unknown_instead_of_active() {
    let scenario = Scenario::new();
    scenario.set_capabilities(Capabilities::none().with_exact_transaction_state(false));
    scenario.on_sql(
        "INSERT INTO t VALUES ('a')",
        Action::Dml {
            rows_affected: 1,
            insert: None,
        },
    );
    let mut connection = connect(&scenario);
    connection
        .execute(&Statement::new("INSERT INTO t VALUES ('a')"))
        .expect("insert");
    assert_eq!(connection.transaction_state(), TransactionState::Unknown);
}

#[test]
fn native_cancel_interrupts_a_blocked_statement() {
    let scenario = Scenario::new();
    scenario.set_capabilities(Capabilities::none().with_cancel(CancelKind::Native));
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN long_running; END;",
        Action::Block(
            BlockSpec::new(std::sync::Arc::clone(&gate))
                .with_cancelled_session_state(SessionState::Usable),
        ),
    );

    let mut connection = connect(&scenario);
    let cancel_handle = connection.cancel_handle();
    assert_eq!(cancel_handle.kind(), CancelKind::Native);

    let worker =
        thread::spawn(move || connection.execute(&Statement::new("BEGIN long_running; END;")));

    assert!(
        gate.wait_until_blocked(Duration::from_secs(5)),
        "the statement should have reached the gate"
    );
    let outcome = cancel_handle.request_cancel().expect("cancel request");
    assert!(outcome.is_requested());

    let result = worker.join().expect("worker thread should not panic");
    let error = result.expect_err("cancelled statement should fail");
    assert_eq!(error.kind(), ErrorKind::Cancelled);
    assert_eq!(error.session_state(), SessionState::Usable);
}

#[test]
fn pre_armed_deadline_cannot_interrupt_but_a_deadline_times_out() {
    let scenario = Scenario::new();
    scenario.set_capabilities(Capabilities::none().with_cancel(CancelKind::PreArmedDeadline));
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN long_running; END;",
        Action::Block(BlockSpec::new(std::sync::Arc::clone(&gate))),
    );

    let mut connection = connect(&scenario);
    let cancel_handle = connection.cancel_handle();

    let statement =
        Statement::new("BEGIN long_running; END;").with_deadline(Duration::from_millis(150));
    let worker = thread::spawn(move || connection.execute(&statement));

    assert!(gate.wait_until_blocked(Duration::from_secs(5)));
    let outcome = cancel_handle.request_cancel().expect("cancel request");
    assert!(
        matches!(outcome, CancelOutcome::NotInterruptible { .. }),
        "a pre-armed-deadline driver must not claim it interrupted anything"
    );

    let result = worker.join().expect("worker thread should not panic");
    let error = result.expect_err("the deadline should fire");
    assert_eq!(error.kind(), ErrorKind::Timeout);
}

#[test]
fn handles_report_after_the_connection_is_closed_rather_than_panicking() {
    let scenario = Scenario::new();
    scenario.on_sql(
        "SELECT 1 FROM dual",
        Action::query(QuerySource::Fixed(QueryPlan::new(
            vec![ColumnSpec::new("N", SqlType::Number)],
            vec![vec![ScriptValue::from(1_i64)]],
        ))),
    );
    let mut connection = connect(&scenario);
    let mut outcome = connection
        .execute(&Statement::new("SELECT 1 FROM dual"))
        .expect("execute");
    let mut cursor = outcome.take_cursor().expect("cursor");

    connection.close().expect("close");

    let error = cursor
        .fetch_batch(one(10))
        .expect_err("a cursor must report after its connection closes, not panic");
    assert_eq!(error.kind(), ErrorKind::DriverInternal);
    assert_eq!(error.session_state(), SessionState::Lost);
}

#[test]
fn caller_thread_never_executes_driver_code() {
    // Mirrors `db-core`'s policy: connect *and* execute both run on the one
    // worker thread that owns the connection, never on the thread that asked
    // for the session (`docs/decisions/0002-driver-api-and-concurrency-model.md`
    // D1/D2). This test drives both from a spawned thread to model that.
    let scenario = Scenario::new();
    scenario.on_sql(
        "SELECT 1 FROM dual",
        Action::query(QuerySource::Fixed(QueryPlan::new(
            vec![ColumnSpec::new("N", SqlType::Number)],
            vec![vec![ScriptValue::from(1_i64)]],
        ))),
    );
    let scenario_for_worker = std::sync::Arc::clone(&scenario);
    let worker = thread::spawn(move || {
        let mut connection = connect(&scenario_for_worker);
        let id = connection.id();
        connection
            .execute(&Statement::new("SELECT 1 FROM dual"))
            .expect("execute");
        id
    });
    let id = worker.join().expect("worker did not panic");

    let seen = scenario.thread_ids_seen(id);
    assert_eq!(
        seen.len(),
        1,
        "only the worker thread touched the connection"
    );
    assert_ne!(seen[0], thread::current().id());
}

// ---------------------------------------------------------------------------
// Mock fidelity: everything below models something a real driver was measured
// doing during the Phase 0 spikes. A test double that only behaves well proves
// nothing about code that has to survive a real one.
// ---------------------------------------------------------------------------

#[test]
fn commit_rollback_and_close_can_each_be_scripted_to_fail() {
    let scenario = Scenario::new();
    scenario.on_sql(
        "INSERT INTO t VALUES ('a')",
        Action::Dml {
            rows_affected: 1,
            insert: Some(("t".to_owned(), vec![ScriptValue::from("a")])),
        },
    );

    let mut connection = connect(&scenario);
    connection
        .execute(&Statement::new("INSERT INTO t VALUES ('a')"))
        .expect("insert");

    scenario.fail_commit(ScriptedError::new(ErrorKind::Resource, "no undo space"));
    let error = connection.commit().expect_err("scripted commit failure");
    assert_eq!(error.kind(), ErrorKind::Resource);
    assert!(
        scenario.committed_rows("t").is_empty(),
        "a failed commit must leave the transaction exactly where it was"
    );
    assert_eq!(connection.transaction_state(), TransactionState::Active);

    scenario.fail_rollback(ScriptedError::new(ErrorKind::Other, "rollback refused"));
    let error = connection
        .rollback()
        .expect_err("scripted rollback failure");
    assert_eq!(error.kind(), ErrorKind::Other);

    scenario.allow_commit();
    connection.commit().expect("commit works again");
    assert_eq!(scenario.committed_rows("t").len(), 1);

    scenario.fail_close(ScriptedError::new(ErrorKind::Other, "close failed"));
    let error = connection.close().expect_err("scripted close failure");
    assert_eq!(error.kind(), ErrorKind::Other);
    assert_eq!(scenario.counts().connections_closed, 1);
}

#[test]
fn handles_can_be_invalidated_by_the_end_of_their_transaction() {
    // Most servers scope a LOB locator to its transaction, and `ROLLBACK`
    // commonly closes cursors (ADR-0002 D2). The mock must be able to model it.
    let scenario = Scenario::new();
    scenario.set_invalidate_handles_on_transaction_end(true);
    scenario.on_sql(
        "SELECT n, doc FROM t",
        Action::query(QuerySource::Fixed(QueryPlan::new(
            vec![
                ColumnSpec::new("N", SqlType::Number),
                ColumnSpec::new("DOC", SqlType::CharacterLob { national: false }),
            ],
            vec![
                vec![
                    ScriptValue::from(1_i64),
                    ScriptValue::Lob {
                        kind: LobKind::Character,
                        bytes: b"hello".to_vec(),
                    },
                ],
                vec![
                    ScriptValue::from(2_i64),
                    ScriptValue::Lob {
                        kind: LobKind::Character,
                        bytes: b"world".to_vec(),
                    },
                ],
            ],
        ))),
    );

    let mut connection = connect(&scenario);
    let mut outcome = connection
        .execute(&Statement::new("SELECT n, doc FROM t"))
        .expect("execute");
    let mut cursor = outcome.take_cursor().expect("cursor");
    let mut batch = cursor.fetch_batch(one(1)).expect("first batch");
    let mut locator = batch
        .column_mut(1)
        .and_then(|column| column.take_lob(0))
        .expect("row 0 holds a LOB");
    assert!(locator.read_chunk(&mut [0_u8; 8]).is_ok());

    connection.commit().expect("commit");

    let error = cursor
        .fetch_batch(one(1))
        .expect_err("the commit invalidated the cursor");
    assert_eq!(error.kind(), ErrorKind::Transaction);
    let error = locator
        .read_chunk(&mut [0_u8; 8])
        .expect_err("and the locator");
    assert_eq!(error.kind(), ErrorKind::Transaction);

    // `close` stays the one call that is always legal and always succeeds.
    cursor.close().expect("close is idempotent-in-spirit");
}

#[test]
fn a_failed_fetch_keeps_reporting_rather_than_looking_complete() {
    let scenario = Scenario::new();
    let plan = QueryPlan::new(
        vec![ColumnSpec::new("N", SqlType::Number)],
        (0..10_i64).map(|n| vec![ScriptValue::from(n)]).collect(),
    )
    .with_fail_on_batch(2, ScriptedError::new(ErrorKind::NetworkLost, "reset"));
    scenario.on_sql("SELECT * FROM big", Action::query(QuerySource::Fixed(plan)));

    let mut connection = connect(&scenario);
    let mut outcome = connection
        .execute(&Statement::new("SELECT * FROM big"))
        .expect("execute");
    let mut cursor = outcome.take_cursor().expect("cursor");
    assert_eq!(
        cursor.fetch_batch(one(3)).expect("first batch").row_count(),
        3
    );
    let error = cursor.fetch_batch(one(3)).expect_err("scripted failure");
    assert_eq!(error.kind(), ErrorKind::NetworkLost);

    for _ in 0..3 {
        let error = cursor
            .fetch_batch(one(3))
            .expect_err("a failed cursor must keep reporting, never look exhausted");
        assert_eq!(
            error.kind(),
            ErrorKind::NetworkLost,
            "an empty 'complete' batch here would turn 3 of 10 rows into the whole answer"
        );
    }
    cursor.close().expect("close stays legal after a failure");
    assert_eq!(scenario.counts().cursors_closed, 1);
}

#[test]
fn close_is_idempotent_and_infallible_in_spirit_after_the_connection_is_gone() {
    // The rule ADR-0002 D2 now states, and the one the Oracle wrapper already
    // implements: `close()` reports `Ok(())` when there is nothing left to
    // release; every *other* method reports `connection_closed`.
    let scenario = Scenario::new();
    scenario.on_sql(
        "SELECT 1 FROM dual",
        Action::query(QuerySource::Fixed(QueryPlan::new(
            vec![ColumnSpec::new("N", SqlType::Number)],
            vec![vec![ScriptValue::from(1_i64)]],
        ))),
    );
    let mut connection = connect(&scenario);
    let mut outcome = connection
        .execute(&Statement::new("SELECT 1 FROM dual"))
        .expect("execute");
    let mut cursor = outcome.take_cursor().expect("cursor");
    connection.close().expect("close");

    let error = cursor.fetch_batch(one(10)).expect_err("fetch must report");
    assert_eq!(error.kind(), ErrorKind::DriverInternal);
    assert_eq!(error.session_state(), SessionState::Lost);

    cursor
        .close()
        .expect("but close has nothing left to release, so it succeeds");
}

#[test]
fn a_deadline_can_be_scripted_to_destroy_the_session() {
    // Spike U-6: upstream's recovery path reads the reset reply with the
    // expired timeout still armed, so a deadline on a PL/SQL sleep returns
    // `NetworkLost` with the connection gone rather than a clean `Timeout`.
    let scenario = Scenario::new();
    scenario.set_capabilities(Capabilities::none().with_cancel(CancelKind::PreArmedDeadline));
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN DBMS_SESSION.SLEEP(20); END;",
        Action::Block(
            BlockSpec::new(std::sync::Arc::clone(&gate)).with_timeout_error(
                ScriptedError::new(ErrorKind::NetworkLost, "connection lost during recovery")
                    .with_session_state(SessionState::Lost),
            ),
        ),
    );

    let mut connection = connect(&scenario);
    let error = connection
        .execute(
            &Statement::new("BEGIN DBMS_SESSION.SLEEP(20); END;")
                .with_deadline(Duration::from_millis(100)),
        )
        .expect_err("the deadline fires");
    assert_eq!(error.kind(), ErrorKind::NetworkLost);
    assert_eq!(error.session_state(), SessionState::Lost);
}

#[test]
fn an_unobserved_cancel_is_delivered_and_changes_nothing() {
    // Spike U-7: `ALTER SYSTEM CANCEL SQL` stops the statement server-side in
    // half a second, and the client never sees ORA-01013 â€” it waits for its own
    // deadline.
    let scenario = Scenario::new();
    scenario.set_capabilities(Capabilities::none().with_cancel(CancelKind::Native));
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN long_running; END;",
        Action::Block(BlockSpec::new(std::sync::Arc::clone(&gate)).with_unobserved_cancel()),
    );

    let mut connection = connect(&scenario);
    let cancel = connection.cancel_handle();
    let gate_for_thread = std::sync::Arc::clone(&gate);
    let canceller = thread::spawn(move || {
        assert!(gate_for_thread.wait_until_blocked(Duration::from_secs(5)));
        cancel.request_cancel().expect("cancel is delivered")
    });

    let error = connection
        .execute(
            &Statement::new("BEGIN long_running; END;").with_deadline(Duration::from_millis(200)),
        )
        .expect_err("the deadline, not the cancel, is what stops it");
    assert_eq!(error.kind(), ErrorKind::Timeout);
    assert_eq!(
        canceller.join().expect("canceller thread"),
        CancelOutcome::Requested,
        "the driver honestly believes it delivered the request, because it did"
    );
}

#[test]
fn a_late_cancel_can_land_on_the_next_statement() {
    // The race the driver contract cannot close: `request_cancel` carries no
    // statement identity, so a driver is free to latch a cancel that arrived
    // when nothing was running. `db-core` narrows the window; it cannot remove
    // it, and the honest thing is to have the nasty behaviour testable.
    let scenario = Scenario::new();
    scenario.set_capabilities(Capabilities::none().with_cancel(CancelKind::Native));
    scenario.set_late_cancel_lands_on_next_statement(true);
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN long_running; END;",
        Action::Block(BlockSpec::new(std::sync::Arc::clone(&gate))),
    );

    let mut connection = connect(&scenario);
    // Nothing is running; the driver keeps the request anyway.
    connection
        .cancel_handle()
        .request_cancel()
        .expect("cancel is accepted");

    let error = connection
        .execute(&Statement::new("BEGIN long_running; END;"))
        .expect_err("the latched cancel hit a statement it was never meant for");
    assert_eq!(error.kind(), ErrorKind::Cancelled);
}

#[test]
fn a_cancel_can_be_scripted_to_fail_outright() {
    let scenario = Scenario::new();
    scenario.set_capabilities(Capabilities::none().with_cancel(CancelKind::Native));
    scenario.fail_cancel(ScriptedError::new(ErrorKind::NetworkLost, "already gone"));
    let connection = connect(&scenario);
    let error = connection
        .cancel_handle()
        .request_cancel()
        .expect_err("the cancel path discovered the session was already gone");
    assert_eq!(error.kind(), ErrorKind::NetworkLost);
    assert_eq!(error.session_state(), SessionState::Lost);
}

#[test]
fn a_panicking_action_panics_inside_the_driver_call() {
    // `db-core` promises to contain this; the mock's job is to be able to
    // produce it. (The real primary driver turns such a panic into a process
    // abort â€” spike U-4 â€” which nothing can contain.)
    let scenario = Scenario::new();
    scenario.on_sql(
        "SELECT boom FROM dual",
        Action::Panic("reldex-driver-mock: scripted panic".to_owned()),
    );
    let mut connection = connect(&scenario);
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = connection.execute(&Statement::new("SELECT boom FROM dual"));
    }));
    assert!(panicked.is_err(), "the scripted action must actually panic");
}

#[test]
fn a_locking_query_is_still_a_query_but_opens_a_transaction() {
    let scenario = Scenario::new();
    let plan = QueryPlan::new(
        vec![ColumnSpec::new("ID", SqlType::Number)],
        vec![vec![ScriptValue::from(1_i64)]],
    );
    scenario.on_sql(
        "SELECT id FROM t FOR UPDATE",
        Action::locking_query(QuerySource::Fixed(plan)),
    );
    let mut connection = connect(&scenario);
    assert_eq!(connection.transaction_state(), TransactionState::Inactive);
    let outcome = connection
        .execute(&Statement::new("SELECT id FROM t FOR UPDATE"))
        .expect("execute");
    assert_eq!(
        outcome.statement_kind(),
        StatementKind::Query,
        "a driver has no honest classification for this other than `Query`"
    );
    assert_eq!(
        connection.transaction_state(),
        TransactionState::Active,
        "which is exactly why the core cannot rely on the statement kind alone"
    );
}
