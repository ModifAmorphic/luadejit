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
            // ---- Stage 4 binary arithmetic ----------------------------
            //
            // Arithmetic opcodes write the result to a temporary
            // (unnamed) slot. We record the expression string under
            // the destination slot but DO NOT emit a line — the
            // expression only surfaces when RET1 reads the slot. This
            // matches the load case's "named local" path's
            // bookkeeping but skips the `lines.push`.
            //
            // Known limitation: nested arithmetic expressions are
            // emitted without parenthesization. This works whenever
            // Lua's precedence matches the bytecode's evaluation order
            // (the common case: `a + b * c`, `a + b + c`) but produces
            // incorrect output for cases where the bytecode reorders
            // against precedence, e.g. `(a + b) * c` would be emitted
            // as `a + b * c`. Correct parenthesization is deferred to
            // a later stage; Stage 4 only emits single binops.
            //
            // VN: left = reg[B], right = num_const[C].
            Opcode::Addvn | Opcode::Subvn | Opcode::Mulvn | Opcode::Divvn | Opcode::Modvn => {
                let left = lookup_operand(&slot_exprs, inst.b())?;
                let right = format_num_const(&main.num_consts, inst.c as usize)?;
                slot_exprs.insert(
                    inst.a,
                    format!("{} {} {}", left, arith_symbol(inst.op), right),
                );
            }
            // NV: left = num_const[B], right = reg[C].
            Opcode::Addnv | Opcode::Subnv | Opcode::Mulnv | Opcode::Divnv | Opcode::Modnv => {
                let left = format_num_const(&main.num_consts, inst.b() as usize)?;
                let right = lookup_operand(&slot_exprs, inst.c)?;
                slot_exprs.insert(
                    inst.a,
                    format!("{} {} {}", left, arith_symbol(inst.op), right),
                );
            }
            // VV: left = reg[B], right = reg[C].
            Opcode::Addvv | Opcode::Subvv | Opcode::Mulvv | Opcode::Divvv | Opcode::Modvv => {
                let left = lookup_operand(&slot_exprs, inst.b())?;
                let right = lookup_operand(&slot_exprs, inst.c)?;
                slot_exprs.insert(
                    inst.a,
                    format!("{} {} {}", left, arith_symbol(inst.op), right),
                );
            }
            Opcode::Pow => {
                // POW has no VN/NV variants — both operands are
                // registers (same VV shape as above). Kept as its own
                // arm rather than folded into the VV match because the
                // operator is fixed (`^`) rather than looked up.
                let left = lookup_operand(&slot_exprs, inst.b())?;
                let right = lookup_operand(&slot_exprs, inst.c)?;
                slot_exprs.insert(inst.a, format!("{} ^ {}", left, right));
            }
            Opcode::Cat => {
                // CAT concatenates regs[B..=C] into A (a range, not
                // just two operands). LuaJIT lowers `a .. b .. c` to a
                // single CAT whose [B, C] covers all three slots.
                let mut parts: Vec<String> = Vec::new();
                for slot in inst.b()..=inst.c {
                    parts.push(lookup_operand(&slot_exprs, slot)?);
                }
                slot_exprs.insert(inst.a, parts.join(" .. "));
            }
            _ => return Err(DecompilerError::NotImplemented),
        }
    }

    if !saw_return {
        return Err(DecompilerError::NotImplemented);
    }
    Ok(lines.join("\n"))
}

/// Resolve a register operand to its recorded expression string. A
/// missing slot means we've hit an instruction whose source we can't
/// reconstruct from what we've seen so far (e.g. an uninitialized
/// register, or a slot populated by an opcode this stage doesn't
/// handle); we bail with [`DecompilerError::NotImplemented`].
fn lookup_operand(slot_exprs: &HashMap<u8, String>, slot: u8) -> Result<String, DecompilerError> {
    slot_exprs
        .get(&slot)
        .cloned()
        .ok_or(DecompilerError::NotImplemented)
}

/// Map an arithmetic opcode to its Lua source operator. Covers all
/// `ADD`/`SUB`/`MUL`/`DIV`/`MOD` variants (VN/NV/VV); `Pow` and `Cat`
/// are handled inline in the walk rather than through this helper
/// since they don't share the *VN/*NV/*VV pattern.
fn arith_symbol(op: Opcode) -> &'static str {
    match op {
        Opcode::Addvn | Opcode::Addnv | Opcode::Addvv => "+",
        Opcode::Subvn | Opcode::Subnv | Opcode::Subvv => "-",
        Opcode::Mulvn | Opcode::Mulnv | Opcode::Mulvv => "*",
        Opcode::Divvn | Opcode::Divnv | Opcode::Divvv => "/",
        Opcode::Modvn | Opcode::Modnv | Opcode::Modvv => "%",
        // Pow and Cat aren't routed through this helper; reaching
        // here is a logic bug rather than a malformed-input case.
        _ => unreachable!("arith_symbol called on non-arithmetic opcode {:?}", op),
    }
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

    // ---- Stage 4: arithmetic expressions -----------------------------
    //
    // These build Modules by hand with the exact bytecode shapes
    // `luajit -bl` produces for the corresponding source, then verify
    // `emit_module` round-trips them. The walk must record each
    // arithmetic result into `slot_exprs` WITHOUT emitting a line —
    // the expression only surfaces at RET1.

    /// Build a minimal Module whose main proto has the given real
    /// instructions, debug var_info records, and constant tables.
    /// Caller provides everything; this just wraps the boilerplate.
    fn module_with(
        real_insts: Vec<Instruction>,
        var_info: Vec<crate::ir::VarInfo>,
        gc_consts: Vec<GcConst>,
        num_consts: Vec<NumConst>,
        framesize: u8,
    ) -> Module {
        let mut insts = vec![Instruction::synthetic_header(Opcode::Funcv, framesize)];
        insts.extend(real_insts);
        Module {
            header: ModuleHeader {
                flags: 0,
                chunkname: None,
            },
            protos: vec![Proto {
                flags: 0,
                numparams: 0,
                framesize,
                upvalues: Vec::<UpvalDesc>::new(),
                gc_consts,
                num_consts,
                insts,
                debug: Some(DebugInfo {
                    var_info,
                    ..DebugInfo::default()
                }),
            }],
        }
    }

    /// Convenience: build an arithmetic instruction in ABC form.
    fn abc(op: Opcode, a: u8, b: u8, c: u8) -> Instruction {
        Instruction {
            op,
            a,
            b_or_d: u32::from(b),
            c,
        }
    }

    /// Convenience: build an AD-format instruction (D = 16-bit
    /// immediate / index).
    fn ad(op: Opcode, a: u8, d: u16) -> Instruction {
        Instruction {
            op,
            a,
            b_or_d: u32::from(d),
            c: 0,
        }
    }

    #[test]
    fn emit_arith_addvn() {
        // `local a = 5; return a + 3`:
        //   KSHORT 0 5; ADDVN 1 0 0; RET1 1 2.
        // ADDVN: dest=A(1), left=reg[B](0)=a, right=num_const[C](0)=3.
        let insts = vec![
            ad(Opcode::Kshort, 0, 5),
            abc(Opcode::Addvn, 1, 0, 0),
            ad(Opcode::Ret1, 1, 2),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "a", 0, 2)],
            Vec::new(),
            vec![NumConst::Int(3)],
            2,
        );
        assert_eq!(emit_module(&module).unwrap(), "local a = 5\nreturn a + 3");
    }

    #[test]
    fn emit_arith_addnv() {
        // `local a = 5; return 3 + a`:
        //   KSHORT 0 5; ADDNV 1 0 0; RET1 1 2.
        // ADDNV: dest=A(1), left=num_const[B](0)=3, right=reg[C](0)=a.
        let insts = vec![
            ad(Opcode::Kshort, 0, 5),
            abc(Opcode::Addnv, 1, 0, 0),
            ad(Opcode::Ret1, 1, 2),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "a", 0, 2)],
            Vec::new(),
            vec![NumConst::Int(3)],
            2,
        );
        assert_eq!(emit_module(&module).unwrap(), "local a = 5\nreturn 3 + a");
    }

    #[test]
    fn emit_arith_addvv_two_locals() {
        // `local a = 1; local b = 2; return a + b`:
        //   KSHORT 0 1; KSHORT 1 2; ADDVV 2 0 1; RET1 2 2.
        let insts = vec![
            ad(Opcode::Kshort, 0, 1),
            ad(Opcode::Kshort, 1, 2),
            abc(Opcode::Addvv, 2, 0, 1),
            ad(Opcode::Ret1, 2, 2),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "a", 0, 3), named_local(1, "b", 1, 3)],
            Vec::new(),
            Vec::new(),
            3,
        );
        assert_eq!(
            emit_module(&module).unwrap(),
            "local a = 1\nlocal b = 2\nreturn a + b"
        );
    }

    #[test]
    fn emit_arith_div_mod_mul_use_correct_symbols() {
        // One test covering the symbol mapping for the non-ADD
        // arithmetic ops via DIVVN/MODVN/MULVN. Stage 4 only emits
        // single binops; the symbol is what differs.
        let div_insts = vec![
            ad(Opcode::Kshort, 0, 10),
            abc(Opcode::Divvn, 1, 0, 0),
            ad(Opcode::Ret1, 1, 2),
        ];
        let div_mod = module_with(
            div_insts,
            vec![named_local(0, "a", 0, 2)],
            Vec::new(),
            vec![NumConst::Int(3)],
            2,
        );
        assert_eq!(emit_module(&div_mod).unwrap(), "local a = 10\nreturn a / 3");

        let mul_insts = vec![
            ad(Opcode::Kshort, 0, 10),
            abc(Opcode::Mulvn, 1, 0, 0),
            ad(Opcode::Ret1, 1, 2),
        ];
        let mul_mod = module_with(
            mul_insts,
            vec![named_local(0, "a", 0, 2)],
            Vec::new(),
            vec![NumConst::Int(3)],
            2,
        );
        assert_eq!(emit_module(&mul_mod).unwrap(), "local a = 10\nreturn a * 3");

        let mod_insts = vec![
            ad(Opcode::Kshort, 0, 10),
            abc(Opcode::Modvn, 1, 0, 0),
            ad(Opcode::Ret1, 1, 2),
        ];
        let mod_mod = module_with(
            mod_insts,
            vec![named_local(0, "a", 0, 2)],
            Vec::new(),
            vec![NumConst::Int(3)],
            2,
        );
        assert_eq!(emit_module(&mod_mod).unwrap(), "local a = 10\nreturn a % 3");
    }

    #[test]
    fn emit_arith_pow() {
        // `local a = 2; return a ^ 10`:
        //   KSHORT 0 2; KSHORT 1 10; POW 1 0 1; RET1 1 2.
        // POW has no VN/NV variants; the right operand is loaded into
        // a register first, then POW takes reg[B], reg[C].
        let insts = vec![
            ad(Opcode::Kshort, 0, 2),
            ad(Opcode::Kshort, 1, 10),
            abc(Opcode::Pow, 1, 0, 1),
            ad(Opcode::Ret1, 1, 2),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "a", 0, 3)],
            Vec::new(),
            Vec::new(),
            2,
        );
        assert_eq!(emit_module(&module).unwrap(), "local a = 2\nreturn a ^ 10");
    }

    #[test]
    fn emit_arith_cat_overwrites_source_slot() {
        // `return "hello" .. " world"`:
        //   KSTR 0 0; KSTR 1 1; CAT 0 0 1; RET1 0 2.
        // CAT writes its result into slot 0 — the SAME slot that held
        // the first KSTR operand. The walk must overwrite slot_exprs[0]
        // with the concat result before RET1 reads it.
        //
        // KSTR uses reverse-index lookup (operand d -> gc_consts[len-1-d]):
        //   operand 0 -> gc_consts[1] = "hello"
        //   operand 1 -> gc_consts[0] = " world"
        // so we store gc_consts in [world, hello] order on disk.
        let insts = vec![
            ad(Opcode::Kstr, 0, 0),
            ad(Opcode::Kstr, 1, 1),
            abc(Opcode::Cat, 0, 0, 1),
            ad(Opcode::Ret1, 0, 2),
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![
                GcConst::Str(b" world".to_vec()),
                GcConst::Str(b"hello".to_vec()),
            ],
            Vec::new(),
            2,
        );
        assert_eq!(
            emit_module(&module).unwrap(),
            "return \"hello\" .. \" world\""
        );
    }

    #[test]
    fn emit_arith_with_float_const_uses_lua_formatter() {
        // `local a = ...; return a / 0.1` — the constant operand
        // flows through format_lua_number. Use a value that would
        // round differently under Rust's `{}`.
        let insts = vec![
            ad(Opcode::Kshort, 0, 1),
            abc(Opcode::Divvn, 1, 0, 0),
            ad(Opcode::Ret1, 1, 2),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "a", 0, 2)],
            Vec::new(),
            vec![NumConst::Num(10.0 / 3.0)],
            2,
        );
        assert_eq!(
            emit_module(&module).unwrap(),
            "local a = 1\nreturn a / 3.3333333333333"
        );
    }

    #[test]
    fn emit_arithmetic_result_to_unread_slot_emits_nothing() {
        // ADDVN writes to slot 1, but RET0 follows (no RET1 reads
        // slot 1). The arithmetic still executes (no NotImplemented),
        // but the dead result produces no emitted line.
        let insts = vec![
            ad(Opcode::Kshort, 0, 5),
            abc(Opcode::Addvn, 1, 0, 0),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "a", 0, 2)],
            Vec::new(),
            vec![NumConst::Int(3)],
            2,
        );
        assert_eq!(emit_module(&module).unwrap(), "local a = 5");
    }

    #[test]
    fn emit_arithmetic_not_implemented_for_unsupported_opcode() {
        // GGET (global get) isn't in the walk's match arms; the
        // default case bails with NotImplemented.
        let insts = vec![
            ad(Opcode::Gget, 0, 0), // unsupported
            ad(Opcode::Ret1, 0, 2),
        ];
        let module = module_with(insts, Vec::new(), Vec::new(), Vec::new(), 1);
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for GGET, got {:?}",
            result
        );
    }

    #[test]
    fn emit_arithmetic_not_implemented_for_missing_operand_slot() {
        // ADDVV reads slots 0 and 1, but only slot 0 has a recorded
        // expression — slot 1 was never written by an instruction we
        // handle. The walk's lookup_operand bails.
        let insts = vec![
            ad(Opcode::Kshort, 0, 1),
            abc(Opcode::Addvv, 2, 0, 1), // slot 1 never populated
            ad(Opcode::Ret1, 2, 2),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "a", 0, 2)],
            Vec::new(),
            Vec::new(),
            3,
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for ADDVV with uninitialized slot 1, got {:?}",
            result
        );
    }

    #[test]
    fn emit_arithmetic_not_implemented_for_missing_num_const() {
        // ADDVN references num_const[0] but the proto has no
        // num_consts table. format_num_const bails with NotImplemented.
        let insts = vec![
            ad(Opcode::Kshort, 0, 5),
            abc(Opcode::Addvn, 1, 0, 0),
            ad(Opcode::Ret1, 1, 2),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "a", 0, 2)],
            Vec::new(),
            Vec::new(), // no num_consts
            2,
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for ADDVN with missing num_const, got {:?}",
            result
        );
    }

    #[test]
    fn emit_walk_regression_stage1_ret0_only() {
        // Stage 1 regression: a single RET0 still emits empty source
        // through the new walk.
        let module = module_with(
            vec![ad(Opcode::Ret0, 0, 1)],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            1,
        );
        assert_eq!(emit_module(&module).unwrap(), "");
    }

    #[test]
    fn emit_walk_regression_stage2_return_const() {
        // Stage 2 regression: transient KSHORT into r0, then RET1.
        let module = module_with(
            vec![ad(Opcode::Kshort, 0, 5), ad(Opcode::Ret1, 0, 2)],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            1,
        );
        assert_eq!(emit_module(&module).unwrap(), "return 5");
    }

    #[test]
    fn emit_walk_regression_no_return_after_load() {
        // A load with no following RET0/RET1: walk falls off the end
        // with saw_return=false -> NotImplemented.
        let module = module_with(
            vec![ad(Opcode::Kshort, 0, 5)],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            1,
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for chunk without return, got {:?}",
            result
        );
    }
}
