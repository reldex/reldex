//! Build/link probe for spike S6 (mobile cross-compile) — **not product
//! code**.
//!
//! `docs/decisions/0001-database-driver-strategy.md` ("Platform viability"
//! and the S6 row of the spike plan) asks whether the core plus the Oracle
//! thin driver — and in particular `rustls`'s default `aws-lc-rs` crypto
//! provider, which compiles C and assembly via `aws-lc-sys` — can be *built
//! and linked* for `aarch64-linux-android` and `aarch64-apple-ios`. Cargo can
//! answer "does it compile" with `cargo check -p ...`, but a `staticlib`
//! (iOS) or `cdylib` (Android) is the shape the future mobile core will
//! actually ship in, and only producing one exercises the *linker*, not just
//! the compiler — e.g. whether the NDK/Xcode toolchain can resolve every
//! symbol `aws-lc-sys`'s compiled C/assembly objects need.
//!
//! This crate exists **only** to be that probe:
//! [`reldex_link_check`] is a single `extern "C"` entry point that a native
//! (non-Rust) caller could invoke, referencing enough of
//! `reldex-driver-oracle-thin`'s and `reldex-db-core`'s public surface —
//! including the crypto-provider install path — that the whole dependency
//! graph, `aws-lc-sys` included, has to be present and linkable for this
//! crate to build at all.
//!
//! Per `AGENTS.md`/`SPEC.md` and `docs/exec-plans/active/phase-0-s6-mobile-cross-compile.md`:
//! **a green build of this crate on a mobile target is cross-compile
//! evidence only.** It proves nothing about running on a physical device —
//! no connect, no TLS handshake, no background/resume, no app packaging. See
//! that exec plan's "What this does NOT prove" section.

// The workspace denies `unsafe_code` (root `Cargo.toml`) because the
// *product* FFI boundary is an open architecture question
// (`docs/architecture/ARCHITECTURE.md` §13 item 2). This crate is not that
// boundary — it is a disposable spike-S6 probe whose only job is to exist as
// an `extern "C"` symbol a native caller could resolve, which in edition 2024
// requires the `#[unsafe(no_mangle)]` attribute form below. `[lints]
// workspace = true` in `Cargo.toml` cannot be combined with an overriding
// `[lints.rust]` table in the same manifest (cargo rejects that
// combination), so the opt-out has to happen here instead, scoped to this
// one crate rather than loosening the workspace default.
#![allow(unsafe_code, reason = "spike S6 link probe; see module docs above")]

use reldex_db_core::{DatabaseDriver, SessionManager};
use reldex_driver_oracle_thin::{OracleThinDriver, install_default_crypto_provider};

/// Touches enough of the core + Oracle thin driver's public API that
/// building this crate proves the whole dependency graph — `reldex-db-core`,
/// `reldex-driver-oracle-thin`, `oracledb`, `rustls`, and `aws-lc-sys`'s
/// compiled C/assembly — links for whatever target this crate is built for.
///
/// Deliberately does **no I/O**: it never calls `connect`, so it is safe to
/// build (and even to execute, though CI only builds it) for a target this
/// host cannot run. [`install_default_crypto_provider`] is the one call
/// picked specifically to force the `aws-lc-rs` default provider — the part
/// of the graph spike S6 cares most about — into the link, since it is the
/// documented entry point into that code rather than an implementation
/// detail this probe would otherwise have to reach around.
///
/// The return value has no meaning beyond "the calls above ran without a
/// panic"; a native caller only needs this symbol to exist and be callable.
#[unsafe(no_mangle)]
pub extern "C" fn reldex_link_check() -> u32 {
    install_default_crypto_provider();
    let driver = OracleThinDriver::new();
    let manager = SessionManager::new();
    let touched = driver.name().len() + manager.limits().max_lob_chunk_bytes().get();
    u32::try_from(touched).unwrap_or(u32::MAX)
}
