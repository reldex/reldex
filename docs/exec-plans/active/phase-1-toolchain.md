# Phase 1 Toolchain — Qt 6.8 LTS / CMake / Ninja / cbindgen / MSVC

- **Date:** 2026-09-20
- **Machine:** Windows 11 Pro 10.0.26200 (developer workstation)
- **Primary shell for this project:** Git Bash (`env.sh`). PowerShell's `env.ps1` is
  provided as an optional twin with the same effect.
- **Scope:** open-source Qt 6.8 LTS (LGPLv3, dynamic linking only), CMake, Ninja,
  cbindgen, and verification against the existing MSVC toolchain. No admin
  elevation was used anywhere in this setup; nothing outside the paths listed
  below was created or modified, and no machine-wide PATH/registry entries were
  touched.

## 1. Versions and install paths

| Component | Version | Install path | Source |
|---|---|---|---|
| Qt | **6.8.3** (kit `win64_msvc2022_64`) | `C:\Qt\6.8.3\msvc2022_64` | download.qt.io via `aqtinstall`, official Qt OSS mirrors |
| aqtinstall | 3.3.0 | user site-packages (`pip install --user`) | PyPI |
| qtshadertools | bundled with the 6.8.3 install (module `qtshadertools`) | `C:\Qt\6.8.3\msvc2022_64` | download.qt.io |
| CMake | 4.4.3 | `C:\Qt\Tools\CMake` | github.com/Kitware/CMake releases (official) |
| Ninja | 1.13.2 | `C:\Qt\Tools\Ninja` | github.com/ninja-build/ninja releases (official) |
| cbindgen | 0.29.4 | `%USERPROFILE%\.cargo\bin\cbindgen.exe` | crates.io via `cargo install` |
| Rust toolchain | rustc 1.98.1 (48a229cea 2026-09-01), cargo 1.98.1 (797e8a9bc 2026-08-05), target `x86_64-pc-windows-msvc` (stable, pinned by the repo's `rust-toolchain.toml`) | pre-existing rustup install | rustup (pre-existing on this machine) |
| MSVC toolset | 14.44.35207 (VC++ 2022) | `C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\MSVC\14.44.35207` | pre-existing Visual Studio install (not modified) |
| Visual Studio | Visual Studio Community 2022, version 17.14.36915.13 | `C:\Program Files\Microsoft Visual Studio\2022\Community` | pre-existing (not modified/installed by this task) |
| Windows SDK (used by vcvars) | 10.0.26100.0 | (part of the existing VS install) | pre-existing |

Python used to run `aqtinstall`: `C:\Python313\python.exe` (Python 3.13.11, pre-existing).

## 2. Exact install commands (reproducible on a second machine / CI)

```bash
# 1. aqtinstall (per-user, no elevation)
python -m pip install --user --upgrade aqtinstall

# 2. Confirm the latest 6.8.x patch published to open-source users
python -m aqt list-qt windows desktop --spec "6.8"        # -> 6.8.0 6.8.1 6.8.2 6.8.3
python -m aqt list-qt windows desktop --arch 6.8.3        # confirms win64_msvc2022_64 exists
python -m aqt list-qt windows desktop --modules 6.8.3 win64_msvc2022_64   # confirms qtshadertools is a separate module

# 3. Install Qt 6.8.3, win64_msvc2022_64 kit, base + qtshadertools, into C:\Qt
python -m aqt install-qt windows desktop 6.8.3 win64_msvc2022_64 -O C:\Qt -m qtshadertools

# 4. CMake (portable zip, official Kitware GitHub release)
curl -L -o cmake-4.4.3-windows-x86_64.zip \
  https://github.com/Kitware/CMake/releases/download/v4.4.3/cmake-4.4.3-windows-x86_64.zip
curl -L -o cmake-4.4.3-SHA-256.txt \
  https://github.com/Kitware/CMake/releases/download/v4.4.3/cmake-4.4.3-SHA-256.txt
sha256sum -c <(grep windows-x86_64.zip cmake-4.4.3-SHA-256.txt)   # verify
# unzip so cmake.exe lands at C:\Qt\Tools\CMake\bin\cmake.exe (top-level
# "cmake-4.4.3-windows-x86_64" folder from the zip renamed to "CMake")

# 5. Ninja (portable zip, official ninja-build GitHub release)
curl -L -o ninja-win.zip \
  https://github.com/ninja-build/ninja/releases/download/v1.13.2/ninja-win.zip
# unzip into C:\Qt\Tools\Ninja  (ninja.exe directly inside; no official
# published checksum file for this asset -- SHA-256 recorded below instead)

# 6. cbindgen (crates.io)
cargo install cbindgen --locked

# 7. Verify MSVC toolchain (no install/modification)
"C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe" -latest -property installationPath
# -> C:\Program Files\Microsoft Visual Studio\2022\Community
```

No step above required administrator elevation. `C:\Qt` was created directly by
the current user (drive-root folder creation did not require elevation on this
machine). Nothing under `C:\Windows`, the registry, or the machine PATH was
touched; PATH changes are process-local only, via `env.sh` / `env.ps1` (section 5).

## 3. Download checksums

| File | SHA-256 | Verified against |
|---|---|---|
| `cmake-4.4.3-windows-x86_64.zip` | `4d52ebab7193a698651639ed80d8d04fd903358843572cf44c7fd234cb7c26ab` | matches Kitware's published `cmake-4.4.3-SHA-256.txt` |
| `ninja-win.zip` | `07fc8261b42b20e71d1720b39068c2e14ffcee6396b76fb7a795fb460b78dc65` | computed locally; ninja-build does not publish a checksum file for this release asset |

## 4. Module/tool inventory (proof of no GPL-only / forbidden modules)

`C:\Qt\6.8.3\msvc2022_64\lib\cmake` contains CMake package directories for
(non-exhaustive, required set only): `Qt6Core`, `Qt6Gui`, `Qt6Qml`, `Qt6Quick`,
`Qt6QuickControls2`, `Qt6QuickTest`, `Qt6Test`, `Qt6Svg`, `Qt6ShaderTools`,
`Qt6LinguistTools`, plus the usual supporting/private packages that Qt6 base +
qtdeclarative + qtsvg + qttools + qtshadertools bring in (e.g. `Qt6Widgets`,
`Qt6Network`, `Qt6Sql`, `Qt6Xml`, `Qt6Help`, `Qt6PrintSupport`, `Qt6Designer`,
`Qt6QmlCompiler`, `Qt6QuickControls2*StyleImpl`, etc.) — all part of the base
Qt6/qtdeclarative/qtsvg/qttools/qtshadertools install, not separate add-ons.

**Confirmed present** (all required by the brief):
- `Qt6Core`, `Qt6Gui`, `Qt6Qml`, `Qt6Quick`, `Qt6QuickControls2`, `Qt6QuickTest`, `Qt6Test`, `Qt6Svg`, `Qt6ShaderTools`, `Qt6LinguistTools` — CMake packages under `lib\cmake`.
- `windeployqt6.exe`, `windeployqt.exe`, `qmlcachegen.exe`, `lupdate.exe`, `lrelease.exe`, `qmltestrunner.exe` — all present under `C:\Qt\6.8.3\msvc2022_64\bin`.

**Confirmed absent** (explicitly forbidden add-ons — checked directly, none installed):
`Qt6Charts`, `Qt6Graphs`, `Qt6DataVisualization`, `Qt6VirtualKeyboard`,
`Qt6Quick3D`, `Qt6Quick3DPhysics`, `Qt6WebEngineCore`, `Qt6WaylandCompositor`,
`Qt6Core5Compat` (qt5compat), `Qt6NetworkAuth` — none of these directories
exist under `lib\cmake`. Only `qtbase`, `qtdeclarative`, `qtsvg`, `qttools`,
`qttranslations` (all bundled in the Qt6 base install) plus the explicitly
requested `qtshadertools` module, plus the small helper components
`d3dcompiler_47` and `opengl32sw` (Qt's standard ANGLE/software-GL runtime
helpers, not separate GPL modules), were installed.

## 5. Licensing note

Qt 6.8.3 was installed via the **open-source** aqtinstall/download.qt.io
channel and is used under **LGPLv3**. Only dynamic linking against the Qt
shared libraries is intended (the default with `qt_add_executable` /
`find_package(Qt6 ...)` as configured here — no static Qt build was
downloaded or built). No Qt Maintenance Tool account login was used or
required; no GPL-only Qt module (e.g. qtvirtualkeyboard's GPL parts,
qtwebengine) was installed.

## 6. Entering the build environment

**Git Bash is the primary, tested entry point for this project.**

```bash
source tools/dev-env/env.sh
```

`env.sh`:
- Spawns a `cmd.exe` child that runs `vcvars64.bat` (found via `vswhere.exe`'s
  reported VS install path) and captures its resulting environment.
- Strips the trailing `\r` that `cmd`'s CRLF `set` output otherwise leaves on
  every captured value (an untreated `\r` on `COMSPEC` corrupts the
  `cmd.exe /C "..."` command line CMake/Ninja generate for linking, breaking
  Ninja's lexer with a cryptic `rules.ninja:N: lexing error`).
- Converts the captured Windows-style `PATH` entries to Unix form with
  `cygpath -u` and **prepends** them to bash's existing `PATH` (never
  overwrites it — overwriting breaks bash's own command resolution).
- Only exports its own `QT_DIR` / `CMAKE_PREFIX_PATH` / Qt-bin-PATH entries
  **after** the vcvars import step. This ordering matters: the vcvars-capture
  child inherits bash's whole environment, and MSYS silently rewrites any
  already-exported POSIX-style path (e.g. `/c/Qt/...`) to Windows form
  (`C:/Qt/...`) the moment it crosses into that child process; `cmd`'s `set`
  dump would then contain the mangled form, and importing it back would
  clobber our own POSIX-style values. Setting our variables afterward avoids
  this class of bug entirely.
- Everything is process-local: no user/machine PATH, environment, or registry
  change persists after the shell exits.

`env.ps1` (optional twin, same effect, for PowerShell 5.1):

```powershell
. .\env.ps1
```

Equivalent process-local-only behavior (dot-source it so variables persist in
your PowerShell session); also imports vcvars64 into the current process only
and prepends Qt/CMake/Ninja to `PATH` for that process.

## 7. Hello-world (Qt Quick) result

Project: `hello-qt/` (CMake + `qt_add_executable` + `qt_add_qml_module`,
`Main.qml` = a `Window` with `Text { text: "สวัสดี Reldex 🚀" }` and a 1s
`Timer` that calls `Qt.quit()`).

**From Git Bash (primary, tested path):**

```bash
source env.sh
cd hello-qt
cmake -S . -B build-bash -G Ninja -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_PREFIX_PATH="C:\Qt\6.8.3\msvc2022_64"
cmake --build build-bash --config Release
cd build-bash
QT_QPA_PLATFORM=offscreen ./HelloReldexQt.exe
echo "exit code: $?"        # -> 0
```

Result: **configure OK, build OK (19/19 targets), run OK, exit code 0.**

**From PowerShell (optional twin, also verified):**

```powershell
. .\env.ps1
Set-Location hello-qt
cmake -S . -B build -G Ninja -DCMAKE_BUILD_TYPE=Release -DCMAKE_PREFIX_PATH="C:\Qt\6.8.3\msvc2022_64"
cmake --build build --config Release
Set-Location build
$env:QT_QPA_PLATFORM = "offscreen"
.\HelloReldexQt.exe
# $LASTEXITCODE -> 0
```

Result: identical — configure OK, build OK, run OK, exit code 0.

Both build trees (`hello-qt/build-bash` from Git Bash, `hello-qt/build` from
PowerShell) were produced and run independently to prove the environment
scripts are equivalent.

## 8. QuickTest result

Test: `hello-qt/tests/tst_basic.qml` (a trivial `TestCase` with two functions,
`test_arithmetic` and `test_string`), run via `qmltestrunner.exe -input`,
offscreen.

```bash
source env.sh
qmltestrunner -input "$(pwd)/hello-qt/tests" -o "$(pwd)/qmltest_out.txt,txt"
# QT_QPA_PLATFORM=offscreen (exported by env.sh's caller / test run)
```

Output:

```
********* Start testing of qmltestrunner *********
Config: Using QtTest library 6.8.3, Qt 6.8.3 (x86_64-little_endian-llp64 shared (dynamic) release build; by MSVC 2022), windows 11
PASS   : qmltestrunner::BasicToolchainSmokeTest::initTestCase()
PASS   : qmltestrunner::BasicToolchainSmokeTest::test_arithmetic()
PASS   : qmltestrunner::BasicToolchainSmokeTest::test_string()
PASS   : qmltestrunner::BasicToolchainSmokeTest::cleanupTestCase()
Totals: 4 passed, 0 failed, 0 skipped, 0 blacklisted, 4ms
********* Finished testing of qmltestrunner *********
```

Exit code: **0**. Verified both from Git Bash and from PowerShell.

## 9. Disk usage

| Path | Size |
|---|---|
| `C:\Qt` (total) | ~2.2 GB (2.06 GiB by exact byte count) |
| `C:\Qt\6.8.3` (the Qt kit) | ~2.0 GB |
| `C:\Qt\Tools\CMake` | ~155 MB |
| `C:\Qt\Tools\Ninja` | ~592 KB |

Well within the ~5 GB disk budget approved for this task.

## 10. Problems and workarounds

1. **`vcvars64.bat` prints `'vswhere.exe' is not recognized`.** `vcvars64.bat`
   shells out to bare `vswhere.exe` for extra SDK/toolset detection but does
   not itself add the VS Installer directory to `PATH`. Harmless (the
   environment still gets set correctly), but noisy and it leaves a stray
   non-zero `$LASTEXITCODE`/exit status behind in PowerShell. **Workaround:**
   both `env.ps1` and `env.sh` prepend
   `C:\Program Files (x86)\Microsoft Visual Studio\Installer` to `PATH`
   *before* invoking `vcvars64.bat` (process-local only), and `env.ps1`
   explicitly resets `$LASTEXITCODE = 0` at the end so it doesn't leak into
   the caller's shell.

2. **`env.ps1`: PowerShell parse errors from an em dash character.** An early
   draft used a Unicode em dash (—) in a comment/string; without a BOM,
   PowerShell 5.1 read the UTF-8 bytes as the system ANSI code page and
   mis-tokenized the file (quotes appeared to terminate early, cascading into
   many unrelated parse errors). **Workaround:** replaced all em dashes with
   plain ASCII hyphens in both `env.ps1` and `env.sh`; `Main.qml`'s Thai text
   and emoji are written as `\uXXXX` / surrogate-pair escapes instead of
   literal UTF-8 bytes for the same reason.

3. **`env.sh` (first draft): quoting `cmd //c "call \"...\" && set"` directly
   from Git Bash silently failed** (`vcvars64.bat` never actually ran; MSVC
   env vars like `INCLUDE`/`LIB`/`VCToolsInstallDir` stayed empty, and `cl`
   resolved to a stale entry already on the ambient `PATH` instead of the one
   `vcvars64.bat` would have set). **Workaround:** generate a small temporary
   `.bat` file (via `mktemp --suffix=.bat` + `cygpath -w`) that does
   `call "vcvars64.bat" && set`, and invoke that file directly with
   `cmd //c`, avoiding fragile nested-quote/`&&` escaping across the
   Bash-to-cmd boundary.

4. **`env.sh`: re-exporting cmd's captured `PATH` verbatim broke bash
   itself** (`ls`, `cat`, `which`, etc. stopped resolving) because
   Windows-style `PATH` (`C:\...;C:\...`) is not valid as bash's own `PATH`.
   **Workaround:** split the captured `PATH` on `;`, convert each entry with
   `cygpath -u`, and **prepend** (never overwrite) the result onto the
   existing Unix-style `PATH`.

5. **`env.sh`: a trailing `\r` from `cmd`'s CRLF `set` output corrupted
   `COMSPEC`**, which CMake/Ninja embed verbatim into the generated
   `CXX_EXECUTABLE_LINKER` rule (`cmd.exe /C "$PRE_LINK && ... && $POST_BUILD"`).
   The stray `\r` landed *inside* the generated `rules.ninja` line (right
   after `cmd.exe`), which Ninja's parser rejected as `rules.ninja:N: lexing
   error`, only reproducible when configuring from Git Bash. **Workaround:**
   strip a trailing `\r` (`${_value%$'\r'}`) from every captured key/value
   pair before exporting.

6. **`env.sh`: MSYS silently mangles already-exported POSIX paths when a
   child Win32 process is spawned.** Originally `QT_DIR_UNIX` (and other
   `QT_*` vars) were exported *before* the vcvars-capture step; because that
   step spawns `cmd.exe` (a native Win32 process) which inherits bash's full
   environment, MSYS rewrote `QT_DIR_UNIX`'s value from `/c/Qt/6.8.3/...` to
   `C:/Qt/6.8.3/...` the moment it crossed into that child. `cmd`'s `set`
   dump then contained the mangled value, and the import loop re-exported it
   under the original name — silently overwriting our correct POSIX-style
   value and breaking every later Unix-style path built from it (e.g. the
   `HelloReldexQt.exe` run initially failed with a confusing
   `error while loading shared libraries: api-ms-win-crt-locale-l1-1-0.dll`,
   because `PATH` no longer actually contained `.../msvc2022_64/bin`).
   **Workaround:** reordered `env.sh` so all of its own `QT_DIR` /
   `CMAKE_PREFIX_PATH` / PATH-prepend exports happen *after* the vcvars
   import step, so they don't exist yet (and can't be swept up and mangled)
   when the `cmd.exe` child's environment is captured.

7. **Bash tool call boundaries don't preserve shell state.** Each `Bash` tool
   invocation is a fresh shell process; `source env.sh` in one call does not
   persist to a later call. Not a bug in `env.sh` itself, but a reminder
   (also called out at the top of `env.sh`/`env.ps1`) that sourcing and use
   must happen in the same shell session/script.

None of the above required elevation, changed machine-wide PATH/registry, or
touched any file outside this scratch directory / `C:\Qt`.

## 11. Where the pieces live in the repository

- `tools/dev-env/env.sh` — primary (Git Bash) environment setup script: `source tools/dev-env/env.sh`.
- `tools/dev-env/env.ps1` — optional PowerShell twin, dot-sourced from the repository root.
- The hello-world Qt Quick project and QuickTest used for the proof above were throwaway scratch files and are
  not kept; milestone task M1.5 adds the real `ui/` CMake project, which supersedes them.

## 12. M1.5 gotchas (CMake + Corrosion + Qt Quick project)

Discovered while building `ui/` (see `ui/README.md` for the design decisions
these gotchas produced).

1. **A statically-linked QML *entry-point* module (loaded via
   `loadFromModule`, never `import`-ed by another `.qml` file) only
   self-registers when it is compiled directly into the binary that loads
   it.** Putting `Main.qml` in its own small static library
   (`reldex_app_qml`) linked by both `Reldex` and `tst_coreinfo` seemed like
   the obvious way to share it, and it built with zero warnings — then
   failed at *runtime* with `No module named "Reldex.App" found`, because
   nothing in `main.cpp` references a symbol from that library, so the
   linker drops the whole archive. Qt's own static-plugin-import machinery
   (`qt6_import_qml_plugins`, run automatically for every `qt_add_executable`
   target) only forces a plugin to link when its *use* is discovered by
   `qmlimportscanner` scanning an `import` statement — an entry point is
   never imported, so that machinery never sees it. **Fix:** attach the QML
   module directly to each executable that loads it (`qt_add_qml_module`
   called once per executable, same source file, no intermediate library).
2. **A *library*-backed QML module's default `OUTPUT_DIRECTORY` breaks
   `qmlimportscanner`-based static-plugin discovery for its consumers, not
   just `qmllint`.** Without `QT_QML_OUTPUT_DIRECTORY` set,
   `qt_add_qml_module()` on a `STATIC` library target defaults
   `OUTPUT_DIRECTORY` to `CMAKE_CURRENT_BINARY_DIR` with **no target-path
   suffix** (only executables get the automatic suffix). This alone is
   merely a configure-time warning ("uses an OUTPUT_DIRECTORY ... which
   should end in the same target path"). The real cost: a consumer's
   `import Reldex.Adapter` then builds and links with **zero errors or
   warnings**, and fails at *runtime* with `module "Reldex.Adapter" plugin
   "reldex_adapterplugin" not found`, because `qmlimportscanner` cannot
   locate the module's `qmldir` under any import path it was given, so it
   never force-links the plugin. **Fix:** set `QT_QML_OUTPUT_DIRECTORY`
   project-wide (`ui/CMakeLists.txt`); it makes every library module's
   output directory end in its own target path *and* gets added to every
   consumer's import path automatically. Two executables sharing one QML
   URI (see gotcha 1) then need an explicit per-target `OUTPUT_DIRECTORY`
   override to avoid colliding on that shared base.
3. **`qt_add_executable(... WIN32_EXECUTABLE ...)` is not a real keyword.**
   `WIN32_EXECUTABLE` is a *target property* name; the `add_executable()`
   keyword (which `qt_add_executable()` passes straight through) is
   `WIN32`, mirrored by `MACOSX_BUNDLE` on Apple. Passing
   `WIN32_EXECUTABLE` as an argument makes CMake try to compile a source
   file literally named `WIN32_EXECUTABLE` and fail with "Cannot find
   source file." Likewise `OUTPUT_NAME` is not a `qt_add_executable()`
   keyword (unlike `qt_add_library`/`qt_add_plugin`) — when the target name
   already matches the desired binary name, nothing extra is needed.
4. **Redirected output from a Windows GUI-subsystem (`WIN32`) executable is
   unreliable to capture from Git Bash.** `./Reldex.exe > out.log 2>&1`
   sometimes reports exit code `127` even though the binary exists and
   runs fine (confirmed via `Get-Process`/PowerShell `Start-Process` in
   parallel), and `qWarning()`/`qFatal()` output that should go to stderr
   does not reliably show up in the redirected file for a `WIN32` target,
   even though the same message handler reliably writes to a *console*
   (`CUI`) target's redirected stderr. Console-subsystem (no `WIN32`
   keyword) executables like `tst_coreinfo.exe` do not have this problem.
   **Workaround used here:** verify a `WIN32` binary's behaviour either
   through a console-subsystem test binary that exercises the same code
   path (that is what `tst_coreinfo` is for), or, when the `.exe` itself
   must be checked, launch/inspect it via PowerShell's `Start-Process
   -RedirectStandardOutput/-RedirectStandardError` and `Get-Process`/
   `Stop-Process` rather than Git Bash job control.
5. **MSVC-style tools (`cl`, `link`, `dumpbin`) need `MSYS_NO_PATHCONV=1`
   from Git Bash whenever an argument starts with a single `/`** (`/W4`,
   `/EHsc`, `/HEADERS`, ...) — MSYS's automatic path conversion silently
   rewrites `/nologo` to a POSIX-style path under `C:\Program Files\Git\`
   and the tool then fails with a confusing "cannot open input file"
   instead of an argument error. CMake/Ninja invoke these tools directly
   (not through a shell that would need this), so ordinary builds are
   unaffected; this only bites when invoking `cl`/`dumpbin`/etc. by hand
   from Git Bash for diagnosis, exactly as `cygpath`/`MSYS_NO_PATHCONV`
   are already called out in §6's `env.sh` notes for other tools.
6. **`tools/dev-env/env.sh`/`env.ps1` hardcoded this one workstation**,
   discovered while writing `.github/workflows/ui.yml` (M1.4): both scripts
   hardcoded `...\2022\Community\...` for `vcvars64.bat`, and unconditionally
   exported `QT_DIR`/`CMAKE_PREFIX_PATH` and prepended `C:\Qt\Tools\CMake`
   /`C:\Qt\Tools\Ninja` to `PATH`. All four assumptions hold on this
   developer's machine and fail on a CI runner: GitHub-hosted `windows-latest`
   ships **Visual Studio Enterprise 2022** (not Community) at a different
   path (confirmed against `actions/runner-images`' published manifest), and
   it already has its own `cmake`/`ninja` on `PATH` plus its own Qt install
   from `jurplel/install-qt-action` (which sets its own `QT_ROOT_DIR`
   /`PATH`). Fixed minimally, in both scripts: the VS install path is now
   found via `vswhere.exe -latest -requires
   Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property
   installationPath` (the same tool step 7 above already uses by hand,
   just automated), and the Qt/CMake/Ninja exports and `PATH` prepends only
   happen when their hardcoded local paths actually exist — otherwise the
   scripts print a note and leave whatever is already set (CI's own
   `CMAKE_PREFIX_PATH`/`PATH`) alone. Verified afterwards to still produce
   an identical environment on this machine (see §6 above); the CI-side
   behaviour is verified in `ui.yml`'s `qt-build` job (§13 below).

## 13. UI CI (M1.4, ADR-0003 D10/K7)

`.github/workflows/ui.yml` is separate from the fast, hermetic `ci.yml`
(Rust-only: fmt/clippy/test, plus the `reldex.h`-is-not-stale check). It
triggers on `push` to `main` and on `pull_request`, filtered to
`ui/**`, `crates/**`, `Cargo.toml`, `Cargo.lock`, and the workflow file
itself, with a `concurrency` group that cancels a superseded run — the same
pattern `ci.yml` already uses.

### Jobs

1. **`ffi-smoke`** (windows/ubuntu/macos, `timeout-minutes: 15`) — runs
   `bash ui/tests/ffi_smoke/run.sh` directly (`--sanitize` on ubuntu only),
   the exact script a developer runs locally, no Qt install. On Windows this
   still goes through `tools/dev-env/env.sh` for MSVC discovery (via
   vswhere) plus `-G Ninja`; an earlier version of this job tried a
   `Visual Studio 17 2022` CMake generator specifically to avoid needing
   vcvars at all, but that generator failed to find any VS instance on the
   actual `windows-latest` runner (§12 item 6 below covers why env.sh's own
   VS discovery had to become dynamic first), so this job now shares the
   exact same env.sh + Ninja path `qt-build` already used successfully.
   Ubuntu additionally configures `-DRELDEX_SANITIZE=ON` and runs with
   `ASAN_OPTIONS=detect_leaks=1`.
2. **`qt-build`** (windows/ubuntu/macos, `timeout-minutes: 25` — this is
   spike criterion K7's budget) — installs Qt 6.8.3 (LGPL, `qtshadertools`
   module only, matching §4-5 above) via `jurplel/install-qt-action`, then
   runs `bash ui/build.sh --test` with `QT_QPA_PLATFORM=offscreen`. Ubuntu
   additionally installs `libgl1 libegl1 libxkbcommon0 libfontconfig1
   libdbus-1-3` — the runtime libraries Qt Quick's plugins `dlopen()` even
   under the offscreen QPA backend. macOS additionally runs
   `.github/scripts/patch-macos-qt-agl.py` against the installed Qt right
   after the install step, working around
   [QTBUG-137687](https://bugreports.qt.io/browse/QTBUG-137687): Qt 6.8.3's
   `FindWrapOpenGL.cmake` still links a hardcoded `-framework AGL`, which
   Apple removed from the macOS 26 (Tahoe) / Xcode 26 SDK that
   `macos-latest` now ships, so every Qt Quick target failed to link
   (`ld: framework 'AGL' not found`) without this. Fixed upstream in Qt
   6.8.4/6.9.2; not applied by bumping the version pin because 6.8.4 was not
   yet published to aqtinstall's open-source macOS channel as of
   2026-09-20 (`aqt list-qt mac desktop --spec 6.8` tops out at 6.8.3), and
   moving to the 6.9 minor line is a version-policy decision for the owner,
   not this task. The script fails the job loudly if the installed file's
   text does not match what it expects, rather than silently no-op'ing.

### Action pins (commit SHA, per repository policy)

| Action | Pinned commit | Version |
| --- | --- | --- |
| `actions/checkout` | `11d5960a326750d5838078e36cf38b85af677262` | v4.4.0 |
| `dtolnay/rust-toolchain` | `02cb101ec7c40f2c49e1d9714d64511d8e1b74de` | master, 2026-09-20 (no version tags; `toolchain: stable` passed explicitly per the action's own SHA-pinning guidance) |
| `Swatinem/rust-cache` | `6323deb102c322ba6fcbdcafc7e3dddab59af2b6` | v2.9.2 |
| `jurplel/install-qt-action` | `bcb88e3bed2e992f5f9e24c0f9e364a231d278eb` | v4.4.0 |

`ci.yml`'s own `Swatinem/rust-cache@v2` is left as-is (out of scope for this
task); `ui.yml` pins its own copy of the same action by commit per this
project's third-party-action policy.

### Cold/warm job times

Measured on PR #16 (`phase-1/m1-4-ffi-smoke`): "cold" is the first-ever run
of this workflow (run `35492858488`, no `Swatinem/rust-cache` or
`jurplel/install-qt-action` cache yet existed); "warm" is the third run
(run `35494036735`, the first fully green one, benefiting from both caches
populated by the first two runs). Total job wall time, `Set up job` through
`Complete job`.

| Job | OS | Cold | Warm | Budget |
| --- | --- | --- | --- | --- |
| `ffi-smoke` | windows-latest | n/a¹ | 1m27s | (no formal budget; K7 is `qt-build` only) |
| `ffi-smoke` | ubuntu-latest (+ASan/UBSan) | 38s | 33s | — |
| `ffi-smoke` | macos-latest | 28s | 24s | — |
| `qt-build` | windows-latest | 3m2s | 2m35s | 25 min (K7) |
| `qt-build` | ubuntu-latest | 2m10s | 1m26s | 25 min (K7) |
| `qt-build` | macos-latest | n/a² | 54s | 25 min (K7) |

¹ The cold run used an earlier `ffi-smoke` design (a `Visual Studio 17 2022`
CMake generator, to avoid touching vcvars) that failed outright on
`windows-latest` (`could not find any instance of Visual Studio`) before
this job existed in its current form (`run.sh` + `env.sh` + Ninja, same as
`qt-build`) — no cold timing exists for the current implementation.
² The cold run's `qt-build`/macos-latest failed at `bash ui/build.sh --test`
(the dylib-copy race `ui/cmake/CopyIfDifferentRetry.cmake` now absorbs, see
§12), before the QTBUG-137687 workaround above even existed. Its `Install
Qt 6.8` step alone (genuinely cold, no cache) took 76s, vs. 19s once warm —
the only cold data point available for that step on this OS.

Every `qt-build` leg finishes in under 3m5s against K7's 25-minute budget —
no OS is close to the limit; Qt install (cold) plus `ui/build.sh --test`
account for nearly all of it.
