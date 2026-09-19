#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# tools/oracle-test-db/init/01_create_test_user.sh
#
# Creates (idempotently) the local test user RELDEX_TEST used by the Phase 0
# driver integration tests, taking its password from the container's
# environment rather than from this file.
#
# This replaces the earlier `01_create_test_user.sql`, which carried
# `IDENTIFIED BY "<a literal>"`. That literal was a throwaway local-only
# default for a database bound to 127.0.0.1, but a tracked file with a
# password in it is a habit worth not having: nothing here is a credential,
# and the value comes from the untracked `tools/oracle-test-db/.env` by way of
# `compose.yaml`.
#
# The image's own hook (`/opt/oracle/runUserScripts.sh`) sources every `*.sh`
# in `/opt/oracle/scripts/setup` on first container start, so this runs with
# the container's environment and `ORACLE_HOME` already set.
#
# It is also safe to run by hand against a container that is already up — the
# usual case after changing the password, and the way it is verified without
# destroying the database:
#
#   docker exec -e RELDEX_TEST_PWD="$RELDEX_TEST_PWD" reldex-oracle19c \
#       bash /opt/oracle/scripts/setup/01_create_test_user.sh
#
# Every statement below is idempotent: re-running it against an existing user
# resets the password and re-grants privileges it already holds, which Oracle
# treats as a no-op.
# ---------------------------------------------------------------------------

set -u

reldex_user="${RELDEX_TEST_USER:-RELDEX_TEST}"

if [ -z "${RELDEX_TEST_PWD:-}" ]; then
    echo "01_create_test_user.sh: RELDEX_TEST_PWD is not set in the container" >&2
    echo "  environment, so ${reldex_user} was NOT created. Set it in" >&2
    echo "  tools/oracle-test-db/.env (see .env.example) and re-run this hook:" >&2
    echo "    docker exec -e RELDEX_TEST_PWD=... reldex-oracle19c \\" >&2
    echo "        bash /opt/oracle/scripts/setup/01_create_test_user.sh" >&2
    exit 1
fi

# The password is interpolated into SQL text below, so refuse the characters
# that would end the quoted identifier or start a SQL*Plus substitution. This
# is a local dev container, but a setup script that can be turned into
# arbitrary SQL by its own configuration is not worth keeping.
case "$RELDEX_TEST_PWD" in
    *\"* | *\'* | *\&* | *\;* | *' '* | *'	'*)
        echo "01_create_test_user.sh: RELDEX_TEST_PWD must not contain a quote," >&2
        echo "  ampersand, semicolon or whitespace. Use letters, digits and" >&2
        echo "  underscores, starting with a letter (Oracle's own rule on this" >&2
        echo "  image is stricter than SQL*Plus's)." >&2
        exit 1
        ;;
esac

case "$reldex_user" in
    *[!A-Za-z0-9_]*)
        echo "01_create_test_user.sh: RELDEX_TEST_USER must be a plain identifier" >&2
        exit 1
        ;;
esac

sqlplus_bin="${ORACLE_HOME:-/opt/oracle/product/19c/dbhome_1}/bin/sqlplus"

# The password reaches SQL*Plus on stdin, never on a command line (where it
# would be visible in `ps`) and never in a file.
"$sqlplus_bin" -s -L "/ as sysdba" <<SQL
SET ECHO OFF
SET VERIFY OFF
SET FEEDBACK OFF
SET TERMOUT ON
WHENEVER SQLERROR EXIT 1

-- This image is Single-Instance Non-CDB (no pluggable databases), so no
-- \`ALTER SESSION SET CONTAINER\` is needed or valid here.
DECLARE
  v_count NUMBER;
  v_user  VARCHAR2(128) := '${reldex_user}';
  v_pwd   VARCHAR2(128) := '${RELDEX_TEST_PWD}';
BEGIN
  SELECT COUNT(*) INTO v_count FROM dba_users WHERE username = v_user;
  IF v_count = 0 THEN
    EXECUTE IMMEDIATE
      'CREATE USER ' || v_user || ' IDENTIFIED BY "' || v_pwd || '" ' ||
      'DEFAULT TABLESPACE USERS TEMPORARY TABLESPACE TEMP';
  ELSE
    -- Idempotent, and it is what makes changing the password in \`.env\` take
    -- effect without recreating the database.
    EXECUTE IMMEDIATE
      'ALTER USER ' || v_user || ' IDENTIFIED BY "' || v_pwd || '"';
  END IF;
END;
/

-- Quota (idempotent - re-issuing an unlimited quota is a no-op).
ALTER USER ${reldex_user} QUOTA UNLIMITED ON USERS;

-- Core session/object privileges (each GRANT is idempotent in Oracle -
-- re-granting an already-held privilege does not error).
GRANT CREATE SESSION   TO ${reldex_user};
GRANT CREATE TABLE     TO ${reldex_user};
GRANT CREATE VIEW      TO ${reldex_user};
GRANT CREATE SEQUENCE  TO ${reldex_user};
GRANT CREATE PROCEDURE TO ${reldex_user};
GRANT CREATE TRIGGER   TO ${reldex_user};
GRANT CREATE TYPE      TO ${reldex_user};
GRANT CREATE SYNONYM   TO ${reldex_user};

-- Catalog/dictionary visibility, needed for Object Browser / metadata
-- exploration tests.
GRANT SELECT_CATALOG_ROLE   TO ${reldex_user};
GRANT SELECT ANY DICTIONARY TO ${reldex_user};

-- DBMS_LOCK.SLEEP is used to simulate long-running queries for cancellation
-- tests. DBMS_SESSION.SLEEP (19c) is available to all users by default and
-- does not require an extra grant.
GRANT EXECUTE ON DBMS_LOCK TO ${reldex_user};

-- DBMS_XPLAN / PLAN_TABLE usability for explain-plan tests. PLAN_TABLE is
-- normally usable via the public synonym without a direct grant; the EXECUTE
-- grant below covers DBMS_XPLAN itself for completeness.
GRANT EXECUTE ON DBMS_XPLAN TO ${reldex_user};

EXIT SUCCESS
SQL

status=$?
if [ "$status" -eq 0 ]; then
    echo "01_create_test_user.sh: ${reldex_user} is ready"
else
    echo "01_create_test_user.sh: sqlplus exited with $status" >&2
fi
exit "$status"
