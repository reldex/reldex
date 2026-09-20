#!/usr/bin/env bash
# ui/build.sh -- one-command build for the Reldex Qt Quick UI skeleton
# (M1.5). Bash-first per AGENTS.md "Scripts and shell": this is the primary,
# tested, documented entry point; it must run under Git Bash on Windows as
# well as on Linux and macOS.
#
# Usage:
#   bash ui/build.sh [--release] [--test] [--clean] [--sanitize]
#
#   --release   Configure CMAKE_BUILD_TYPE=Release instead of the default
#               RelWithDebInfo (see ui/CMakeLists.txt for why RelWithDebInfo
#               is the default for this skeleton).
#   --test      Run `ctest --output-on-failure` after a successful build.
#   --clean     Remove the build directory before configuring (a full,
#               from-scratch build).
#   --sanitize  Configure -DRELDEX_SANITIZE=ON: -fsanitize=address,undefined
#               for our own targets -- reldex_adapter, the Reldex app, every
#               QTest binary (GCC/Clang only; see ui/cmake/Sanitizers.cmake --
#               on MSVC this warns and is ignored, same as
#               ui/tests/ffi_smoke). Qt and the reldex-ffi Rust cdylib stay
#               uninstrumented, but ASan still intercepts their malloc/free
#               calls from this process. Builds into a separate directory
#               (build/ui-<config>-asan) so a sanitized and a plain build
#               never fight over the same tree. With --test, also sets
#               ASan/UBSan/LeakSanitizer runtime options for the ctest run
#               and passes ctest -V so every test's own output (including
#               tst_teardown's K5 iteration counts and live-count
#               assertions -- ADR-0003) lands in the log, not just failures.
#
# On Windows this sources tools/dev-env/env.sh (MSVC + Qt + CMake + Ninja),
# matching docs/exec-plans/active/phase-1-toolchain.md -- process-local only,
# nothing machine-wide is touched. On Linux/macOS it assumes cmake/ninja are
# already on PATH and Qt 6.8 is discoverable via CMAKE_PREFIX_PATH or
# Qt6_DIR; that path is NOT exercised on this (Windows) machine -- see
# ui/README.md "Known limitations".

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." >/dev/null 2>&1 && pwd)"

BUILD_TYPE="RelWithDebInfo"
RUN_TESTS=0
CLEAN=0
SANITIZE=0

for arg in "$@"; do
    case "$arg" in
        --release)
            BUILD_TYPE="Release"
            ;;
        --test)
            RUN_TESTS=1
            ;;
        --clean)
            CLEAN=1
            ;;
        --sanitize)
            SANITIZE=1
            ;;
        -h|--help)
            sed -n '2,35p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "ui/build.sh: unknown argument: $arg" >&2
            exit 1
            ;;
    esac
done

# Build tree lives outside ui/, at the repo root, and is git-ignored
# (root .gitignore's generic "build/" pattern already covers it). A
# sanitized build gets its own directory (mirrors
# ui/tests/ffi_smoke/run.sh's own build/ffi-smoke-asan split) so switching
# --sanitize on and off never invalidates the other build's cache.
BUILD_DIR="${REPO_ROOT}/build/ui-${BUILD_TYPE}"
if [ "${SANITIZE}" -eq 1 ]; then
    BUILD_DIR="${BUILD_DIR}-asan"
fi

# --- Windows: bring in MSVC + Qt + CMake + Ninja (Git Bash / MSYS only) ----
case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*)
        # shellcheck source=/dev/null
        source "${REPO_ROOT}/tools/dev-env/env.sh"
        ;;
    *)
        echo "ui/build.sh: non-Windows platform detected (uname: $(uname -s))." >&2
        echo "  Assuming cmake/ninja are already on PATH and Qt 6.8 is" >&2
        echo "  discoverable via CMAKE_PREFIX_PATH or Qt6_DIR. This path has" >&2
        echo "  not been exercised by the author of this script -- see" >&2
        echo "  ui/README.md 'Known limitations'." >&2
        ;;
esac

if [ "${CLEAN}" -eq 1 ] && [ -d "${BUILD_DIR}" ]; then
    echo "ui/build.sh: removing ${BUILD_DIR}"
    rm -rf "${BUILD_DIR}"
fi

CMAKE_CONFIGURE_ARGS=(-S "${SCRIPT_DIR}" -B "${BUILD_DIR}" -G Ninja -DCMAKE_BUILD_TYPE="${BUILD_TYPE}")
CONFIGURE_MSG="${BUILD_TYPE}"
if [ "${SANITIZE}" -eq 1 ]; then
    CMAKE_CONFIGURE_ARGS+=(-DRELDEX_SANITIZE=ON)
    CONFIGURE_MSG="${CONFIGURE_MSG}, sanitize"
fi

echo "ui/build.sh: configuring (${CONFIGURE_MSG}) into ${BUILD_DIR}"
cmake "${CMAKE_CONFIGURE_ARGS[@]}"

echo "ui/build.sh: building"
BUILD_ARGS=("${BUILD_DIR}")
if [ "${SANITIZE}" -eq 1 ]; then
    # --verbose: print every compiler invocation, so the -fsanitize flags on
    # our own targets are visible in the build log as evidence (ADR-0003
    # K5), not just asserted by this script or ui/cmake/Sanitizers.cmake.
    BUILD_ARGS+=(--verbose)
fi
cmake --build "${BUILD_ARGS[@]}"

if [ "${RUN_TESTS}" -eq 1 ]; then
    echo "ui/build.sh: running ctest"
    CTEST_ARGS=(--test-dir "${BUILD_DIR}" --output-on-failure)
    if [ "${SANITIZE}" -eq 1 ]; then
        # GCC/Clang only -- ui/cmake/Sanitizers.cmake warns and ignores
        # RELDEX_SANITIZE on MSVC, so these are harmless-but-unused env vars
        # there (nothing links libasan/libubsan to read them).
        #   detect_leaks=1           -- LeakSanitizer runs as part of ASan.
        #   abort_on_error=1         -- crash instead of exit(1), so a CI
        #                               runner's core/log capture behaves the
        #                               same as any other crash.
        #   strict_string_checks=1   -- catches misuse of string functions
        #                               (overlapping args, etc.) that plain
        #                               ASan interceptors don't reject.
        export ASAN_OPTIONS="detect_leaks=1:abort_on_error=1:strict_string_checks=1${ASAN_OPTIONS:+:${ASAN_OPTIONS}}"
        # print_stacktrace=1 -- an UBSan finding is otherwise a one-line
        # diagnostic with no way to tell which of our targets hit it.
        # halt_on_error=1    -- stop at the first UB finding rather than
        #                       continuing past it into undefined behaviour.
        export UBSAN_OPTIONS="print_stacktrace=1:halt_on_error=1${UBSAN_OPTIONS:+:${UBSAN_OPTIONS}}"
        # ui/tests/lsan.supp: specific, commented, third-party-only
        # suppressions (Qt/fontconfig/GL/offscreen-stack noise, or a
        # documented intentional Rust static -- ADR-0003 A17). Anything
        # suppressed there is never a ui/adapter, ui/tests or reldex_ffi
        # frame; see that file's header.
        export LSAN_OPTIONS="suppressions=${SCRIPT_DIR}/tests/lsan.supp${LSAN_OPTIONS:+:${LSAN_OPTIONS}}"
        # -V: print every test's own output, not just failing ones, so
        # tst_teardown's K5 iteration count and live-count assertions are
        # visible in the log as evidence, not just its pass/fail line.
        CTEST_ARGS+=(-V)
    fi
    ctest "${CTEST_ARGS[@]}"
fi
