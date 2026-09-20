#!/usr/bin/env bash
# Regenerates crates/ffi/include/reldex.h from the crate's source.
#
# The header is generated output that is committed (ADR-0003 D7): the adapter
# builds against the committed copy, and `cargo test -p reldex-ffi --test
# header` fails when it no longer matches the source. Run this after any change
# to the ABI, and commit the result in the same change.
#
# Usage:
#   crates/ffi/gen-header.sh            # regenerate in place
#   crates/ffi/gen-header.sh --check    # fail if the committed copy is stale
#
# Bash-first per AGENTS.md: runs under Git Bash on Windows as well as on Linux
# and macOS. Needs `cbindgen` on PATH (`cargo install cbindgen`; the pinned
# version this header was generated with is recorded below).
set -euo pipefail

# The version used to generate the committed header. cbindgen's output changes
# between releases, so a different version is a reason to review the diff, not
# a failure.
EXPECTED_CBINDGEN_VERSION="0.29.4"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
config="${script_dir}/cbindgen.toml"
output="${script_dir}/include/reldex.h"

if ! command -v cbindgen >/dev/null 2>&1; then
  echo "error: cbindgen is not on PATH. Install it with:" >&2
  echo "  cargo install cbindgen --version ${EXPECTED_CBINDGEN_VERSION}" >&2
  exit 127
fi

actual_version="$(cbindgen --version | awk '{print $2}')"
if [ "${actual_version}" != "${EXPECTED_CBINDGEN_VERSION}" ]; then
  echo "note: cbindgen ${actual_version} (the committed header was generated with \
${EXPECTED_CBINDGEN_VERSION}); review the diff carefully." >&2
fi

check_only="no"
if [ "${1:-}" = "--check" ]; then
  check_only="yes"
elif [ -n "${1:-}" ]; then
  echo "usage: $(basename "$0") [--check]" >&2
  exit 2
fi

mkdir -p "${script_dir}/include"
tmp="$(mktemp)"
trap 'rm -f "${tmp}"' EXIT

# `--crate` rather than a path so cbindgen resolves the package through cargo
# metadata; `parse_deps = false` in the config keeps it to this crate's source.
cbindgen --config "${config}" --crate reldex-ffi --output "${tmp}" --quiet

if [ "${check_only}" = "yes" ]; then
  if ! diff -u "${output}" "${tmp}"; then
    echo >&2
    echo "error: ${output} is stale. Run crates/ffi/gen-header.sh and commit the result." >&2
    exit 1
  fi
  echo "reldex.h is up to date."
else
  mv "${tmp}" "${output}"
  trap - EXIT
  echo "wrote ${output}"
fi
