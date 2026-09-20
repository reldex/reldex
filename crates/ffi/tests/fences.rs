//! The fence around `unsafe` (ADR-0003 D2), enforced the way
//! `crates/db-core/tests/dependency_rules.rs` enforces the dependency
//! direction: as a test, so a violation fails CI instead of quietly landing.
//!
//! Two properties:
//!
//! 1. the workspace still **denies** `unsafe_code`;
//! 2. the only crates that opt out are this one — the single FFI boundary —
//!    and `mobile-link-check`, the disposable spike-S6 link probe whose whole
//!    reason to exist is one `extern "C"` symbol.
//!
//! A third crate appearing here is not a lint failure to be silenced; it is an
//! architecture decision that needs an ADR.

use std::fs;
use std::path::{Path, PathBuf};

/// Crate directories (relative to `crates/`) allowed to opt out, with why.
const ALLOWED: [(&str, &str); 2] = [
    (
        "ffi",
        "the single FFI boundary; ADR-0003 D2 makes it the one exception",
    ),
    (
        "mobile-link-check",
        "a build/link probe for spike S6, not product code",
    ),
];

fn workspace_root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `<root>/crates/ffi`.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the manifest lives two levels below the workspace root")
        .to_path_buf()
}

fn rust_files(root: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            rust_files(&path, into);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            into.push(path);
        }
    }
}

#[test]
fn only_the_ffi_boundary_and_the_link_probe_allow_unsafe_code() {
    let root = workspace_root();
    let crates = root.join("crates");
    let mut files = Vec::new();
    rust_files(&crates, &mut files);
    assert!(
        files.len() > 20,
        "expected to find the workspace's sources under {}",
        crates.display()
    );

    let mut offenders = Vec::new();
    let mut found_ffi = false;
    for file in &files {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        if !text.contains("allow(unsafe_code") && !text.contains("allow(\n    unsafe_code") {
            continue;
        }
        let relative = file
            .strip_prefix(&crates)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        let owner = relative.split('/').next().unwrap_or_default().to_owned();
        if owner == "ffi" {
            found_ffi = true;
        }
        if !ALLOWED.iter().any(|(name, _)| *name == owner) {
            offenders.push(relative);
        }
    }

    assert!(
        offenders.is_empty(),
        "these files opt out of the workspace's `unsafe_code = \"deny\"` but are not the FFI \
         boundary: {offenders:?}. Adding a third such crate is an architecture decision \
         (ADR-0003 D2), not a lint to silence."
    );
    assert!(
        found_ffi,
        "reldex-ffi must carry the opt-out; without it the crate cannot be the boundary"
    );
}

#[test]
fn the_workspace_still_denies_unsafe_code() {
    let manifest = fs::read_to_string(workspace_root().join("Cargo.toml"))
        .expect("the workspace manifest is readable");
    assert!(
        manifest.contains("unsafe_code = \"deny\""),
        "the workspace lint that makes this crate an exception must stay in place"
    );
}

#[test]
fn the_boundary_stays_small_enough_to_audit() {
    // ADR-0003 D2: "a reviewer must be able to read the whole crate in an
    // hour". This is a smoke alarm, not a budget: if the boundary grows past
    // it, the growth should be a deliberate, reviewed decision rather than
    // something that happened.
    let mut files = Vec::new();
    rust_files(&workspace_root().join("crates/ffi/src"), &mut files);
    let lines: usize = files
        .iter()
        .filter_map(|file| fs::read_to_string(file).ok())
        .map(|text| text.lines().count())
        .sum();
    assert!(
        lines < 6_000,
        "crates/ffi/src is {lines} lines; ADR-0003 D2 expects a boundary a reviewer can read \
         in one sitting"
    );
}
