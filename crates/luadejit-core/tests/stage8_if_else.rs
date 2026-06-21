//! Stage 8 integration tests: decompile `if/else` chunks.
//!
//! Stage 8 extends the structural recovery to handle the two-branch
//! `if/else` shape. LuaJIT's codegen always emits a "skip-else" JMP
//! between the then-body and the else-body; the recovery detects
//! this JMP (either live at the then-body's tail, or dead after a
//! returning then-body) and processes both branches. These fixtures
//! pin the two shapes:
//!
//! * `if_else_return` — `if x then return 1 else return 2 end`. Both
//!   branches return. The then-body's RET1 makes the "skip-else"
//!   JMP dead (unreachable), but its target identifies the merge.
//! * `if_else_fallthrough` — `local y = 0; if x then y = 1 else
//!   y = 2 end; return y`. Both branches fall through to a merge
//!   that has two predecessors. The then-body's terminator is the
//!   LIVE "skip-else" JMP. Exercises slot tracking across branches:
//!   both branches reassign `y`, so the merge's `RET1 0 2` reads
//!   `Var("y")` and emits `return y`.
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
fn decompiles_if_else_both_branches_return() {
    // `if x then return 1 else return 2 end`:
    //   GGET 0 0; ISF 0; JMP => 0007;
    //   KSHORT 0 1; RET1;          (then-body, returns)
    //   JMP => 0009;               (dead "skip-else" JMP)
    //   KSHORT 0 2; RET1;          (else-body, returns)
    //   RET0 0 1.                  (merge: implicit return)
    assert_eq!(
        decompile_fixture("if_else_return"),
        "if x then\n    return 1\nelse\n    return 2\nend"
    );
}

#[test]
fn decompiles_if_else_both_branches_fall_through() {
    // `local y = 0; if x then y = 1 else y = 2 end; return y`:
    //   KSHORT 0 0; GGET 1 0; ISF 1; JMP => 0007;
    //   KSHORT 0 1; JMP => 0008;       (then-body + live "skip-else")
    //   KSHORT 0 2;                    (else-body)
    //   RET1 0 2.                      (merge: return y)
    assert_eq!(
        decompile_fixture("if_else_fallthrough"),
        "local y = 0\nif x then\n    y = 1\nelse\n    y = 2\nend\nreturn y"
    );
}
