-- Every alternative-quote delimiter form.
DECLARE
    a VARCHAR2(50) := q'[bracket ] form]';
    b VARCHAR2(50) := q'{brace } form}';
    c VARCHAR2(50) := q'<angle > form>';
    d VARCHAR2(50) := q'(paren ) form)';
    e VARCHAR2(50) := q'!bang ! form!';
    f VARCHAR2(50) := nq'[national bracket]';
    g VARCHAR2(50) := Q'#hash # form#';
BEGIN
    NULL;
END;
/
