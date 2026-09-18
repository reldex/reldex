//! Test-support mock database driver.
//!
//! Responsibility: will implement the `reldex-db-driver-api` contract with
//! an in-memory/fake backend so `reldex-db-core` logic can be tested without
//! a real database (`docs/architecture/ARCHITECTURE.md` §4). Integration
//! tests that require a real database stay separate from these tests
//! (`AGENTS.md`, "Testing").
//!
//! Dependency rules (`docs/architecture/ARCHITECTURE.md` §2):
//! - Allowed dependencies: `reldex-db-driver-api` only.
//! - Forbidden: depending on `reldex-db-core` or on any other
//!   `reldex-driver-*` crate.
//!
//! This is a Phase 0 skeleton (Workstream A): intentionally empty.
