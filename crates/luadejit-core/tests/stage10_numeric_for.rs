//! Stage 10 integration tests: decompile numeric-for loops.
//!
//! Stage 10 adds `for var = start, stop[, step] do body end` support.
//! LuaJIT lowers the numeric-for to a FORI/FORL pair surrounding the
//! body, with start/stop/step loaded into slots A, A+1, A+2 and the
//! visible loop variable occupying slot A+3. The four fixtures pin
//! the supported shapes:
//!
//! * `for_simple_body` — `for i = 1, 10 do local x = i end`. Default
//!   step of 1 is omitted from output. Body uses MOV (the load of `i`
//!   into `x`).
//! * `for_with_step` — `for i = 1, 10, 2 do local x = i end`. Non-1
//!   step is preserved in the output.
//! * `for_return` — `for i = 1, 3 do return i end`. Body returns the
//!   loop variable directly (RET1 reads slot A+3). The loop latch
//!   (FORL) ends up in its own dead block unreachable after the
//!   RET1; the recovery skips it via the loop-exit edge.
//! * `for_accumulator` — `local sum = 0; for i = 1, 10 do
//!   sum = sum + i end; return sum`. Exercises a pre-loop local, an
//!   Assign (not LocalDecl) inside the body, and a post-loop return.
//!
//! As with Stage 7-9 fixtures, CI runs without luajit installed, so
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
fn decompiles_for_simple_body() {
    // `for i = 1, 10 do local x = i end`:
    //   KSHORT 0 1; KSHORT 1 10; KSHORT 2 1;
    //   FORI 0 => 0007;
    //   MOV 4 3;          (x = i)
    //   FORL 0 => 0005;
    //   RET0.
    // Step (slot 2) is KSHORT 2 1 → Expr::Int(1) → omitted.
    assert_eq!(
        decompile_fixture("for_simple_body"),
        "for i = 1, 10 do\n    local x = i\nend"
    );
}

#[test]
fn decompiles_for_with_step() {
    // `for i = 1, 10, 2 do local x = i end`:
    //   KSHORT 0 1; KSHORT 1 10; KSHORT 2 2;
    //   FORI 0 => 0007; MOV 4 3; FORL 0 => 0005; RET0.
    // Step (slot 2) is KSHORT 2 2 → Expr::Int(2) → preserved.
    assert_eq!(
        decompile_fixture("for_with_step"),
        "for i = 1, 10, 2 do\n    local x = i\nend"
    );
}

#[test]
fn decompiles_for_return() {
    // `for i = 1, 3 do return i end`:
    //   KSHORT 0 1; KSHORT 1 3; KSHORT 2 1;
    //   FORI 0 => 0007;
    //   RET1 3 2;        (return i — slot A+3=3)
    //   FORL 0 => 0005;  (dead block: unreachable after RET1)
    //   RET0.
    // The recovery uses LoopInit.exit to skip the dead FORL block
    // and continue at the RET0.
    assert_eq!(
        decompile_fixture("for_return"),
        "for i = 1, 3 do\n    return i\nend"
    );
}

#[test]
fn decompiles_for_accumulator() {
    // `local sum = 0; for i = 1, 10 do sum = sum + i end; return sum`:
    //   KSHORT 0 0;       (sum = 0)
    //   KSHORT 1 1; KSHORT 2 10; KSHORT 3 1;
    //   FORI 1 => 0008;   (base = slot 1; index at slot 4)
    //   ADDVV 0 0 4;      (sum = sum + i)
    //   FORL 1 => 0006;
    //   RET1 0 2.         (return sum)
    // Exercises: pre-loop local decl, body Assign (not LocalDecl),
    // and post-loop return — all threading the slot map correctly.
    assert_eq!(
        decompile_fixture("for_accumulator"),
        "local sum = 0\nfor i = 1, 10 do\n    sum = sum + i\nend\nreturn sum"
    );
}
