//! Test-support mock database driver (`docs/exec-plans/active/phase-0.md`
//! Workstream A).
//!
//! Implements the full `reldex-db-driver-api` contract with an in-memory,
//! deterministic, **scriptable** backend, so `reldex-db-core` can be tested
//! without a database (`docs/architecture/ARCHITECTURE.md` §4). It contains
//! no SQL parser: a test scripts behaviour by registering canned [`Action`]s
//! against exact statement text or a predicate ([`Scenario::on_sql`],
//! [`Scenario::on_predicate`]).
//!
//! Dependency rules (`docs/architecture/ARCHITECTURE.md` §2):
//! - Allowed dependencies: `reldex-db-driver-api` only.
//! - Forbidden: depending on `reldex-db-core` or on any other
//!   `reldex-driver-*` crate.
//!
//! # Shape of a test
//!
//! ```
//! use std::num::NonZeroUsize;
//! use reldex_db_driver_api::{
//!     ConnectionParams, Credentials, DatabaseDriver, Endpoint, Statement,
//! };
//! use reldex_driver_mock::{Action, ColumnSpec, MockDriver, QueryPlan, QuerySource, Scenario, ScriptValue};
//!
//! let scenario = Scenario::new();
//! scenario.on_sql(
//!     "SELECT name FROM dual",
//!     Action::query(QuerySource::Fixed(QueryPlan::new(
//!         vec![ColumnSpec::new("NAME", reldex_db_driver_api::SqlType::VARCHAR)],
//!         vec![vec![ScriptValue::from("hello")]],
//!     ))),
//! );
//!
//! let driver = MockDriver::new(scenario);
//! let params = ConnectionParams::new(
//!     Endpoint::ConnectString("mock".to_owned()),
//!     Credentials::External,
//! );
//! let mut connection = driver.connect(&params).expect("connect");
//! let mut outcome = connection
//!     .execute(&Statement::new("SELECT name FROM dual"))
//!     .expect("execute");
//! let mut cursor = outcome.take_cursor().expect("has rows");
//! let batch = cursor
//!     .fetch_batch(NonZeroUsize::new(10).expect("non-zero"))
//!     .expect("fetch");
//! assert_eq!(batch.row_count(), 1);
//! ```

mod connection;
mod cursor;
mod scenario;

pub use connection::{MockConnection, MockDriver};
pub use cursor::MockCursor;
pub use scenario::{
    Action, BlockGate, BlockSpec, ColumnSpec, Counts, Matcher, QueryPlan, QuerySource, Scenario,
    ScriptValue, ScriptedError,
};
