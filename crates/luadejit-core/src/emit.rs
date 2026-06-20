//! Source emission from a parsed [`Module`].
//!
//! - **Stage 1**: handles the degenerate case where the main proto's
//!   only real instruction (excluding the synthetic FUNC* header) is
//!   `RET0`. That case corresponds to a Lua source chunk of just
//!   `return`, which round-trips to empty source.
//! - **Stage 2**: handles `return <const>` for the four constant-load
//!   opcodes (`KSHORT`, `KNUM`, `KSTR`, `KPRI`) followed by
//!   `RET1 0 2`. The emitted source is `return <const>` with no
//!   trailing newline.
//!
//! Every other input returns [`DecompilerError::NotImplemented`] —
//! the pipeline grows into more cases in later stages.

use crate::ir::{GcConst, Instruction, Module, NumConst, Opcode, Proto};
use crate::DecompilerError;

/// Emit Lua source from a parsed module.
///
/// See the module docs for the cases currently handled. Returns
/// [`DecompilerError::NotImplemented`] for any input that doesn't
/// match a handled shape.
pub fn emit_module(module: &Module) -> Result<String, DecompilerError> {
    let main = module.main_proto();
    // Slot 0 is the synthesized FUNC* header; real instructions start
    // at index 1.
    let real_insts = &main.insts[1..];

    // Stage 1: RET0-only -> empty source.
    if real_insts.len() == 1 && real_insts[0].op == Opcode::Ret0 {
        return Ok(String::new());
    }

    // Stage 2: [load_const, RET1 0 2] -> "return <const>".
    if real_insts.len() == 2 && real_insts[1].op == Opcode::Ret1 {
        return emit_return_const(main, &real_insts[0], &real_insts[1]);
    }

    Err(DecompilerError::NotImplemented)
}

/// Emit a `return <const>` chunk: a single constant-load instruction
/// into r0 followed by `RET1 0 2`.
///
/// `RET1 A D` returns one value from `r(A)`; for `return <single
/// const>` LuaJIT always emits `RET1 0 2` (`A=0` = the loaded
/// register, `D=2` = multres base for "1 return value"). We enforce
/// that exact shape plus `load.a == 0` and bail with
/// [`DecompilerError::NotImplemented`] for anything else, since other
/// shapes belong to later stages (multi-return, partial returns,
/// etc.).
fn emit_return_const(
    proto: &Proto,
    load: &Instruction,
    ret: &Instruction,
) -> Result<String, DecompilerError> {
    // RET1 must target r0 with the single-value-return convention.
    if ret.a != 0 || ret.d() != 2 {
        return Err(DecompilerError::NotImplemented);
    }
    // The load must target r0 (the register RET1 reads from).
    if load.a != 0 {
        return Err(DecompilerError::NotImplemented);
    }
    let expr = const_load_expr(proto, load)?;
    Ok(format!("return {}", expr))
}

/// Render the source expression produced by a constant-load
/// instruction. Returns [`DecompilerError::NotImplemented`] for
/// opcodes/load shapes this stage doesn't handle.
fn const_load_expr(proto: &Proto, load: &Instruction) -> Result<String, DecompilerError> {
    match load.op {
        Opcode::Kshort => {
            // D is a signed 16-bit immediate (the value itself, not
            // an index). Reinterpret the u16 bits as i16, then widen
            // to i32 preserving sign.
            let val = load.d() as i16 as i32;
            Ok(format!("{}", val))
        }
        Opcode::Knum => {
            // D is a forward index into num_consts.
            let idx = load.d() as usize;
            let nc = proto
                .num_consts
                .get(idx)
                .ok_or(DecompilerError::NotImplemented)?;
            match nc {
                NumConst::Int(i) => Ok(format!("{}", i)),
                NumConst::Num(f) => {
                    // Stage 2 limitation: Rust's `{}` float formatting
                    // matches Lua's `luajit -bl`/`lua_tostring` for
                    // common cases like `3.14`, but differs for some
                    // values (e.g. `3.0` -> Rust emits `3`, Lua emits
                    // `3.0`). Full Lua-compatible float formatting is
                    // deferred to a later stage; for now we accept the
                    // round-trip mismatch on those edge cases.
                    Ok(format!("{}", f))
                }
            }
        }
        Opcode::Kstr => {
            // D is a reverse index into gc_consts — use the helper to
            // avoid the classic forward-vs-reverse indexing bug.
            let gc = proto.gc_const_for_operand(load.d())?;
            match gc {
                GcConst::Str(bytes) => {
                    // Stage 2 limitation: use Rust's `{:?}` formatting,
                    // which matches Lua's escaping for common cases
                    // (\", \n, \t, \\). Full Lua string escaping (e.g.
                    // the exact set of control-char shorthands and
                    // non-printable hex forms) is deferred to a later
                    // stage. The format doesn't guarantee UTF-8; we use
                    // a lossy conversion so emit never panics on
                    // well-formed bytecode.
                    let s = String::from_utf8_lossy(bytes);
                    Ok(format!("{:?}", s))
                }
                // Non-string GC constants loaded via KSTR aren't part
                // of any current stage.
                _ => Err(DecompilerError::NotImplemented),
            }
        }
        Opcode::Kpri => match load.d() {
            0 => Ok("nil".to_string()),
            1 => Ok("false".to_string()),
            2 => Ok("true".to_string()),
            _ => Err(DecompilerError::NotImplemented),
        },
        _ => Err(DecompilerError::NotImplemented),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
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

    /// Build a main proto whose real instructions are exactly
    /// `[load, RET1 0 2]` — the Stage 2 shape. Caller supplies the
    /// load instruction and any tables its opcode resolves against.
    fn return_const_module(
        load: Instruction,
        gc_consts: Vec<GcConst>,
        num_consts: Vec<NumConst>,
    ) -> Module {
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
                gc_consts,
                num_consts,
                insts: vec![
                    Instruction::synthetic_header(Opcode::Funcv, 1),
                    load,
                    Instruction {
                        op: Opcode::Ret1,
                        a: 0,
                        b_or_d: 2,
                        c: 0,
                    },
                ],
                debug: Some(DebugInfo::default()),
            }],
        }
    }

    #[test]
    fn emit_return_const_kshort() {
        // `return 5`: KSHORT 0 5; RET1 0 2.
        let load = Instruction {
            op: Opcode::Kshort,
            a: 0,
            b_or_d: 5,
            c: 0,
        };
        let module = return_const_module(load, Vec::new(), Vec::new());
        assert_eq!(emit_module(&module).unwrap(), "return 5");
    }

    #[test]
    fn emit_return_const_kshort_negative() {
        // `return -7`: KSHORT's D is signed 16-bit; -7 as i16 is
        // 0xFFF9 = 65529 as u16.
        let load = Instruction {
            op: Opcode::Kshort,
            a: 0,
            b_or_d: 0xFFF9,
            c: 0,
        };
        let module = return_const_module(load, Vec::new(), Vec::new());
        assert_eq!(emit_module(&module).unwrap(), "return -7");
    }

    #[test]
    fn emit_return_const_kpri_nil() {
        // `return nil`: KPRI 0 0.
        let load = Instruction {
            op: Opcode::Kpri,
            a: 0,
            b_or_d: 0,
            c: 0,
        };
        let module = return_const_module(load, Vec::new(), Vec::new());
        assert_eq!(emit_module(&module).unwrap(), "return nil");
    }

    #[test]
    fn emit_return_const_kpri_true() {
        // `return true`: KPRI 0 2.
        let load = Instruction {
            op: Opcode::Kpri,
            a: 0,
            b_or_d: 2,
            c: 0,
        };
        let module = return_const_module(load, Vec::new(), Vec::new());
        assert_eq!(emit_module(&module).unwrap(), "return true");
    }

    #[test]
    fn emit_return_const_kpri_false() {
        // `return false`: KPRI 0 1.
        let load = Instruction {
            op: Opcode::Kpri,
            a: 0,
            b_or_d: 1,
            c: 0,
        };
        let module = return_const_module(load, Vec::new(), Vec::new());
        assert_eq!(emit_module(&module).unwrap(), "return false");
    }

    #[test]
    // The fixture value 3.14 trips clippy::approx_constant (PI). We
    // intentionally use 3.14 here so the unit test mirrors the
    // `return_float` integration fixture exactly.
    #[allow(clippy::approx_constant)]
    fn emit_return_const_knum() {
        // `return 3.14`: KNUM 0 0; num_consts = [Num(3.14)].
        // KNUM's D is a FORWARD index.
        let load = Instruction {
            op: Opcode::Knum,
            a: 0,
            b_or_d: 0,
            c: 0,
        };
        let module = return_const_module(load, Vec::new(), vec![NumConst::Num(3.14_f64)]);
        assert_eq!(emit_module(&module).unwrap(), "return 3.14");
    }

    #[test]
    fn emit_return_const_knum_int_const() {
        // A boxed-int num const should format as an integer.
        let load = Instruction {
            op: Opcode::Knum,
            a: 0,
            b_or_d: 0,
            c: 0,
        };
        let module = return_const_module(load, Vec::new(), vec![NumConst::Int(42)]);
        assert_eq!(emit_module(&module).unwrap(), "return 42");
    }

    #[test]
    fn emit_return_const_kstr() {
        // `return "foo"`: KSTR 0 0; gc_consts = [Str("foo")].
        // KSTR's D is a REVERSE index — operand 0 resolves to
        // gc_consts[len-1-0] = gc_consts[0] when len == 1.
        let load = Instruction {
            op: Opcode::Kstr,
            a: 0,
            b_or_d: 0,
            c: 0,
        };
        let module = return_const_module(load, vec![GcConst::Str(b"foo".to_vec())], Vec::new());
        assert_eq!(emit_module(&module).unwrap(), "return \"foo\"");
    }

    #[test]
    fn emit_return_const_not_implemented_for_non_zero_ret_a() {
        // RET1 1 2: A != 0 — not the single-const-return shape.
        let mut module = return_const_module(
            Instruction {
                op: Opcode::Kshort,
                a: 0,
                b_or_d: 5,
                c: 0,
            },
            Vec::new(),
            Vec::new(),
        );
        module.protos[0].insts[2].a = 1;
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for RET1 with A != 0, got {:?}",
            result
        );
    }

    #[test]
    fn emit_return_const_not_implemented_for_wrong_ret_d() {
        // RET1 0 3: D != 2 — not the single-value-return convention.
        let mut module = return_const_module(
            Instruction {
                op: Opcode::Kshort,
                a: 0,
                b_or_d: 5,
                c: 0,
            },
            Vec::new(),
            Vec::new(),
        );
        module.protos[0].insts[2].b_or_d = 3;
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for RET1 with D != 2, got {:?}",
            result
        );
    }

    #[test]
    fn emit_return_const_not_implemented_for_load_to_nonzero_reg() {
        // KSHORT 1 5: load targets r1, but RET1 reads r0.
        let mut module = return_const_module(
            Instruction {
                op: Opcode::Kshort,
                a: 0,
                b_or_d: 5,
                c: 0,
            },
            Vec::new(),
            Vec::new(),
        );
        module.protos[0].insts[1].a = 1;
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for load targeting r != 0, got {:?}",
            result
        );
    }

    #[test]
    fn emit_return_const_not_implemented_for_unsupported_load_op() {
        // Unsupported load opcode (e.g. MOV) in the load slot.
        let load = Instruction {
            op: Opcode::Mov,
            a: 0,
            b_or_d: 0,
            c: 0,
        };
        let module = return_const_module(load, Vec::new(), Vec::new());
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for unsupported load op, got {:?}",
            result
        );
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
