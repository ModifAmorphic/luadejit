//! Stage 9 integration tests: decompile compound `if` conditions.
//!
//! Stage 9 extends the structural recovery to handle every ISxx
//! comparison op (instead of just ISF/IST) and short-circuiting
//! `and`/`or` condition chains. The five fixtures pin the supported
//! shapes:
//!
//! * `cond_equal` — `if a == b then return 1 end`. LuaJIT lowers
//!   `==` to `ISNEV` (test `!=`; JMP when the user's `==` is false).
//! * `cond_less_than` — `if a < b then return 1 end`. Lowers to
//!   `ISGE` (test `>=`).
//! * `cond_not_equal` — `if a ~= b then return 1 end`. Lowers to
//!   `ISEQV` (test `==`).
//! * `cond_and` — `if a and b then return 1 end`. Two `ISF+JMP`
//!   pairs that both skip to the merge when their condition fails.
//! * `cond_or` — `if a or b then return 1 end`. An `IST+JMP` pair
//!   that short-circuits to the then-body, followed by an `ISF+JMP`
//!   pair that skips to the merge.
//!
//! As with the Stage 7-8 fixtures, CI runs without luajit installed,
//! so the `.bc` files are committed alongside the `source.lua` files
//! rather than generated at test time.

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
fn decompiles_equality_condition() {
    // `if a == b then return 1 end`:
    //   GGET 0 0; GGET 1 1; ISNEV 0 1; JMP => 0007;
    //   KSHORT 0 1; RET1; RET0.
    // ISNEV tests R[0] != R[1]; JMP fires when `a == b` is false.
    // The user's condition is the complement: Equal.
    assert_eq!(
        decompile_fixture("cond_equal"),
        "if a == b then\n    return 1\nend"
    );
}

#[test]
fn decompiles_less_than_condition() {
    // `if a < b then return 1 end`:
    //   GGET 0 0; GGET 1 1; ISGE 0 1; JMP => 0007;
    //   KSHORT 0 1; RET1; RET0.
    // ISGE tests R[0] >= R[1]; JMP fires when `a < b` is false.
    // The user's condition is the complement: LessThan.
    assert_eq!(
        decompile_fixture("cond_less_than"),
        "if a < b then\n    return 1\nend"
    );
}

#[test]
fn decompiles_not_equal_condition() {
    // `if a ~= b then return 1 end`:
    //   GGET 0 0; GGET 1 1; ISEQV 0 1; JMP => 0007;
    //   KSHORT 0 1; RET1; RET0.
    // ISEQV tests R[0] == R[1]; JMP fires when `a ~= b` is false.
    // The user's condition is the complement: NotEqual.
    assert_eq!(
        decompile_fixture("cond_not_equal"),
        "if a ~= b then\n    return 1\nend"
    );
}

#[test]
fn decompiles_and_chain_condition() {
    // `if a and b then return 1 end`:
    //   GGET 0 0; ISF 0; JMP => 0009;     (CB1, true_edge → merge)
    //   GGET 0 1; ISF 0; JMP => 0009;     (CB2, true_edge → merge)
    //   KSHORT 0 1; RET1; RET0.
    // Both CBs share a true_edge (the merge); the AND signature.
    // Each ISF's complement is just the value (truthiness).
    assert_eq!(
        decompile_fixture("cond_and"),
        "if a and b then\n    return 1\nend"
    );
}

#[test]
fn decompiles_or_chain_condition() {
    // `if a or b then return 1 end`:
    //   GGET 0 0; IST 0; JMP => 0007;     (CB1, true_edge → then-body)
    //   GGET 0 1; ISF 0; JMP => 0009;     (CB2, true_edge → merge)
    //   KSHORT 0 1; RET1; RET0.
    // CB1 short-circuits via true_edge to the then-body (= CB2's
    // false_edge); CB2 skips to the merge. The OR signature: the
    // first CB contributes its test condition verbatim (`a`),
    // the last contributes its complement (`b`).
    assert_eq!(
        decompile_fixture("cond_or"),
        "if a or b then\n    return 1\nend"
    );
}
