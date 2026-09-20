#!/usr/bin/env bash
# ui/tests/ffi_smoke/run.sh -- one-command configure+build+ctest for the
# Qt-free C/C++ smoke harness (M1.4, ADR-0003 D10 item 2). Bash-first per
# AGENTS.md "Scripts and shell": this is the primary, tested, documented
# entry point; it must run under Git Bash on Windows as well as on Linux and
# macOS.
#
# Usage:
#   bash ui/tests/ffi_smoke/run.sh [--sanitize] [--clean]
#
#   --sanitize  Configure -DRELDEX_SANITIZE=ON: -fsanitize=address,undefined
#               for the C/C++ targets (GCC/Clang only; reldex-ffi's Rust code
#               is never instrumented, but ASan still intercepts its
#               malloc/free calls from this process). Sets
#               ASAN_OPTIONS=detect_leaks=1 for the ctest run.
#   --clean     Remove this harness's build directory before configuring.
#
# This directory builds standalone (it has its own project(), see
# CMakeLists.txt) and does not need Qt at all. On Windows this sources
# tools/dev-env/env.sh for MSVC + CMake + Ninja, matching ui/build.sh; on
# Linux/macOS it assumes cmake/ninja/a C/C++ toolchain and the Rust toolchain
# are already on PATH.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." >/dev/null 2>&1 && pwd)"

SANITIZE=0
CLEAN=0

for arg in "$@"; do
    case "$arg" in
        --sanitize)
            SANITIZE=1
            ;;
        --clean)
            CLEAN=1
            ;;
        -h|--help)
            sed -n '2,17p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "run.sh: unknown argument: $arg" >&2
            exit 1
            ;;
    esac
done

BUILD_DIR="${REPO_ROOT}/build/ffi-smoke"
if [ "${SANITIZE}" -eq 1 ]; then
    BUILD_DIR="${BUILD_DIR}-asan"
fi

# --- Windows: bring in MSVC + CMake + Ninja (Git Bash / MSYS only) ---------
case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*)
        # shellcheck source=/dev/null
        source "${REPO_ROOT}/tools/dev-env/env.sh"
        ;;
    *)
        : # Linux/macOS: assume cmake/ninja/cc/c++/cargo are already on PATH.
        ;;
esac

if [ "${CLEAN}" -eq 1 ] && [ -d "${BUILD_DIR}" ]; then
    echo "run.sh: removing ${BUILD_DIR}"
    rm -rf "${BUILD_DIR}"
fi

CMAKE_CONFIGURE_ARGS=(-S "${SCRIPT_DIR}" -B "${BUILD_DIR}" -G Ninja -DCMAKE_BUILD_TYPE=RelWithDebInfo)
if [ "${SANITIZE}" -eq 1 ]; then
    CMAKE_CONFIGURE_ARGS+=(-DRELDEX_SANITIZE=ON)
fi

echo "run.sh: configuring into ${BUILD_DIR}"
cmake "${CMAKE_CONFIGURE_ARGS[@]}"

echo "run.sh: building"
cmake --build "${BUILD_DIR}"

echo "run.sh: running ctest"
if [ "${SANITIZE}" -eq 1 ]; then
    export ASAN_OPTIONS="detect_leaks=1${ASAN_OPTIONS:+:${ASAN_OPTIONS}}"
fi
ctest --test-dir "${BUILD_DIR}" --output-on-failure
