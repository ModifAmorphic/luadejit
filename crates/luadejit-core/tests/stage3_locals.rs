//! Stage 3 integration tests: decompile `local x = <const>` chunks.
//!
//! Each fixture is a tiny Lua chunk compiled by luajit (`-bg -t raw`)
//! whose body declares a single local and (optionally) returns it.
//! The decompiler must consult the debug section's `var_info` to
//! distinguish these from the Stage 2 `return <const>` chunks: the
//! real bytecode instructions are identical, so the *only* signal is
//! the named-local record covering slot 0 at the load instruction.
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
fn decompiles_local_int() {
    assert_eq!(decompile_fixture("local_int"), "local x = 5\nreturn x");
}

#[test]
fn decompiles_local_negative_int() {
    assert_eq!(decompile_fixture("local_neg_int"), "local x = -7\nreturn x");
}

#[test]
fn decompiles_local_float() {
    assert_eq!(decompile_fixture("local_float"), "local x = 3.14\nreturn x");
}

#[test]
fn decompiles_local_str() {
    // Expected source is literally: local x = "foo"\nreturn x
    assert_eq!(
        decompile_fixture("local_str"),
        "local x = \"foo\"\nreturn x"
    );
}

#[test]
fn decompiles_local_true() {
    assert_eq!(decompile_fixture("local_true"), "local x = true\nreturn x");
}

#[test]
fn decompiles_local_false() {
    assert_eq!(
        decompile_fixture("local_false"),
        "local x = false\nreturn x"
    );
}

#[test]
fn decompiles_local_nil() {
    assert_eq!(decompile_fixture("local_nil"), "local x = nil\nreturn x");
}

#[test]
fn decompiles_local_no_return() {
    // `local x = 5` with no explicit return — the implicit RET0 at
    // end of chunk. The emitted source has no `return` statement.
    assert_eq!(decompile_fixture("local_no_return"), "local x = 5");
}
