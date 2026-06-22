//! Stage 14 integration tests: decompile table literals and writes.
//!
//! Stage 14 adds table construction and table writes on top of
//! Stage 13's table reads. The six fixtures pin the supported
//! shapes:
//!
//! * `table_literal_array` — `local t = {1, 2, 3}`. TDUP with an
//!   array-only template.
//! * `table_literal_hash` — `local t = {a = 1, b = 2}`. TDUP with a
//!   hash-only template (string keys).
//! * `table_literal_mixed` — `local t = {1, 2, x = 3}`. TDUP with
//!   both array and hash parts in the template.
//! * `table_empty` — `local t = {}`. TNEW (empty table; no
//!   template).
//! * `table_field_set` — `local t = {}; t.x = 1`. TNEW + TSETS.
//! * `table_index_set` — `local t = {}; t[0] = 42`. TNEW + TSETB.
//!
//! # TDUP / TNEW operand conventions (verified via `luajit -bl`)
//!
//! LuaJIT uses TDUP for ALL non-empty table literals — the table is
//! pre-built as a GC constant and copied at runtime. TNEW is only
//! used for empty tables (`{}`).
//!
//! - **TDUP A D** (AD format): A = destination slot, D = gc_consts
//!   reverse index (resolved via [`Proto::gc_const_for_operand`]).
//!   The GC constant should be `GcConst::Tab(TableConst)`.
//! - **TNEW A D** (AD format): A = destination slot, D = array size
//!   hint (asynchronously allocated). The recovery ignores D — emit
//!   `{}`.
//!
//! # TSET* operand conventions (verified via `luajit -bl`)
//!
//! All three are ABC format. Note the operand order: **A = value,
//! B = table, C = key**. This is REVERSED from TGET* (where
//! A = destination, B = table, C = key).
//!
//! - **TSETS A B C**: `R[B][KSTR[reverse(C)]] = R[A]`. C is a
//!   reverse-indexed GC string constant.
//! - **TSETB A B C**: `R[B][C] = R[A]`. C is an inline integer
//!   literal key (NOT a register).
//! - **TSETV A B C**: `R[B][R[C]] = R[A]`. C is a register key.
//!
//! # What's NOT covered (deferred to later stages)
//!
//! * `TSETM` (multres table population for `{f()}`) — Stage 15.
//! * Merging `TNEW` + subsequent `TSET*` into a single table literal
//!   — the recovery emits them as separate statements in Stage 14.
//! * `TGETR`/`TSETR` (FFI — rare).
//!
//! As with Stage 7-13 fixtures, CI runs without luajit installed, so
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
fn decompiles_table_literal_array() {
    // `local t = {1, 2, 3}`:
    //   TDUP  0 0      ; copy template table from gc_consts[reverse(0)] → slot 0
    //   RET0  0 1.
    // Template has 3 array entries: [Int(1), Int(2), Int(3)].
    assert_eq!(
        decompile_fixture("table_literal_array"),
        "local t = {1, 2, 3}"
    );
}

#[test]
fn decompiles_table_literal_hash() {
    // `local t = {a = 1, b = 2}`:
    //   TDUP  0 0
    //   RET0  0 1.
    // Template hash entries: [(Str("a"), Int(1)), (Str("b"), Int(2))].
    assert_eq!(
        decompile_fixture("table_literal_hash"),
        "local t = {a = 1, b = 2}"
    );
}

#[test]
fn decompiles_table_literal_mixed() {
    // `local t = {1, 2, x = 3}`:
    //   TDUP  0 0
    //   RET0  0 1.
    // Template: array=[Int(1), Int(2)], hash=[(Str("x"), Int(3))].
    assert_eq!(
        decompile_fixture("table_literal_mixed"),
        "local t = {1, 2, x = 3}"
    );
}

#[test]
fn decompiles_empty_table() {
    // `local t = {}`:
    //   TNEW  0 0      ; create empty table → slot 0 (D is array size hint)
    //   RET0  0 1.
    assert_eq!(decompile_fixture("table_empty"), "local t = {}");
}

#[test]
fn decompiles_table_field_set() {
    // `local t = {}; t.x = 1`:
    //   TNEW   0 0     ; empty table → slot 0
    //   KSHORT 1 1     ; value 1 → slot 1
    //   TSETS  1 0 0   ; R[0][KSTR[reverse(0)]] = R[1], i.e. t.x = 1
    //   RET0   0 1.
    // Stage 14 emits TNEW and TSETS as separate statements (no merge
    // into a single literal).
    assert_eq!(
        decompile_fixture("table_field_set"),
        "local t = {}\nt.x = 1"
    );
}

#[test]
fn decompiles_table_index_set() {
    // `local t = {}; t[0] = 42`:
    //   TNEW   0 0     ; empty table → slot 0
    //   KSHORT 1 42    ; value 42 → slot 1
    //   TSETB  1 0 0   ; R[0][0] = R[1], i.e. t[0] = 42 (C is a literal key)
    //   RET0   0 1.
    assert_eq!(
        decompile_fixture("table_index_set"),
        "local t = {}\nt[0] = 42"
    );
}
