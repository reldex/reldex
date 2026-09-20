//! Shared helpers for the generated-query integration tests. Not a test
//! binary itself (no `tests/support.rs`), so each test file pulls it in with
//! `mod support;`.
//!
//! Each test binary only uses a subset of these, so unused-item warnings here
//! are expected and not a signal of dead production code.
#![allow(dead_code)]

use std::num::NonZeroUsize;
use std::sync::Arc;

use reldex_db_driver_api::{
    ConnectionParams, Credentials, DatabaseConnection, DatabaseDriver, Endpoint,
};
use reldex_driver_mock::{MockDriver, Scenario};

/// Opens a connection to `scenario` through a fresh [`MockDriver`].
#[must_use]
pub(crate) fn connect(scenario: &Arc<Scenario>) -> Box<dyn DatabaseConnection> {
    let driver = MockDriver::new(Arc::clone(scenario));
    let params = ConnectionParams::new(
        Endpoint::ConnectString("mock".to_owned()),
        Credentials::External,
    );
    driver.connect(&params).expect("connect should succeed")
}

/// A non-zero `usize`, for `fetch_batch`'s `max_rows` parameter.
#[must_use]
pub(crate) fn one(max: usize) -> NonZeroUsize {
    NonZeroUsize::new(max).expect("non-zero")
}
