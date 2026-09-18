//! Thin (pure-Rust, non-OCI) Oracle Database driver.
//!
//! This crate implements the `reldex-db-driver-api` contract for the default,
//! OCI-free connection path to Oracle Database 19c and later (`SPEC.md` §7,
//! `docs/architecture/ARCHITECTURE.md` §4). Vendor-specific code and types live
//! only here; nothing from the underlying Oracle crate appears in this crate's
//! public API, so `reldex-db-core` and the UI never see an Oracle type.
//!
//! ```no_run
//! use reldex_db_driver_api::{
//!     ConnectionParams, Credentials, DatabaseDriver, Endpoint, Secret, Statement,
//! };
//! use reldex_driver_oracle_thin::OracleThinDriver;
//!
//! # fn main() -> reldex_db_driver_api::DbResult<()> {
//! let driver = OracleThinDriver::new();
//! let mut connection = driver.connect(&ConnectionParams::new(
//!     Endpoint::HostPort {
//!         host: "127.0.0.1".to_owned(),
//!         port: 1521,
//!         service: "RELDEX".to_owned(),
//!     },
//!     Credentials::UserPassword {
//!         username: std::env::var("RELDEX_TEST_ORACLE_USER").unwrap_or_default(),
//!         password: Secret::new(std::env::var("RELDEX_TEST_ORACLE_PASSWORD").unwrap_or_default()),
//!     },
//! ))?;
//! let outcome = connection.execute(&Statement::new("SELECT 1 FROM dual"))?;
//! # let _ = outcome;
//! # Ok(())
//! # }
//! ```
//!
//! # Dependencies (`AGENTS.md`, "Dependencies")
//!
//! **`oracledb`, pinned to `=26.0.0-beta.3`.** This is Oracle's own pure-Rust
//! thin driver (<https://github.com/oracle/rust-oracledb>, docs at
//! <https://oracle.github.io/rust-oracledb/>), adopted by **ADR-0001**
//! (`docs/decisions/0001-database-driver-strategy.md`). It speaks Oracle's TTC
//! protocol directly, so there is no Instant Client to ship, no `unsafe` FFI to
//! audit, and no `PATH`/`LD_LIBRARY_PATH` deployment problem — the three costs
//! ADR-0001 weighed the alternatives against. It is used under the Universal
//! Permissive License v1.0 / Apache 2.0.
//!
//! The pin is **exact** because the crate is pre-GA and its own documentation
//! says the API is subject to change; a caret range would let a beta bump break
//! the build silently. `26.0.0-beta.3` (published 2026-09-08) was the newest
//! version on crates.io when this crate was written. Upgrading is a deliberate
//! act: re-run the Phase 0 spikes, because several behaviours this wrapper
//! works around are undocumented internals rather than contracts.
//!
//! **Transport security.** `oracledb` reaches TLS (TCPS) through `rustls` 0.23,
//! whose default crypto provider is `aws-lc-rs`. That provider builds C and
//! assembly and therefore needs a C toolchain — MSVC on Windows, which the
//! Phase 0 machine has. It built without intervention, so the `ring` provider
//! was not needed; see `docs/exec-plans/active/phase-0-spike-results.md` for
//! the build measurement. TLS itself is **not** advertised in
//! [`Capabilities`](reldex_db_driver_api::Capabilities): the Phase 0 test
//! database has no TLS listener, so spike S8 has not run, and
//! [`connect`](reldex_db_driver_api::DatabaseDriver::connect) refuses
//! [`TlsMode::Required`](reldex_db_driver_api::TlsMode::Required) rather than
//! silently opening a plaintext connection.
//!
//! # What this driver can and cannot do
//!
//! The capabilities it reports are deliberately conservative — every `true` is
//! backed by a spike in `docs/exec-plans/active/phase-0-spike-results.md`:
//!
//! - **Cancellation is
//!   [`PreArmedDeadline`](reldex_db_driver_api::CancelKind::PreArmedDeadline),
//!   not [`Native`](reldex_db_driver_api::CancelKind::Native).** `oracledb`
//!   holds one mutex across a whole round trip and exposes no break/interrupt
//!   call, so a running statement cannot be interrupted on request from another
//!   thread. A deadline armed *before* the call through
//!   [`Statement::with_deadline`](reldex_db_driver_api::Statement::with_deadline)
//!   does work, and
//!   [`request_cancel`](reldex_db_driver_api::CancelHandle::request_cancel)
//!   reports
//!   [`NotInterruptible`](reldex_db_driver_api::CancelOutcome::NotInterruptible)
//!   with the time remaining so the UI can say what will actually happen.
//!   `SPEC.md` §24.8 therefore cannot be met by this driver alone today.
//! - **The transaction state is not exact.** The upstream crate tracks whether
//!   a transaction is in progress but does not expose it, so this driver
//!   reports [`Unknown`](reldex_db_driver_api::TransactionState::Unknown) after
//!   anything that might have opened one, and only claims
//!   [`Inactive`](reldex_db_driver_api::TransactionState::Inactive) after
//!   connect, commit, rollback and DDL.
//! - **An error position is available for PL/SQL only.** This upstream version
//!   discards the server's SQL error offset; the ORA-06550 line and column are
//!   recovered by parsing the message text.
//! - **Auto-commit is off** and nothing in this crate commits implicitly. DDL
//!   still commits server-side, which is reported through
//!   [`committed_implicitly`](reldex_db_driver_api::ExecutionOutcome::committed_implicitly)
//!   rather than hidden.
//!
//! # Values
//!
//! `NUMBER` is carried losslessly through the upstream type's canonical decimal
//! rendering into [`Number`](reldex_db_driver_api::Number) (40 significant
//! digits, the precision Oracle itself stores), never through `f64`. `DATE`,
//! `TIMESTAMP` and `TIMESTAMP WITH TIME ZONE` become
//! [`Timestamp`](reldex_db_driver_api::Timestamp) through its lenient
//! historical constructor, so a value the proleptic Gregorian calendar rejects
//! (a `1500-02-29` stored years ago, a BC date) is returned instead of failing
//! the fetch. `CLOB`, `NCLOB` and `BLOB` stay lazy locators streamed in bounded
//! chunks. A column whose type this contract cannot express becomes
//! [`Unsupported`](reldex_db_driver_api::ColumnData::Unsupported) text, so one
//! odd column never hides a whole table.
//!
//! # Threading
//!
//! Everything here is blocking and single-threaded per connection, as ADR-0002
//! requires: `reldex-db-core` owns one worker thread per session and every call
//! on a connection happens on it. The one exception is
//! [`cancel_handle`](reldex_db_driver_api::DatabaseConnection::cancel_handle),
//! whose handle is `Send + Sync` and never blocks on the connection's own lock.

mod binds;
mod classify;
mod conn;
mod cursor;
mod error;
mod lob;
mod value;

pub use conn::{EXT_STATEMENT_CACHE_SIZE, OracleThinDriver};
