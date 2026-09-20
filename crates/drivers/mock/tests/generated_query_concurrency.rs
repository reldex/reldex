//! Proves that a statement blocked on one session never stalls another
//! session's progress — the second half of the S15 FFI/UI spike's claim
//! (the first half, scrolling a 1,000,000-row result, is
//! `tests/generated_query.rs`).
//!
//! `docs/decisions/0002-driver-api-and-concurrency-model.md` D1 gives each
//! `db-core` session its own worker OS thread, so two sessions are independent
//! as long as nothing below `db-core` secretly serialises them.
//! `reldex-driver-mock` must not depend on `reldex-db-core`
//! (`crates/db-core/tests/dependency_rules.rs` forbids any
//! `reldex-driver-*` crate from doing so, dev-dependency included), so this
//! test cannot drive the claim through real `db-core` sessions. It reproduces
//! the same shape directly instead: two native threads, each owning its own
//! `MockConnection` opened from the same `Scenario`, which is exactly what a
//! `db-core` worker thread does with the connection it owns.

mod support;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use reldex_db_driver_api::{Statement, StatementKind};
use reldex_driver_mock::{Action, BlockGate, BlockSpec, GeneratedQuerySpec, Scenario};
use support::{connect, one};

#[test]
fn a_blocked_statement_on_one_connection_never_stalls_a_generated_query_on_another() {
    let scenario = Scenario::new();
    let gate = BlockGate::new();
    scenario.on_sql(
        "BEGIN long_running; END;",
        Action::Block(
            BlockSpec::new(Arc::clone(&gate))
                .with_release_statement_kind(StatementKind::PlSqlBlock),
        ),
    );
    const ROWS: u64 = 50_000;
    scenario.on_sql(
        "SELECT * FROM big",
        Action::GeneratedQuery(
            GeneratedQuerySpec::s14_shape(ROWS, 1).with_per_fetch_latency(Duration::from_millis(1)),
        ),
    );

    // Session A: blocks until this test explicitly releases it. Nothing
    // releases the gate until after session B has finished below, so
    // `blocker` genuinely cannot complete early — this is not a timing race.
    let blocking_scenario = Arc::clone(&scenario);
    let blocker = thread::spawn(move || {
        let mut connection = connect(&blocking_scenario);
        connection.execute(&Statement::new("BEGIN long_running; END;"))
    });
    assert!(
        gate.wait_until_blocked(Duration::from_secs(5)),
        "session A's statement should have reached the gate"
    );

    // Session B: its own connection, its own (simulated) thread, streaming a
    // sizeable generated result to completion while session A sits blocked.
    let other_scenario = Arc::clone(&scenario);
    let other = thread::spawn(move || {
        let mut connection = connect(&other_scenario);
        let mut outcome = connection
            .execute(&Statement::new("SELECT * FROM big"))
            .expect("execute");
        let mut cursor = outcome.take_cursor().expect("cursor");
        let mut total = 0_u64;
        loop {
            let batch = cursor.fetch_batch(one(1_000)).expect("fetch");
            if batch.is_empty() {
                break;
            }
            total += batch.row_count() as u64;
        }
        cursor.close().expect("close");
        total
    });

    let total = other.join().expect("session B's thread should not panic");
    assert_eq!(total, ROWS, "session B must still receive every row");

    // Proof of independence: session A is still parked. It cannot have
    // finished, because nothing has released it yet.
    assert!(
        !blocker.is_finished(),
        "session A finished before being released; it was never really blocking anything"
    );

    gate.release();
    let result = blocker.join().expect("session A's thread should not panic");
    result.expect("the released statement succeeds");
}
