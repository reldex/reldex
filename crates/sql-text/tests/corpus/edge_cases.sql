-- Labelled anonymous block containing a labelled nested block. Neither
-- label's `<<`/`>>`/name may be mistaken for a block starter or for
-- ordinary tracked depth keywords.
<<outer_block>>
DECLARE
  v_count NUMBER := 0;
BEGIN
  <<inner_block>>
  BEGIN
    v_count := v_count + 1;
  END inner_block;
END outer_block;
/
SELECT 1 FROM dual;

-- A nested subprogram declared inside a procedure's own declare section:
-- the inner PROCEDURE's BEGIN/END must not be confused with the outer's.
CREATE OR REPLACE PROCEDURE outer_p AS
  PROCEDURE inner_p IS
  BEGIN
    NULL;
  END inner_p;
BEGIN
  DELETE FROM important_table;
END outer_p;
/
SELECT 2 FROM dual;

-- A package body with two members (one containing its own nested anonymous
-- block) *and* an initialization section after them.
CREATE OR REPLACE PACKAGE BODY pkg_demo AS
  PROCEDURE do_it(p_id IN NUMBER) IS
    v_count NUMBER := 0;
  BEGIN
    FOR i IN 1..10 LOOP
      v_count := v_count + i;
    END LOOP;
    BEGIN
      NULL;
    END;
    COMMIT;
  END do_it;

  FUNCTION calc(p_x IN NUMBER) RETURN NUMBER IS
  BEGIN
    RETURN p_x * 2;
  END calc;
BEGIN
  v_init_flag := 1;
END pkg_demo;
/
SELECT 3 FROM dual;

-- A compound trigger with two timing-point sections, each its own
-- BEGIN/END pair, followed by the trigger's own close.
CREATE OR REPLACE TRIGGER compound_trg
FOR INSERT ON some_table
COMPOUND TRIGGER

  BEFORE STATEMENT IS
  BEGIN
    NULL;
  END BEFORE STATEMENT;

  AFTER STATEMENT IS
  BEGIN
    NULL;
  END AFTER STATEMENT;

END compound_trg;
/
DROP TABLE should_still_run_after_compound_trigger;

-- A trigger whose body is a bare CALL, with no BEGIN/END at all.
CREATE TRIGGER call_trg AFTER INSERT ON x FOR EACH ROW CALL p(:NEW.id);
SELECT 4 FROM dual;

-- An object type spec: parenthesized attribute/method list, no BEGIN/END.
CREATE TYPE point_t AS OBJECT (
  x NUMBER,
  y NUMBER,
  MEMBER FUNCTION distance_to(other IN point_t) RETURN NUMBER
);
SELECT 5 FROM dual;

-- The simple type-synonym form: no parens at all either.
CREATE TYPE happy_day IS BOOLEAN;
SELECT 6 FROM dual;

-- Forward declarations in a package spec: each ends at its own `;`, owing
-- no body.
CREATE OR REPLACE PACKAGE forward_decls AS
  PROCEDURE go;
  FUNCTION calc_it(p_x NUMBER) RETURN NUMBER;
END forward_decls;
/

-- A call-spec: IS LANGUAGE JAVA ... ends at its own terminator, no PL/SQL
-- body at all.
CREATE OR REPLACE FUNCTION native_add(a NUMBER, b NUMBER) RETURN NUMBER
  IS LANGUAGE JAVA
  NAME 'Adder.add(int, int) return int';
SELECT 7 FROM dual;

-- Opaque (non-PL/SQL) Java source: its own `;` characters are ordinary
-- content, not terminators.
CREATE OR REPLACE AND COMPILE JAVA SOURCE NAMED "Adder" AS
public class Adder {
  public static int add(int a, int b) { return a + b; }
}
/
SELECT 8 FROM dual;

-- Conditional-compilation directives inside a block: `$END`'s `END` must
-- never be read as the block's own closing keyword.
BEGIN
  $IF $$my_flag $THEN
    NULL;
  $ELSE
    NULL;
  $END;
  COMMIT;
END;
/
DROP TABLE should_also_still_run_after_dollar_if;

-- EXECUTE IMMEDIATE with an inline PL/SQL block as a string literal: the
-- `;`s inside the string must not be read as real terminators (already true
-- because they are inside a String token, not scanned as depth keywords).
BEGIN
  EXECUTE IMMEDIATE 'BEGIN NULL; END;';
END;
/

-- A SQL CASE *expression* (bare `END`, no `END CASE`) inside a block.
BEGIN
  SELECT CASE WHEN 1 = 1 THEN 'a' ELSE 'b' END INTO v_result FROM dual;
END;
/

-- `END` used as a quoted identifier/alias must not be read as a keyword at
-- all (it is a QuotedIdentifier token, not Keyword/Identifier).
BEGIN
  SELECT x AS "END" INTO v_x FROM t;
END;
/

-- Two consecutive lone `/` lines: the second is its own (empty) statement.
SELECT 9 FROM dual;
/
/
SELECT 10 FROM dual;
