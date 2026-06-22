//! Stage 15 integration tests: function literals (FNEW), length
//! operator (LEN), and global assignment (GSET).
//!
//! Stage 15 adds three new opcode handlers on top of Stage 14's table
//! support:
//!
//! * **FNEW** — `local f = function(...) ... end`. The handler
//!   resolves the child proto via the module's children-first
//!   post-order `protos` array, recursively recovers its body, and
//!   wraps the result in `Expr::Function`. Fixtures:
//!   - `func_simple` — zero params, returns a constant.
//!   - `func_with_param` — one param, returns it.
//!   - `func_two_params` — two params, arithmetic in the body.
//!
//! * **LEN** — `local x = #t`. Lowered from `LEN A D` where
//!   `R[A] = #R[D]`.
//!
//! * **GSET** — `x = 42`. Lowered from `GSET A D` where
//!   `globals[KSTR[reverse(D)]] = R[A]`. Reuses `Stmt::Assign` since
//!   a global write is just `name = value` at the Lua source level.
//!
//! # What's NOT covered (deferred to later stages)
//!
//! * Upvalues (`UGET`/`USETx`/`UCLO`) — bail with NotImplemented
//!   when `numuv > 0`.
//! * Tail calls (`CALLT`/`CALLMT`).
//! * Nested function children (a child proto with its own children) —
//!   the simple-case child-proto resolution formula doesn't apply.
//! * Multres (Stage 16, reordered).
//!
//! As with Stage 7-14 fixtures, CI runs without luajit installed, so
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
fn decompiles_func_simple() {
    // `local f = function() return 1 end`:
    //   -- child proto:
    //   0001 KSHORT 0 1
    //   0002 RET1   0 2
    //   -- main proto:
    //   0001 FNEW   0 0    ; create function → slot 0
    //   0002 RET0   0 1.
    assert_eq!(
        decompile_fixture("func_simple"),
        "local f = function()\n    return 1\nend"
    );
}

#[test]
fn decompiles_func_with_param() {
    // `local f = function(x) return x end`:
    //   -- child proto (numparams=1):
    //   0001 RET1   0 2
    //   -- main proto:
    //   0001 FNEW   0 0
    //   0002 RET0   0 1.
    // Parameter `x` at slot 0 is pre-populated so RET1 resolves to
    // `return x`.
    assert_eq!(
        decompile_fixture("func_with_param"),
        "local f = function(x)\n    return x\nend"
    );
}

#[test]
fn decompiles_func_two_params() {
    // `local f = function(x, y) return x + y end`:
    //   -- child proto (numparams=2):
    //   0001 ADDVV  2 0 1    ; x + y → slot 2
    //   0002 RET1   2 2
    //   -- main proto:
    //   0001 FNEW   0 0
    //   0002 RET0   0 1.
    assert_eq!(
        decompile_fixture("func_two_params"),
        "local f = function(x, y)\n    return x + y\nend"
    );
}

#[test]
fn decompiles_len_op() {
    // `local x = #t`:
    //   0001 GGET  0 0    ; load global "t" → slot 0
    //   0002 LEN   0 0    ; R[0] = #R[0]
    //   0003 RET0  0 1.
    assert_eq!(decompile_fixture("len_op"), "local x = #t");
}

#[test]
fn decompiles_gset_simple() {
    // `x = 42`:
    //   0001 KSHORT 0 42   ; 42 → slot 0
    //   0002 GSET   0 0    ; globals["x"] = R[0]
    //   0003 RET0   0 1.
    assert_eq!(decompile_fixture("gset_simple"), "x = 42");
}
