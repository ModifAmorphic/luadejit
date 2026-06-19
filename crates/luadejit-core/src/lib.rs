//! luadejit-core: LuaJIT bytecode decompiler library.
//!
//! Currently at Stage 0 (project skeleton). The `decompile` function
//! is a stub; real implementation lands in subsequent stages per
//! `docs/implementation-plan.md`.

/// Decompile LuaJIT bytecode bytes into Lua source code.
///
/// Stage 0 stub: always returns `DecompilerError::NotImplemented`.
/// Real pipeline (frontend → CFG → SSA → analyses → transforms →
/// structural recovery → phi elimination → emission) comes in
/// Stages 1–17 of the implementation plan.
pub fn decompile(_bytes: &[u8]) -> Result<String, DecompilerError> {
    Err(DecompilerError::NotImplemented)
}

/// Errors that can occur during decompilation.
#[derive(Debug)]
pub enum DecompilerError {
    /// Decompile functionality not yet implemented for this input.
    NotImplemented,
    // Additional variants added in later stages.
}

impl std::fmt::Display for DecompilerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecompilerError::NotImplemented => write!(f, "decompile not implemented yet"),
        }
    }
}

impl std::error::Error for DecompilerError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_returns_not_implemented() {
        let result = decompile(b"some bytes");
        assert!(matches!(result, Err(DecompilerError::NotImplemented)));
    }
}
