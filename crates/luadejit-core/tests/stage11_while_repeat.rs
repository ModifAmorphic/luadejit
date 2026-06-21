//! Stage 11 integration tests: decompile `while` and `repeat` loops.
//!
//! Stage 11 adds `while cond do body end` and `repeat body until cond`
//! support. Both shapes are detected at the [`ConditionalBranch`] arm
//! of the recovery walk:
//!
//! * `while` — the false_edge block (the body) ends in a
//!   [`Jump`](crate::cfg::Terminator::Jump) whose target is the
//!   current block (the loop header). The ISxx tests the EXIT
//!   condition; the user's `while`-condition is its complement.
//! * `repeat` — the [`ConditionalBranch`]'s true_edge or false_edge
//!   points back to the current block (self-loop). The ISxx tests the
//!   CONTINUE condition; the user's `until`-condition is its complement.
//!
//! The three fixtures pin the supported shapes:
//!
//! * `while_basic` — `local i = 0; while i < 10 do i = i + 1 end;
//!   return i`. Pre-loop local decl, while with body reassignment,
//!   post-loop return.
//! * `while_simple` — `local i = 0; while i < 3 do i = i + 1 end`.
//!   No explicit return (implicit RET0 → no Stmt emitted).
//! * `repeat_basic` — `local i = 0; repeat i = i + 1 until i >= 3;
//!   return i`. Note LuaJIT canonicalizes ordered comparisons to
//!   `<`/`<=` by swapping operands (the same limitation as Stage 9's
//!   ordered-comparison conditions), so `until i >= 3` round-trips
//!   as `until 3 <= i` — semantically equivalent but non-canonical.
//!
//! As with Stage 7-10 fixtures, CI runs without luajit installed, so
//! the `.bc` files are committed alongside the `source.lua` files.
//!
//! [`ConditionalBranch`]: crate::cfg::Terminator::ConditionalBranch

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
fn decompiles_while_basic() {
    // `local i = 0; while i < 10 do i = i + 1 end; return i`:
    //   KSHORT 0 0;             (i = 0)
    //   KSHORT 1 10; ISGE 0 1; JMP => 0008;   (loop header)
    //   LOOP 1 => 0008; ADDVN 0 0 0; JMP => 0002;  (body, back-edge)
    //   RET1 0 2.              (exit: return i)
    // ISGE tests R[0] >= R[1] = `i >= 10`; the user's condition is
    // the complement: `i < 10`.
    assert_eq!(
        decompile_fixture("while_basic"),
        "local i = 0\nwhile i < 10 do\n    i = i + 1\nend\nreturn i"
    );
}

#[test]
fn decompiles_while_simple() {
    // `local i = 0; while i < 3 do i = i + 1 end`:
    //   KSHORT 0 0; KSHORT 1 3; ISGE 0 1; JMP => 0008;
    //   LOOP 1 => 0008; ADDVN 0 0 0; JMP => 0002;
    //   RET0 0 1.   (implicit return → no Stmt emitted)
    assert_eq!(
        decompile_fixture("while_simple"),
        "local i = 0\nwhile i < 3 do\n    i = i + 1\nend"
    );
}

#[test]
fn decompiles_repeat_basic() {
    // `local i = 0; repeat i = i + 1 until i >= 3; return i`:
    //   KSHORT 0 0;                  (i = 0)
    //   LOOP 1 => 0007; ADDVN 0 0 0; (body)
    //   KSHORT 1 3; ISGT 1 0; JMP => 0002;  (continue: 3 > i)
    //   RET1 0 2.                    (exit: return i)
    // ISGT A=1 D=0 tests R[1] > R[0] = `3 > i` (continue condition);
    // the user's until-condition is the complement: `3 <= i`. Note
    // LuaJIT canonicalizes ordered comparisons to `<`/`<=`, so the
    // source's `i >= 3` round-trips as the equivalent `3 <= i`.
    assert_eq!(
        decompile_fixture("repeat_basic"),
        "local i = 0\nrepeat\n    i = i + 1\nuntil 3 <= i\nreturn i"
    );
}
