//! Stage 4 integration tests: decompile single binary arithmetic
//! expressions whose result is returned.
//!
//! Each fixture is a tiny Lua chunk compiled by luajit (`-bg -t raw`)
//! whose body declares zero or more locals and then `return`s a
//! single arithmetic expression. Stage 4 handles all three arithmetic
//! operand forms:
//!
//! * `*VN` variants — variable + number constant (`a + 3`).
//! * `*NV` variants — number constant + variable (`3 + a`).
//! * `*VV` variants — variable + variable (`a + b`).
//!
//! plus `POW` (`a ^ b`) and `CAT` (`a .. b`). The decompiler threads
//! expressions through a slot map and surfaces the result at `RET1`;
//! the integration tests verify the exact emitted source.
//!
//! `arith_folded_div` exercises the LuaJIT-compatible number
//! formatter end-to-end: `10 / 3` is constant-folded by the compiler
//! into a single `KNUM` whose value is `3.3333333333333` (LuaJIT's
//! `%.14g` output), and the decompiler must round-trip that exact
//! string. Rust's default `{}` formatting would emit
//! `3.3333333333333335` and fail this test.
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
fn decompiles_addvn() {
    // `local a = 5; return a + 3`: KSHORT 0 5; ADDVN 1 0 0; RET1 1 2.
    assert_eq!(
        decompile_fixture("arith_addvn"),
        "local a = 5\nreturn a + 3"
    );
}

#[test]
fn decompiles_addnv() {
    // `local a = 5; return 3 + a`: KSHORT 0 5; ADDNV 1 0 0; RET1 1 2.
    assert_eq!(
        decompile_fixture("arith_addnv"),
        "local a = 5\nreturn 3 + a"
    );
}

#[test]
fn decompiles_addvv() {
    // `local a = 1; local b = 2; return a + b`:
    //   KSHORT 0 1; KSHORT 1 2; ADDVV 2 0 1; RET1 2 2.
    assert_eq!(
        decompile_fixture("arith_addvv"),
        "local a = 1\nlocal b = 2\nreturn a + b"
    );
}

#[test]
fn decompiles_subvv() {
    // `local a = 1; local b = 2; return a - b`:
    //   KSHORT 0 1; KSHORT 1 2; SUBVV 2 0 1; RET1 2 2.
    assert_eq!(
        decompile_fixture("arith_subvv"),
        "local a = 1\nlocal b = 2\nreturn a - b"
    );
}

#[test]
fn decompiles_divvn() {
    // `local a = 10; return a / 3`: KSHORT 0 10; DIVVN 1 0 0; RET1 1 2.
    assert_eq!(
        decompile_fixture("arith_divvn"),
        "local a = 10\nreturn a / 3"
    );
}

#[test]
fn decompiles_modvn() {
    // `local a = 10; return a % 3`: KSHORT 0 10; MODVN 1 0 0; RET1 1 2.
    assert_eq!(
        decompile_fixture("arith_modvn"),
        "local a = 10\nreturn a % 3"
    );
}

#[test]
fn decompiles_pow() {
    // `local a = 2; return a ^ 10`:
    //   KSHORT 0 2; KSHORT 1 10; POW 1 0 1; RET1 1 2.
    // Note POW loads the constant into a slot first (no VN variant),
    // then takes reg[B], reg[C].
    assert_eq!(decompile_fixture("arith_pow"), "local a = 2\nreturn a ^ 10");
}

#[test]
fn decompiles_cat() {
    // `return "hello" .. " world"`:
    //   KSTR 0 0; KSTR 1 1; CAT 0 0 1; RET1 0 2.
    // CAT writes into slot 0 (the same slot the first KSTR loaded);
    // RET1 then reads it back. The walk must overwrite slot 0's
    // expression with the CAT result.
    assert_eq!(
        decompile_fixture("arith_cat"),
        "return \"hello\" .. \" world\""
    );
}

#[test]
fn decompiles_folded_div_with_lua_number_formatter() {
    // `return 10 / 3` is constant-folded by the compiler to
    // `KNUM 0 0` (value 3.3333333333333) + RET1 — Stage 2's shape,
    // but the value requires the Stage 4 number formatter to
    // round-trip correctly. Rust's `{}` formatting would emit
    // `3.3333333333333335`.
    assert_eq!(
        decompile_fixture("arith_folded_div"),
        "return 3.3333333333333"
    );
}
