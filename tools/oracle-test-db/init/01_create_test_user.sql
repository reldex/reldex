-- ---------------------------------------------------------------------------
-- tools/oracle-test-db/init/01_create_test_user.sql
--
-- Creates (idempotently) a local application/test user, RELDEX_TEST, used by
-- Phase 0 driver integration tests. Safe to re-run: every statement either
-- checks for existence first or ignores an "already exists" style error.
--
-- Run automatically by the doctorkirk/oracle-19c image's setup hook on first
-- container start (mounted read-only to /opt/oracle/scripts/setup), or
-- manually via:
--   docker exec -i reldex-oracle19c sqlplus / as sysdba < 01_create_test_user.sql
--
-- The image's setup hook does not expand shell/env vars inside the SQL
-- file, so the password below is a documented local dev default (matches
-- .env.example's RELDEX_TEST_PWD). Change it here and in .env if you need a
-- different value - this is a local-only container bound to 127.0.0.1.
-- ---------------------------------------------------------------------------

WHENEVER SQLERROR CONTINUE

-- This image is Single-Instance Non-CDB (no pluggable databases), so no
-- `ALTER SESSION SET CONTAINER` is needed or valid here.

DECLARE
  v_count NUMBER;
BEGIN
  SELECT COUNT(*) INTO v_count FROM dba_users WHERE username = 'RELDEX_TEST';

  IF v_count = 0 THEN
    EXECUTE IMMEDIATE
      'CREATE USER RELDEX_TEST IDENTIFIED BY "Reldex_Test_19c" ' ||
      'DEFAULT TABLESPACE USERS TEMPORARY TABLESPACE TEMP';
  END IF;
END;
/

-- Quota (idempotent - re-issuing an unlimited quota is a no-op).
ALTER USER RELDEX_TEST QUOTA UNLIMITED ON USERS;

-- Core session/object privileges (each GRANT is idempotent in Oracle -
-- re-granting an already-held privilege does not error).
GRANT CREATE SESSION   TO RELDEX_TEST;
GRANT CREATE TABLE     TO RELDEX_TEST;
GRANT CREATE VIEW      TO RELDEX_TEST;
GRANT CREATE SEQUENCE  TO RELDEX_TEST;
GRANT CREATE PROCEDURE TO RELDEX_TEST;
GRANT CREATE TRIGGER   TO RELDEX_TEST;
GRANT CREATE TYPE      TO RELDEX_TEST;
GRANT CREATE SYNONYM   TO RELDEX_TEST;

-- Catalog/dictionary visibility, needed for Object Browser / metadata
-- exploration tests.
GRANT SELECT_CATALOG_ROLE     TO RELDEX_TEST;
GRANT SELECT ANY DICTIONARY   TO RELDEX_TEST;

-- DBMS_LOCK.SLEEP is used to simulate long-running queries for
-- cancellation tests. DBMS_SESSION.SLEEP (19c) is available to all users
-- by default and does not require an extra grant.
GRANT EXECUTE ON DBMS_LOCK TO RELDEX_TEST;

-- DBMS_XPLAN / PLAN_TABLE usability for explain-plan tests. PLAN_TABLE is
-- normally usable via the public synonym without a direct grant; the
-- EXECUTE grant below covers DBMS_XPLAN itself for completeness.
GRANT EXECUTE ON DBMS_XPLAN TO RELDEX_TEST;

EXIT;
