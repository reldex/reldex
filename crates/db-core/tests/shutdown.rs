//! Dropping a session must not hang or leak its worker thread.

mod support;

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use reldex_db_core::Statement;
use reldex_driver_mock::{Action, ScriptValue};

/// Runs `f` on its own thread and fails the test if it does not finish
/// within `timeout`, rather than letting a hang block the whole suite
/// forever.
fn with_timeout_guard(timeout: Duration, f: impl FnOnce() + Send + 'static) {
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    match rx.recv_timeout(timeout) {
        Ok(()) => {
            handle.join().expect("guarded thread should not panic");
        }
        Err(_) => panic!("operation did not complete within {timeout:?}; it hung"),
    }
}

#[test]
fn dropping_an_idle_session_does_not_hang() {
    with_timeout_guard(Duration::from_secs(5), || {
        let scenario = support::scenario();
        let session = support::open(&scenario);
        drop(session);
    });
}

#[test]
fn dropping_a_session_with_a_possibly_active_transaction_does_not_hang_or_commit() {
    with_timeout_guard(Duration::from_secs(5), || {
        let scenario = support::scenario();
        scenario.on_sql(
            "INSERT INTO t VALUES ('a')",
            Action::Dml {
                rows_affected: 1,
                insert: Some(("t".to_owned(), vec![ScriptValue::from("a")])),
            },
        );
        let session = support::open(&scenario);
        session
            .execute(Statement::new("INSERT INTO t VALUES ('a')"))
            .wait()
            .expect("insert");
        assert!(session.has_possibly_active_transaction());
        drop(session);

        assert!(
            scenario.committed_rows("t").is_empty(),
            "dropping a session must never silently commit a possibly-active transaction"
        );
    });
}

#[test]
fn dropping_the_session_manager_side_does_not_leak_or_hang_across_many_sessions() {
    with_timeout_guard(Duration::from_secs(10), || {
        let scenario = support::scenario();
        for _ in 0..16 {
            let session = support::open(&scenario);
            drop(session);
        }
    });
}
