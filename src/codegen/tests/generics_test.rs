use crate::test_utils::tests::codegen;

#[test]
fn generic_function_has_no_declaration() {
    let prg = codegen(
        r"
        FUNCTION MAX<T : ANY_NUM> : T VAR_INPUT in1, in2 : T END_VAR END_FUNCTION
        ",
    );

    insta::assert_snapshot!(prg);
}

#[test]
fn generic_function_call_generates_real_type_call() {
    let prg = codegen(
        r"
        @EXTERNAL FUNCTION MAX<T : ANY_NUM> : T VAR_INPUT in1, in2 : T END_VAR END_FUNCTION
        FUNCTION MAX__DINT : DINT VAR_INPUT in1, in2 : DINT END_VAR END_FUNCTION

        PROGRAM prg 
        VAR
            a, b : INT;
        END_VAR

        MAX(1,2);
        MAX(a,b);
        END_PROGRAM
        ",
    );

    insta::assert_snapshot!(prg);
}

#[test]
fn generic_output_parameter() {
    // GIVEN ... (see comments in st-code)
    let src = r"
        // ... a generic function FOO with a T, defined by a VAR_OUT 
        // parameter (which will be interally treated as a pointer)
            FUNCTION foo <T: ANY_INT> : T
                VAR_INPUT   in1 : DATE; END_VAR
                VAR_OUTPUT  out1: T;    END_VAR
            END_FUNCTION

        // ...AND an implementatino for INT
            FUNCTION foo__INT : INT
                VAR_INPUT   in1 : DATE; END_VAR
                VAR_OUTPUT  out1: INT;  END_VAR
            END_FUNCTION

        // ...AND an implementatino for BYTE
            FUNCTION foo__BYTE : BYTE
                VAR_INPUT   in1 : DATE; END_VAR
                VAR_OUTPUT  out1: BYTE; END_VAR
            END_FUNCTION

        // ... AND a program calling foo with an INT-parameter
            PROGRAM prg
            VAR 
                theInt, iResult : INT; 
                theByte, bResult : BYTE; 
                data : DATE;
            END_VAR

            iResult := foo(data, theInt);
            bResult := foo(data, theByte);
            END_VAR
        ";

    // THEN we expect a first call to foo__INT with out1 passed as a pointer
    // AND we expect a second call to foo__BYTE with out1 passed as a pointer
    insta::assert_snapshot!(codegen(src));
}