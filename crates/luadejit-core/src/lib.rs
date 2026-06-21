//! luadejit-core: LuaJIT bytecode decompiler library.
//!
//! The pipeline is: parse bytecode ([`ir::Module::from_bytes`])
//! → CFG ([`cfg::Cfg::build`]) → structural recovery
//! ([`structure::recover`]) → emit source ([`emit::emit_module`]).
//! Stage 7 connects the CFG infrastructure (Stages 7a/7b) to the
//! emitter by routing every decompilation through the AST:
//! linear code (Stages 1-6 shapes) round-trips unchanged, and the
//! new capability is `if x then return 1 end` (Stage 7c-7e).
//! Anything not yet handled returns
//! [`DecompilerError::NotImplemented`].

pub mod cfg;
pub mod emit;
pub mod frontend;
pub mod ir;
pub mod number;
pub mod structure;

use ir::Module;

/// Decompile LuaJIT bytecode bytes into Lua source code.
///
/// Currently handles three shapes of main proto:
/// - **Stage 1**: a single `RET0` (source was just `return`) →
///   emits empty source.
/// - **Stage 2**: `[<load_const>, RET1 0 2]` where the load is one of
///   KSHORT/KNUM/KSTR/KPRI and no var_info names the load's target →
///   emits `return <const>` for integer, number, string, bool, and
///   nil constants.
/// - **Stage 3**: same `[<load_const>, RET1 0 2]` (or `[..., RET0]`)
///   shape, but the debug section's var_info names the load's target
///   slot as a live local → emits `local x = <const>[; return x]`.
///   The bytecode is identical to Stage 2's; var_info is the
///   discriminator.
///
/// Returns [`DecompilerError::NotImplemented`] for any other shape
/// and [`DecompilerError::InvalidBytecode`] for malformed bytecode.
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
