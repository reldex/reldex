//! Shared helpers for `reldex-db-core`'s integration tests. Not a test binary
//! itself (no `tests/support.rs`), so each test file pulls it in with
//! `mod support;`.
//!
//! Each test binary only uses a subset of these, so unused-item warnings here
//! are expected and not a signal of dead production code.
#![allow(dead_code)]

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use std::sync::mpsc;
use std::thread;

use reldex_db_core::{
    ConnectionParams, DatabaseDriver, DatabaseSession, SessionLimits, SessionManager,
};
use reldex_db_driver_api::{Credentials, Endpoint};
use reldex_driver_mock::{MockDriver, Scenario};

/// A fresh, empty scenario with sane test defaults (see [`Scenario::new`]).
#[must_use]
pub(crate) fn scenario() -> Arc<Scenario> {
    Scenario::new()
}

/// Opens a session against `scenario` through the real `db-core` session
/// layer (spawns a worker thread and connects on it).
#[must_use]
pub(crate) fn open(scenario: &Arc<Scenario>) -> DatabaseSession {
    open_with(scenario, SessionManager::new())
}

/// Opens a session with non-default [`SessionLimits`].
#[must_use]
pub(crate) fn open_with_limits(scenario: &Arc<Scenario>, limits: SessionLimits) -> DatabaseSession {
    open_with(scenario, SessionManager::new().with_limits(limits))
}

fn open_with(scenario: &Arc<Scenario>, manager: SessionManager) -> DatabaseSession {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(MockDriver::new(Arc::clone(scenario)));
    let params = ConnectionParams::new(
        Endpoint::ConnectString("mock".to_owned()),
        Credentials::External,
    );
    manager
        .open_session(driver, params)
        .expect("session should open against the mock driver")
}

/// Runs `f` on its own thread and fails the test if it does not finish within
/// `timeout`, rather than letting a hang block the whole suite forever.
pub(crate) fn with_timeout_guard(timeout: Duration, f: impl FnOnce() + Send + 'static) {
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

/// Shorthand for a non-zero row/byte count in test call sites.
#[must_use]
pub(crate) fn n(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("test-provided count must be non-zero")
}

/// A short bound for waits that must complete quickly in a correct
/// implementation, without being so tight that normal scheduling jitter
/// causes a flaky failure.
#[must_use]
pub(crate) fn short_timeout() -> Duration {
    Duration::from_secs(5)
}
