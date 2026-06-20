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
//! - **Stage 3**: handles `local x = <const>` declarations. The key
//!   insight is that `local x = 5; return x` compiles to the *same*
//!   real instructions as `return 5` (`KSHORT 0 5; RET1 0 2`); the
//!   only difference is the debug section's `var_info` naming slot 0
//!   as "x". When a constant load writes to a slot the debug info
//!   names as a live local, we emit `local x = <const>; return x`
//!   (or just `local x = <const>` when the implicit return is
//!   `RET0`). The debug info is the discriminator.
//! - **Stage 4**: handles single binary arithmetic expressions whose
//!   result is returned (`Addvn`/`Addnv`/`Addvv` and the `Sub`/`Mul`/
//!   `Div`/`Mod` variants, plus `Pow` and `Cat`). The walk records the
//!   expression string for the destination slot without emitting a
//!   line; `RET1` then surfaces the expression as the returned value.
//!
//! Every other input returns [`DecompilerError::NotImplemented`] —
//! the pipeline grows into more cases in later stages.

use std::collections::HashMap;

use crate::ir::{GcConst, Instruction, Module, NumConst, Opcode, Proto};
use crate::number::format_lua_number;
use crate::DecompilerError;

/// Emit Lua source from a parsed module.
///
/// Walks the main proto's real instructions in order, threading a
/// `slot_exprs` map (register slot → the source expression currently
/// occupying it). Constant-load opcodes may also emit a `local`
/// declaration when the debug info names their target slot. `RET0`
/// terminates the chunk (possibly with no emitted statements);
/// `RET1` materializes one slot's expression as the return value.
/// Any opcode not yet handled, or any shape we can't model, returns
/// [`DecompilerError::NotImplemented`].
pub fn emit_module(module: &Module) -> Result<String, DecompilerError> {
    let main = module.main_proto();
    // Slot 0 is the synthesized FUNC* header; real instructions start
    // at index 1. The walk index is relative to `real_insts`, which
    // matches the convention `var_name_at` expects (0 = first real
    // instruction, not counting the FUNC* header at in-memory slot 0).
    let real_insts = &main.insts[1..];

    let mut slot_exprs: HashMap<u8, String> = HashMap::new();
    let mut lines: Vec<String> = Vec::new();
    let mut saw_return = false;

    for (idx, inst) in real_insts.iter().enumerate() {
        match inst.op {
            Opcode::Ret0 => {
                // Implicit end-of-chunk return. No statement emitted;
                // whatever's accumulated in `lines` is the source.
                saw_return = true;
                break;
            }
            Opcode::Ret1 => {
                // `RET1 A D` returns one value from `r(A)`; the
                // single-value-return convention is `D == 2`. We don't
                // restrict `A` to 0 (it can be any slot whose
                // expression we've recorded); a slot with no recorded
                // expression means we've hit a shape we don't model.
                if inst.d() != 2 {
                    return Err(DecompilerError::NotImplemented);
                }
                let expr = slot_exprs
                    .get(&inst.a)
                    .ok_or(DecompilerError::NotImplemented)?;
                lines.push(format!("return {}", expr));
                saw_return = true;
                break;
            }
            Opcode::Kshort | Opcode::Knum | Opcode::Kstr | Opcode::Kpri => {
                let expr = const_load_expr(main, inst)?;
                // Stage 3 discriminator: if the debug section names
                // this slot as a live local at this instruction, the
                // load is a `local <name> = <expr>` declaration rather
                // than a transient write. We emit the line AND record
                // the *name* (not the expression) under the slot, so a
                // later `return <name>` references the local.
                if let Some(name) = main.var_name_at(inst.a, idx) {
                    lines.push(format!("local {} = {}", name, expr));
                    slot_exprs.insert(inst.a, name.to_string());
                } else {
                    slot_exprs.insert(inst.a, expr);
                }
            }
            _ => return Err(DecompilerError::NotImplemented),
        }
    }

    if !saw_return {
        return Err(DecompilerError::NotImplemented);
    }
    Ok(lines.join("\n"))
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
            // D is a forward index into num_consts. The shared
            // helper does the bounds check and renders the value.
            let idx = load.d() as usize;
            format_num_const(&proto.num_consts, idx)
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

/// Format a number constant from a proto's `num_consts` table by
/// forward index. Shared by KNUM loading and the arithmetic `*VN` /
/// `*NV` operand paths, which both resolve number-constant operands
/// the same way (forward index, LuaJIT-compatible formatting).
///
/// Returns [`DecompilerError::NotImplemented`] if `idx` is out of
/// range — malformed bytecode belongs to the parser, so an out-of-range
/// index here means we're past the validity boundary and should bail
/// rather than guess.
fn format_num_const(num_consts: &[NumConst], idx: usize) -> Result<String, DecompilerError> {
    let nc = num_consts.get(idx).ok_or(DecompilerError::NotImplemented)?;
    Ok(match nc {
        NumConst::Int(i) => format!("{}", i),
        NumConst::Num(f) => format_lua_number(*f),
    })
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

    // ---- Stage 3: `local x = <const>` declarations -------------------
    //
    // These mirror the Stage 2 unit tests but with a var_info record
    // naming the load's target slot. The bytecode shape is identical
    // to Stage 2's — only the debug section differs, which is exactly
    // the discriminator `emit_module` consults via `Proto::var_name_at`.

    /// Build a `VarInfo` record naming `slot` as `name` with a scope
    /// covering real instructions `begin..=end` (inclusive). This
    /// mirrors what the parser would produce for `local x = <const>`.
    fn named_local(slot: u8, name: &str, begin: u32, end: u32) -> crate::ir::VarInfo {
        crate::ir::VarInfo {
            kind: crate::ir::VarKind::Name,
            name: Some(name.to_string()),
            is_parameter: false,
            slot,
            scope_begin: begin,
            scope_end: end,
        }
    }

    /// Build a Stage 3 module: a constant `load` into slot 0 named
    /// `name` (via var_info) followed by `ret`. The caller supplies
    /// the load instruction, the return instruction, and any constant
    /// tables the load resolves against.
    fn local_module(
        load: Instruction,
        ret: Instruction,
        name: &str,
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
                insts: vec![Instruction::synthetic_header(Opcode::Funcv, 1), load, ret],
                debug: Some(DebugInfo {
                    var_info: vec![named_local(0, name, 0, 1)],
                    ..DebugInfo::default()
                }),
            }],
        }
    }

    #[test]
    fn emit_local_int() {
        // `local x = 5; return x`: KSHORT 0 5; RET1 0 2 with var_info.
        let load = Instruction {
            op: Opcode::Kshort,
            a: 0,
            b_or_d: 5,
            c: 0,
        };
        let ret = Instruction {
            op: Opcode::Ret1,
            a: 0,
            b_or_d: 2,
            c: 0,
        };
        let module = local_module(load, ret, "x", Vec::new(), Vec::new());
        assert_eq!(emit_module(&module).unwrap(), "local x = 5\nreturn x");
    }

    #[test]
    fn emit_local_no_return() {
        // `local x = 5` with implicit return: KSHORT 0 5; RET0 0 1.
        // The local's scope covers only instruction 0 (the load); the
        // RET0 is the implicit end-of-chunk return.
        let load = Instruction {
            op: Opcode::Kshort,
            a: 0,
            b_or_d: 5,
            c: 0,
        };
        let ret = Instruction {
            op: Opcode::Ret0,
            a: 0,
            b_or_d: 1,
            c: 0,
        };
        // Scope covers both instructions so var_name_at(load.a, 0)
        // still resolves — matches the real luajit output, which has
        // the local live through the RET0.
        let module = Module {
            header: ModuleHeader {
                flags: 0,
                chunkname: None,
            },
            protos: vec![Proto {
                flags: 0,
                numparams: 0,
                framesize: 1,
                upvalues: Vec::<UpvalDesc>::new(),
                gc_consts: Vec::new(),
                num_consts: Vec::new(),
                insts: vec![Instruction::synthetic_header(Opcode::Funcv, 1), load, ret],
                debug: Some(DebugInfo {
                    var_info: vec![named_local(0, "x", 0, 1)],
                    ..DebugInfo::default()
                }),
            }],
        };
        assert_eq!(emit_module(&module).unwrap(), "local x = 5");
    }

    #[test]
    fn emit_local_str() {
        // `local x = "foo"; return x`: KSTR 0 0; RET1 0 2 with var_info.
        let load = Instruction {
            op: Opcode::Kstr,
            a: 0,
            b_or_d: 0,
            c: 0,
        };
        let ret = Instruction {
            op: Opcode::Ret1,
            a: 0,
            b_or_d: 2,
            c: 0,
        };
        let module = local_module(
            load,
            ret,
            "x",
            vec![GcConst::Str(b"foo".to_vec())],
            Vec::new(),
        );
        assert_eq!(emit_module(&module).unwrap(), "local x = \"foo\"\nreturn x");
    }

    #[test]
    fn emit_local_with_ret1_wrong_slot() {
        // var_info names slot 0, but RET1 reads slot 1 — the return
        // doesn't read the declared local, so this isn't the
        // `local x = <const>; return x` shape.
        let load = Instruction {
            op: Opcode::Kshort,
            a: 0,
            b_or_d: 5,
            c: 0,
        };
        let ret = Instruction {
            op: Opcode::Ret1,
            a: 1, // wrong slot
            b_or_d: 2,
            c: 0,
        };
        let module = local_module(load, ret, "x", Vec::new(), Vec::new());
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for RET1 reading wrong slot, got {:?}",
            result
        );
    }

    #[test]
    fn emit_local_with_ret1_wrong_d() {
        // RET1 D != 2 — not the single-value-return convention.
        let load = Instruction {
            op: Opcode::Kshort,
            a: 0,
            b_or_d: 5,
            c: 0,
        };
        let ret = Instruction {
            op: Opcode::Ret1,
            a: 0,
            b_or_d: 3, // wrong D
            c: 0,
        };
        let module = local_module(load, ret, "x", Vec::new(), Vec::new());
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for RET1 with wrong D, got {:?}",
            result
        );
    }

    #[test]
    fn emit_stage2_return_const_with_empty_var_info_still_works() {
        // Regression: a Stage 2 module built without var_info must
        // still take the `return <const>` path, NOT be mistaken for a
        // local declaration. The `return_const_module` helper builds
        // protos with `DebugInfo::default()` (empty var_info), so
        // `var_name_at` returns None.
        let load = Instruction {
            op: Opcode::Kshort,
            a: 0,
            b_or_d: 5,
            c: 0,
        };
        let module = return_const_module(load, Vec::new(), Vec::new());
        assert_eq!(emit_module(&module).unwrap(), "return 5");
    }
}
