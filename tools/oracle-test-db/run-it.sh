#!/usr/bin/env sh
# Runs the opt-in Oracle integration tests against the local test database.
#
#   sh tools/oracle-test-db/run-it.sh                  # every spike
#   sh tools/oracle-test-db/run-it.sh s2_fidelity      # one test file
#   sh tools/oracle-test-db/run-it.sh s4_cancel -- --nocapture
#   RELDEX_IT_PACKAGE=reldex-core-poc sh tools/oracle-test-db/run-it.sh m5_2_result_store_live
#
# `RELDEX_IT_PACKAGE` picks the crate whose `oracle-it` tests run; the default
# is the Oracle driver's. Tests that need `db-core` as well as the driver live
# in `reldex-core-poc` (a driver crate may not depend on `db-core`).
#
# It loads `tools/oracle-test-db/.env` (untracked; see `.env.example`) and turns
# it into the environment the tests read. The passwords are never echoed, never
# passed on a command line, and never written to a file: they are exported into
# this shell, which the child `cargo` inherits.
#
# The tests are behind the `oracle-it` feature, so `cargo test --workspace`
# stays green on a machine with no database.

set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo=$(CDPATH= cd -- "$here/../.." && pwd)
env_file="$here/.env"

if [ ! -f "$env_file" ]; then
    echo "$env_file not found. Copy .env.example to .env and fill it in (see README.md)." >&2
    exit 1
fi

# Read key=value pairs without `source`, so a stray shell metacharacter in a
# password cannot be executed.
read_value() {
    sed -n "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*//p" "$env_file" \
        | head -n 1 \
        | sed -e 's/^"//' -e "s/^'//" -e 's/"$//' -e "s/'$//" -e 's/[[:space:]]*$//'
}

reldex_pwd=$(read_value RELDEX_TEST_PWD)
oracle_pwd=$(read_value ORACLE_PWD)

if [ -z "$reldex_pwd" ]; then
    echo "RELDEX_TEST_PWD is missing from $env_file" >&2
    exit 1
fi
if [ -z "$oracle_pwd" ]; then
    echo "ORACLE_PWD is missing from $env_file" >&2
    exit 1
fi

# The listener answers to the SERVICE NAME, not the SID: the `//host:port:SID`
# shorthand does not work against this container (see README.md).
RELDEX_TEST_ORACLE_DSN='127.0.0.1:1521/RELDEX'
RELDEX_TEST_ORACLE_USER='RELDEX_TEST'
RELDEX_TEST_ORACLE_PASSWORD="$reldex_pwd"
export RELDEX_TEST_ORACLE_DSN RELDEX_TEST_ORACLE_USER RELDEX_TEST_ORACLE_PASSWORD

# Opt-in extra, used only by spike S4's privileged-cancel candidate
# (`ALTER SYSTEM CANCEL SQL`). Tests that need it skip themselves when it is
# absent, so an ordinary run never requires a DBA password.
RELDEX_TEST_ORACLE_SYSTEM_USER='SYSTEM'
RELDEX_TEST_ORACLE_SYSTEM_PASSWORD="$oracle_pwd"
export RELDEX_TEST_ORACLE_SYSTEM_USER RELDEX_TEST_ORACLE_SYSTEM_PASSWORD

# Opt-in extra, used only by spike S13 (`AS SYSDBA` over the listener, which
# this image authenticates against its password file). Same password as above;
# a separate pair of variables so the role is explicit at the call site and a
# checkout that does not want a SYSDBA test can unset just these two.
RELDEX_TEST_ORACLE_SYSDBA_USER='SYS'
RELDEX_TEST_ORACLE_SYSDBA_PASSWORD="$oracle_pwd"
export RELDEX_TEST_ORACLE_SYSDBA_USER RELDEX_TEST_ORACLE_SYSDBA_PASSWORD

# Spike S8 (TCPS). Set only when the TLS listener has been enabled and the
# CA certificate exported (`startup/10_enable_tcps.sh`, then the export step in
# README.md, "TCPS"); the S8 tests skip themselves and say so otherwise.
#
# `localhost`, not `127.0.0.1`: the listener's certificate carries the DNS name
# and no IP address, and the client verifies whatever the descriptor's HOST
# says. That is not a quirk of this setup — it is what S8 found, and
# `s8_tcps.rs` has a test that depends on the numeric form failing.
if [ -f "$here/wallet/ewallet.pem" ]; then
    RELDEX_TEST_ORACLE_TCPS_DSN='tcps://localhost:2484/RELDEX'
    RELDEX_TEST_ORACLE_TCPS_CA_DIR="$here/wallet"
    export RELDEX_TEST_ORACLE_TCPS_DSN RELDEX_TEST_ORACLE_TCPS_CA_DIR
    if [ -f "$here/wallet-untrusted/ewallet.pem" ]; then
        RELDEX_TEST_ORACLE_TCPS_WRONG_CA_DIR="$here/wallet-untrusted"
        export RELDEX_TEST_ORACLE_TCPS_WRONG_CA_DIR
    fi
    echo "tcps:     $RELDEX_TEST_ORACLE_TCPS_DSN (CA from $RELDEX_TEST_ORACLE_TCPS_CA_DIR)"
fi

echo "database: $RELDEX_TEST_ORACLE_USER@$RELDEX_TEST_ORACLE_DSN"

package="${RELDEX_IT_PACKAGE:-reldex-driver-oracle-thin}"
echo "package:  $package"

cd "$repo"
# A leading `--` means "no test file was named; pass the rest to the harness".
if [ "$#" -gt 0 ] && [ "$1" != "--" ]; then
    first="$1"
    shift
    exec cargo test -p "$package" --features oracle-it --test "$first" "$@"
fi
exec cargo test -p "$package" --features oracle-it "$@"
