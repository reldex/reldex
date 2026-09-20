# Reldex UI (M1.5 skeleton)

Minimal CMake + Corrosion + Qt Quick project that proves the
`reldex-ffi` -> thin C++ adapter -> QML path exists end to end. Per
`AGENTS.md` scope discipline, this is deliberately small: one `CoreInfo`
singleton exposing `reldex_abi_version()`, one window that prints it,
one test. No hub, no session, no result grid — that is M1.6 onward.

## Prerequisites

- CMake >= 3.24, Ninja
- Qt 6.8 (LGPL open-source build; see "Licence" below)
- Rust stable (workspace-pinned toolchain) with the `x86_64-pc-windows-msvc`
  target (or your platform's default target on Linux/macOS)
- Network access the first time you configure (Corrosion is fetched via
  `FetchContent`; afterwards it is cached in the build tree)

On Windows, everything above except Rust is installed and pinned per
`docs/exec-plans/active/phase-1-toolchain.md`; `tools/dev-env/env.sh` puts it
all on `PATH` for the current shell only.

## Build

```bash
bash ui/build.sh --clean --test
```

- `--release` — configure `CMAKE_BUILD_TYPE=Release` instead of the default
  `RelWithDebInfo`. RelWithDebInfo is the default because this is a mixed
  Rust+C++ build and an unoptimized (`dev`-profile) `reldex-ffi` is
  noticeably slower once real batches are involved (M1.6+); RelWithDebInfo
  keeps optimizations and symbols for both languages, which is what the
  day-to-day inner loop wants.
- `--test` — run `ctest --output-on-failure` after building.
- `--clean` — remove the build directory first.

The build tree lands at `build/ui-<config>` at the **repository root**
(outside `ui/`), matching the project convention that generated trees stay
out of source directories; the root `.gitignore`'s existing generic
`build/` / `build-*/` patterns already cover it, so no `.gitignore` change
was needed for this task.

On Windows, `ui/build.sh` sources `tools/dev-env/env.sh` itself (detected via
`uname -s` matching `MINGW*`/`MSYS*`/`CYGWIN*`) — you do not need to source
it yourself first. On Linux/macOS the script assumes `cmake`/`ninja` are
already on `PATH` and Qt 6.8 is discoverable via `CMAKE_PREFIX_PATH` or
`Qt6_DIR`; **that path has not been exercised** by whoever last verified
this file — only Windows was available. If something is wrong with it,
that is the first thing to check.

## Layout

```text
ui/CMakeLists.txt   Top-level: Qt, Corrosion/reldex-ffi, shared helpers
ui/cmake/           CompilerWarnings.cmake (the only CMake helper module)
ui/adapter/         reldex_adapter: thin static lib + QML module
                     (Reldex.Adapter) — CoreInfo singleton only
ui/app/             Reldex executable + Main.qml (Reldex.App QML module)
ui/tests/           tst_coreinfo (QTest), offscreen, run via CTest
ui/build.sh         One-command build (bash-first; see AGENTS.md)
```

### Why `ui/tests` re-attaches `../app/Main.qml` instead of sharing a library

The natural design is a small `reldex_app_qml` static library holding
`Main.qml`, linked by both `Reldex` and `tst_coreinfo`. It does not work:
a *statically linked entry-point* QML module (one loaded imperatively via
`loadFromModule`, never `import`-ed by another `.qml` file) only
self-registers when it is compiled directly into the binary that loads
it. Nothing in `main.cpp` references any symbol from a separate
`reldex_app_qml.lib`, so the linker is free to drop the whole archive —
confirmed by hand: the build succeeds warning-free, and the app fails at
*runtime* with `No module named "Reldex.App" found`.

The fix used here: `qt_add_qml_module()` is called directly on **both**
`Reldex` (`ui/app/CMakeLists.txt`) and `tst_coreinfo`
(`ui/tests/CMakeLists.txt`), each pointing at the same `ui/app/Main.qml`
file (not a copy). Each executable gets its own compiled copy of the
`Reldex.App` module in its own binary, which is exactly what makes
self-registration reliable. `ui/tests/CMakeLists.txt` gives its copy an
explicit `OUTPUT_DIRECTORY` (`qml-tests/Reldex/App` instead of the default
`qml/Reldex/App`) so the two do not collide, and `NO_CACHEGEN` because the
AOT qmlcache compiler cannot name its intermediate files sanely for a
source file outside the target's own directory tree.

### Why `QT_QML_OUTPUT_DIRECTORY` is set at all

`reldex_adapter`'s `Reldex.Adapter` module is consumed by `import
Reldex.Adapter` from `Main.qml`. A *library*-backed QML module's default
`OUTPUT_DIRECTORY` (with `QT_QML_OUTPUT_DIRECTORY` unset) is just
`CMAKE_CURRENT_BINARY_DIR`, with **no target-path suffix** — harmless on
its own (just a `qmllint` warning), except that a consumer's
`qmlimportscanner`-driven static-plugin linking uses that same convention
to find the module. Without the fix, `Reldex.exe` and `tst_coreinfo.exe`
both built and linked with **zero warnings**, and then both failed at
*runtime* with `module "Reldex.Adapter" plugin "reldex_adapterplugin" not
found` the moment they tried to load `Main.qml`. Setting
`QT_QML_OUTPUT_DIRECTORY` project-wide (`ui/CMakeLists.txt`) fixes both the
warning and the runtime failure, because it is also what
`_qt_internal_collect_qml_import_paths()` adds to every consumer's import
path automatically.

## Dependencies

**Corrosion** (`corrosion-rs/corrosion`, MIT licence) drives `cargo` from
CMake (ADR-0003 D8): Qt's own tooling (`qt_add_qml_module`, moc,
`windeployqt`, the mobile packaging targets) is CMake-native, so inverting
that — driving CMake from cargo — costs far more than one
`corrosion_import_crate()` call. It also handles target triples, build
profiles, and the platform link libraries Rust's std needs on Windows
(`ntdll`, `userenv`, `bcrypt`), which the configure log for this project
confirms it resolved automatically.

Pinned at tag **`v0.6.1`**, commit `1499b14e4906a2890f5cee1547c8848db261753d`
(the latest tagged release at the time this was written — checked via the
GitHub API, not guessed). `ui/CMakeLists.txt` pins by commit, not by the
mutable tag name, so a re-tag upstream cannot silently change what gets
built; the tag name is kept alongside it purely for a human to recognise
the version at a glance. Corrosion is fetched with `FetchContent` into the
build tree — it is a CMake-only dependency (no Cargo.toml entry, no Rust
crate), matching `AGENTS.md`'s "no production dependency without
documenting why."

`reldex-ffi` is imported by package name (`CRATES reldex-ffi`) from the
workspace `Cargo.toml` one directory up. Its `[lib] name = "reldex_ffi"`
(underscored) is what Corrosion uses for the generated CMake target names:
`reldex_ffi-static` and `reldex_ffi-shared`.

## Library kind and DLL deployment

ADR-0003 D8/D9 leaves desktop linkage as "cdylib on desktop, staticlib
reserved for iOS." This project links **`reldex_ffi-shared`** (the cdylib)
into `reldex_adapter`, which is `PUBLIC`, so it propagates to every
consumer (`Reldex`, `tst_coreinfo`) without each one repeating it.
`reldex_ffi-static` is imported by Corrosion but unused so far — nothing in
this skeleton needs it; it stays available for when an iOS target links it
directly, per D9.

Windows has no rpath, so `reldex_ffi.dll` (and its `.pdb`) must sit next to
every executable that (transitively) links `reldex_ffi-shared`. A small
CMake function in `ui/CMakeLists.txt`, `reldex_deploy_ffi_dll(<target>)`,
adds a `POST_BUILD` step doing
`cmake -E copy_if_different $<TARGET_FILE:reldex_ffi-shared> $<TARGET_FILE_DIR:target>`;
it is called once each for `Reldex` and `tst_coreinfo`. Qt's own DLLs are
resolved via `PATH` (`tools/dev-env/env.sh` puts Qt's `bin/` there); no
`windeployqt` step exists yet because this is a dev-build skeleton, not
packaging (that is a later M6 task).

Rust's own build artefacts (the actual `target/` cargo uses) live inside
the CMake build tree, under `build/ui-<config>/cargo/`, which is Corrosion's
default and is deliberately left as-is — the disk cost (a few hundred MB
per configuration) is an acceptable trade for not needing a second,
separately-managed Cargo target directory. It is git-ignored along with
the rest of `build/`.

## Licence note

Qt 6.8 is used under **LGPLv3, dynamically linked only**
(`find_package(Qt6 ...)` + `qt_add_executable`/`qt_add_library` link Qt
the normal shared-library way; nothing here builds or links a static Qt).
Only `Core`, `Gui`, `Qml`, `Quick`, `Test` are used — no GPL-only Qt module,
no Qt Creator dependency. See
`docs/exec-plans/active/phase-1-toolchain.md` §4–5 for the full inventory
of what was installed and the explicit confirmation that no GPL-only
module (`Qt6Charts`, `Qt6WebEngineCore`, etc.) is present.

## Known limitations

- **Linux/macOS are untested.** The CMake is written to work there (Qt
  discovered via `CMAKE_PREFIX_PATH`/`Qt6_DIR`, Corrosion is
  platform-agnostic, `ui/build.sh` has a non-Windows branch), but nobody
  has run it — only a Windows 11 + MSVC 2022 + Qt 6.8.3 machine was
  available. CI for all three OSes is M6.7, not this task.
- **No `windeployqt` / packaging step.** Qt DLLs are found via `PATH` in
  this dev environment; a real installer/package needs `windeployqt` (or
  the CMake `qt_generate_deploy_app_script()` equivalent), which is out of
  scope for a build skeleton.
- **No hub, no session, no result model.** `CoreInfo` only calls
  `reldex_abi_version()`. Wrapping the hub/session lifecycle into QObjects
  and building `ResultTableModel` is M1.6, and is explicitly not
  pre-empted here.
- **The offscreen platform plugin warns about missing fonts**
  (`QFontDatabase: Cannot find font directory .../lib/fonts. ... Qt no
  longer ships fonts.`) every run, since this Qt install has none deployed.
  It is a `qWarning()` from Qt's font subsystem, not a QML warning, so it
  does not fail `tst_coreinfo` (which only asserts on `QQmlEngine::warnings`
  emissions) — but it will show up in CTest/CI logs and is worth knowing
  about rather than mistaking for a real regression.
