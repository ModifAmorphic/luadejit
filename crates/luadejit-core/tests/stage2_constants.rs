//! Stage 2 integration tests: decompile `return <const>` chunks.
//!
//! Each fixture is a tiny Lua chunk compiled by luajit (`-bg -t raw`)
//! from a `source.lua` whose entire body is a single `return <const>`
//! statement. The decompiler is expected to round-trip each chunk:
//! `return 5` -> `"return 5"`, `return "foo"` -> `"return \"foo\""`,
//! and so on.
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
fn decompiles_return_int() {
    assert_eq!(decompile_fixture("return_int"), "return 5");
}

#[test]
fn decompiles_return_negative_int() {
    assert_eq!(decompile_fixture("return_neg_int"), "return -7");
}

#[test]
fn decompiles_return_float() {
    assert_eq!(decompile_fixture("return_float"), "return 3.14");
}

#[test]
fn decompiles_return_str() {
    // Expected source is literally: return "foo"
    assert_eq!(decompile_fixture("return_str"), "return \"foo\"");
}

#[test]
fn decompiles_return_true() {
    assert_eq!(decompile_fixture("return_true"), "return true");
}

#[test]
fn decompiles_return_false() {
    assert_eq!(decompile_fixture("return_false"), "return false");
}

#[test]
fn decompiles_return_nil() {
    assert_eq!(decompile_fixture("return_nil"), "return nil");
}
