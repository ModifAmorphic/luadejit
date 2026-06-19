//! Source emission from a parsed [`Module`].
//!
//! Stage 1 implements only the degenerate case: a main proto whose
//! only real instruction (excluding the synthetic FUNC* header) is
//! `RET0`. That case corresponds to a Lua source chunk of just
//! `return`, which round-trips to empty source. Every other input
//! returns [`DecompilerError::NotImplemented`] — the pipeline grows
//! into more cases in later stages.

use crate::frontend::{Module, Opcode};
use crate::DecompilerError;

/// Emit Lua source from a parsed module.
///
/// Stage 1: handles only the RET0-only main proto. Returns empty
/// source for that case. Returns `NotImplemented` for anything else.
pub fn emit_module(module: &Module) -> Result<String, DecompilerError> {
    let main = module.main_proto();
    // Slot 0 is the synthesized FUNC* header; real instructions start
    // at index 1.
    let real_insts = &main.insts[1..];

    if real_insts.len() == 1 && real_insts[0].op == Opcode::Ret0 {
        return Ok(String::new());
    }

    Err(DecompilerError::NotImplemented)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::{
        DebugInfo, GcConst, Instruction, ModuleHeader, NumConst, Opcode, Proto, UpvalDesc,
    };

    fn ret_only_module() -> Module {
        Module {
            header: ModuleHeader {
                flags: 0,
                chunkname: None,
            },
            protos: vec![Proto {
                flags: 0,
                numparams: 0,
                framesize: 1,
                upvalues: Vec::<UpvalDesc>::new(),
                gc_consts: Vec::<GcConst>::new(),
                num_consts: Vec::<NumConst>::new(),
                insts: vec![
                    Instruction::synthetic_header(Opcode::Funcv, 1),
                    Instruction {
                        op: Opcode::Ret0,
                        a: 0,
                        b_or_d: 1,
                        c: 0,
                    },
                ],
                debug: Some(DebugInfo::default()),
            }],
        }
    }

    #[test]
    fn emit_ret0_only_returns_empty_source() {
        let module = ret_only_module();
        let source = emit_module(&module).expect("RET0-only should emit");
        assert_eq!(source, "");
    }

    #[test]
    fn emit_returns_not_implemented_for_other_inputs() {
        // A module whose main proto has a non-RET0 instruction.
        let mut module = ret_only_module();
        module.protos[0].insts[1] = Instruction {
            op: Opcode::Ret1,
            a: 0,
            b_or_d: 2,
            c: 0,
        };
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented, got {:?}",
            result
        );
    }
}
