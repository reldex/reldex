//! Phase 0 validation harness (`SPEC.md` §26): proves the database
//! architecture without the full Qt Quick desktop UI
//! (`docs/architecture/ARCHITECTURE.md` §3).
//!
//! Dependency rules (`docs/architecture/ARCHITECTURE.md` §2):
//! - Allowed dependencies: `reldex-db-core` only.
//! - Forbidden: depending directly on any `reldex-driver-*` crate, or on
//!   Qt/QML/FFI code; this binary must go through `reldex-db-core`.
//!
//! This is a Phase 0 skeleton (Workstream A): intentionally does no driver
//! or session work yet. It only proves the workspace builds and links
//! `reldex-db-core`.

// This is a CLI-only Phase 0 harness, not a UI: printing a one-line version
// banner to stdout is the intended output surface, so the workspace-wide
// `print_stdout` lint is deliberately allowed on `main`.
#[allow(clippy::print_stdout)]
fn main() {
    println!("reldex-core-poc {}", env!("CARGO_PKG_VERSION"));
}
