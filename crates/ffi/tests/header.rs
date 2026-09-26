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

/// A structural guard against `cbindgen.toml`'s `[export] include` allowlist
/// (ADR-0003 A27) silently losing an entry. `every_forced_type_reaches_the_
/// header` only checks names `cbindgen.toml` *already lists* -- it cannot
/// catch one being removed, because a type cbindgen never reaches through a
/// function signature just stops appearing in both the freshly generated
/// header and, once regenerated and committed, the checked-in one, with
/// `gen-header.sh --check` reporting "up to date" the entire time (this is
/// exactly how the M2.11 review round 1 gap went unnoticed: a fresh
/// generation was just as incomplete as the stale commit it was diffed
/// against). This test instead rediscovers every `#[repr(C)]`/`#[repr(i32)]`
/// `pub struct`/`pub enum` directly from `src/*.rs`, independently of
/// `cbindgen.toml`, and asserts each one is *defined* in the committed
/// header -- however cbindgen reached it, allowlist or a real signature.
#[test]
fn every_repr_c_type_reaches_the_header() {
    let header = header();
    let defined: Vec<&str> = header
        .lines()
        .map(str::trim_end)
        .filter_map(|line| {
            line.strip_prefix("enum ")
                .or_else(|| line.strip_prefix("typedef struct "))
        })
        .map(|rest| rest.trim_end_matches(" {"))
        .collect();

    let mut names: Vec<String> = Vec::new();
    for (path, text) in sources() {
        let lines: Vec<&str> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            let Some(rest) = trimmed
                .strip_prefix("pub enum ")
                .or_else(|| trimmed.strip_prefix("pub struct "))
            else {
                continue;
            };
            // `#[repr(C)]`/`#[repr(i32)]` may sit a few lines above the
            // declaration (derives and doc comments come between them, in
            // either order), so a small backward window rather than "the
            // line right above".
            let window_start = index.saturating_sub(8);
            let has_repr = lines[window_start..index].iter().any(|candidate| {
                let candidate = candidate.trim();
                candidate == "#[repr(C)]" || candidate == "#[repr(i32)]"
            });
            if !has_repr {
                continue;
            }
            let name = rest
                .split(['<', '{', '(', ' ', ':', ';'])
                .next()
                .unwrap_or_else(|| {
                    panic!("could not read a type name from `{trimmed}` in {path:?}")
                });
            names.push(name.trim().to_owned());
        }
    }
    names.sort_unstable();
    names.dedup();
    assert!(
        names.len() >= 30,
        "only found {} repr(C)/repr(i32) public types; the parser above is probably broken",
        names.len()
    );

    let missing: Vec<&String> = names
        .iter()
        .filter(|name| !defined.contains(&name.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "these #[repr(C)]/#[repr(i32)] types are not defined in include/reldex.h: {missing:?}. \
         If a type is intentionally never used by value in an exported function's signature, add \
         it to cbindgen.toml's `[export] include` list and regenerate (ADR-0003 A27); otherwise \
         this is a genuine gap."
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

/// `ReldexWakeFn` is the one declaration cbindgen does **not** generate: it is
/// hand-written in `cbindgen.toml`'s `after_includes` so that it sits inside
/// the header's `extern "C"` block (ADR-0003 A24). Nothing else ties it to the
/// Rust type alias, so a change to `hub.rs`'s `ReldexWakeFn` would compile
/// happily while the header kept describing the old signature.
///
/// This is that tie: the exact text the header must contain, and the exact
/// Rust alias it mirrors.
#[test]
fn the_hand_written_waker_typedef_matches_the_rust_alias() {
    let header = header();
    for needle in [
        "typedef void (*ReldexWakeFn)(void *user_data);",
        "using ReldexWakeFnNoexcept = void (*)(void *user_data) noexcept;",
        // The MSVC-aware guard: `__cplusplus` alone is 199711L there unless
        // `/Zc:__cplusplus` is passed, which would silently drop the alias.
        "#define RELDEX_CPLUSPLUS _MSVC_LANG",
        "#if defined(RELDEX_CPLUSPLUS) && RELDEX_CPLUSPLUS >= 201703L",
        "#define RELDEX_HAVE_WAKE_FN_NOEXCEPT 1",
    ] {
        assert!(
            header.contains(needle),
            "include/reldex.h no longer contains the hand-written line {needle:?}; it lives in \
             crates/ffi/cbindgen.toml under `after_includes`"
        );
    }

    let hub = fs::read_to_string(crate_dir().join("src/hub.rs")).expect("hub.rs is readable");
    assert!(
        hub.contains("pub type ReldexWakeFn = Option<extern \"C\" fn(user_data: *mut c_void)>;"),
        "the Rust `ReldexWakeFn` alias changed. The header's typedef is hand-written and does \
         not follow it: update `after_includes` in crates/ffi/cbindgen.toml, regenerate, and \
         update this test."
    );

    // And cbindgen must still be told not to generate a second one. Checked
    // by substring, not the exact `exclude = [...]` line, since
    // `ReldexWorkspaceWakeFn` (added alongside this one, same reason) now
    // shares that list.
    let config =
        fs::read_to_string(crate_dir().join("cbindgen.toml")).expect("cbindgen.toml is readable");
    assert!(
        config.contains("\"ReldexWakeFn\""),
        "cbindgen must keep excluding ReldexWakeFn, or the header defines it twice"
    );
    assert_eq!(
        header.matches("(*ReldexWakeFn)").count(),
        1,
        "ReldexWakeFn must be declared exactly once"
    );
}

/// The same tie as [`the_hand_written_waker_typedef_matches_the_rust_alias`],
/// for [`ReldexWorkspaceWakeFn`] (M2.11's review round 2, should-fix #12):
/// it had the exact same C++-linkage hazard `ReldexWakeFn` was hand-written
/// in `after_includes` to avoid, but was left as a plain Rust `pub type`
/// alias -- which cbindgen would emit *outside* the `extern "C"` block, the
/// bug this test exists to catch.
#[test]
fn the_hand_written_workspace_waker_typedef_matches_the_rust_alias() {
    let header = header();
    for needle in [
        "typedef void (*ReldexWorkspaceWakeFn)(void *user_data);",
        "using ReldexWorkspaceWakeFnNoexcept = void (*)(void *user_data) noexcept;",
        "#define RELDEX_HAVE_WORKSPACE_WAKE_FN_NOEXCEPT 1",
    ] {
        assert!(
            header.contains(needle),
            "include/reldex.h no longer contains the hand-written line {needle:?}; it lives in \
             crates/ffi/cbindgen.toml under `after_includes`"
        );
    }

    let workspace =
        fs::read_to_string(crate_dir().join("src/workspace.rs")).expect("workspace.rs is readable");
    assert!(
        workspace.contains(
            "pub type ReldexWorkspaceWakeFn = Option<extern \"C\" fn(user_data: *mut c_void)>;"
        ),
        "the Rust `ReldexWorkspaceWakeFn` alias changed. The header's typedef is hand-written and \
         does not follow it: update `after_includes` in crates/ffi/cbindgen.toml, regenerate, and \
         update this test."
    );

    let config =
        fs::read_to_string(crate_dir().join("cbindgen.toml")).expect("cbindgen.toml is readable");
    assert!(
        config.contains("\"ReldexWorkspaceWakeFn\""),
        "cbindgen must keep excluding ReldexWorkspaceWakeFn, or the header defines it twice"
    );
    assert_eq!(
        header.matches("(*ReldexWorkspaceWakeFn)").count(),
        1,
        "ReldexWorkspaceWakeFn must be declared exactly once"
    );
}
