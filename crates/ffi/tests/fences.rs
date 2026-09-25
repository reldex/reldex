//! The fence around `unsafe` (ADR-0003 D2), enforced the way
//! `crates/db-core/tests/dependency_rules.rs` enforces the dependency
//! direction: as a test, so a violation fails CI instead of quietly landing.
//!
//! Two properties:
//!
//! 1. the workspace still **denies** `unsafe_code`;
//! 2. the only code that opts out is this crate — the single FFI boundary —,
//!    `mobile-link-check`, the disposable spike-S6 link probe whose whole
//!    reason to exist is one `extern "C"` symbol, and one *file* of
//!    `reldex-secrets`: the Windows Credential Manager calls (ADR-0007 S5),
//!    not the rest of that crate.
//!
//! Another entry here is not a lint failure to be silenced; it is an
//! architecture decision that needs an ADR.

use std::fs;
use std::path::{Path, PathBuf};

/// Paths (relative to `crates/`) allowed to opt out, with why: a crate
/// directory allows every file in it, a file path allows that file only.
const ALLOWED: [(&str, &str); 3] = [
    (
        "ffi",
        "the single FFI boundary; ADR-0003 D2 makes it the one exception",
    ),
    (
        "mobile-link-check",
        "a build/link probe for spike S6, not product code",
    ),
    (
        "secrets/src/wincred.rs",
        "the Credential Manager calls, Windows only; ADR-0007 S5",
    ),
];

fn is_allowed(relative: &str) -> bool {
    ALLOWED.iter().any(|(path, _)| {
        relative == *path
            || relative
                .strip_prefix(path)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Whether `text` lifts the `unsafe_code` lint anywhere: `allow(...)` or
/// `expect(...)` (inner or outer attribute, `cfg_attr` included) whose
/// parenthesised list names `unsafe_code`, in any position and across line
/// breaks. A lint whose name merely contains it does not count.
fn opts_out_of_unsafe_code(text: &str) -> bool {
    ["allow(", "expect("].iter().any(|opener| {
        text.match_indices(opener).any(|(start, _)| {
            let list = &text[start + opener.len()..];
            let list = &list[..list.find(')').unwrap_or(list.len())];
            list.split(|c: char| c == ',' || c.is_whitespace())
                .any(|item| item == "unsafe_code")
        })
    })
}

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
fn only_the_ffi_boundary_the_link_probe_and_the_credential_calls_allow_unsafe_code() {
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
        if !opts_out_of_unsafe_code(&text) {
            continue;
        }
        let relative = file
            .strip_prefix(&crates)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        if relative.starts_with("ffi/") {
            found_ffi = true;
        }
        if !is_allowed(&relative) {
            offenders.push(relative);
        }
    }

    assert!(
        offenders.is_empty(),
        "these files opt out of the workspace's `unsafe_code = \"deny\"` but are not on the \
         allowed list: {offenders:?}. Adding one is an architecture decision (ADR-0003 D2, \
         ADR-0007 S5), not a lint to silence."
    );
    assert!(
        found_ffi,
        "reldex-ffi must carry the opt-out; without it the crate cannot be the boundary"
    );
}

#[test]
fn every_way_of_lifting_the_lint_is_seen() {
    for lifted in [
        "#![allow(unsafe_code)]",
        "#![allow(\n    unsafe_code,\n    reason = \"x\"\n)]",
        "#[expect(unsafe_code)]",
        "#![expect(unsafe_code, reason = \"x\")]",
        "#[allow(dead_code, unsafe_code)]",
        "#![cfg_attr(windows, allow(unsafe_code))]",
    ] {
        assert!(opts_out_of_unsafe_code(lifted), "{lifted}");
    }
    for not_lifted in [
        "#![deny(unsafe_code)]",
        "#![forbid(unsafe_code)]",
        "#![allow(dead_code)]",
        "#![allow(clippy::undocumented_unsafe_code_blocks)]",
    ] {
        assert!(!opts_out_of_unsafe_code(not_lifted), "{not_lifted}");
    }
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
    //
    // Raised once, deliberately, in M2.11: that task adds six FFI families in
    // one change (server output control, statement splitting, metadata,
    // settings/profiles/the local store's composition root, credentials, and
    // history/worksheets/layout) on top of the session family this limit was
    // originally sized for. 12,000 is not "however big M2.11 happens to
    // land" — it is sized with headroom above the actual total so the next
    // *unplanned* crossing is still a signal, and ADR-0003's M2.11 amendment
    // records the new number and the reasoning next to this one. Growing
    // past 12,000 without raising this again, deliberately, is the bug this
    // test exists to catch; reaching it is also the cue to consider whether
    // the composition-root families (M2.11 items 5-7) belong in a sibling
    // crate of their own rather than growing `crates/ffi` further.
    let mut files = Vec::new();
    rust_files(&workspace_root().join("crates/ffi/src"), &mut files);
    let lines: usize = files
        .iter()
        .filter_map(|file| fs::read_to_string(file).ok())
        .map(|text| text.lines().count())
        .sum();
    assert!(
        lines < 12_000,
        "crates/ffi/src is {lines} lines; ADR-0003 D2 expects a boundary a reviewer can read \
         in one sitting (raised once already, in M2.11 — see the comment above and ADR-0003's \
         M2.11 amendment)"
    );
}
