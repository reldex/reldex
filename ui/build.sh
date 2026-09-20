#!/usr/bin/env bash
# ui/build.sh -- one-command build for the Reldex Qt Quick UI skeleton
# (M1.5). Bash-first per AGENTS.md "Scripts and shell": this is the primary,
# tested, documented entry point; it must run under Git Bash on Windows as
# well as on Linux and macOS.
#
# Usage:
#   bash ui/build.sh [--release] [--test] [--clean]
#
#   --release  Configure CMAKE_BUILD_TYPE=Release instead of the default
#              RelWithDebInfo (see ui/CMakeLists.txt for why RelWithDebInfo
#              is the default for this skeleton).
#   --test     Run `ctest --output-on-failure` after a successful build.
#   --clean    Remove the build directory before configuring (a full,
#              from-scratch build).
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
        -h|--help)
            sed -n '2,19p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "ui/build.sh: unknown argument: $arg" >&2
            exit 1
            ;;
    esac
done

# Build tree lives outside ui/, at the repo root, and is git-ignored
# (root .gitignore's generic "build/" pattern already covers it).
BUILD_DIR="${REPO_ROOT}/build/ui-${BUILD_TYPE}"

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

echo "ui/build.sh: configuring (${BUILD_TYPE}) into ${BUILD_DIR}"
cmake -S "${SCRIPT_DIR}" -B "${BUILD_DIR}" -G Ninja \
    -DCMAKE_BUILD_TYPE="${BUILD_TYPE}"

echo "ui/build.sh: building"
cmake --build "${BUILD_DIR}"

if [ "${RUN_TESTS}" -eq 1 ]; then
    echo "ui/build.sh: running ctest"
    ctest --test-dir "${BUILD_DIR}" --output-on-failure
fi
