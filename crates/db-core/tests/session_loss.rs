//! Session-loss semantics: network loss mid-fetch marks the session lost,
//! subsequent calls fail fast, and the manager never silently reconnects or
//! replaces it (`SPEC.md` §18).

mod support;

use reldex_db_core::{SessionState, Statement};
use reldex_db_driver_api::ErrorKind;
use reldex_driver_mock::{Action, ColumnSpec, QueryPlan, QuerySource, ScriptValue, ScriptedError};

#[test]
fn network_loss_mid_fetch_marks_the_session_lost_and_fails_fast_afterwards() {
    let scenario = support::scenario();
    let columns = vec![ColumnSpec::new("N", reldex_db_driver_api::SqlType::Number)];
    let rows: Vec<Vec<ScriptValue>> = (0..10_i64).map(|v| vec![ScriptValue::from(v)]).collect();
    let plan = QueryPlan::new(columns, rows).with_fail_on_batch(
        2,
        ScriptedError::new(ErrorKind::NetworkLost, "connection reset mid-fetch"),
    );
    scenario.on_sql("SELECT * FROM big", Action::Query(QuerySource::Fixed(plan)));

    let session = support::open(&scenario);
    let outcome = session
        .execute(Statement::new("SELECT * FROM big"))
        .wait()
        .expect("execute should succeed");
    let result = outcome.result.expect("query produced a result set");

    let first = session
        .fetch_batch(result, support::n(3))
        .wait()
        .expect("first batch is fine");
    assert_eq!(first.row_count(), 3);
    assert!(!session.is_lost());

    let error = session
        .fetch_batch(result, support::n(3))
        .wait()
        .expect_err("the second batch is scripted to lose the network");
    assert_eq!(error.kind(), ErrorKind::NetworkLost);

    assert!(
        session.is_lost(),
        "a NetworkLost error must move the session to Lost"
    );
    assert_eq!(session.session_state(), SessionState::Lost);

    let connection_id = session.connection_id();

    // Every later request fails fast with a clear error instead of touching
    // the driver again, and the session is never silently replaced: the
    // connection id never changes and no reconnect is attempted.
    let error = session
        .fetch_batch(result, support::n(3))
        .wait()
        .expect_err("further fetches on the lost session must fail fast");
    assert_eq!(error.session_state(), SessionState::Lost);

    let error = session
        .execute(Statement::new("SELECT 1 FROM dual"))
        .wait()
        .expect_err("new statements on the lost session must fail fast too");
    assert_eq!(error.session_state(), SessionState::Lost);

    let error = session
        .commit()
        .wait()
        .expect_err("commit must also fail fast");
    assert_eq!(error.session_state(), SessionState::Lost);

    assert_eq!(
        session.connection_id(),
        connection_id,
        "db-core must never silently reconnect or replace a lost session"
    );
}
