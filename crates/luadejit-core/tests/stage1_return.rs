//! Stage 1 integration test: decompile a chunk that's just `return`.
//!
//! The fixture bytecode is produced by luajit from
//! `tests/fixtures/return/source.lua` (content: `return\n`). The
//! expected decompiler output is empty source — the chunk contains no
//! real statements.

use std::fs;

#[test]
fn decompiles_return_to_empty_source() {
    let bc_path = "tests/fixtures/return/input.bc";
    let bytes =
        fs::read(bc_path).unwrap_or_else(|e| panic!("failed to read fixture {}: {}", bc_path, e));

    let result = luadejit_core::decompile(&bytes);

    assert!(
        result.is_ok(),
        "expected decompile to succeed, got: {:?}",
        result
    );
    let source = result.unwrap();
    assert_eq!(
        source, "",
        "expected empty source for return-only chunk, got: {:?}",
        source
    );
}
