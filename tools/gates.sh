#!/usr/bin/env bash
# tools/gates.sh -- local pre-push / pre-PR gate (ADR-0005, 2026-09-24).
#
# Run this before opening or updating a PR. It is a fast local superset of
# what hosted CI checks (plus the header check, a doc-lint on the crates
# that are clean today, and, when discoverable/healthy on this machine, the
# Qt UI build+test and the real-database integration suite), so most
# failures are caught here instead of waiting on a CI round trip.
#
# **CI remains the merge gate.** The repository is public specifically so
# hosted-runner minutes are free (ADR-0005, superseding the earlier plan to
# stop using Actions) -- `.github/workflows/{ci,ui,mobile-cross-compile}.yml`
# are unchanged and still gate merges on all three OS. This script does not
# replace that; paste its summary line into the PR body as a second signal,
# not as a substitute for a green CI run.
#
# What this proves and what it does NOT: this only ever runs on ONE machine
# (Windows, MSVC, this developer's toolchain) against the local Oracle 19c
# test container. It does not by itself verify Linux, macOS, AddressSanitizer/
# UBSan/LeakSanitizer, or iOS cross-compile -- CI covers those. Read every
# SKIP line in the summary before trusting a green run: SKIP is not
# evidence, it is a gap this particular run did not close.
#
# Usage:
#   bash tools/gates.sh                 # every stage
#   bash tools/gates.sh --quick         # fmt, clippy, test, header only
#   bash tools/gates.sh --no-db         # skip the real-database suite
#   bash tools/gates.sh --no-ui         # skip the Qt/CMake build+test
#   bash tools/gates.sh --only <stage>  # run exactly one stage
#
# Stages, in order: fmt, clippy, test, header, doc, ui, db.
#   fmt     cargo fmt --all -- --check
#   clippy  cargo clippy --workspace --all-targets -- -D warnings, plus the
#           oracle-thin oracle-it feature build and the reldex-ffi
#           --no-default-features build (ADR-0003 A8: a cargo feature must
#           never change the ABI; the default-mock build must stay clean too)
#   test    cargo test --workspace
#   header  bash crates/ffi/gen-header.sh --check (crates/ffi/include/reldex.h
#           must not be stale -- ADR-0003 D7)
#   doc     RUSTDOCFLAGS="-D warnings" cargo doc --no-deps, scoped to the
#           crates that are clean today (reldex-ffi, reldex-sql-text,
#           reldex-db-driver-api, reldex-mobile-link-check,
#           reldex-core-poc, reldex-workspace). reldex-driver-mock, reldex-db-core and
#           reldex-driver-oracle-thin have pre-existing broken/private
#           intra-doc links, unrelated to this script; not fixed here.
#   ui      bash ui/build.sh --test, only when Qt 6.8 + CMake + Ninja (and,
#           on Windows, Visual Studio via vswhere.exe) are discoverable;
#           otherwise SKIP with the specific reason.
#   db      the whole reldex-driver-oracle-thin `oracle-it` integration
#           suite against tools/oracle-test-db/, only when the
#           `reldex-oracle19c` container reports healthy (this script may
#           `docker start` it, nothing else with docker); otherwise SKIP
#           with the specific reason. Never prints the container's
#           credentials -- tools/oracle-test-db/run-it.sh already keeps them
#           out of argv and out of any log this script produces.
#
# Bash-first per AGENTS.md "Scripts and shell": runs under Git Bash on
# Windows as well as on Linux/macOS. No PowerShell twin (optional per
# AGENTS.md; not provided here).
#
# Exit status: non-zero if any RUN stage FAILed. A SKIPped stage never fails
# the run by itself -- the summary line is where a reader notices the gap.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." >/dev/null 2>&1 && pwd)"
cd "${REPO_ROOT}"

ALL_STAGES=(fmt clippy test header doc ui db)
declare -A STAGE_STATUS
declare -A STAGE_SECONDS
OVERALL_FAIL=0

QUICK=0
NO_DB=0
NO_UI=0
ONLY=""

usage() {
    sed -n '2,45p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --quick)
            QUICK=1
            shift
            ;;
        --no-db)
            NO_DB=1
            shift
            ;;
        --no-ui)
            NO_UI=1
            shift
            ;;
        --only)
            ONLY="${2:-}"
            if [ -z "${ONLY}" ]; then
                echo "tools/gates.sh: --only requires a stage name" >&2
                exit 2
            fi
            shift 2
            ;;
        --only=*)
            ONLY="${1#--only=}"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "tools/gates.sh: unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

if [ -n "${ONLY}" ]; then
    valid=0
    for s in "${ALL_STAGES[@]}"; do
        [ "$s" = "${ONLY}" ] && valid=1
    done
    if [ "${valid}" -ne 1 ]; then
        echo "tools/gates.sh: unknown stage for --only: ${ONLY} (expected one of: ${ALL_STAGES[*]})" >&2
        exit 2
    fi
fi

if [ -n "${ONLY}" ]; then
    SELECTED=("${ONLY}")
elif [ "${QUICK}" -eq 1 ]; then
    SELECTED=(fmt clippy test header)
else
    SELECTED=(fmt clippy test header doc ui db)
fi

is_selected() {
    local s="$1" x
    for x in "${SELECTED[@]}"; do
        [ "$x" = "$s" ] && return 0
    done
    return 1
}

# --- timing / result bookkeeping -------------------------------------------

run_timed() {
    # $1 = stage name, remaining args = the command to run.
    local name="$1"
    shift
    local t0 t1 elapsed rc
    t0=$(date +%s)
    if "$@"; then
        rc=0
    else
        rc=$?
    fi
    t1=$(date +%s)
    elapsed=$((t1 - t0))
    if [ "${rc}" -eq 0 ]; then
        STAGE_STATUS[$name]="PASS"
    else
        STAGE_STATUS[$name]="FAIL"
        OVERALL_FAIL=1
    fi
    STAGE_SECONDS[$name]="${elapsed}"
    printf '== %s: %s (%ss)\n' "${name}" "${STAGE_STATUS[$name]}" "${elapsed}"
}

skip_stage() {
    local name="$1" reason="$2"
    STAGE_STATUS[$name]="SKIP (${reason})"
    STAGE_SECONDS[$name]=0
    printf '== %s: %s\n' "${name}" "${STAGE_STATUS[$name]}"
}

# --- stage bodies ------------------------------------------------------

do_fmt() {
    cargo fmt --all -- --check
}

do_clippy() {
    local rc=0
    cargo clippy --workspace --all-targets -- -D warnings || rc=1
    # ADR-0003 A8: the oracle-thin driver's opt-in integration-test feature
    # must also stay clippy-clean, and so must the FFI boundary built with
    # no concrete driver at all (the shape a real product build takes).
    cargo clippy -p reldex-driver-oracle-thin --all-targets --features oracle-it -- -D warnings || rc=1
    cargo clippy -p reldex-ffi --no-default-features -- -D warnings || rc=1
    return "${rc}"
}

do_test() {
    cargo test --workspace
}

do_header() {
    bash crates/ffi/gen-header.sh --check
}

# Crates that document cleanly today under -D warnings. reldex-driver-mock,
# reldex-db-core and reldex-driver-oracle-thin do NOT (pre-existing broken
# and private intra-doc links, unrelated to this change). Scoping the gate
# to the clean set, rather than fixing or silently dropping the doc gate
# entirely, is the deliberate choice here.
DOC_CLEAN_CRATES=(
    reldex-ffi
    reldex-sql-text
    reldex-db-driver-api
    reldex-mobile-link-check
    reldex-core-poc
    reldex-workspace
)

do_doc() {
    local args=(doc --no-deps)
    local c
    for c in "${DOC_CLEAN_CRATES[@]}"; do
        args+=(-p "$c")
    done
    RUSTDOCFLAGS="-D warnings" cargo "${args[@]}"
}

# Reason string on stdout, exit 0 if discoverable / 1 if not.
ui_discoverable_reason() {
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*)
            local vswhere="/c/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe"
            if [ ! -x "${vswhere}" ]; then
                echo "Visual Studio not found (no vswhere.exe at the expected path)"
                return 1
            fi
            if [ ! -d "/c/Qt/6.8.3/msvc2022_64" ] \
                && [ -z "${QT_DIR:-}" ] && [ -z "${CMAKE_PREFIX_PATH:-}" ] && [ -z "${Qt6_DIR:-}" ]; then
                echo "no local Qt 6.8.3 msvc2022_64 install and no QT_DIR/CMAKE_PREFIX_PATH/Qt6_DIR set"
                return 1
            fi
            if [ ! -x "/c/Qt/Tools/CMake/bin/cmake.exe" ] && ! command -v cmake >/dev/null 2>&1; then
                echo "cmake not found on PATH or under C:/Qt/Tools/CMake"
                return 1
            fi
            if [ ! -x "/c/Qt/Tools/Ninja/ninja.exe" ] && ! command -v ninja >/dev/null 2>&1; then
                echo "ninja not found on PATH or under C:/Qt/Tools/Ninja"
                return 1
            fi
            return 0
            ;;
        *)
            if ! command -v cmake >/dev/null 2>&1; then
                echo "cmake not on PATH"
                return 1
            fi
            if ! command -v ninja >/dev/null 2>&1; then
                echo "ninja not on PATH"
                return 1
            fi
            if [ -z "${CMAKE_PREFIX_PATH:-}" ] && [ -z "${Qt6_DIR:-}" ]; then
                echo "Qt6 not discoverable (set CMAKE_PREFIX_PATH or Qt6_DIR); see ui/README.md 'Known limitations'"
                return 1
            fi
            return 0
            ;;
    esac
}

maybe_run_ui() {
    local reason
    if ! reason=$(ui_discoverable_reason); then
        skip_stage ui "${reason}"
        return
    fi
    run_timed ui bash ui/build.sh --test
}

# Reason string on stdout, exit 0 if the DB is ready / 1 if not. Never
# prints anything from tools/oracle-test-db/.env.
db_health_reason() {
    if ! command -v docker >/dev/null 2>&1; then
        echo "docker not on PATH"
        return 1
    fi
    if ! MSYS_NO_PATHCONV=1 docker inspect reldex-oracle19c >/dev/null 2>&1; then
        echo "container reldex-oracle19c does not exist (see tools/oracle-test-db/README.md)"
        return 1
    fi
    local status
    status=$(MSYS_NO_PATHCONV=1 docker inspect -f '{{.State.Health.Status}}' reldex-oracle19c 2>/dev/null || echo "unknown")
    if [ "${status}" = "healthy" ]; then
        return 0
    fi
    # Nudge it awake, but do not block the whole gate run on an Oracle
    # container's full startup (it can take minutes): one short wait, then
    # SKIP with the observed status rather than hang.
    echo "starting reldex-oracle19c (was: ${status})..." >&2
    MSYS_NO_PATHCONV=1 docker start reldex-oracle19c >/dev/null 2>&1 || true
    sleep 5
    status=$(MSYS_NO_PATHCONV=1 docker inspect -f '{{.State.Health.Status}}' reldex-oracle19c 2>/dev/null || echo "unknown")
    if [ "${status}" = "healthy" ]; then
        return 0
    fi
    echo "reldex-oracle19c is not healthy yet (status: ${status}) -- wait for it and re-run"
    return 1
}

maybe_run_db() {
    local reason
    if ! reason=$(db_health_reason); then
        skip_stage db "${reason}"
        return
    fi
    run_timed db bash tools/oracle-test-db/run-it.sh
}

# --- run selected stages, in order --------------------------------------

for stage in "${ALL_STAGES[@]}"; do
    if ! is_selected "${stage}"; then
        if [ -n "${ONLY}" ]; then
            skip_stage "${stage}" "not selected (--only ${ONLY})"
        else
            skip_stage "${stage}" "not selected (--quick)"
        fi
        continue
    fi

    case "${stage}" in
        fmt) run_timed fmt do_fmt ;;
        clippy) run_timed clippy do_clippy ;;
        test) run_timed test do_test ;;
        header) run_timed header do_header ;;
        doc) run_timed doc do_doc ;;
        ui)
            if [ "${NO_UI}" -eq 1 ]; then
                skip_stage ui "--no-ui"
            else
                maybe_run_ui
            fi
            ;;
        db)
            if [ "${NO_DB}" -eq 1 ]; then
                skip_stage db "--no-db"
            else
                maybe_run_db
            fi
            ;;
    esac
done

# --- summary -------------------------------------------------------------

os_name() {
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*) echo "Windows" ;;
        Linux*) echo "Linux" ;;
        Darwin*) echo "macOS" ;;
        *) uname -s ;;
    esac
}

sha="$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")"
date_str="$(date +%Y-%m-%d)"

parts=()
for s in "${ALL_STAGES[@]}"; do
    if [ "${STAGE_STATUS[$s]}" = "PASS" ] || [ "${STAGE_STATUS[$s]}" = "FAIL" ]; then
        parts+=("${s} ${STAGE_STATUS[$s]} (${STAGE_SECONDS[$s]}s)")
    else
        parts+=("${s} ${STAGE_STATUS[$s]}")
    fi
done

joined=""
for p in "${parts[@]}"; do
    if [ -z "${joined}" ]; then
        joined="${p}"
    else
        joined="${joined} · ${p}"
    fi
done
summary_line="Gates (local, $(os_name), ${date_str}, \`${sha}\`): ${joined}"

echo
echo "${summary_line}"

if [ "${OVERALL_FAIL}" -ne 0 ]; then
    exit 1
fi
exit 0
