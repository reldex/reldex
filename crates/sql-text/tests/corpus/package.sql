-- Package spec and body exercising nested blocks, PL/SQL constructs, and a
-- trailing comment containing a semicolon.
CREATE OR REPLACE PACKAGE pkg_demo AS
    PROCEDURE do_it(p_id IN NUMBER);
    FUNCTION calc(p_x IN NUMBER) RETURN NUMBER;
END pkg_demo;
/

CREATE OR REPLACE PACKAGE BODY pkg_demo AS
    PROCEDURE do_it(p_id IN NUMBER) IS
        v_count NUMBER := 0;
    BEGIN
        FOR i IN 1..10 LOOP
            IF MOD(i, 2) = 0 THEN
                v_count := v_count + 1;
            ELSE
                CASE
                    WHEN i = 1 THEN v_count := v_count + 100;
                    ELSE v_count := v_count - 1;
                END CASE;
            END IF;
        END LOOP;
        -- a comment that mentions a terminator: END; must not be confused here
        BEGIN
            NULL; -- nested anonymous block
        END;
        COMMIT;
    END do_it;

    FUNCTION calc(p_x IN NUMBER) RETURN NUMBER IS
    BEGIN
        RETURN p_x * 2; /* block comment with a semicolon; right here */
    END calc;
END pkg_demo;
/
