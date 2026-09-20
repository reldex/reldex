CREATE OR REPLACE TRIGGER trg_demo
BEFORE INSERT OR UPDATE ON demo_table
REFERENCING NEW AS NEW OLD AS OLD
FOR EACH ROW
DECLARE
    v_msg VARCHAR2(100);
BEGIN
    IF :NEW.amount < 0 THEN
        v_msg := q'[negative amount: ]' || TO_CHAR(:NEW.amount);
        RAISE_APPLICATION_ERROR(-20001, v_msg);
    END IF;
END;
/
