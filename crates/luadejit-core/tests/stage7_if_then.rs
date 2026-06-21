//! Stage 7 integration tests: decompile `if/then` chunks.
//!
//! Stage 7 connects the CFG infrastructure (Stages 7a/7b) to the
//! emitter: every decompilation now flows through the AST pipeline
//! (`parse → CFG → structure::recover → emit`). These fixtures pin
//! the first three shapes the new pipeline handles beyond Stages 1-6:
//!
//! * `if_then_return` — `if x then return 1 end`. The then-body
//!   returns; the merge is just an implicit `RET0`. Exercises ISF
//!   condition interpretation (the user's `if x` is the truthiness
//!   of the ISF's tested slot).
//! * `if_then_fallthrough` — `local y = 0; if x then y = 1 end;
//!   return y`. The then-body falls through into a merge block that
//!   has two predecessors (the entry's JMP and the then-body's
//!   fallthrough). Exercises the recovery's merge-stop logic.
//! * `if_not_x` — `if not x then return 1 end`. Uses IST instead of
//!   ISF; the user's condition is the negation of IST's test, so the
//!   AST surfaces `Expr::Not`.
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
fn decompiles_if_then_return() {
    // `if x then return 1 end`:
    //   GGET 0 0; ISF 0; JMP => 0006; KSHORT 0 1; RET1 0 2; RET0 0 1.
    assert_eq!(
        decompile_fixture("if_then_return"),
        "if x then\n    return 1\nend"
    );
}

#[test]
fn decompiles_if_then_with_fallthrough_merge() {
    // `local y = 0; if x then y = 1 end; return y`:
    //   KSHORT 0 0; GGET 1 0; ISF 1; JMP => 0006; KSHORT 0 1; RET1 0 2.
    assert_eq!(
        decompile_fixture("if_then_fallthrough"),
        "local y = 0\nif x then\n    y = 1\nend\nreturn y"
    );
}

#[test]
fn decompiles_if_not_x() {
    // `if not x then return 1 end`:
    //   GGET 0 0; IST 0; JMP => 0006; KSHORT 0 1; RET1 0 2; RET0 0 1.
    assert_eq!(
        decompile_fixture("if_not_x"),
        "if not x then\n    return 1\nend"
    );
}
