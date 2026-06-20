//! luadejit-core: LuaJIT bytecode decompiler library.
//!
//! The pipeline is: parse bytecode ([`ir::Module::from_bytes`])
//! → emit source ([`emit::emit_module`]). Higher-level stages of the
//! implementation plan (CFG, SSA, structural recovery, etc.) slot in
//! between these two steps in later work; Stage 1 implements only the
//! degenerate RET0-only case end-to-end so the plumbing is proven.

pub mod emit;
pub mod frontend;
pub mod ir;

use ir::Module;

/// Decompile LuaJIT bytecode bytes into Lua source code.
///
/// Stage 1: handles only the degenerate case where the main proto's
/// only real instruction is `RET0` (i.e. the source chunk was just
/// `return`). Returns empty source for that case. Returns
/// [`DecompilerError::NotImplemented`] for any other input and
/// [`DecompilerError::InvalidBytecode`] for malformed bytecode.
pub fn decompile(bytes: &[u8]) -> Result<String, DecompilerError> {
    let module = Module::from_bytes(bytes)?;
    let source = emit::emit_module(&module)?;
    Ok(source)
}

/// Errors that can occur during decompilation.
#[derive(Debug)]
pub enum DecompilerError {
    /// Decompile functionality not yet implemented for this input.
    NotImplemented,
    /// The input is not well-formed LuaJIT bytecode. `offset` is the
    /// absolute byte position where parsing failed.
    InvalidBytecode { offset: usize, reason: String },
}

impl std::fmt::Display for DecompilerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecompilerError::NotImplemented => write!(f, "decompile not implemented yet"),
            DecompilerError::InvalidBytecode { offset, reason } => {
                write!(f, "invalid bytecode at offset {}: {}", offset, reason)
            }
        }
    }
}

impl std::error::Error for DecompilerError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Garbage input should fail at the parser, not return NotImplemented.
    /// Stage 0's stub returned NotImplemented for any input; Stage 1
    /// instead surfaces InvalidBytecode for malformed dumps.
    #[test]
    fn garbage_returns_invalid_bytecode() {
        let result = decompile(b"some bytes");
        match result {
            Err(DecompilerError::InvalidBytecode { reason, .. }) => {
                assert!(reason.contains("magic"));
            }
            other => panic!(
                "expected Err(InvalidBytecode) for garbage input, got {:?}",
                other
            ),
        }
    }
}
