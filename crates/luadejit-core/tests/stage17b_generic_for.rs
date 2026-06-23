//! Stage 17 part 2 integration tests: decompile generic-for loops.
//!
//! Stage 17 part 2 adds `for k[, v[, ...]] in iterator do body end`
//! support — the last unhandled loop type. LuaJIT lowers the
//! generic-for to an ISNEXT/ITERN/ITERL sequence surrounding the body,
//! with the iterator setup (typically `pairs(t)` / `ipairs(t)`) emitted
//! as a CALL producing 3 results (iterator function, state, control) at
//! the slots immediately preceding ISNEXT's base.
//!
//! The two fixtures pin the supported shapes:
//!
//! * `for_in_basic` — `for k, v in pairs(t) do print(k, v) end`. Two
//!   loop variables; exercises the full ISNEXT/ITERN/ITERL sequence
//!   with a 2-variable body.
//! * `for_in_single` — `for k in pairs(t) do print(k) end`. Single
//!   loop variable. The spec text suggested `for i in ipairs(t)`, but
//!   that source lowers to JMP/ITERC/ITERL (the call-iterator
//!   variant), which the spec explicitly defers ("Do NOT handle
//!   ITERC"). The equivalent single-variable case via `pairs` lowers
//!   to ISNEXT/ITERN/ITERL and is used here instead — same shape the
//!   stage targets, just a different library function.
//!
//! As with prior stages' fixtures, CI runs without luajit installed,
//! so the `.bc` files are committed alongside the `source.lua` files.

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
fn decompiles_for_in_basic() {
    // `for k, v in pairs(t) do print(k, v) end`:
    //   GGET 0 0      ; "pairs"
    //   GGET 2 1      ; "t"
    //   CALL 0 4 2    ; pairs(t) → 3 results (iter/state/ctl) at slots 0..2
    //   ISNEXT 3 => 9 ; A=3 (k at 3, v at 4); exit target = idx 9
    //   GGET 5 2      ; "print"
    //   MOV 7 3       ; copy k
    //   MOV 8 4       ; copy v
    //   CALL 5 1 3    ; print(k, v)
    //   ITERN 3 3 3   ; advance iterator
    //   ITERL 3 => 5  ; latch: jump back to body if more
    //   RET0.
    assert_eq!(
        decompile_fixture("for_in_basic"),
        "for k, v in pairs(t) do\n    print(k, v)\nend"
    );
}

#[test]
fn decompiles_for_in_single() {
    // `for k in pairs(t) do print(k) end`:
    //   GGET 0 0      ; "pairs"
    //   GGET 2 1      ; "t"
    //   CALL 0 4 2    ; pairs(t) → 3 results at slots 0..2
    //   ISNEXT 3 => 8 ; A=3 (k at 3); exit target = idx 8
    //   GGET 4 2      ; "print"
    //   MOV 6 3       ; copy k
    //   CALL 4 1 2    ; print(k)
    //   ITERN 3 2 3   ; advance iterator (B=2: 1 loop variable + 1)
    //   ITERL 3 => 5  ; latch
    //   RET0.
    //
    // Note: the spec text suggested `for i in ipairs(t)`, but that
    // lowers to ITERC rather than ITERN.ITERC is out of scope per
    // the spec ("Do NOT handle ITERC"); `for k in pairs(t)` produces
    // the equivalent ISNEXT/ITERN/ITERL shape this stage targets.
    assert_eq!(
        decompile_fixture("for_in_single"),
        "for k in pairs(t) do\n    print(k)\nend"
    );
}
