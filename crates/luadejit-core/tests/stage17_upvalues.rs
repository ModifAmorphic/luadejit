//! Stage 17 integration tests: upvalue handling (UGET/USETx/UCLO).
//!
//! Stage 17 adds the first cross-proto analysis in the project: a
//! child proto's `upvalues` are resolved against the PARENT's context
//! before the child is recovered, so UGET (read), USETV/USETS/USETN/
//! USETP (write), and UCLO (close, modeled as a no-op) can reference
//! the captured variables by their source-level names.
//!
//! Fixtures:
//! - `upvalue_read` — `local f = function() return x end` (UGET in a
//!   return). The child captures the parent's local `x`.
//! - `upvalue_write` — `local f = function() x = x + 1; return x end`
//!   (UGET + arithmetic + USETV). The child reads AND writes the
//!   captured `x`.
//! - `upvalue_multi` — `local f = function() return x + y end`
//!   (two open upvalues captured in one closure).
//!
//! As with Stage 7-16 fixtures, CI runs without luajit installed, so
//! the `.bc` files are committed alongside the `source.lua` files.
//!
//! # What's NOT covered (deferred to later stages)
//!
//! * Generic-for (`ISNEXT`/`ITERN`/`ITERL`).
//! * `TSETM` / `CALLM` method-call gap.
//! * Deeper upvalue nesting (a closed upvalue whose parent itself has
//!   unresolved upvalues) — bails with NotImplemented.

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
fn decompiles_upvalue_read() {
    // `local x = 42; local f = function() return x end`:
    //   -- child proto:
    //   0001 UGET   0 0    ; R[0] = upvalue[0] (= parent's "x")
    //   0002 RET1   0 2    ; return x
    //   -- main proto:
    //   0001 KSHORT 0 42
    //   0002 FNEW   1 0
    //   0003 UCLO   0 => 0004   ; close upvalues >= slot 0 (no-op)
    //   0004 RET0   0 1.
    assert_eq!(
        decompile_fixture("upvalue_read"),
        "local x = 42\nlocal f = function()\n    return x\nend"
    );
}

#[test]
fn decompiles_upvalue_write() {
    // `local x = 42; local f = function() x = x + 1; return x end`:
    //   -- child proto:
    //   0001 UGET   0 0    ; R[0] = upvalue[0] (x)
    //   0002 ADDVN  0 0 0  ; R[0] = R[0] + 1
    //   0003 USETV  0 0    ; upvalue[0] = R[0]  (write back to x)
    //   0004 UGET   0 0    ; R[0] = upvalue[0]  (re-read for return)
    //   0005 RET1   0 2    ; return x.
    assert_eq!(
        decompile_fixture("upvalue_write"),
        "local x = 42\nlocal f = function()\n    x = x + 1\n    return x\nend"
    );
}

#[test]
fn decompiles_upvalue_multi() {
    // `local x = 1; local y = 2; local f = function() return x + y end`:
    //   -- child proto:
    //   0001 UGET   0 0    ; R[0] = upvalue[0] (x)
    //   0002 UGET   1 1    ; R[1] = upvalue[1] (y)
    //   0003 ADDVV  0 0 1  ; R[0] = R[0] + R[1]
    //   0004 RET1   0 2    ; return x + y.
    assert_eq!(
        decompile_fixture("upvalue_multi"),
        "local x = 1\nlocal y = 2\nlocal f = function()\n    return x + y\nend"
    );
}
