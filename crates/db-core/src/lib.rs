//! Vendor-neutral core: sessions, transactions, query execution, results,
//! metadata, and workspace state (`docs/architecture/ARCHITECTURE.md` §3).
//!
//! Dependency rules (`docs/architecture/ARCHITECTURE.md` §2):
//! - Allowed dependencies: `reldex-db-driver-api` only.
//! - Forbidden: depending on any concrete `reldex-driver-*` crate, or on
//!   Qt/QML/FFI/UI code. This crate must remain UI-independent.
//!
//! This is a Phase 0 skeleton (Workstream A): intentionally empty. Session,
//! transaction, query, result, and error-model design are open ADR
//! questions (`docs/architecture/ARCHITECTURE.md` §13) and are out of scope
//! for this skeleton. `crates/db-core/tests/dependency_rules.rs` enforces
//! the dependency-direction rule above so it cannot regress silently.
