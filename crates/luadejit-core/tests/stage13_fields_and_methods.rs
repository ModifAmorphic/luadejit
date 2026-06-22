//! Stage 13 integration tests: decompile field access and method calls.
//!
//! Stage 13 adds three table-read opcodes (`TGETS`/`TGETV`/`TGETB`)
//! and method-call detection on top of Stage 12's CALL handling. The
//! six fixtures pin the supported shapes:
//!
//! * `field_access` — `local x = obj.field`. TGETS with a string
//!   constant key (the common case).
//! * `index_num` — `local x = obj[5]`. TGETB with an inline integer
//!   literal key.
//! * `index_var` — `local x = t[k]`. TGETV with a register key.
//! * `method_call` — `obj:method(1)`. Bare method call with one
//!   explicit arg.
//! * `method_no_arg` — `obj:method()`. Bare method call with zero
//!   explicit args.
//! * `method_with_result` — `local x = obj:method()`. Method call
//!   with one result stored in a named local.
//!
//! # TGET operand conventions (verified via `luajit -bl`)
//!
//! All three are `A B C` (ABC format):
//! - **A** = destination slot.
//! - **B** = table/object register.
//! - **C** = the key, encoded per opcode:
//!   - `TGETS` — reverse-indexed GC string constant.
//!   - `TGETB` — inline integer literal (NOT a register).
//!   - `TGETV` — key register.
//!
//! # Method call detection
//!
//! `obj:method(args...)` compiles to:
//! ```text
//! GGET   obj_slot, "obj"
//! MOV    arg_base, obj_slot     ; self = obj
//! TGETS  A, obj_slot, "method"  ; A = obj.method (a Field expr)
//! [<arg loads>]
//! CALL   A, B, C
//! ```
//! The recovery detects the method-call shape by inspecting the
//! CALL: if slot A holds `Expr::Field { obj, name }` and the self
//! slot (A+2 in FR2, A+1 in FR1) holds `obj`, the CALL is a method
//! call. The implicit self is dropped from the argument list.
//!
//! # FR1/FR2 CALL fix
//!
//! Stage 13 also fixes the CALL handler's argument base for FR1
//! protos. The system-luajit fixtures in the test suite are FR2
//! (gap at A+1); the Darktide corpus is FR1 (no gap, args at A+1).
//! `is_fr2` is now threaded from `module.header.is_fr2()` through
//! `recover` into the CALL handler.
//!
//! # What's NOT covered (deferred to later stages)
//!
//! * `TSETS`/`TSETV`/`TSETB` (table writes — Stage 14).
//! * `TNEW`/`TDUP` (table construction — Stage 14).
//! * `TGETR`/`TSETR` (FFI — rare).
//!
//! As with Stage 7-12 fixtures, CI runs without luajit installed, so
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
fn decompiles_field_access() {
    // `local x = obj.field`:
    //   GGET  0 0      ; "obj" at slot 0
    //   TGETS 0 0 1    ; R[0] = R[0][KSTR[reverse(1)]] = obj.field
    //   RET0  0 1.
    // var_info names slot 0 as `x` at the TGETS (real-idx 1), so the
    // recovery recognizes it as a LocalDecl.
    assert_eq!(decompile_fixture("field_access"), "local x = obj.field");
}

#[test]
fn decompiles_index_num() {
    // `local x = obj[5]`:
    //   GGET  0 0      ; "obj" at slot 0
    //   TGETB 0 0 5    ; R[0] = R[0][5] (C is a literal integer key)
    //   RET0  0 1.
    assert_eq!(decompile_fixture("index_num"), "local x = obj[5]");
}

#[test]
fn decompiles_index_var() {
    // `local x = t[k]`:
    //   GGET  0 0      ; "t" at slot 0
    //   GGET  1 1      ; "k" at slot 1
    //   TGETV 0 0 1    ; R[0] = R[0][R[1]] = t[k]
    //   RET0  0 1.
    assert_eq!(decompile_fixture("index_var"), "local x = t[k]");
}

#[test]
fn decompiles_method_call_with_one_arg() {
    // `obj:method(1)`:
    //   GGET   0 0     ; obj at slot 0
    //   MOV    2 0     ; self = obj → slot 2 (fills the FR2 gap)
    //   TGETS  0 0 1   ; obj.method → slot 0
    //   KSHORT 3 1     ; explicit arg → slot 3
    //   CALL   0 1 3   ; A=0, B=1 (0 results), C=3 (2 args: self + explicit)
    //   RET0   0 1.
    assert_eq!(decompile_fixture("method_call"), "obj:method(1)");
}

#[test]
fn decompiles_method_call_no_args() {
    // `obj:method()`:
    //   GGET   0 0     ; obj at slot 0
    //   MOV    2 0     ; self → slot 2
    //   TGETS  0 0 1   ; obj.method → slot 0
    //   CALL   0 1 2   ; A=0, B=1, C=2 (1 arg: self only — 0 explicit)
    //   RET0   0 1.
    assert_eq!(decompile_fixture("method_no_arg"), "obj:method()");
}

#[test]
fn decompiles_method_call_with_result() {
    // `local x = obj:method()`:
    //   GGET   0 0     ; obj at slot 0
    //   MOV    2 0     ; self → slot 2
    //   TGETS  0 0 1   ; obj.method → slot 0
    //   CALL   0 2 2   ; A=0, B=2 (1 result at slot 0), C=2 (self only)
    //   RET0   0 1.
    // The result overwrites slot 0; var_info names slot 0 as `x` at
    // the CALL's real-idx, so the recovery recognizes it as a
    // LocalDecl and emits `local x = obj:method()`.
    assert_eq!(
        decompile_fixture("method_with_result"),
        "local x = obj:method()"
    );
}
