//! Thin (pure-Rust, non-OCI) Oracle Database driver.
//!
//! Responsibility: will implement the `reldex-db-driver-api` contract for
//! the default, OCI-free connection path to Oracle Database 19c+
//! (`SPEC.md` §7, `docs/architecture/ARCHITECTURE.md` §4). Vendor-specific
//! code and types belong only in this crate and must never leak into
//! `reldex-db-core` or any generic API.
//!
//! Dependency rules (`docs/architecture/ARCHITECTURE.md` §2):
//! - Allowed dependencies: `reldex-db-driver-api` and, per ADR-0001, the
//!   pinned `oracledb` crate (to be added with the first implementation).
//! - Forbidden: depending on `reldex-db-core` or on any other
//!   `reldex-driver-*` crate.
//!
//! This is a Phase 0 skeleton (Workstream A): intentionally empty for now.
//! Per **ADR-0001** (`docs/decisions/0001-database-driver-strategy.md`) this
//! crate wraps Oracle's official `oracledb` crate (`oracle/rust-oracledb`:
//! pure Rust, thin, blocking), pinned to an exact version because it is
//! pre-GA. No `oracledb` type may appear in this crate's public API.
