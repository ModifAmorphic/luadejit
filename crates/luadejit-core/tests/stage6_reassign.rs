//! Stage 6 integration tests: local reassignment.
//!
//! Stage 6 closes the reassignment gap in the walk-based emitter.
//! Stages 3-5 always emitted `local <name> = <expr>` whenever a store
//! hit a slot the debug info named as a live local. That produced
//! valid-but-wrong Lua for reassignments:
//!
//! ```text
//! local a = 1
//! local a = 2   -- wrong: re-declares `a` instead of reassigning
//! ```
//!
//! Stage 6 splits that path: when the store's instruction index
//! equals the variable's `scope_begin` it's a declaration (`local a =
//! ...`); when the slot is in scope but past the declaration point,
//! it's a reassignment (`a = ...`, no `local`).
//!
//! CI runs without luajit installed, so the `.bc` files are committed
//! alongside the `source.lua` files rather than generated at test
//! time.

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
fn decompiles_simple_reassignment() {
    // `local a = 1; a = 2; return a`:
    //   KSHORT 0 1; KSHORT 0 2; RET1 0 2.
    // Both KSHORT write to slot 0, but only the first is the
    // declaration (scope_begin 0). The second is a reassignment and
    // must omit `local`.
    assert_eq!(
        decompile_fixture("reassign_simple"),
        "local a = 1\na = 2\nreturn a"
    );
}

#[test]
fn decompiles_reassignment_with_arithmetic() {
    // `local a = 1; a = a + 1; return a`:
    //   KSHORT 0 1; ADDVN 0 0 0; RET1 0 2.
    // ADDVN reads slot 0 (the prior KSHORT's "a") and writes back to
    // slot 0 — the same instruction both reads and writes the
    // variable. is_var_declaration_at(0, 1) is false (scope_begin 0
    // != 1), so the walk emits `a = a + 1`.
    assert_eq!(
        decompile_fixture("reassign_arith"),
        "local a = 1\na = a + 1\nreturn a"
    );
}

#[test]
fn decompiles_reassignment_via_mov() {
    // `local a = 1; local b = 2; a = b; return a`:
    //   KSHORT 0 1; KSHORT 1 2; MOV 0 1; RET1 0 2.
    // MOV copies slot 1 (b) into slot 0 (a). The MOV's target slot 0
    // is in scope (named "a") but past its scope_begin, so the walk
    // emits `a = b` without `local`.
    assert_eq!(
        decompile_fixture("reassign_mov"),
        "local a = 1\nlocal b = 2\na = b\nreturn a"
    );
}

#[test]
fn decompiles_reassignment_with_string_const() {
    // `local a = "foo"; a = "bar"; return a`:
    //   KSTR 0 0; KSTR 0 1; RET1 0 2.
    // KSTR is AD format using the reverse GC index — operand 0 ->
    // gc_consts[1] = "foo", operand 1 -> gc_consts[0] = "bar". The
    // second KSTR is a reassignment of slot 0.
    assert_eq!(
        decompile_fixture("reassign_str"),
        "local a = \"foo\"\na = \"bar\"\nreturn a"
    );
}
