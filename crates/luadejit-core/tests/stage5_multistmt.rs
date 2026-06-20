//! Stage 5 integration tests: decompile multi-statement sequences.
//!
//! Stage 4's walk-based emitter already handled most multi-statement
//! chunks (three declarations, returning the first slot, mixing types,
//! chained arithmetic). Stage 5 closes two gaps the research session
//! identified:
//!
//! * Arithmetic instructions (`ADDVN`/`ADDVV`/...) whose destination
//!   slot has a named local now emit `local <name> = <expr>` —
//!   previously all arithmetic results were treated as unnamed
//!   temporaries. Covered by `multi_arith_local` and
//!   `multi_chain_arith`.
//! * `MOV A D` copies a slot's expression into another slot, with the
//!   same named-local handling (`local b = a`). Covered by `multi_mov`.
//!
//! The remaining fixtures pin the previously-working multi-statement
//! shapes so future refactors can't regress them silently.
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
fn decompiles_arith_to_named_local() {
    // `local a = 1; local b = a + 2; return b`:
    //   KSHORT 0 1; ADDVN 1 0 0; RET1 1 2.
    // The ADDVN result lands in slot 1, which var_info names "b".
    assert_eq!(
        decompile_fixture("multi_arith_local"),
        "local a = 1\nlocal b = a + 2\nreturn b"
    );
}

#[test]
fn decompiles_mov_to_named_local() {
    // `local a = 1; local b = a; return b`:
    //   KSHORT 0 1; MOV 1 0; RET1 1 2.
    assert_eq!(
        decompile_fixture("multi_mov"),
        "local a = 1\nlocal b = a\nreturn b"
    );
}

#[test]
fn decompiles_three_declarations() {
    // Three locals declared in sequence, returning the last. Stage 4's
    // walk already handled this; pinned here as a regression fixture.
    assert_eq!(
        decompile_fixture("multi_three_decls"),
        "local x = 1\nlocal y = 2\nlocal z = 3\nreturn z"
    );
}

#[test]
fn decompiles_return_first_of_two_declarations() {
    // `local x = 1; local y = 2; return x`: the walk must emit both
    // declarations and surface the FIRST slot's expression at RET1,
    // not the last.
    assert_eq!(
        decompile_fixture("multi_return_first"),
        "local x = 1\nlocal y = 2\nreturn x"
    );
}

#[test]
fn decompiles_mixed_type_declarations() {
    // `local x = 5; local y = "foo"; return y`: one int local, one
    // string local. The KSTR → KSHORT sequence mixes const-load arms.
    assert_eq!(
        decompile_fixture("multi_mixed_types"),
        "local x = 5\nlocal y = \"foo\"\nreturn y"
    );
}

#[test]
fn decompiles_chained_arithmetic_with_named_result() {
    // `local a = 1; local b = 2; local c = a + b; return c`:
    //   KSHORT 0 1; KSHORT 1 2; ADDVV 2 0 1; RET1 2 2.
    // Slot 2 is named "c" — the named-local arithmetic path emits
    // `local c = a + b` and RET1 surfaces the name.
    assert_eq!(
        decompile_fixture("multi_chain_arith"),
        "local a = 1\nlocal b = 2\nlocal c = a + b\nreturn c"
    );
}

#[test]
fn decompiles_two_declarations_with_no_return() {
    // `local x = 1; local y = 2` with implicit return: the chunk ends
    // in RET0, so no `return` statement is emitted.
    assert_eq!(
        decompile_fixture("multi_no_return"),
        "local x = 1\nlocal y = 2"
    );
}
