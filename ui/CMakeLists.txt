cmake_minimum_required(VERSION 3.24)

project(ReldexUi
    DESCRIPTION "Reldex Qt Quick UI (thin adapter over the reldex-ffi C ABI)"
    LANGUAGES CXX C
)

# ---------------------------------------------------------------------------
# Basics
# ---------------------------------------------------------------------------
# AGENTS.md "Keep the C++/Qt adapter thin" -- C++17 is enough for a thin
# adapter; nothing here needs newer-standard features.
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

# Qt's moc/rcc/uic integration.
set(CMAKE_AUTOMOC ON)
set(CMAKE_AUTORCC ON)
set(CMAKE_AUTOUIC OFF) # no .ui files in this project

if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
    # RelWithDebInfo, not Debug: this is a Rust+C++ mixed build and an
    # unoptimized Rust `dev` build of reldex-ffi is noticeably slower for
    # anything beyond a trivial mock scenario (M1.6+ will fetch real batches).
    # RelWithDebInfo keeps optimizations plus symbols for both languages,
    # which matters most for the day-to-day inner loop this skeleton is for.
    # `ui/build.sh --release` switches to a pure Release build; Corrosion maps
    # CMAKE_BUILD_TYPE to a matching cargo profile automatically (Debug ->
    # `dev`, everything else -> `release`).
    set(CMAKE_BUILD_TYPE "RelWithDebInfo" CACHE STRING "Build type" FORCE)
endif()

list(APPEND CMAKE_MODULE_PATH "${CMAKE_CURRENT_SOURCE_DIR}/cmake")
include(CompilerWarnings)

# Declared before include(Sanitizers) below, which reads it immediately:
# option() must run first so RELDEX_SANITIZE has a value by the time that
# file applies it.
option(RELDEX_SANITIZE
    "Build our own targets (reldex_adapter, the Reldex app, every QTest binary, the ffi_smoke harness) with -fsanitize=address,undefined (GCC/Clang only; Qt and the reldex-ffi Rust cdylib stay uninstrumented, but ASan still intercepts their malloc/free from this process). See ui/build.sh --sanitize."
    OFF
)

# See ui/cmake/Sanitizers.cmake for what this instruments, what it never
# touches, and why (ADR-0003 K5). Included before any add_subdirectory()
# below (directory-scoped compile/link options are inherited by every
# subdirectory added afterwards), so reldex_adapter, Reldex and every QTest
# binary pick this up with no per-target opt-in.
include(Sanitizers)

# Captured here (this file's own scope), not read directly inside
# reldex_deploy_ffi_dll() below: CMAKE_CURRENT_SOURCE_DIR inside a function
# body reflects the CALLER's directory (e.g. ui/app when ui/app/CMakeLists.txt
# calls it), not this file's -- using it directly there resolved to a
# nonexistent ui/app/cmake/CopyIfDifferentRetry.cmake (M1.4 CI finding).
set(RELDEX_UI_CMAKE_DIR "${CMAKE_CURRENT_SOURCE_DIR}/cmake")

# ---------------------------------------------------------------------------
# Qt 6.8 -- LGPL modules only. No Qt Creator, no GPL-only module.
# ---------------------------------------------------------------------------
find_package(Qt6 6.8 REQUIRED COMPONENTS Core Gui Qml Quick QuickControls2 Test)

qt_standard_project_setup(REQUIRES 6.8)

# Every QML module's OUTPUT_DIRECTORY is placed under a common base and made
# to end in its own target path (e.g. .../qml/Reldex/Adapter). Two things
# depend on this, confirmed by hand while bringing the pieces together:
#   1. Without it, a *library*-backed module (reldex_adapter) defaults its
#      OUTPUT_DIRECTORY to CMAKE_CURRENT_BINARY_DIR with no target-path
#      suffix, which is merely a qmllint warning on its own -- but it is also
#      what a *consuming* target's qmlimportscanner-based static-plugin
#      linking uses to resolve `import Reldex.Adapter`. Without a matching
#      base import path, the consumer builds and links clean but fails at
#      *runtime* with `module "Reldex.Adapter" plugin "reldex_adapterplugin"
#      not found` -- the static plugin was simply never linked in.
#   2. Setting QT_QML_OUTPUT_DIRECTORY is what makes `_qt_internal_collect_
#      qml_import_paths()` add this base to every consuming target's import
#      path automatically, which is what actually resolves point 1.
# ui/app and ui/tests both separately attach the same Main.qml under the
# same "Reldex.App" URI (see ui/tests/CMakeLists.txt for why), so ui/tests
# overrides OUTPUT_DIRECTORY explicitly to keep the two from colliding on
# this shared base.
set(QT_QML_OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}/qml")

# ---------------------------------------------------------------------------
# Corrosion -- CMake drives cargo (ADR-0003 D8), fetched at a pinned tag.
# See ui/README.md "Dependencies" for why Corrosion is required and why this
# pin was chosen.
# ---------------------------------------------------------------------------
include(FetchContent)

set(CORROSION_GIT_TAG "v0.6.1") # pinned; see ui/README.md
set(CORROSION_GIT_COMMIT "1499b14e4906a2890f5cee1547c8848db261753d") # tag's commit, for FetchContent's own record

FetchContent_Declare(
    Corrosion
    GIT_REPOSITORY https://github.com/corrosion-rs/corrosion.git
    GIT_TAG ${CORROSION_GIT_COMMIT}
)
FetchContent_MakeAvailable(Corrosion)

# The reldex-ffi crate lives in the Cargo workspace one directory up.
set(RELDEX_WORKSPACE_MANIFEST "${CMAKE_CURRENT_SOURCE_DIR}/../Cargo.toml")
set(RELDEX_FFI_INCLUDE_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../crates/ffi/include")

corrosion_import_crate(
    MANIFEST_PATH ${RELDEX_WORKSPACE_MANIFEST}
    CRATES reldex-ffi
)

# `reldex-ffi`'s [lib] name is `reldex_ffi` (crates/ffi/Cargo.toml), so
# Corrosion names the imported targets `reldex_ffi-static` / `reldex_ffi-shared`.
# ADR-0003 D8/D9: link the cdylib on desktop (dynamic); the staticlib is for
# the iOS target later, where the app links it directly with no packaging
# step of its own. Nothing in this skeleton uses `reldex_ffi-static` yet.
if(NOT TARGET reldex_ffi-shared)
    message(FATAL_ERROR
        "Corrosion did not create the `reldex_ffi-shared` target. Check that "
        "crates/ffi/Cargo.toml still declares `crate-type = [\"cdylib\", ...]` "
        "and that CRATES above matches its package name.")
endif()

# ---------------------------------------------------------------------------
# Helper: copy the reldex_ffi shared library next to a target's binary so it
# runs without installing anything system-wide (Windows has no rpath).
#
# Goes through ui/cmake/CopyIfDifferentRetry.cmake (a plain retrying
# copy_if_different) rather than calling `cmake -E copy_if_different`
# directly: M1.4's CI (ui.yml, qt-build, macos-latest) hit a transient
# "source not found" from this exact copy under enough ninja parallelism
# (see that script's header comment for what was and was not established
# about the cause). Not reproduced on windows-latest/ubuntu-latest.
# ---------------------------------------------------------------------------
function(reldex_deploy_ffi_dll target)
    add_custom_command(TARGET ${target} POST_BUILD
        COMMAND ${CMAKE_COMMAND}
                "-DSRC=$<TARGET_FILE:reldex_ffi-shared>"
                "-DDST=$<TARGET_FILE_DIR:${target}>"
                -P "${RELDEX_UI_CMAKE_DIR}/CopyIfDifferentRetry.cmake"
        COMMENT "Deploying reldex_ffi shared library next to ${target}"
        VERBATIM
    )
endfunction()

# ---------------------------------------------------------------------------
# M3.1: the app shell's QML files, listed once (by filename, relative to
# ui/app/) and shared between ui/app (the real executable, where they are
# used as plain relative QML_FILES) and ui/tests (which re-attaches the same
# "Reldex.App" module into its own test binaries -- see
# ui/tests/CMakeLists.txt for why a statically linked QML entry-point module
# only self-registers when compiled directly into the binary that loads it,
# the same reason ui/app/CMakeLists.txt attaches these files to `Reldex`
# directly rather than to a shared library, and for why ui/tests builds each
# one back into an absolute path plus a QT_RESOURCE_ALIAS instead of reusing
# this list's relative form directly).
# ---------------------------------------------------------------------------
set(RELDEX_APP_QML_FILES
    Main.qml
    Harness.qml
    Theme.qml
    Sidebar.qml
    ObjectBrowserPanel.qml
    WorksheetArea.qml
    OutputPanes.qml
    StatusBar.qml
)

enable_testing()

add_subdirectory(adapter)
add_subdirectory(app)
add_subdirectory(tests)
