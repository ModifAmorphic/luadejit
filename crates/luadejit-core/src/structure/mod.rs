//! Structural recovery: turn a [`Proto`]'s [`Cfg`] into an AST.
//!
//! Stage 7's [`recover`] walks the CFG built by [`crate::cfg::Cfg::build`]
//! and produces a list of [`Stmt`] nodes that [`crate::emit`] can format
//! back into Lua source. The recovery handles two shapes:
//!
//! - **Linear code**: a chain of `Fallthrough` blocks ending in
//!   `Return`. Produces a flat list of statements. This is what
//!   Stages 1-6 produced via the walk-based emitter; the new pipeline
//!   emits identical source through the AST.
//! - **Single `if/then`**: an entry block ending in
//!   [`ConditionalBranch`](crate::cfg::Terminator::ConditionalBranch)
//!   (ISF or IST + JMP), with a then-body that either returns or
//!   falls through to a post-`if` merge block. The continuation after
//!   the `if` is also recovered (typically a final `return`).
//!
//! Everything else (if/else, loops, function calls, nested `if`,
//! compound conditions) bails with [`DecompilerError::NotImplemented`]
//! — Stage 8 and later pick those up.
//!
//! ## Slot → Expr tracking
//!
//! The walk-based Stages 1-6 stored `HashMap<u8, String>` (slot →
//! source-fragment string). The new pipeline stores
//! `HashMap<u8, Expr>` instead. The decision logic mirrors the old
//! walk-based emitter's slot-routing helper:
//!
//! - Named local at this instruction (`var_name_at` returns `Some`,
//!   `is_var_declaration_at` returns `true`) → `Expr::Var(name)`,
//!   emit `local name = expr`.
//! - Reassignment (slot is in scope but past its `scope_begin`) →
//!   `Expr::Var(name)`, emit `name = expr` (no `local`).
//! - Unnamed temp → store the computed `Expr`, emit no line.
//!
//! The slot map is threaded through `if/then` recovery unchanged.
//! Both branches agree on the variable *name* (it's the same local
//! in scope), so reading the slot after the merge surfaces the right
//! name. Value reconciliation across branches is phi-elimination
//! territory (Stage 8+); Stage 7 doesn't need it because the
//! supported fixtures don't introduce merge-time conflicts.

use std::collections::HashMap;

use crate::cfg::{BlockId, Cfg, InstructionId, Terminator};
use crate::ir::{GcConst, Instruction, NumConst, Opcode, Proto};
use crate::DecompilerError;

// ---- AST types -------------------------------------------------------

/// A Lua source-level statement.
#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    /// `local name = expr` (fresh declaration).
    LocalDecl { name: String, expr: Expr },
    /// `name = expr` (reassignment; no `local`).
    Assign { name: String, expr: Expr },
    /// `return expr` (Some) or implicit `return` (None — emits no
    /// line; the source chunk just ends).
    Return(Option<Expr>),
    /// `if cond then then_body end` (with an optional `else` body —
    /// Stage 7 always emits `else_body: None`; the variant is here
    /// so later stages don't have to retread emit's match arms).
    If {
        cond: Expr,
        then_body: Vec<Stmt>,
        else_body: Option<Vec<Stmt>>,
    },
}

/// A Lua expression. Stage 7's variants cover the values Stages 1-6
/// already round-tripped (`Int`, `Float`, `Str`, `Nil`, `True`,
/// `False`, `Var`, `BinOp`) plus the new `Global` (for GGET-loaded
/// globals) and `Not` (for the `if not x then` IST case).
///
/// `PartialEq` is derived for unit tests that compare recovered AST
/// nodes against expected shapes. `f64`'s `PartialEq` is fine here:
/// tests compare against literal floats written into [`Expr::Float`]
/// by the same code that builds them, so the bit patterns match.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// A named local or any value resolved back to its source name
    /// (via `var_name_at`). The payload is the variable's name.
    Var(String),
    /// A signed 32-bit integer literal (`KSHORT`, or `KNUM`-with-int).
    Int(i32),
    /// A floating-point literal (`KNUM`-with-num). Emitted through
    /// [`crate::number::format_lua_number`] at emit time.
    Float(f64),
    /// A string literal (`KSTR`). Raw bytes; the format does not
    /// guarantee UTF-8, so emit uses lossy conversion.
    Str(Vec<u8>),
    Nil,
    True,
    False,
    /// A binary operator application. `Cat` (`..`) chains via
    /// right-leaning nesting; the renderer doesn't parenthesize, so
    /// Lua's left-associativity has to match the bytecode's
    /// evaluation order (it does for the cases Stage 7 supports).
    BinOp {
        op: BinOpKind,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// Logical negation `not expr`. Stage 7 uses this solely for the
    /// `if not x then` shape, whose bytecode lowers to `IST` + `JMP`
    /// (the IST holds when `x` is truthy; the user's condition is
    /// the negation).
    Not(Box<Expr>),
    /// A global variable reference loaded via `GGET`. The payload is
    /// the global's name (resolved from the GC constant the `GGET`
    /// references). At the Lua source level a global is just a bare
    /// name, so emit prints the payload verbatim.
    Global(String),
}

/// The binary operator kind attached to [`Expr::BinOp`]. Stage 7
/// covers the seven Lua binops the arithmetic opcodes lower to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BinOpKind {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Concat,
}

// ---- Recovery -------------------------------------------------------

/// Recover source-level structure from a [`Proto`]'s [`Cfg`].
///
/// Walks the CFG starting at the entry block, threading a
/// `slot_exprs` map and producing [`Stmt`] nodes. Returns
/// [`DecompilerError::NotImplemented`] for any CFG shape Stage 7
/// doesn't model: if/else, compound conditions, loops, function
/// calls, tail calls, nested `if`, and any block whose terminator
/// isn't `Return`, `Fallthrough`, or a single `ConditionalBranch`.
pub fn recover(proto: &Proto, cfg: &Cfg) -> Result<Vec<Stmt>, DecompilerError> {
    if cfg.blocks.is_empty() {
        return Ok(Vec::new());
    }
    // Stage 7 supports at most one ConditionalBranch per function —
    // nested `if` and sequential `if`s both need Stage 8+ (else
    // branches, scope-aware recovery). Counting up front keeps the
    // walk simple: it doesn't have to track depth or sequence.
    let conditional_branches = cfg
        .blocks
        .iter()
        .filter(|b| matches!(b.terminator, Terminator::ConditionalBranch { .. }))
        .count();
    if conditional_branches > 1 {
        return Err(DecompilerError::NotImplemented);
    }
    // if/else codegen produces an unreachable dead-jump block (the
    // JMP after the then-body's RET1) plus an unreachable merge (its
    // target). Stage 7 if/then has no unreachable blocks; bailing
    // here keeps the walk from misreading an else-body as the
    // post-`if` continuation.
    if !all_blocks_reachable(cfg) {
        return Err(DecompilerError::NotImplemented);
    }
    let mut slot_exprs: HashMap<u8, Expr> = HashMap::new();
    let mut stmts = Vec::new();
    let mut ctx = RecoveryCtx {
        proto,
        cfg,
        slot_exprs: &mut slot_exprs,
    };
    recover_from(cfg.entry, false, &mut ctx, &mut stmts)?;
    Ok(stmts)
}

/// Whether every block in `cfg` is reachable from the entry via some
/// control-flow path. Computed by an iterative DFS over successor
/// edges. Used by [`recover`] to reject CFGs with dead blocks (the
/// structural signature of if/else codegen).
fn all_blocks_reachable(cfg: &Cfg) -> bool {
    let n = cfg.blocks.len();
    if n == 0 {
        return true;
    }
    let mut visited = vec![false; n];
    let mut stack: Vec<BlockId> = vec![cfg.entry];
    visited[cfg.entry.0 as usize] = true;
    while let Some(b) = stack.pop() {
        for (succ, _) in &cfg.blocks[b.0 as usize].succs {
            if !visited[succ.0 as usize] {
                visited[succ.0 as usize] = true;
                stack.push(*succ);
            }
        }
    }
    visited.iter().all(|v| *v)
}

/// Walk-time shared state. Borrows the proto + cfg immutably and the
/// slot map mutably for the duration of recovery. The lifetime is
/// kept internal to this module: callers go through [`recover`].
struct RecoveryCtx<'a> {
    proto: &'a Proto,
    cfg: &'a Cfg,
    slot_exprs: &'a mut HashMap<u8, Expr>,
}

/// Walk a chain of blocks starting at `start`, appending [`Stmt`]s
/// to `stmts`.
///
/// `stop_at_merge` controls how a [`Terminator::Fallthrough`] into a
/// block with multiple predecessors is treated:
///
/// - `false` (top-level walk / post-`if` continuation): follow into
///   the merge. This is how the post-`if` continuation block (which
///   has preds from both branches) gets processed.
/// - `true` (inside a then-body): stop at the merge. The
///   then-body's fallthrough into the merge is *not* part of the
///   then-body's source; the merge belongs to the outer walk.
///
/// Encountering a [`Terminator::Jump`] or [`Terminator::TailCall`]
/// (or any unrecognized terminator instruction) bails with
/// [`DecompilerError::NotImplemented`].
fn recover_from(
    start: BlockId,
    stop_at_merge: bool,
    ctx: &mut RecoveryCtx,
    stmts: &mut Vec<Stmt>,
) -> Result<(), DecompilerError> {
    let mut current = Some(start);
    while let Some(block_id) = current {
        let block = &ctx.cfg.blocks[block_id.0 as usize];
        match block.terminator {
            Terminator::Return => {
                // The RET* instruction is the block's last; every
                // earlier instruction is a regular walk step.
                let last_idx = block.insts.last().map(|id| id.0 as usize).unwrap();
                for inst_id in &block.insts {
                    let idx = inst_id.0 as usize;
                    if idx == last_idx {
                        process_return_inst(ctx.proto, ctx.slot_exprs, stmts, idx)?;
                        break;
                    }
                    process_inst(ctx, stmts, *inst_id)?;
                }
                break;
            }
            Terminator::Fallthrough(next) => {
                // No explicit terminator instruction — process every
                // instruction in the block, then advance.
                for inst_id in &block.insts {
                    process_inst(ctx, stmts, *inst_id)?;
                }
                if stop_at_merge && ctx.cfg.blocks[next.0 as usize].preds.len() > 1 {
                    // The then-body falls through into the post-`if`
                    // merge. Stop here — the merge belongs to the
                    // outer walk, not the then-body.
                    break;
                }
                current = Some(next);
            }
            Terminator::ConditionalBranch {
                condition,
                true_edge,
                false_edge,
            } => {
                let cond_idx = condition.0 as usize;
                // Walk every instruction strictly before the ISxx.
                // The ISxx itself and the JMP that follows it are
                // the terminator pair; they're consumed below.
                for inst_id in &block.insts {
                    if (inst_id.0 as usize) >= cond_idx {
                        break;
                    }
                    process_inst(ctx, stmts, *inst_id)?;
                }
                let cond_inst = &ctx.proto.insts[cond_idx];
                // ISF/IST test R[D] (A is unused for these two — see
                // format doc §5: `ISF | if !D<VAR> | if falsy(R[D])
                // then jump`). The other ISxx variants (ISLT, ISEQV,
                // …) have different operand layouts; they're Stage 9
                // and rejected below.
                let cond_slot = cond_inst.d() as u8;
                let cond_expr = ctx
                    .slot_exprs
                    .get(&cond_slot)
                    .cloned()
                    .ok_or(DecompilerError::NotImplemented)?;
                // ISxx semantics:
                // - ISF: tests "is slot falsy?" If true, JMP (skip
                //   then-body). The user's `if` condition is the
                //   inverse — "truthy" — which in Lua is just the
                //   bare expression, so no wrapping.
                // - IST: tests "is slot truthy?" If true, JMP (skip
                //   then-body). The user's condition is the negation
                //   — "falsy" — which surfaces as `not <expr>`.
                // Any other ISxx (ISLT, ISEQV, …) is a compound
                // condition (Stage 9).
                let if_cond = match cond_inst.op {
                    Opcode::Isf => cond_expr,
                    Opcode::Ist => Expr::Not(Box::new(cond_expr)),
                    _ => return Err(DecompilerError::NotImplemented),
                };
                let mut then_body: Vec<Stmt> = Vec::new();
                recover_from(false_edge, true, ctx, &mut then_body)?;
                stmts.push(Stmt::If {
                    cond: if_cond,
                    then_body,
                    else_body: None,
                });
                current = Some(true_edge);
            }
            Terminator::Jump(_) | Terminator::TailCall(_) => {
                // Standalone JMP (loops, gotos) and tail calls aren't
                // Stage 7 territory.
                return Err(DecompilerError::NotImplemented);
            }
        }
    }
    Ok(())
}

/// Process the trailing RET* instruction at absolute index `idx`.
///
/// `RET0` is the implicit end-of-chunk return (no Stmt emitted).
/// `RET1 A D` with `D == 2` is the single-value return convention;
/// the returned expression comes from `slot_exprs[A]`. Anything else
/// (other RET variants, `RET1` with `D != 2`, or a missing slot
/// expression) bails with [`DecompilerError::NotImplemented`].
fn process_return_inst(
    proto: &Proto,
    slot_exprs: &HashMap<u8, Expr>,
    stmts: &mut Vec<Stmt>,
    idx: usize,
) -> Result<(), DecompilerError> {
    let inst = &proto.insts[idx];
    match inst.op {
        Opcode::Ret0 => Ok(()),
        Opcode::Ret1 => {
            if inst.d() != 2 {
                return Err(DecompilerError::NotImplemented);
            }
            let expr = slot_exprs
                .get(&inst.a)
                .cloned()
                .ok_or(DecompilerError::NotImplemented)?;
            stmts.push(Stmt::Return(Some(expr)));
            Ok(())
        }
        // RET/RETM (multi-value returns) aren't Stage 7 territory.
        _ => Err(DecompilerError::NotImplemented),
    }
}

/// Process one non-terminator instruction: build its [`Expr`], then
/// route the destination slot through [`assign_slot_ast`].
///
/// `inst_id` is the absolute instruction index (i.e. into
/// [`Proto::insts`]); the real-instruction index used for debug-info
/// lookups is derived internally as `idx - 1` (excluding the
/// synthetic FUNC* header at slot 0).
fn process_inst(
    ctx: &mut RecoveryCtx,
    stmts: &mut Vec<Stmt>,
    inst_id: InstructionId,
) -> Result<(), DecompilerError> {
    let idx = inst_id.0 as usize;
    let inst = &ctx.proto.insts[idx];
    // var_name_at / is_var_declaration_at index real instructions
    // from 0 (excluding the FUNC* header at in-memory slot 0).
    let real_idx = idx - 1;
    match inst.op {
        Opcode::Kshort | Opcode::Knum | Opcode::Kstr | Opcode::Kpri => {
            let expr = build_const_expr(ctx.proto, inst)?;
            assign_slot_ast(ctx.proto, ctx.slot_exprs, stmts, inst.a, expr, real_idx);
        }
        Opcode::Gget => {
            // GGET A D loads the global named by gc_consts[reverse(D)]
            // into slot A. For Stage 7 the global must be a string
            // constant (the common case); non-string GC constants
            // loaded via GGET aren't part of any current stage.
            let gc = ctx.proto.gc_const_for_operand(inst.d())?;
            match gc {
                GcConst::Str(bytes) => {
                    let name = String::from_utf8_lossy(bytes).into_owned();
                    assign_slot_ast(
                        ctx.proto,
                        ctx.slot_exprs,
                        stmts,
                        inst.a,
                        Expr::Global(name),
                        real_idx,
                    );
                }
                _ => return Err(DecompilerError::NotImplemented),
            }
        }
        Opcode::Addvn
        | Opcode::Subvn
        | Opcode::Mulvn
        | Opcode::Divvn
        | Opcode::Modvn
        | Opcode::Addnv
        | Opcode::Subnv
        | Opcode::Mulnv
        | Opcode::Divnv
        | Opcode::Modnv
        | Opcode::Addvv
        | Opcode::Subvv
        | Opcode::Mulvv
        | Opcode::Divvv
        | Opcode::Modvv => {
            let expr = build_arith_expr(ctx.proto, ctx.slot_exprs, inst)?;
            assign_slot_ast(ctx.proto, ctx.slot_exprs, stmts, inst.a, expr, real_idx);
        }
        Opcode::Pow => {
            // POW has no VN/NV variants — both operands are
            // registers. Kept as its own arm because the operator
            // is fixed (`^`) rather than opcode-derived.
            let left = lookup_operand(ctx.slot_exprs, inst.b())?;
            let right = lookup_operand(ctx.slot_exprs, inst.c)?;
            let expr = Expr::BinOp {
                op: BinOpKind::Pow,
                left: Box::new(left),
                right: Box::new(right),
            };
            assign_slot_ast(ctx.proto, ctx.slot_exprs, stmts, inst.a, expr, real_idx);
        }
        Opcode::Cat => {
            // CAT concatenates regs[B..=C] into A (a range, not just
            // two operands). LuaJIT lowers `a .. b .. c` to a single
            // CAT covering all three slots. We rebuild it as a
            // right-leaning BinOp chain (`a .. (b .. c)`); Lua's `..`
            // is right-associative so this matches the source shape
            // for the cases Stage 7 handles.
            let mut parts: Vec<Expr> = Vec::new();
            for slot in inst.b()..=inst.c {
                parts.push(lookup_operand(ctx.slot_exprs, slot)?);
            }
            let expr = parts
                .into_iter()
                .reduce(|acc, part| Expr::BinOp {
                    op: BinOpKind::Concat,
                    left: Box::new(acc),
                    right: Box::new(part),
                })
                .ok_or(DecompilerError::NotImplemented)?;
            assign_slot_ast(ctx.proto, ctx.slot_exprs, stmts, inst.a, expr, real_idx);
        }
        Opcode::Mov => {
            // MOV A D copies slot D's expression into slot A. The
            // source must have a recorded expression (otherwise the
            // shape is one we don't model). MOV is AD format, so the
            // source register is D; registers are 8-bit.
            let source_slot = inst.d() as u8;
            let source_expr = ctx
                .slot_exprs
                .get(&source_slot)
                .cloned()
                .ok_or(DecompilerError::NotImplemented)?;
            assign_slot_ast(
                ctx.proto,
                ctx.slot_exprs,
                stmts,
                inst.a,
                source_expr,
                real_idx,
            );
        }
        _ => return Err(DecompilerError::NotImplemented),
    }
    Ok(())
}

/// Resolve a register operand to its recorded [`Expr`]. A missing
/// slot means we've hit an instruction whose source we can't
/// reconstruct from what we've seen so far (an uninitialized
/// register, or a slot populated by an opcode this stage doesn't
/// handle) — bail with [`DecompilerError::NotImplemented`].
fn lookup_operand(slot_exprs: &HashMap<u8, Expr>, slot: u8) -> Result<Expr, DecompilerError> {
    slot_exprs
        .get(&slot)
        .cloned()
        .ok_or(DecompilerError::NotImplemented)
}

/// Record a slot's expression. AST analog of the old walk-based
/// `assign_slot` helper (the emit module no longer has this; the
/// logic moved here when the pipeline gained an AST stage).
///
/// If the proto's debug section names the slot as a live local at
/// `inst_index`, emit either a declaration or a reassignment:
///
/// - **Declaration** ([`Proto::is_var_declaration_at`] returns true
///   — the instruction index equals the variable's `scope_begin`):
///   push [`Stmt::LocalDecl`] (`local name = expr`).
/// - **Reassignment** (the slot has a named local in scope, but the
///   instruction is past its declaration point): push
///   [`Stmt::Assign`] (`name = expr`, no `local`).
///
/// Both paths store [`Expr::Var`] `(name)` under the slot — a later
/// `return <name>` references the local rather than the original
/// expression. When the slot has no named local, the expression is
/// recorded as an unnamed temporary that only surfaces when a later
/// instruction (e.g. `RET1`) reads the slot.
fn assign_slot_ast(
    proto: &Proto,
    slot_exprs: &mut HashMap<u8, Expr>,
    stmts: &mut Vec<Stmt>,
    slot: u8,
    expr: Expr,
    inst_index: usize,
) {
    if let Some(name) = proto.var_name_at(slot, inst_index) {
        if proto.is_var_declaration_at(slot, inst_index) {
            stmts.push(Stmt::LocalDecl {
                name: name.to_string(),
                expr,
            });
        } else {
            stmts.push(Stmt::Assign {
                name: name.to_string(),
                expr,
            });
        }
        slot_exprs.insert(slot, Expr::Var(name.to_string()));
    } else {
        slot_exprs.insert(slot, expr);
    }
}

/// Build the source [`Expr`] produced by a constant-load
/// instruction (`KSHORT`/`KNUM`/`KSTR`/`KPRI`). Returns
/// [`DecompilerError::NotImplemented`] for opcodes/load shapes this
/// stage doesn't handle.
fn build_const_expr(proto: &Proto, load: &Instruction) -> Result<Expr, DecompilerError> {
    match load.op {
        Opcode::Kshort => {
            // D is a signed 16-bit immediate (the value itself, not
            // an index). Reinterpret the u16 bits as i16, then widen
            // to i32 preserving sign.
            let val = load.d() as i16 as i32;
            Ok(Expr::Int(val))
        }
        Opcode::Knum => {
            // D is a forward index into num_consts.
            let idx = load.d() as usize;
            build_num_const_expr(proto, idx)
        }
        Opcode::Kstr => {
            // D is a reverse index into gc_consts — use the helper to
            // avoid the classic forward-vs-reverse indexing bug.
            let gc = proto.gc_const_for_operand(load.d())?;
            match gc {
                GcConst::Str(bytes) => Ok(Expr::Str(bytes.clone())),
                _ => Err(DecompilerError::NotImplemented),
            }
        }
        Opcode::Kpri => match load.d() {
            0 => Ok(Expr::Nil),
            1 => Ok(Expr::False),
            2 => Ok(Expr::True),
            _ => Err(DecompilerError::NotImplemented),
        },
        _ => Err(DecompilerError::NotImplemented),
    }
}

/// Build the source [`Expr`] for a number constant by forward index
/// into [`Proto::num_consts`]. Shared by `KNUM` loading and the
/// arithmetic `*VN` / `*NV` operand paths. Returns
/// [`DecompilerError::NotImplemented`] if `idx` is out of range —
/// malformed bytecode belongs to the parser, so an out-of-range
/// index here means we're past the validity boundary.
fn build_num_const_expr(proto: &Proto, idx: usize) -> Result<Expr, DecompilerError> {
    let nc = proto
        .num_consts
        .get(idx)
        .ok_or(DecompilerError::NotImplemented)?;
    Ok(match nc {
        NumConst::Int(i) => Expr::Int(*i),
        NumConst::Num(f) => Expr::Float(*f),
    })
}

/// Build a binary-arithmetic [`Expr`] for an arithmetic instruction.
/// Routes to the correct operand resolution based on the opcode's
/// variant (`*VN`, `*NV`, `*VV`). `POW` and `CAT` are handled
/// inline in [`process_inst`] since they don't fit the VN/NV/VV
/// pattern.
fn build_arith_expr(
    proto: &Proto,
    slot_exprs: &HashMap<u8, Expr>,
    inst: &Instruction,
) -> Result<Expr, DecompilerError> {
    let op = binop_kind_for_arith(inst.op)?;
    match inst.op {
        // VN: left = reg[B], right = num_const[C].
        Opcode::Addvn | Opcode::Subvn | Opcode::Mulvn | Opcode::Divvn | Opcode::Modvn => {
            let left = lookup_operand(slot_exprs, inst.b())?;
            let right = build_num_const_expr(proto, inst.c as usize)?;
            Ok(Expr::BinOp {
                op,
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        // NV: left = num_const[B], right = reg[C].
        Opcode::Addnv | Opcode::Subnv | Opcode::Mulnv | Opcode::Divnv | Opcode::Modnv => {
            let left = build_num_const_expr(proto, inst.b() as usize)?;
            let right = lookup_operand(slot_exprs, inst.c)?;
            Ok(Expr::BinOp {
                op,
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        // VV: left = reg[B], right = reg[C].
        Opcode::Addvv | Opcode::Subvv | Opcode::Mulvv | Opcode::Divvv | Opcode::Modvv => {
            let left = lookup_operand(slot_exprs, inst.b())?;
            let right = lookup_operand(slot_exprs, inst.c)?;
            Ok(Expr::BinOp {
                op,
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        // Reaching here is a logic bug — `binop_kind_for_arith`
        // already rejected the opcode.
        _ => unreachable!(
            "build_arith_expr called on non-arithmetic opcode {:?}",
            inst.op
        ),
    }
}

/// Map an arithmetic opcode to its [`BinOpKind`]. Covers the
/// `ADD`/`SUB`/`MUL`/`DIV`/`MOD` variants (VN/NV/VV); `POW` and
/// `CAT` are handled inline in [`process_inst`] since they don't
/// share the VN/NV/VV pattern.
fn binop_kind_for_arith(op: Opcode) -> Result<BinOpKind, DecompilerError> {
    match op {
        Opcode::Addvn | Opcode::Addnv | Opcode::Addvv => Ok(BinOpKind::Add),
        Opcode::Subvn | Opcode::Subnv | Opcode::Subvv => Ok(BinOpKind::Sub),
        Opcode::Mulvn | Opcode::Mulnv | Opcode::Mulvv => Ok(BinOpKind::Mul),
        Opcode::Divvn | Opcode::Divnv | Opcode::Divvv => Ok(BinOpKind::Div),
        Opcode::Modvn | Opcode::Modnv | Opcode::Modvv => Ok(BinOpKind::Mod),
        _ => Err(DecompilerError::NotImplemented),
    }
}

#[cfg(test)]
mod tests {
    //! These tests exercise the new AST pipeline end-to-end (parse
    //! → CFG → [`recover`] → [`crate::emit::emit_module`]) so they
    //! double as the Stage 1-6 regression suite: every shape the
    //! walk-based emitter handled must produce identical source
    //! through the AST. The fixtures mirror the bytecode `luajit -bl`
    //! produces for the corresponding source.
    //!
    //! The pipeline-level tests live here (rather than in
    //! `emit.rs`) because the recovery logic they exercise — slot
    //! tracking, declaration vs. reassignment, operand resolution —
    //! is implemented in this module. `emit.rs` keeps a separate
    //! suite for its formatter (statement/expression → string).

    use super::*;
    use crate::cfg::Cfg;
    use crate::emit::emit_module;
    use crate::ir::{
        DebugInfo, GcConst, Instruction, Module, ModuleHeader, NumConst, Opcode, Proto, UpvalDesc,
        VarInfo, VarKind,
    };

    // ---- shared test builders ------------------------------------------

    /// Build a `VarInfo` record naming `slot` as `name` with a scope
    /// covering real instructions `begin..=end` (inclusive).
    fn named_local(slot: u8, name: &str, begin: u32, end: u32) -> VarInfo {
        VarInfo {
            kind: VarKind::Name,
            name: Some(name.to_string()),
            is_parameter: false,
            slot,
            scope_begin: begin,
            scope_end: end,
        }
    }

    /// Build a minimal Module whose main proto has the given real
    /// instructions, debug var_info records, and constant tables.
    fn module_with(
        real_insts: Vec<Instruction>,
        var_info: Vec<VarInfo>,
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

    /// Run the full decompile pipeline on a module and return the
    /// source. Used by the regression tests so they read like the
    /// integration tests (`decompile_fixture`).
    fn emit(module: &Module) -> String {
        emit_module(module).expect("emit should succeed for supported shape")
    }

    // ====================================================================
    // Stage 1: RET0-only chunk → empty source
    // ====================================================================

    #[test]
    fn emit_ret0_only_returns_empty_source() {
        let module = module_with(
            vec![ad(Opcode::Ret0, 0, 1)],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            1,
        );
        assert_eq!(emit(&module), "");
    }

    // ====================================================================
    // Stage 2: `return <const>`
    // ====================================================================

    /// Build a Stage 2 module: a constant `load` into slot 0 (no
    /// var_info) followed by `RET1 0 2`. Caller supplies the load
    /// instruction and any tables it resolves against.
    fn return_const_module(
        load: Instruction,
        gc_consts: Vec<GcConst>,
        num_consts: Vec<NumConst>,
    ) -> Module {
        module_with(
            vec![load, ad(Opcode::Ret1, 0, 2)],
            Vec::new(),
            gc_consts,
            num_consts,
            1,
        )
    }

    #[test]
    fn emit_return_const_kshort() {
        let module = return_const_module(ad(Opcode::Kshort, 0, 5), Vec::new(), Vec::new());
        assert_eq!(emit(&module), "return 5");
    }

    #[test]
    fn emit_return_const_kshort_negative() {
        // -7 as i16 is 0xFFF9 = 65529 as u16.
        let module = return_const_module(ad(Opcode::Kshort, 0, 0xFFF9), Vec::new(), Vec::new());
        assert_eq!(emit(&module), "return -7");
    }

    #[test]
    fn emit_return_const_kpri_nil() {
        let module = return_const_module(ad(Opcode::Kpri, 0, 0), Vec::new(), Vec::new());
        assert_eq!(emit(&module), "return nil");
    }

    #[test]
    fn emit_return_const_kpri_true() {
        let module = return_const_module(ad(Opcode::Kpri, 0, 2), Vec::new(), Vec::new());
        assert_eq!(emit(&module), "return true");
    }

    #[test]
    fn emit_return_const_kpri_false() {
        let module = return_const_module(ad(Opcode::Kpri, 0, 1), Vec::new(), Vec::new());
        assert_eq!(emit(&module), "return false");
    }

    #[test]
    // The fixture value 3.14 trips clippy::approx_constant (PI). We
    // intentionally use 3.14 here so the unit test mirrors the
    // `return_float` integration fixture exactly.
    #[allow(clippy::approx_constant)]
    fn emit_return_const_knum() {
        // KNUM's D is a FORWARD index.
        let module = return_const_module(
            ad(Opcode::Knum, 0, 0),
            Vec::new(),
            vec![NumConst::Num(3.14)],
        );
        assert_eq!(emit(&module), "return 3.14");
    }

    #[test]
    fn emit_return_const_knum_int_const() {
        // A boxed-int num const should format as an integer.
        let module =
            return_const_module(ad(Opcode::Knum, 0, 0), Vec::new(), vec![NumConst::Int(42)]);
        assert_eq!(emit(&module), "return 42");
    }

    #[test]
    fn emit_return_const_kstr() {
        // KSTR's D is a REVERSE index — operand 0 → gc_consts[0]
        // when len == 1.
        let module = return_const_module(
            ad(Opcode::Kstr, 0, 0),
            vec![GcConst::Str(b"foo".to_vec())],
            Vec::new(),
        );
        assert_eq!(emit(&module), "return \"foo\"");
    }

    #[test]
    fn emit_return_const_not_implemented_for_non_zero_ret_a() {
        // RET1 1 2: A != 0 — not the single-const-return shape.
        let mut module = return_const_module(ad(Opcode::Kshort, 0, 5), Vec::new(), Vec::new());
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
        let mut module = return_const_module(ad(Opcode::Kshort, 0, 5), Vec::new(), Vec::new());
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
        // KSHORT 1 5: load targets r1, but RET1 reads r0 — no
        // recorded expression for the RET1 slot.
        let mut module = return_const_module(ad(Opcode::Kshort, 0, 5), Vec::new(), Vec::new());
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
        let module = return_const_module(ad(Opcode::Mov, 0, 0), Vec::new(), Vec::new());
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for unsupported load op, got {:?}",
            result
        );
    }

    #[test]
    fn emit_returns_not_implemented_for_other_inputs() {
        // A module whose main proto has a non-RET0 instruction with
        // no recorded expression for RET1's slot.
        let mut module = module_with(
            vec![ad(Opcode::Ret0, 0, 1)],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            1,
        );
        module.protos[0].insts[1] = ad(Opcode::Ret1, 0, 2);
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented, got {:?}",
            result
        );
    }

    // ====================================================================
    // Stage 3: `local x = <const>` declarations
    // ====================================================================

    /// Build a Stage 3 module: a constant `load` into slot 0 named
    /// `name` (via var_info) followed by `ret`.
    fn local_module(
        load: Instruction,
        ret: Instruction,
        name: &str,
        gc_consts: Vec<GcConst>,
        num_consts: Vec<NumConst>,
    ) -> Module {
        module_with(
            vec![load, ret],
            vec![named_local(0, name, 0, 1)],
            gc_consts,
            num_consts,
            1,
        )
    }

    #[test]
    fn emit_local_int() {
        let module = local_module(
            ad(Opcode::Kshort, 0, 5),
            ad(Opcode::Ret1, 0, 2),
            "x",
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(emit(&module), "local x = 5\nreturn x");
    }

    #[test]
    fn emit_local_no_return() {
        // `local x = 5` with implicit return: KSHORT 0 5; RET0 0 1.
        let module = module_with(
            vec![ad(Opcode::Kshort, 0, 5), ad(Opcode::Ret0, 0, 1)],
            vec![named_local(0, "x", 0, 1)],
            Vec::new(),
            Vec::new(),
            1,
        );
        assert_eq!(emit(&module), "local x = 5");
    }

    #[test]
    fn emit_local_str() {
        let module = local_module(
            ad(Opcode::Kstr, 0, 0),
            ad(Opcode::Ret1, 0, 2),
            "x",
            vec![GcConst::Str(b"foo".to_vec())],
            Vec::new(),
        );
        assert_eq!(emit(&module), "local x = \"foo\"\nreturn x");
    }

    #[test]
    fn emit_local_with_ret1_wrong_slot() {
        // var_info names slot 0, but RET1 reads slot 1.
        let module = local_module(
            ad(Opcode::Kshort, 0, 5),
            Instruction {
                op: Opcode::Ret1,
                a: 1,
                b_or_d: 2,
                c: 0,
            },
            "x",
            Vec::new(),
            Vec::new(),
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for RET1 reading wrong slot, got {:?}",
            result
        );
    }

    #[test]
    fn emit_local_with_ret1_wrong_d() {
        let module = local_module(
            ad(Opcode::Kshort, 0, 5),
            Instruction {
                op: Opcode::Ret1,
                a: 0,
                b_or_d: 3,
                c: 0,
            },
            "x",
            Vec::new(),
            Vec::new(),
        );
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
        // still take the `return <const>` path, NOT be mistaken for
        // a local declaration.
        let module = return_const_module(ad(Opcode::Kshort, 0, 5), Vec::new(), Vec::new());
        assert_eq!(emit(&module), "return 5");
    }

    // ====================================================================
    // Stage 4: arithmetic expressions
    // ====================================================================

    #[test]
    fn emit_arith_addvn() {
        // `local a = 5; return a + 3`:
        //   KSHORT 0 5; ADDVN 1 0 0; RET1 1 2.
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
        assert_eq!(emit(&module), "local a = 5\nreturn a + 3");
    }

    #[test]
    fn emit_arith_addnv() {
        // `local a = 5; return 3 + a`:
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
        assert_eq!(emit(&module), "local a = 5\nreturn 3 + a");
    }

    #[test]
    fn emit_arith_addvv_two_locals() {
        // `local a = 1; local b = 2; return a + b`:
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
        assert_eq!(emit(&module), "local a = 1\nlocal b = 2\nreturn a + b");
    }

    #[test]
    fn emit_arith_div_mod_mul_use_correct_symbols() {
        let div_mod = module_with(
            vec![
                ad(Opcode::Kshort, 0, 10),
                abc(Opcode::Divvn, 1, 0, 0),
                ad(Opcode::Ret1, 1, 2),
            ],
            vec![named_local(0, "a", 0, 2)],
            Vec::new(),
            vec![NumConst::Int(3)],
            2,
        );
        assert_eq!(emit(&div_mod), "local a = 10\nreturn a / 3");

        let mul_mod = module_with(
            vec![
                ad(Opcode::Kshort, 0, 10),
                abc(Opcode::Mulvn, 1, 0, 0),
                ad(Opcode::Ret1, 1, 2),
            ],
            vec![named_local(0, "a", 0, 2)],
            Vec::new(),
            vec![NumConst::Int(3)],
            2,
        );
        assert_eq!(emit(&mul_mod), "local a = 10\nreturn a * 3");

        let mod_mod = module_with(
            vec![
                ad(Opcode::Kshort, 0, 10),
                abc(Opcode::Modvn, 1, 0, 0),
                ad(Opcode::Ret1, 1, 2),
            ],
            vec![named_local(0, "a", 0, 2)],
            Vec::new(),
            vec![NumConst::Int(3)],
            2,
        );
        assert_eq!(emit(&mod_mod), "local a = 10\nreturn a % 3");
    }

    #[test]
    fn emit_arith_pow() {
        // `local a = 2; return a ^ 10`:
        //   KSHORT 0 2; KSHORT 1 10; POW 1 0 1; RET1 1 2.
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
        assert_eq!(emit(&module), "local a = 2\nreturn a ^ 10");
    }

    #[test]
    fn emit_arith_cat_overwrites_source_slot() {
        // `return "hello" .. " world"`:
        //   KSTR 0 0; KSTR 1 1; CAT 0 0 1; RET1 0 2.
        // KSTR reverse-index: operand 0 → gc_consts[1], operand 1 → gc_consts[0].
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
        assert_eq!(emit(&module), "return \"hello\" .. \" world\"");
    }

    #[test]
    fn emit_arith_with_float_const_uses_lua_formatter() {
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
        assert_eq!(emit(&module), "local a = 1\nreturn a / 3.3333333333333");
    }

    #[test]
    fn emit_arithmetic_result_to_unread_slot_emits_nothing() {
        // ADDVN writes to slot 1, but RET0 follows. The arithmetic
        // executes but the dead result produces no emitted line.
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
        assert_eq!(emit(&module), "local a = 5");
    }

    #[test]
    fn emit_not_implemented_for_unsupported_opcode() {
        // TGETV (table get) isn't in the recovery's match arms;
        // the default case bails with NotImplemented.
        let insts = vec![ad(Opcode::Tgetv, 0, 0), ad(Opcode::Ret1, 0, 2)];
        // ^-- Tgetv is ABC, but `ad` only sets D; that's fine for
        // testing NotImplemented routing — the opcode dispatch
        // bails before operand interpretation.
        let module = module_with(insts, Vec::new(), Vec::new(), Vec::new(), 1);
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for TGETV, got {:?}",
            result
        );
    }

    #[test]
    fn emit_arithmetic_not_implemented_for_missing_operand_slot() {
        // ADDVV reads slots 0 and 1, but only slot 0 has a recorded
        // expression — slot 1 was never written by a handled opcode.
        let insts = vec![
            ad(Opcode::Kshort, 0, 1),
            abc(Opcode::Addvv, 2, 0, 1),
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
        let insts = vec![
            ad(Opcode::Kshort, 0, 5),
            abc(Opcode::Addvn, 1, 0, 0),
            ad(Opcode::Ret1, 1, 2),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "a", 0, 2)],
            Vec::new(),
            Vec::new(),
            2,
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for ADDVN with missing num_const, got {:?}",
            result
        );
    }

    // ====================================================================
    // Stage 5: named-local arithmetic + MOV
    // ====================================================================

    #[test]
    fn emit_named_local_arithmetic() {
        // `local a = 1; local b = a + 2; return b`.
        let insts = vec![
            ad(Opcode::Kshort, 0, 1),
            abc(Opcode::Addvn, 1, 0, 0),
            ad(Opcode::Ret1, 1, 2),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "a", 0, 2), named_local(1, "b", 1, 2)],
            Vec::new(),
            vec![NumConst::Int(2)],
            2,
        );
        assert_eq!(emit(&module), "local a = 1\nlocal b = a + 2\nreturn b");
    }

    #[test]
    fn emit_mov_to_named_local() {
        // `local a = 1; local b = a; return b`.
        let insts = vec![
            ad(Opcode::Kshort, 0, 1),
            ad(Opcode::Mov, 1, 0),
            ad(Opcode::Ret1, 1, 2),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "a", 0, 2), named_local(1, "b", 1, 2)],
            Vec::new(),
            Vec::new(),
            2,
        );
        assert_eq!(emit(&module), "local a = 1\nlocal b = a\nreturn b");
    }

    #[test]
    fn emit_mov_to_temporary() {
        // Slot 1 has NO var_info entry, so the MOV stores the source
        // expression as an unnamed temporary; no `local` line.
        let insts = vec![
            ad(Opcode::Kshort, 0, 5),
            ad(Opcode::Mov, 1, 0),
            ad(Opcode::Ret1, 1, 2),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "a", 0, 2)],
            Vec::new(),
            Vec::new(),
            2,
        );
        assert_eq!(emit(&module), "local a = 5\nreturn a");
    }

    #[test]
    fn emit_mov_not_implemented_for_uninitialized_source() {
        let insts = vec![ad(Opcode::Mov, 1, 0), ad(Opcode::Ret1, 1, 2)];
        let module = module_with(insts, Vec::new(), Vec::new(), Vec::new(), 2);
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for MOV with uninitialized source, got {:?}",
            result
        );
    }

    #[test]
    fn emit_arith_unnamed_temporary_preserved() {
        // `local a = 1; local b = 2; return a + b` — ADDVV result
        // lands in slot 2, which has NO var_info entry.
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
        assert_eq!(emit(&module), "local a = 1\nlocal b = 2\nreturn a + b");
    }

    #[test]
    fn emit_walk_regression_stage1_ret0_only() {
        let module = module_with(
            vec![ad(Opcode::Ret0, 0, 1)],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            1,
        );
        assert_eq!(emit(&module), "");
    }

    #[test]
    fn emit_walk_regression_stage2_return_const() {
        let module = module_with(
            vec![ad(Opcode::Kshort, 0, 5), ad(Opcode::Ret1, 0, 2)],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            1,
        );
        assert_eq!(emit(&module), "return 5");
    }

    #[test]
    fn emit_walk_regression_no_return_after_load() {
        // A load with no following RET0/RET1: the proto's only block
        // ends with a non-RET instruction. CFG classifies it as
        // Return (no successor) with the load as the last
        // instruction; process_return_inst bails because the last
        // instruction isn't a RET*.
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

    // ====================================================================
    // Stage 6: local reassignment
    // ====================================================================

    #[test]
    fn emit_reassignment_omits_local_keyword() {
        // `local a = 1; a = 2; return a`:
        let insts = vec![
            ad(Opcode::Kshort, 0, 1),
            ad(Opcode::Kshort, 0, 2),
            ad(Opcode::Ret1, 0, 2),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "a", 0, 2)],
            Vec::new(),
            Vec::new(),
            1,
        );
        assert_eq!(emit(&module), "local a = 1\na = 2\nreturn a");
    }

    #[test]
    fn emit_reassignment_with_arith() {
        // `local a = 1; a = a + 1; return a`:
        let insts = vec![
            ad(Opcode::Kshort, 0, 1),
            abc(Opcode::Addvn, 0, 0, 0),
            ad(Opcode::Ret1, 0, 2),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "a", 0, 2)],
            Vec::new(),
            vec![NumConst::Int(1)],
            1,
        );
        assert_eq!(emit(&module), "local a = 1\na = a + 1\nreturn a");
    }

    // ====================================================================
    // Stage 7: structural recovery (if/then) — direct AST tests
    // ====================================================================

    /// Helper: build a Stage-7 if/then module mirroring the bytecode
    /// `luajit -bl` produces for `if x then <body> end` shapes.
    /// Caller supplies the body's instructions and the proto's
    /// gc_consts (so the GGET operand resolves).
    fn if_then_module(body_insts: Vec<Instruction>, gc_consts: Vec<GcConst>) -> Module {
        if_then_module_with_test_op(Opcode::Isf, body_insts, gc_consts)
    }

    /// Variant of `if_then_module` that lets the caller swap in a
    /// different ISxx opcode (default ISF). Used to exercise IST
    /// and the compound-condition NotImplemented path.
    fn if_then_module_with_test_op(
        test_op: Opcode,
        body_insts: Vec<Instruction>,
        gc_consts: Vec<GcConst>,
    ) -> Module {
        // Layout (abs idx):
        //   0: FUNC* (synthetic header)
        //   1: GGET 0 0     (entry block)
        //   2: <test_op> 0
        //   3: JMP => idx_of_RET0
        //   4..(4+body_len-1): body_insts
        //   4+body_len: RET0 0 1   (the merge / implicit return)
        let body_len = body_insts.len();
        let ret0_idx = 1 + 3 + body_len; // abs idx of the RET0
                                         // JMP formula (see cfg::Cfg docs): target = jmp_idx + 1 + j,
                                         // so j = target - (jmp_idx + 1) = ret0_idx - 4. D = 0x8000 + j.
        let jmp_d = 0x8000u16 + (ret0_idx as u16 - 4);
        let mut real_insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(test_op, 0, 0),
            ad(Opcode::Jmp, 1, jmp_d),
        ];
        real_insts.extend(body_insts);
        real_insts.push(ad(Opcode::Ret0, 0, 1));
        module_with(real_insts, Vec::new(), gc_consts, Vec::new(), 2)
    }

    /// `if x then return 1 end` — the canonical Stage 7 fixture.
    /// Bytecode: GGET 0 0; ISF 0; JMP => 0006; KSHORT 0 1; RET1 0 2; RET0 0 1.
    #[test]
    fn recover_if_then_return() {
        let module = if_then_module(
            vec![ad(Opcode::Kshort, 0, 1), ad(Opcode::Ret1, 0, 2)],
            vec![GcConst::Str(b"x".to_vec())],
        );
        assert_eq!(emit(&module), "if x then\n    return 1\nend");
    }

    /// Same fixture recovered directly to AST (not via emit_module).
    /// Verifies the Stmt tree shape rather than the formatted string.
    /// Implicit `RET0` produces no Stmt — it's just the chunk ending
    /// — so the AST is the single `If` node.
    #[test]
    fn recover_if_then_return_ast_shape() {
        let module = if_then_module(
            vec![ad(Opcode::Kshort, 0, 1), ad(Opcode::Ret1, 0, 2)],
            vec![GcConst::Str(b"x".to_vec())],
        );
        let proto = module.main_proto();
        let cfg = Cfg::build(proto);
        let ast = recover(proto, &cfg).expect("recover should succeed");

        assert_eq!(ast.len(), 1, "expected just [If] (implicit RET0 → no Stmt)");
        match &ast[0] {
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                assert_eq!(*cond, Expr::Global("x".to_string()), "cond");
                assert_eq!(*else_body, None, "else_body");
                assert_eq!(then_body.len(), 1, "then_body len");
                match &then_body[0] {
                    Stmt::Return(Some(expr)) => {
                        assert_eq!(*expr, Expr::Int(1), "return expr");
                    }
                    other => panic!("expected Return(Some(Int(1))), got {:?}", other),
                }
            }
            other => panic!("expected If, got {:?}", other),
        }
    }

    /// `local y = 0; if x then y = 1 end; return y` — the
    /// fallthrough-merge fixture. Verifies that:
    /// 1. The then-body's KSHORT is recognized as an Assign (not a
    ///    declaration) — slot 0 is in scope past scope_begin.
    /// 2. The then-body stops at the merge block (the merge has two
    ///    preds) instead of dragging the merge's RET1 into the
    ///    then-body.
    /// 3. The post-`if` continuation processes the merge and emits
    ///    `return y`.
    #[test]
    fn recover_if_then_fallthrough_merge() {
        // Bytecode (from luajit -bl on the fixture source):
        //   KSHORT 0 0     ; y = 0
        //   GGET  1 0      ; load x
        //   ISF   1        ; tests R[D]=R[1]=x (A is unused for IST/ISF)
        //   JMP   2 => 0006
        //   KSHORT 0 1     ; y = 1 (reassignment)
        //   RET1  0 2      ; return y (merge)
        let insts = vec![
            ad(Opcode::Kshort, 0, 0),
            ad(Opcode::Gget, 1, 0),
            // ISF's tested slot lives in D, not A — see format doc §5.
            // A=0 is the encoding luajit emits (unused for IST/ISF).
            ad(Opcode::Isf, 0, 1),
            // JMP at abs idx 4; target = abs idx 6 (the RET1).
            // j = target - (jmp_idx + 1) = 6 - 5 = 1; D = 0x8001.
            ad(Opcode::Jmp, 2, 0x8001),
            ad(Opcode::Kshort, 0, 1),
            ad(Opcode::Ret1, 0, 2),
        ];
        let module = module_with(
            insts,
            // y is declared at real-idx 0 (the first KSHORT); scope
            // covers the whole chunk so the second KSHORT reads as
            // a reassignment.
            vec![named_local(0, "y", 0, 4)],
            vec![GcConst::Str(b"x".to_vec())],
            Vec::new(),
            2,
        );
        assert_eq!(
            emit(&module),
            "local y = 0\nif x then\n    y = 1\nend\nreturn y"
        );
    }

    /// `if not x then return 1 end` — the IST case. The IST tests
    /// "is slot truthy?"; JMP taken means the then-body is skipped,
    /// so the user's condition is the negation: `not <expr>`.
    #[test]
    fn recover_if_not_x_uses_ist() {
        // Bytecode: GGET 0 0; IST 0; JMP => 0006; KSHORT 0 1; RET1; RET0.
        let module = if_then_module_with_test_op(
            Opcode::Ist,
            vec![ad(Opcode::Kshort, 0, 1), ad(Opcode::Ret1, 0, 2)],
            vec![GcConst::Str(b"x".to_vec())],
        );
        assert_eq!(emit(&module), "if not x then\n    return 1\nend");
    }

    /// Compound conditions (ISLT, ISEQV, …) are Stage 9 territory.
    /// The recovery must bail rather than guess at a condition
    /// expression.
    #[test]
    fn recover_compound_condition_is_not_supported() {
        let module = if_then_module_with_test_op(
            Opcode::Islt,
            vec![ad(Opcode::Kshort, 0, 1), ad(Opcode::Ret1, 0, 2)],
            vec![GcConst::Str(b"x".to_vec())],
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for ISLT (compound condition), got {:?}",
            result
        );
    }

    /// If/else is Stage 8. The recovery should bail when it sees an
    /// else branch (i.e. when the then-body returns but the
    /// ConditionalBranch's true_edge leads to code that's not the
    /// implicit-return merge).
    #[test]
    fn recover_if_then_else_is_not_supported() {
        // Bytecode (matches the cfg test fixture):
        //   GGET 0 0; ISF 0; JMP => 0007;
        //   KSHORT 0 1; RET1;          (then-body, returns)
        //   JMP => 0009;               (dead jump after RET1)
        //   KSHORT 0 2; RET1;          (else-body, returns)
        //   RET0 0 1.                  (merge)
        let insts = vec![
            ad(Opcode::Gget, 0, 0),     // idx 1
            ad(Opcode::Isf, 0, 0),      // idx 2
            ad(Opcode::Jmp, 1, 0x8003), // idx 3: target = idx 7
            ad(Opcode::Kshort, 0, 1),   // idx 4
            ad(Opcode::Ret1, 0, 2),     // idx 5
            ad(Opcode::Jmp, 0, 0x8002), // idx 6: dead, target = idx 9
            ad(Opcode::Kshort, 0, 2),   // idx 7
            ad(Opcode::Ret1, 0, 2),     // idx 8
            ad(Opcode::Ret0, 0, 1),     // idx 9
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![GcConst::Str(b"x".to_vec())],
            Vec::new(),
            1,
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for if/else, got {:?}",
            result
        );
    }

    /// Nested if (a ConditionalBranch inside a then-body) isn't
    /// Stage 7. The recovery bails when it encounters the inner
    /// ConditionalBranch.
    #[test]
    fn recover_nested_if_is_not_supported() {
        // Outer if/then with an inner if/then as the body:
        //   GGET 0 0; ISF 0; JMP => 0009  (outer)
        //   GGET 1 1; ISF 1; JMP => 0008  (inner)
        //   KSHORT 0 1; RET1               (inner then-body)
        //   RET0                            (inner merge = outer body end)
        //   RET0                            (outer merge)
        // The outer then-body's terminator is a ConditionalBranch;
        // recover_from sees it and bails.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),     // idx 1 — outer cond slot 0
            ad(Opcode::Isf, 0, 0),      // idx 2
            ad(Opcode::Jmp, 1, 0x8005), // idx 3: target = idx 9 (outer merge)
            ad(Opcode::Gget, 1, 1),     // idx 4 — inner cond slot 1
            ad(Opcode::Isf, 1, 0),      // idx 5
            ad(Opcode::Jmp, 1, 0x8001), // idx 6: target = idx 8 (inner merge)
            ad(Opcode::Kshort, 0, 1),   // idx 7 — inner then-body
            ad(Opcode::Ret1, 0, 2),     // idx 8 (also inner merge target — conflict)
            ad(Opcode::Ret0, 0, 1),     // idx 9 (outer merge)
        ];
        // The above layout is a bit contrived (the inner merge and
        // then-body-end coincide). What matters for this test is
        // that recover_from encounters a ConditionalBranch while
        // walking the outer then-body — that triggers NotImplemented.
        let module = module_with(
            insts,
            Vec::new(),
            vec![GcConst::Str(b"x".to_vec()), GcConst::Str(b"y".to_vec())],
            Vec::new(),
            2,
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for nested if, got {:?}",
            result
        );
    }

    /// A standalone JMP (loops, gotos) classifies as Terminator::Jump;
    /// the recovery bails on sight.
    #[test]
    fn recover_standalone_jmp_is_not_supported() {
        //   KSHORT 0 1; JMP => 0004; KSHORT 0 2; RET0.
        let insts = vec![
            ad(Opcode::Kshort, 0, 1),   // idx 1
            ad(Opcode::Jmp, 0, 0x8001), // idx 2: target = idx 4
            ad(Opcode::Kshort, 0, 2),   // idx 3
            ad(Opcode::Ret0, 0, 1),     // idx 4
        ];
        let module = module_with(insts, Vec::new(), Vec::new(), Vec::new(), 1);
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for standalone JMP, got {:?}",
            result
        );
    }

    /// Tail calls (CALLT/CALLMT) classify as Terminator::TailCall.
    #[test]
    fn recover_tail_call_is_not_supported() {
        let module = module_with(
            vec![ad(Opcode::Gget, 0, 0), ad(Opcode::Callt, 0, 2)],
            Vec::new(),
            vec![GcConst::Str(b"f".to_vec())],
            Vec::new(),
            1,
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for tail call, got {:?}",
            result
        );
    }

    /// GGET with a non-string GC constant isn't supported.
    #[test]
    fn recover_gget_non_string_gc_const_is_not_supported() {
        // GGET referencing a Child proto (KNUM-like; legal opcode,
        // unsupported payload for Stage 7).
        let module = module_with(
            vec![ad(Opcode::Gget, 0, 0), ad(Opcode::Ret0, 0, 1)],
            Vec::new(),
            vec![GcConst::Child],
            Vec::new(),
            1,
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for GGET with non-Str GC const, got {:?}",
            result
        );
    }

    /// `if 2 > 1 then return 1 end` would lower to an ISGT/ISGE
    /// compound condition — Stage 9. Verifies NotImplemented.
    /// Uses ISGE directly to avoid constant-folding surprises.
    #[test]
    fn recover_isxx_other_than_isf_ist_is_not_supported() {
        let module = if_then_module_with_test_op(
            Opcode::Isge,
            vec![ad(Opcode::Kshort, 0, 1), ad(Opcode::Ret1, 0, 2)],
            vec![GcConst::Str(b"x".to_vec())],
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for ISGE, got {:?}",
            result
        );
    }
}
