//! Stage 16 integration tests: multres support — multi-return values,
//! tail calls, varargs, multres returns, and multres call args.
//!
//! Stage 16 adds the multres opcode family on top of Stage 15's
//! function literals:
//!
//! * **CALL with B > 2** — `local a, b, c = f()`. Known multiple
//!   results. The handler checks that every result slot is a fresh
//!   local declaration and emits `Stmt::LocalDeclMulti`.
//! * **CALL with B == 0** — multres results (the count is determined
//!   by the consumer). The handler stores the call expression in slot
//!   A; a later RETM/CALLM picks it up.
//! * **CALLT/CALLMT** — tail calls. Previously a CFG-terminator
//!   bailout; now lowered to `return f(args)`.
//! * **VARG** — `local x = ...` (B == 2, one fixed result) or
//!   multres varargs (B == 0, consumed by RETM/CALLM). Lowers to
//!   `Expr::Vararg`.
//! * **RETM** — `return f()` or `return ...`. Returns the multres
//!   produced by the previous instruction (slot A).
//! * **CALLM** — `print(f())`. Call with multres args (C == 0); the
//!   args come from the previous CALL/VARG.
//! * **FNEW + vararg child** — `function(...) ... end`. Stage 16
//!   appends `...` to the parameter list when the child proto carries
//!   `PROTO_VARARG`.
//!
//! # What's NOT covered (deferred to later stages)
//!
//! * Upvalues (`UGET`/`USETx`/`UCLO`) — still bail with NotImplemented
//!   when `numuv > 0`.
//! * TSETM (multres table population for `{f()}`) — Stage 17.
//! * CALLM with C > 0 (fixed args + trailing multres, e.g.
//!   `f(a, b, ...)` with explicit fixed args) — deferred.
//! * RETM with D > 0 (known multres count) — rare; deferred.
//!
//! As with Stage 7-15 fixtures, CI runs without luajit installed, so
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
fn decompiles_multi_return() {
    // `local a, b, c = f()`:
    //   GGET 0 0      ; "f" at slot 0
    //   CALL  0 4 1   ; A=0, B=4 (3 results), C=1 (0 args)
    //   RET0  0 1.
    // Each result slot (0, 1, 2) is the declaration point of a named
    // local (a, b, c). The handler emits Stmt::LocalDeclMulti.
    assert_eq!(decompile_fixture("multi_return"), "local a, b, c = f()");
}

#[test]
fn decompiles_tail_call() {
    // `return f()`:
    //   GGET  0 0     ; "f" at slot 0
    //   CALLT 0 1.    ; A=0, D=1 (0 args + 1)
    // CALLT is a CFG terminator (Terminator::TailCall). Stage 16
    // lowers it to Stmt::Return(Some(Expr::Call { f, [] })).
    assert_eq!(decompile_fixture("tail_call"), "return f()");
}

#[test]
fn decompiles_vararg_local() {
    // `local f = function(...) local x = ... end`:
    //   -- child proto (vararg, numparams=0):
    //   VARG 0 2 0    ; A=0, B=2 (1 result), D=0
    //   RET0 0 1
    //   -- main proto:
    //   FNEW 0 0      ; closure → slot 0
    //   RET0 0 1.
    // The child carries PROTO_VARARG; Stage 16 appends `...` to the
    // parameter list. VARG with B=2 lowers to `Expr::Vararg` stored
    // at slot 0, which the var_info names `x` → `local x = ...`.
    assert_eq!(
        decompile_fixture("vararg_local"),
        "local f = function(...)\n    local x = ...\nend"
    );
}

#[test]
fn decompiles_vararg_return() {
    // `local f = function(...) return ... end`:
    //   -- child proto (vararg, numparams=0):
    //   VARG 0 0 0    ; A=0, B=0 (multres)
    //   RETM 0 0      ; return multres from slot 0
    //   -- main proto:
    //   FNEW 0 0
    //   RET0 0 1.
    // VARG with B=0 stores `Expr::Vararg` in slot 0 for the consumer.
    // RETM with D=0 returns slot 0's expr → `return ...`.
    assert_eq!(
        decompile_fixture("vararg_return"),
        "local f = function(...)\n    return ...\nend"
    );
}

#[test]
fn decompiles_call_multires() {
    // `print(f())`:
    //   GGET 0 0      ; "print" at slot 0
    //   GGET 2 1      ; "f" at slot 2
    //   CALL 2 0 1    ; call f() with B=0 (multres results → slot 2)
    //   CALLM 0 1 0   ; call print with C=0 (multres args from previous CALL)
    //   RET0 0 1.
    // The inner CALL stores Expr::Call{f, []} in slot 2. CALLM looks
    // up slot arg_base (= 2 in FR2) for the multres arg, builds
    // Expr::Call{print, [Call{f, []}]}, and emits it as a bare call.
    assert_eq!(decompile_fixture("call_multires"), "print(f())");
}
