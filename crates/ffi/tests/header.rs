//! `include/reldex.h` is generated output that is committed, so it can go
//! stale silently: someone adds an export, the Rust side compiles, and the
//! adapter never sees it. These tests are the guard.
//!
//! Two layers, because they fail in different environments:
//!
//! 1. **Always**: every `#[unsafe(no_mangle)] pub extern "C"` function and
//!    every type named in `cbindgen.toml`'s `[export] include` must appear in
//!    the committed header. This needs no tools, so it runs everywhere — and
//!    it catches the realistic mistake, which is adding an export and
//!    forgetting to regenerate.
//! 2. **When `cbindgen` is on PATH**: regenerate into a temporary file and
//!    diff. This is exact, and it is what `crates/ffi/gen-header.sh --check`
//!    runs in CI. Without cbindgen the test reports that it skipped rather
//!    than passing quietly.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn header() -> String {
    fs::read_to_string(crate_dir().join("include/reldex.h"))
        .expect("the generated header is committed next to the crate")
}

fn sources() -> Vec<(PathBuf, String)> {
    let src = crate_dir().join("src");
    let mut out = Vec::new();
    let entries = fs::read_dir(&src).expect("the crate has a src directory");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|extension| extension == "rs")
            && let Ok(text) = fs::read_to_string(&path)
        {
            out.push((path, text));
        }
    }
    assert!(out.len() > 5, "expected to read the crate's modules");
    out
}

/// The names of the functions the crate exports, read from the source rather
/// than from a list this test would also have to maintain.
fn exported_functions() -> Vec<String> {
    let mut names = Vec::new();
    for (path, text) in sources() {
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            if line.trim() != "#[unsafe(no_mangle)]" {
                continue;
            }
            // The signature may be split over several lines; the name is on
            // the first one.
            let signature = lines.peek().copied().unwrap_or_else(|| {
                panic!("a no_mangle attribute with nothing after it in {path:?}")
            });
            let name = signature
                .split("fn ")
                .nth(1)
                .and_then(|rest| rest.split(['(', '<']).next())
                .unwrap_or_else(|| {
                    panic!("could not read the exported name from `{signature}` in {path:?}")
                });
            names.push(name.trim().to_owned());
        }
    }
    names.sort_unstable();
    names.dedup();
    names
}

/// The types `cbindgen.toml` forces into the header because no signature
/// reaches them (every enum crosses as an `int32_t`).
fn forced_exports() -> Vec<String> {
    let config = fs::read_to_string(crate_dir().join("cbindgen.toml"))
        .expect("the cbindgen config is committed");
    let start = config
        .find("include = [")
        .expect("`[export] include` is what keeps the enums in the header");
    let end = config[start..]
        .find(']')
        .expect("the include list is closed")
        + start;
    config[start..end]
        .lines()
        .skip(1)
        .filter_map(|line| line.trim().trim_end_matches(',').strip_prefix('"'))
        .filter_map(|line| line.strip_suffix('"'))
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_export_reaches_the_header() {
    let header = header();
    let functions = exported_functions();
    assert!(
        functions.len() >= 25,
        "only found {} exported functions; the parser above is probably broken",
        functions.len()
    );

    let missing: Vec<&String> = functions
        .iter()
        .filter(|name| !header.contains(name.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "these exports are not in include/reldex.h: {missing:?}. Run crates/ffi/gen-header.sh \
         and commit the result."
    );
}

#[test]
fn every_forced_type_reaches_the_header() {
    let header = header();
    let forced = forced_exports();
    assert!(
        forced.len() >= 15,
        "only parsed {} forced exports from cbindgen.toml",
        forced.len()
    );
    // A definition, not merely a forward declaration: the adapter has to be
    // able to switch on the enum and read the struct.
    let defined: Vec<&str> = header
        .lines()
        .map(str::trim_end)
        .filter_map(|line| {
            line.strip_prefix("enum ")
                .or_else(|| line.strip_prefix("typedef struct "))
        })
        .map(|rest| rest.trim_end_matches(" {"))
        .collect();
    let missing: Vec<&String> = forced
        .iter()
        .filter(|name| !defined.contains(&name.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "cbindgen.toml lists these types but the header does not define them: {missing:?}"
    );
}

#[test]
fn the_abi_version_macro_matches_the_rust_constants() {
    // The adapter checks `reldex_abi_version() == RELDEX_ABI_VERSION` before
    // anything else (ADR-0003 D7). The header builds that macro from two
    // constants cbindgen emits; if either drifts the check silently passes on
    // a mismatched library.
    let header = header();
    for (name, value) in [
        (
            "RELDEX_ABI_VERSION_MAJOR",
            reldex_ffi::RELDEX_ABI_VERSION_MAJOR,
        ),
        (
            "RELDEX_ABI_VERSION_MINOR",
            reldex_ffi::RELDEX_ABI_VERSION_MINOR,
        ),
    ] {
        let needle = format!("#define {name} {value}");
        assert!(
            header.contains(&needle),
            "the header does not define `{needle}`"
        );
    }
    assert!(
        header.contains("#define RELDEX_ABI_VERSION "),
        "the packed macro the adapter compares against is missing"
    );
    let packed =
        (reldex_ffi::RELDEX_ABI_VERSION_MAJOR << 16) | reldex_ffi::RELDEX_ABI_VERSION_MINOR;
    assert_eq!(
        reldex_ffi::reldex_abi_version(),
        packed,
        "the function and the macro must agree on the packing"
    );
}

#[test]
fn the_committed_header_matches_what_cbindgen_generates() {
    let Some(cbindgen) = which("cbindgen") else {
        // Not a silent pass: CI runs `gen-header.sh --check`, and a developer
        // without the tool should know this check did not run.
        eprintln!(
            "skipped: `cbindgen` is not on PATH. Install it with \
             `cargo install cbindgen --version 0.29.4` to check the header here; CI runs \
             `crates/ffi/gen-header.sh --check` regardless."
        );
        return;
    };

    let output = Path::new(env!("CARGO_TARGET_TMPDIR")).join("reldex-header-check.h");
    let status = Command::new(cbindgen)
        .current_dir(crate_dir())
        .args([
            "--config",
            "cbindgen.toml",
            "--crate",
            "reldex-ffi",
            "--quiet",
        ])
        .arg("--output")
        .arg(&output)
        .status()
        .expect("cbindgen runs");
    assert!(status.success(), "cbindgen failed: {status}");

    let generated = fs::read_to_string(&output).expect("cbindgen wrote a header");
    let committed = header();
    assert_eq!(
        normalize(&committed),
        normalize(&generated),
        "include/reldex.h is stale. Run crates/ffi/gen-header.sh and commit the result."
    );
}

/// Line endings differ between a checkout with `core.autocrlf` and cbindgen's
/// own output; the contract is the content, not the bytes.
fn normalize(text: &str) -> Vec<&str> {
    text.lines().map(str::trim_end).collect()
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<String> = std::env::var("PATHEXT")
        .map(|value| {
            value
                .split(';')
                .filter(|part| !part.is_empty())
                .map(str::to_lowercase)
                .collect()
        })
        .unwrap_or_default();
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
        for extension in &extensions {
            let candidate = directory.join(format!("{program}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
