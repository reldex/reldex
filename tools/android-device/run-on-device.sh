#!/usr/bin/env bash
# Runs the Reldex physical-device checks on an attached Android phone.
#
#   tools/android-device/run-on-device.sh
#   tools/android-device/run-on-device.sh --checks ping,tcps
#   tools/android-device/run-on-device.sh --skip-build --show-commands
#
# It cross-compiles `reldex-device-check` for `aarch64-linux-android`, pushes it
# (and, for the TCPS check, the test CA certificate) to /data/local/tmp, opens
# the `adb reverse` tunnels the device needs to reach this PC's loopback-only
# test database, runs the checks with the credentials in the device process's
# environment, and then removes the files it pushed.
#
# This is the primary entry point. `run-on-device.ps1` is a PowerShell twin
# kept in step with it; either produces the same run.
#
# It changes nothing on the phone: no developer-options or USB setting, no
# `adb usb`/`adb tcpip`/`adb reboot`/`adb kill-server`, and it never touches the
# USB-debugging authorisation. The `adb reverse` mappings are left in place for
# the next run unless `--remove-reverse` is passed.
#
# --- Secrets ---------------------------------------------------------------
# The password is read from `tools/oracle-test-db/.env` (untracked) at run time
# and handed to the device over `adb shell`'s **standard input**, so it never
# appears on a command line (not in this PC's process list, not in the device's
# `ps`), never lands on the device's filesystem, and never reaches this
# script's own output. Tracing is turned off around the read and never turned
# on; `--show-commands` prints the device command with the secret redacted.
#
# Only the **public** CA certificate (`wallet/ewallet.pem`) is pushed; the
# script refuses to push a file containing a private key.

set -euo pipefail
set +x           # never trace: the password passes through this shell.

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$script_dir/../.." && pwd)

remote_dir=/data/local/tmp/reldex-device-check
target=aarch64-linux-android
binary_name=reldex-device-check

checks=()
env_file=
api_level=26
skip_build=0
keep_files=0
remove_reverse=0
show_commands=0

usage() {
    sed -n '2,32p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    cat <<'EOF'

Options:
  --checks a,b,c     run only these checks, in order (default: all)
                     facts ping types transaction bulk tcps deadline
  --env-file PATH    where to read the database password from
  --api-level N      Android API level to build against (default 26)
  --skip-build       reuse the binary already in target/
  --keep-files       leave the pushed files on the device
  --remove-reverse   also remove the adb reverse mappings this run opened
  --show-commands    print the device command, with the password redacted
  -h, --help         this text
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --checks)         IFS=', ' read -r -a checks <<<"$2"; shift 2 ;;
        --env-file)       env_file=$2; shift 2 ;;
        --api-level)      api_level=$2; shift 2 ;;
        --skip-build)     skip_build=1; shift ;;
        --keep-files)     keep_files=1; shift ;;
        --remove-reverse) remove_reverse=1; shift ;;
        --show-commands)  show_commands=1; shift ;;
        -h|--help)        usage; exit 0 ;;
        -*)               echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
        *)                checks+=("$1"); shift ;;
    esac
done

step() { printf '==> %s\n' "$*"; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

# Git Bash rewrites an argument that looks like a POSIX path before handing it
# to a native Windows program: `adb shell ls /data/local/tmp` becomes
# `... ls C:/Program Files/Git/data/local/tmp`. Every adb call goes through
# this wrapper, which turns that off for the call.
adb_() { MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' adb "$@"; }

# A path a native Windows program will understand. A no-op off Windows.
native_path() {
    if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s' "$1"; fi
}

# --- 1. Device -------------------------------------------------------------

command -v adb >/dev/null 2>&1 ||
    die "adb is not on PATH. Add \$LOCALAPPDATA/Android/Sdk/platform-tools."

attached=$(adb_ devices | awk 'NR > 1 && $2 == "device" { print $1 }')
count=$(printf '%s\n' "$attached" | grep -c . || true)
[ "$count" -ne 0 ] ||
    die "No Android device is attached (or USB debugging is not authorised). 'adb devices' must list one as 'device'."
[ "$count" -eq 1 ] ||
    die "More than one device is attached; this script does not choose for you. Detach all but one."

step "Device facts"
for property in ro.product.model ro.product.manufacturer ro.build.version.release \
                ro.build.version.sdk ro.product.cpu.abi ro.build.id; do
    printf '    %-28s %s\n' "$property" "$(adb_ shell getprop "$property" | tr -d '\r')"
done
printf '    %-28s %s\n' kernel "$(adb_ shell uname -srm | tr -d '\r')"

# --- 2. Build --------------------------------------------------------------

binary="$repo/target/$target/release/$binary_name"

if [ "$skip_build" -eq 0 ]; then
    sdk=${ANDROID_SDK_ROOT:-${ANDROID_HOME:-${LOCALAPPDATA:-$HOME}/Android/Sdk}}
    [ -d "$sdk/ndk" ] || die "No Android NDK under $sdk/ndk."
    # Newest installed NDK, so a later upgrade needs no edit here.
    ndk=$(find "$sdk/ndk" -maxdepth 1 -mindepth 1 -type d | sort -V | tail -1)
    [ -n "$ndk" ] || die "No Android NDK under $sdk/ndk."

    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*) host_tag=windows-x86_64; clang_ext=.cmd; ar_ext=.exe ;;
        Darwin)               host_tag=darwin-x86_64;  clang_ext=;     ar_ext= ;;
        *)                    host_tag=linux-x86_64;   clang_ext=;     ar_ext= ;;
    esac
    ndk_bin="$ndk/toolchains/llvm/prebuilt/$host_tag/bin"
    clang="$ndk_bin/aarch64-linux-android$api_level-clang$clang_ext"
    llvm_ar="$ndk_bin/llvm-ar$ar_ext"
    [ -f "$clang" ] || die "$clang not found; API level $api_level is not in this NDK."

    step "Building $binary_name for $target (API $api_level) with NDK $(basename "$ndk")"
    # Exported for this process only, never written to a tracked
    # .cargo/config.toml: where the NDK lives is a property of the machine.
    # `cargo-ndk` is deliberately not used — these three variables are all the
    # Android build needs, and `aws-lc-sys` wants no cmake, NASM or bindgen.
    export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$(native_path "$clang")"
    export CC_aarch64_linux_android="$(native_path "$clang")"
    export AR_aarch64_linux_android="$(native_path "$llvm_ar")"
    cargo build --release --target "$target" \
        --manifest-path "$(native_path "$repo/Cargo.toml")" -p reldex-core-poc
fi
[ -f "$binary" ] || die "$binary not found; run without --skip-build."
printf '    binary %s bytes\n' "$(wc -c <"$binary" | tr -d ' ')"

# --- 3. Credentials --------------------------------------------------------

if [ -z "$env_file" ]; then
    # This checkout's own copy when it has one, otherwise the main checkout's:
    # a linked worktree does not carry the untracked .env.
    if [ -f "$repo/tools/oracle-test-db/.env" ]; then
        env_file="$repo/tools/oracle-test-db/.env"
    else
        env_file="$repo/../../tools/oracle-test-db/.env"
    fi
fi
[ -f "$env_file" ] ||
    die "$env_file not found. Pass --env-file <path> (default: the main checkout's tools/oracle-test-db/.env)."
env_file=$(cd "$(dirname "$env_file")" && pwd)/$(basename "$env_file")

# Parsed by hand rather than sourced: the file is data, not script, and this
# way a stray command in it cannot run. Nothing below echoes a value.
secret=$(
    while IFS= read -r line || [ -n "$line" ]; do
        line=${line%$'\r'}
        case "$line" in ''|'#'*) continue ;; esac
        key=${line%%=*}
        [ "$key" = "RELDEX_TEST_PWD" ] || continue
        value=${line#*=}
        value=${value#\"} ; value=${value%\"}
        value=${value#\'} ; value=${value%\'}
        printf '%s' "$value"
    done <"$env_file"
)
[ -n "$secret" ] || die "RELDEX_TEST_PWD is missing from $env_file"

# The device reaches this PC's loopback-only listeners through `adb reverse`,
# so from the device's point of view the database is on its own 127.0.0.1.
dsn='127.0.0.1:1521/RELDEX'
user='RELDEX_TEST'
# `localhost`, not `127.0.0.1`: the test listener's certificate carries the DNS
# name and no IP address, and host-name verification is on and cannot be turned
# off in this driver. Spike S8 found this; run-it.ps1 uses the same form.
tcps_dsn='tcps://localhost:2484/RELDEX'

# --- 4. Wallet (public CA certificate only) --------------------------------

wallet_source=$(dirname "$env_file")/wallet/ewallet.pem
push_wallet=0
if [ -f "$wallet_source" ]; then
    if grep -q -- '-----BEGIN .*PRIVATE KEY-----' "$wallet_source"; then
        die "$wallet_source contains a private key; refusing to push it to the device."
    fi
    grep -q -- '-----BEGIN CERTIFICATE-----' "$wallet_source" ||
        die "$wallet_source holds no certificate."
    push_wallet=1
else
    printf '    note: %s not found; the tcps check will skip.\n' "$wallet_source"
fi

# --- 5. Push + tunnels -----------------------------------------------------

reverses=()
pushed=0

cleanup() {
    step "Cleaning up"
    # Only this run's own artefacts. Nothing here touches a device setting, the
    # USB-debugging authorisation, or the adb server.
    if [ "$keep_files" -eq 0 ]; then
        if [ "$pushed" -eq 1 ]; then
            adb_ shell "rm -rf $remote_dir" >/dev/null 2>&1 || true
            printf '    removed %s\n' "$remote_dir"
        fi
    else
        printf '    --keep-files: %s is still on the device.\n' "$remote_dir"
    fi
    if [ "$remove_reverse" -eq 1 ]; then
        for port in "${reverses[@]:-}"; do
            [ -n "$port" ] || continue
            adb_ reverse --remove "tcp:$port" >/dev/null 2>&1 || true
        done
        printf '    removed %s reverse tunnel(s)\n' "${#reverses[@]}"
    elif [ "${#reverses[@]}" -gt 0 ]; then
        printf '    kept %s reverse tunnel(s); pass --remove-reverse to drop them\n' "${#reverses[@]}"
    fi
}
trap cleanup EXIT

step "Pushing to $remote_dir"
adb_ shell "rm -rf $remote_dir; mkdir -p $remote_dir/wallet" >/dev/null
pushed=1
adb_ push "$(native_path "$binary")" "$remote_dir/" >/dev/null
adb_ shell "chmod 700 $remote_dir/$binary_name" >/dev/null
if [ "$push_wallet" -eq 1 ]; then
    adb_ push "$(native_path "$wallet_source")" "$remote_dir/wallet/" >/dev/null
    adb_ shell "chmod 600 $remote_dir/wallet/ewallet.pem" >/dev/null
fi

step "Opening adb reverse tunnels (device loopback -> this PC's loopback)"
for port in 1521 2484; do
    adb_ reverse "tcp:$port" "tcp:$port" >/dev/null ||
        die "adb reverse tcp:$port failed."
    reverses+=("$port")
    printf '    tcp:%s -> tcp:%s\n' "$port" "$port"
done

# --- 6. Run ----------------------------------------------------------------

exports="export RELDEX_TEST_ORACLE_DSN='$dsn'; export RELDEX_TEST_ORACLE_USER='$user'"
if [ "$push_wallet" -eq 1 ]; then
    exports="$exports; export RELDEX_TEST_ORACLE_TCPS_DSN='$tcps_dsn'"
    exports="$exports; export RELDEX_TEST_ORACLE_TCPS_CA_DIR='$remote_dir/wallet'"
fi
# The password arrives on stdin. Two things make that robust:
#  * the `@@` sentinels, because a PowerShell caller (the .ps1 twin) prepends a
#    UTF-8 BOM to a native command's stdin and appends CR; stripping to the
#    sentinels removes both without touching the value. Harmless here, where
#    printf writes the bytes as given, and it keeps the two scripts identical.
#  * `read -r`, so a backslash in the password is not an escape.
prelude='IFS= read -r RPW; RPW=${RPW#*@@}; RPW=${RPW%@@*}; '
prelude="${prelude}export RELDEX_TEST_ORACLE_PASSWORD=\"\$RPW\"; unset RPW; $exports; "
argument=""
[ "${#checks[@]}" -eq 0 ] || argument=" ${checks[*]}"
remote_command="${prelude}exec $remote_dir/$binary_name$argument"

if [ "$show_commands" -eq 1 ]; then
    step "Commands, for the record (password redacted)"
    printf "    printf '@@<RELDEX_TEST_PWD>@@\\\\n' | adb shell -T '%s'\n" \
        "${remote_command//\$RPW/<redacted>}"
fi

step "Running $binary_name on the device"
# `-T` keeps adb from allocating a pty, so nothing echoes the secret back.
code=0
printf '@@%s@@\n' "$secret" | adb_ shell -T "$remote_command" || code=$?
unset secret

exit "$code"
