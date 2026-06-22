//! Stage 12 integration tests: decompile function calls.
//!
//! Stage 12 adds regular (non-tail, non-multres, non-method) function
//! calls. The five fixtures pin the supported shapes:
//!
//! * `call_no_result` — `print("hello")`. A bare call whose result
//!   is discarded (`CALL A B C` with `B == 1`).
//! * `call_with_result` — `local x = tostring(42)`. A call with a
//!   single result stored in a named local (`B == 2`). The local
//!   declaration surfaces via `assign_slot`'s debug-info check.
//! * `call_multi_arg` — `print("a", "b", "c")`. A bare call with
//!   three arguments (`C == 4`).
//! * `call_no_arg` — `f()`. A bare call with zero arguments
//!   (`C == 1`).
//! * `call_in_return` — `return f(1)`. A call whose single result
//!   is stored in an unnamed temp (slot A) and then returned by
//!   `RET1`. No Stmt is emitted for the call itself; the call
//!   surfaces as `Expr::Call` inside the `Return`.
//!
//! # CALL operand convention (verified empirically via hex dump)
//!
//! `CALL A B C` (ABC format):
//! - **A** = base slot. Function is at slot A.
//! - **B-1** = number of return values expected (B == 0 means
//!   multres — deferred to Stage 15).
//! - **C-1** = number of arguments (C == 0 means multres call from
//!   a VARG/previous CALL — Stage 15).
//! - **Arguments** are at slots A+2 through A+C. Slot A+1 is unused
//!   for regular calls (gap).
//! - **Results** overwrite slots starting at A.
//!
//! # What's NOT covered (deferred to later stages)
//!
//! * `CALLT`/`CALLMT` (tail calls) are CFG terminators and bail
//!   with `NotImplemented` from the recovery walk. The
//!   `call_in_return` fixture uses `return (f(1))` (parens) rather
//!   than `return f(1)` because the latter lowers to a tail call
//!   in LuaJIT's codegen — the parens force a regular CALL+RET1.
//! * `CALLM` (multres call) and multiple return values (`B > 2`)
//!   are Stage 15.
//! * Method calls (`obj:method()`) are Stage 13.
//!
//! As with Stage 7-11 fixtures, CI runs without luajit installed, so
//! the `.bc` files are committed alongside the `source.lua` files.

use std::fs;

/// Read a fixture's bytecode and decompile it, panicking with the
/// fixture path on any I/O error.
fn decompile_fixture(dir: &str) -> String {
    let bc_path = format!("tests/fixtures/{}/input.bc", dir);
    let bytes =
        fs::read(&bc_path).unwrap_or_else(|e| panic!("failed to read fixture {}: {}", bc_path, e));
    luadejit_core::decompile(&bytes)
        .unwrap_or_else(|e| panic!("decompile of {} failed: {:?}", bc_path, e))
}

#[test]
fn decompiles_call_no_result() {
    // `print("hello")`:
    //   GGET 0 0      ; "print" at slot 0
    //   KSTR 2 1      ; "hello" at slot 2 (gap at slot 1)
    //   CALL 0 1 2    ; A=0, B=1 (0 results), C=2 (1 arg at slot 2)
    //   RET0 0 1.
    assert_eq!(decompile_fixture("call_no_result"), "print(\"hello\")");
}

#[test]
fn decompiles_call_with_result_in_local() {
    // `local x = tostring(42)` (no explicit return):
    //   GGET   0 0     ; "tostring" at slot 0
    //   KSHORT 2 42    ; 42 at slot 2 (gap at slot 1)
    //   CALL   0 2 2   ; A=0, B=2 (1 result at slot 0), C=2 (1 arg)
    //   RET0   0 1.
    // The result overwrites slot 0; var_info names slot 0 as `x`
    // with scope_begin = the CALL's real-idx, so the recovery
    // recognizes the slot as a LocalDecl and emits
    // `local x = tostring(42)`.
    assert_eq!(
        decompile_fixture("call_with_result"),
        "local x = tostring(42)"
    );
}

#[test]
fn decompiles_call_multiple_args() {
    // `print("a", "b", "c")`:
    //   GGET 0 0      ; "print"
    //   KSTR 2 1; KSTR 3 2; KSTR 4 3   ; 3 string args at slots 2,3,4
    //   CALL 0 1 4    ; A=0, B=1 (0 results), C=4 (3 args)
    //   RET0 0 1.
    assert_eq!(
        decompile_fixture("call_multi_arg"),
        "print(\"a\", \"b\", \"c\")"
    );
}

#[test]
fn decompiles_call_no_args() {
    // `f()`:
    //   GGET 0 0      ; "f"
    //   CALL 0 1 1    ; A=0, B=1 (0 results), C=1 (0 args)
    //   RET0 0 1.
    // The arg-slot loop `(A+2)..=(A+C)` is empty when C == 1, so
    // args is `vec![]` and emit produces `f()`.
    assert_eq!(decompile_fixture("call_no_arg"), "f()");
}

#[test]
fn decompiles_call_in_return() {
    // `return (f(1))` — the parens force a regular CALL rather than
    // a tail call (LuaJIT lowers bare `return f(1)` to CALLT, which
    // is deferred). The decompiler doesn't preserve the redundant
    // parens, so the round-trip is the canonical `return f(1)`:
    //   GGET   0 0     ; "f" at slot 0
    //   KSHORT 2 1     ; arg at slot 2
    //   CALL   0 2 2   ; A=0, B=2 (1 result at slot 0), C=2 (1 arg)
    //   RET1   0 2.    ; return slot 0
    // Slot 0 has no var_info name, so the CALL stores Expr::Call as
    // an unnamed temp; RET1 reads slot 0 and emits `return f(1)`.
    assert_eq!(decompile_fixture("call_in_return"), "return f(1)");
}
