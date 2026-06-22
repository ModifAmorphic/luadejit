//! Structural recovery: turn a [`Proto`]'s [`Cfg`] into an AST.
//!
//! Stage 7's [`recover`] walks the CFG built by [`crate::cfg::Cfg::build`]
//! and produces a list of [`Stmt`] nodes that [`crate::emit`] can format
//! back into Lua source. The recovery handles three shapes:
//!
//! - **Linear code**: a chain of `Fallthrough` blocks ending in
//!   `Return`. Produces a flat list of statements. This is what
//!   Stages 1-6 produced via the walk-based emitter; the new pipeline
//!   emits identical source through the AST.
//! - **Single `if` (with optional `else`)**: an entry block ending
//!   in [`ConditionalBranch`](crate::cfg::Terminator::ConditionalBranch)
//!   (ISF or IST + JMP), with a then-body that either returns or
//!   falls through to a post-`if` merge block. Stage 7 covered the
//!   no-`else` case; Stage 8 adds the `else` branch via the
//!   "skip-else" JMP that LuaJIT's codegen always emits between
//!   the two bodies. The continuation after the `if` is also
//!   recovered (typically a final `return`).
//! - **Compound conditions (Stage 9)**: any ISxx comparison op
//!   (ISLT/ISGE/ISLE/ISGT/ISEQV/ISNEV/ISEQS/ISNES/ISEQN/ISNEN/ISEQP/ISNEP)
//!   and short-circuiting `and`/`or` chains. A single CB with an
//!   ISxx comparison produces a comparison expression (`a == b`,
//!   `a < b`, …). A linear chain of CBs connected via `false_edge`
//!   produces an [`Expr::And`] or [`Expr::Or`] expression: all CBs
//!   sharing a true_edge → merge is AND; the first CB short-circuiting
//!   to the then-body is OR.
//!
//! Everything else (loops, nested `if`, `elseif`
//! chains, sequential `if`s) bails with
//! [`DecompilerError::NotImplemented`] — later stages pick those up.
//!
//! **Stage 12** adds regular function calls (`print("hello")`,
//! `f(1, 2)`, `local x = tostring(42)`). The recovery handles CALL
//! with zero or one explicit results and a fixed (non-multres)
//! argument list. Tail calls (CALLT/CALLMT) remain CFG-terminator
//! bailouts; method calls (Stage 13) and multres (Stage 15) are
//! deferred.
//!
//! **Stage 13** adds field access (`obj.field`, `obj[5]`, `t[k]`)
//! via TGETS/TGETB/TGETV and method-call detection (`obj:m(1)`).
//! The recovery detects method calls at the CALL handler: when the
//! function slot holds `Expr::Field` and the self slot (A+2 in FR2,
//! A+1 in FR1) holds the Field's `obj`, the CALL desugars to
//! `obj:method(args)` with the implicit self dropped. Stage 13 also
//! fixes the FR1/FR2 argument-base bug — the CALL handler now honors
//! the proto's frame convention instead of assuming FR2.
//!
//! **Stage 11** adds `while cond do … end` and `repeat … until cond`
//! support. Both are detected at the [`ConditionalBranch`] arm of
//! [`recover_from`]:
//!
//! - **Repeat** — the CB's `true_edge` or `false_edge` points back to
//!   the CB's own block (self-loop). The ISxx tests the CONTINUE
//!   condition; the user's `until`-condition is its complement.
//! - **While** — the CB's `false_edge` (the body) ends in a
//!   [`Jump`](Terminator::Jump) whose target is the CB's own block
//!   (the loop header). The ISxx tests the EXIT condition; the user's
//!   `while`-condition is its complement.
//!
//! Both shapes share the existing `build_condition` helper (Stage 9)
//! and inherit its documented canonicalization limitation for ordered
//! comparisons (e.g. `i >= 3` canonicalizes to `3 <= i` in the
//! emitted source).
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
//! The slot map is snapshotted before each branch body and restored
//! between bodies, so the then-body and else-body each see the
//! entry-block state in isolation. At the merge, the map reflects
//! whichever branch was processed last (the else-body, when there is
//! one). For named locals this is correct — both branches agree on
//! the name — so `return <name>` at the merge references the right
//! local. Value reconciliation for unnamed temps across merges is
//! phi-elimination territory (later stages); Stage 8 doesn't need it
//! because the supported fixtures don't introduce merge-time
//! conflicts.

use std::collections::{HashMap, HashSet};

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
    /// Stage 7 emits `else_body: None`; Stage 8 populates it for the
    /// `if/else` shape).
    If {
        cond: Expr,
        then_body: Vec<Stmt>,
        else_body: Option<Vec<Stmt>>,
    },
    /// `for var = start, stop[, step] do body end` (Stage 10). When
    /// `step` is `None` the source omitted it (default step of 1).
    For {
        var: String,
        start: Expr,
        stop: Expr,
        step: Option<Expr>,
        body: Vec<Stmt>,
    },
    /// `while cond do body end` (Stage 11). The condition is the
    /// complement of the ISxx test (so `while i < 10` is recovered
    /// from an ISGE testing `i >= 10`).
    While { cond: Expr, body: Vec<Stmt> },
    /// `repeat body until cond` (Stage 11). The condition is the
    /// complement of the ISxx test = the exit condition the user
    /// wrote. Note LuaJIT canonicalizes ordered comparisons to
    /// `<`/`<=` by swapping operands, so `until i >= 3` (which
    /// canonicalizes to `3 <= i`) round-trips as `until 3 <= i`
    /// — semantically correct but non-canonical (same limitation as
    /// Stage 9's ordered-comparison conditions).
    Repeat { cond: Expr, body: Vec<Stmt> },
    /// A bare function call whose result is discarded (Stage 12):
    /// `print("hello")`. The recovery pushes this when `CALL` has
    /// B=1 (zero results). When a call has a single result that's
    /// actually used, the call surfaces as an [`Expr::Call`] inside
    /// another statement (LocalDecl / Assign / Return) instead.
    Call { func: Expr, args: Vec<Expr> },
    /// A bare method call whose result is discarded (Stage 13):
    /// `obj:method(1)`. Same routing logic as [`Stmt::Call`]: when
    /// the call has a single result that's used, it surfaces as an
    /// [`Expr::MethodCall`] inside another statement instead.
    MethodCall {
        obj: Expr,
        method: String,
        args: Vec<Expr>,
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
    /// `left and right` — Stage 9's chained AND condition. Lua's
    /// `and` returns the left operand if it is falsy, otherwise the
    /// right operand; in a boolean context this is the logical AND.
    /// Emit prints it without parenthesization (Stage 9 test cases
    /// don't mix `and` and `or` in a single condition; Lua's
    /// precedence — `and` tighter than `or`, both looser than
    /// comparisons — keeps naive concatenation correct in practice).
    And(Box<Expr>, Box<Expr>),
    /// `left or right` — Stage 9's chained OR condition. Same
    /// formatting caveat as [`Expr::And`].
    Or(Box<Expr>, Box<Expr>),
    /// A function call `func(args...)` (Stage 12). Emitted without
    /// parenthesization of `func` (Stage 12 fixtures only call bare
    /// globals; calls on table fields or parenthesized expressions
    /// are later stages).
    Call {
        func: Box<Expr>,
        args: Vec<Expr>,
    },
    /// `obj.field` — table field access via string key (Stage 13).
    /// Lowered from `TGETS A B C` where C is a reverse-indexed
    /// GC string constant; the source operand is `slot[B]` and the
    /// field name is the constant.
    Field {
        obj: Box<Expr>,
        name: String,
    },
    /// `obj[key]` — table index access (Stage 13). Lowered from
    /// `TGETB A B C` (key is an inline integer literal in C) or
    /// `TGETV A B C` (key is `slot[C]`).
    Index {
        obj: Box<Expr>,
        key: Box<Expr>,
    },
    /// `obj:method(args...)` — method call with implicit self
    /// (Stage 13). Lowered from a `CALL` whose base slot holds an
    /// [`Expr::Field`] and whose self slot (A+2 in FR2, A+1 in FR1)
    /// matches the Field's `obj`. The recovery drops the implicit
    /// self from the arg list — only explicit source arguments
    /// appear here.
    MethodCall {
        obj: Box<Expr>,
        method: String,
        args: Vec<Expr>,
    },
}

/// The binary operator kind attached to [`Expr::BinOp`]. Stage 7
/// covers the seven Lua binops the arithmetic opcodes lower to;
/// Stage 9 adds the six comparison operators the ISxx ops lower to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BinOpKind {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Concat,
    // ---- Stage 9: comparison operators (lowered from ISxx ops) ----
    /// `==` (LuaJIT ISEQV/ISEQS/ISEQN/ISEQP).
    Equal,
    /// `~=` (LuaJIT ISNEV/ISNES/ISNEN/ISNEP).
    NotEqual,
    /// `<` (LuaJIT ISLT).
    LessThan,
    /// `>` (LuaJIT ISGT).
    GreaterThan,
    /// `<=` (LuaJIT ISLE).
    LessEqual,
    /// `>=` (LuaJIT ISGE).
    GreaterEqual,
}

// ---- Recovery -------------------------------------------------------

/// Recover source-level structure from a [`Proto`]'s [`Cfg`].
///
/// Walks the CFG starting at the entry block, threading a
/// `slot_exprs` map and producing [`Stmt`] nodes. Returns
/// [`DecompilerError::NotImplemented`] for any CFG shape the
/// recovery doesn't model: nested `if`, `elseif` chains, sequential
/// `if`s, compound `while`/`repeat` conditions, nested loops,
/// endless loops, `break`/`continue`, tail calls, and any block
/// whose terminator isn't recognized.
///
/// **Stages 7-10** handle linear code, single `if/then` and
/// `if/else`, compound `and`/`or` conditions, and numeric-`for`
/// loops. **Stage 11** adds `while`/`repeat` loops with simple
/// single-condition headers. **Stage 12** adds regular (non-tail,
/// non-multres, non-method) function calls. **Stage 13** adds
/// field access (`obj.field`, `obj[k]`) and method-call detection
/// (`obj:method(args)`), and threads `is_fr2` through so the CALL
/// handler honors the proto's frame convention.
///
/// Unreachable blocks (e.g. the dead "skip-else" JMP LuaJIT emits
/// after a returning then-body) are *not* rejected wholesale — they
/// are part of the if/else structural signature. The walk simply
/// never enters them: the recovery uses the dead JMP only to
/// identify the merge target, never to process its instructions.
pub fn recover(proto: &Proto, cfg: &Cfg, is_fr2: bool) -> Result<Vec<Stmt>, DecompilerError> {
    if cfg.blocks.is_empty() {
        return Ok(Vec::new());
    }
    // Stage 7-8 supported at most one ConditionalBranch per function
    // (nested `if`, `elseif` chains, and sequential `if`s all bail
    // here). Stage 9 relaxes this to also accept a single chain of
    // ConditionalBranches connected via `false_edge` — that's the
    // CFG shape LuaJIT emits for `if a and b then …` and
    // `if a or b then …`. Any other multi-CB layout (nested `if`,
    // sequential `if`s, `elseif` chains) still bails.
    if !is_single_cb_chain(cfg) {
        return Err(DecompilerError::NotImplemented);
    }
    let mut slot_exprs: HashMap<u8, Expr> = HashMap::new();
    let mut visited: HashSet<BlockId> = HashSet::new();
    let mut stmts = Vec::new();
    let mut ctx = RecoveryCtx {
        proto,
        cfg,
        is_fr2,
        slot_exprs: &mut slot_exprs,
        visited: &mut visited,
    };
    recover_from(cfg.entry, false, &mut ctx, &mut stmts)?;
    Ok(stmts)
}

/// Whether every [`ConditionalBranch`](Terminator::ConditionalBranch)
/// in `cfg` belongs to a single linear chain connected via
/// `false_edge`, reachable from the entry by following
/// [`Fallthrough`](Terminator::Fallthrough) edges.
///
/// The Stage 9 chain shape covers `if a and b then …` and
/// `if a or b then …`, where LuaJIT's codegen emits 2+ ISxx+JMP
/// pairs in sequence. Every other multi-CB layout bails:
///
/// - **Nested `if`**: the inner CB is inside the outer's then-body,
///   not the outer's `false_edge` continuation. For simple nested
///   ifs whose bytecode happens to be identical to an AND chain,
///   this check passes (the decompiler then emits `a and b`, which
///   is semantically equivalent at the bytecode level).
/// - **Sequential `if`s**: the second CB follows the first's merge
///   (a non-CB block), so the chain walk stops at length 1.
/// - **`elseif` chains**: the first CB's `false_edge` is its own
///   then-body (a non-CB block), not the next CB.
fn is_single_cb_chain(cfg: &Cfg) -> bool {
    let cb_count = cfg
        .blocks
        .iter()
        .filter(|b| matches!(b.terminator, Terminator::ConditionalBranch { .. }))
        .count();
    if cb_count <= 1 {
        return true;
    }
    // Find the first CB reachable from the entry via Fallthrough only
    // (the chain's root). If the entry's walk hits any other
    // terminator first, there's no chain root and we bail.
    let mut current = cfg.entry;
    loop {
        match cfg.blocks[current.0 as usize].terminator {
            Terminator::Fallthrough(next) => current = next,
            Terminator::ConditionalBranch { .. } => break,
            _ => return false,
        }
    }
    // Walk the chain via false_edge; compare length against total.
    collect_cb_chain(cfg, current).len() == cb_count
}

/// Collect the chain of [`ConditionalBranch`](Terminator::ConditionalBranch)
/// blocks starting at `start`, walking forward via `false_edge`
/// until the next block isn't a ConditionalBranch.
///
/// `start` itself must be a ConditionalBranch block; the returned
/// vector always contains at least `start`.
fn collect_cb_chain(cfg: &Cfg, start: BlockId) -> Vec<BlockId> {
    let mut chain = vec![start];
    let mut current = start;
    while let Terminator::ConditionalBranch { false_edge, .. } =
        cfg.blocks[current.0 as usize].terminator
    {
        if matches!(
            cfg.blocks[false_edge.0 as usize].terminator,
            Terminator::ConditionalBranch { .. }
        ) {
            chain.push(false_edge);
            current = false_edge;
        } else {
            break;
        }
    }
    chain
}

/// Extract `(true_edge, false_edge)` from a
/// [`ConditionalBranch`](Terminator::ConditionalBranch) block.
fn cb_edges(cfg: &Cfg, block_id: BlockId) -> (BlockId, BlockId) {
    match cfg.blocks[block_id.0 as usize].terminator {
        Terminator::ConditionalBranch {
            true_edge,
            false_edge,
            ..
        } => (true_edge, false_edge),
        _ => unreachable!(
            "cb_edges called on non-ConditionalBranch block {:?}",
            block_id
        ),
    }
}

/// Walk-time shared state. Borrows the proto + cfg immutably and the
/// slot map mutably for the duration of recovery. The lifetime is
/// kept internal to this module: callers go through [`recover`].
struct RecoveryCtx<'a> {
    proto: &'a Proto,
    cfg: &'a Cfg,
    /// Whether this proto uses the FR2 (two-slot) frame convention.
    /// FR2 leaves a one-slot gap between a CALL's function slot (A)
    /// and its first argument slot (A+2). FR1 (no gap) packs args
    /// starting at A+1. All Darktide-corpus protos are FR1; the
    /// system-luajit fixtures in the test suite are FR2. Threaded
    /// through from [`emit_module`], which reads `module.header.is_fr2()`.
    is_fr2: bool,
    slot_exprs: &'a mut HashMap<u8, Expr>,
    /// Blocks already entered by some `recover_from` invocation on the
    /// current recovery walk. Stage 10's safety mechanism: any
    /// *unexpected* control-flow cycle (one the recovery doesn't
    /// handle explicitly, e.g. a back-edge that isn't a recognized
    /// loop pattern) returns [`DecompilerError::NotImplemented`]
    /// instead of recursing forever.
    ///
    /// Recognized loops never re-enter a block via the back-edge
    /// because the loop-latch arm stops the walk rather than
    /// following the back-edge. So in normal operation this set
    /// accumulates every block visited exactly once; the membership
    /// check only fires on a malformed CFG or a future loop pattern
    /// the recovery hasn't been taught to short-circuit.
    visited: &'a mut HashSet<BlockId>,
}

/// Walk a chain of blocks starting at `start`, appending [`Stmt`]s
/// to `stmts`. Returns the id of the last block examined — used by
/// the caller to detect whether the branch body ended with a
/// "skip-else" JMP (the if/else signature).
///
/// `stop_at_merge` controls how a [`Terminator::Fallthrough`] into a
/// block with multiple predecessors is treated:
///
/// - `false` (top-level walk / post-`if` continuation): follow into
///   the merge. This is how the post-`if` continuation block (which
///   has preds from both branches) gets processed.
/// - `true` (inside a branch body): stop at the merge. The branch
///   body's fallthrough into the merge is *not* part of the branch
///   body's source; the merge belongs to the outer walk.
///
/// When `stop_at_merge` is true the walk also stops (rather than
/// bailing) on a [`Terminator::Jump`] — this is the live "skip-else"
/// JMP at the end of a then-body that doesn't return (Stage 8
/// if/else fallthrough). Top-level walks (`stop_at_merge == false`)
/// still bail on Jump, since a standalone JMP at the top level is a
/// loop or goto (later stages).
///
/// Encountering a [`Terminator::TailCall`] (or any unrecognized
/// terminator) bails with [`DecompilerError::NotImplemented`].
fn recover_from(
    start: BlockId,
    stop_at_merge: bool,
    ctx: &mut RecoveryCtx,
    stmts: &mut Vec<Stmt>,
) -> Result<BlockId, DecompilerError> {
    // Visited-block safety net (Stage 10). A block already entered by
    // some `recover_from` invocation on this walk means we've hit a
    // control-flow cycle the recovery doesn't handle explicitly.
    // `HashSet::insert` returns false when the value was already
    // present, so the negation means "already visited".
    if !ctx.visited.insert(start) {
        return Err(DecompilerError::NotImplemented);
    }
    let mut current = Some(start);
    let mut last_block = start;
    while let Some(block_id) = current {
        last_block = block_id;
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
                    // The branch body falls through into the post-`if`
                    // merge. Stop here — the merge belongs to the
                    // outer walk, not the branch body.
                    break;
                }
                current = Some(next);
            }
            Terminator::Jump(target) => {
                // Process every instruction in the block except the
                // trailing JMP itself (the JMP is a "skip-else" or a
                // goto, not source). When the block is a standalone
                // JMP with no preceding instructions, this loop is a
                // no-op.
                let last_idx = block.insts.last().map(|id| id.0 as usize).unwrap();
                for inst_id in &block.insts {
                    if (inst_id.0 as usize) == last_idx {
                        break;
                    }
                    process_inst(ctx, stmts, *inst_id)?;
                }
                if stop_at_merge {
                    // Inside a branch body, a Jump terminator is the
                    // live "skip-else" JMP — the then-body jumps over
                    // the else-body to the merge. Stop without
                    // advancing; the caller uses `target` to identify
                    // the merge via [`branch_shape`].
                    break;
                }
                // Top-level standalone JMP (loops, gotos) isn't
                // supported.
                let _ = target;
                return Err(DecompilerError::NotImplemented);
            }
            Terminator::ConditionalBranch {
                condition: first_condition,
                true_edge: first_true_edge,
                false_edge: first_false_edge,
            } => {
                // Stage 9 doesn't support nested `if` (a CB inside a
                // branch body). When `stop_at_merge` is set we're
                // inside a then-/else-body — encountering a CB here
                // means the source had a nested `if`.
                if stop_at_merge {
                    return Err(DecompilerError::NotImplemented);
                }
                // ---- Stage 11: while/repeat loop detection ----
                //
                // Before the if/then/else chain logic, check whether
                // this CB is a loop header:
                //   1. Self-loop (repeat): one of the CB's edges points
                //      back to the current block.
                //   2. Body jumps back (while): the false_edge block
                //      (the body) ends in a Jump whose target is the
                //      current block.
                //
                // Order matters: the self-loop check has to come first
                // because a degenerate `while true do … end` would
                // have both edges pointing at the header (handled as
                // repeat-shape here, then bailed on below since the
                // ISxx doesn't fit). Compound conditions (chain.len()
                // > 1) fall through to the chain logic below and bail
                // via the body's LOOP marker — out of scope for Stage
                // 11.

                // Repeat: self-loop. The CB's true_edge or false_edge
                // is the current block.
                if first_true_edge == block_id || first_false_edge == block_id {
                    let exit = if first_true_edge == block_id {
                        first_false_edge
                    } else {
                        first_true_edge
                    };
                    let cond_idx = first_condition.0 as usize;
                    // The block layout for `repeat … until cond` is
                    //   [LOOP?, body_code, condition_setup, ISxx, JMP]
                    // The body is everything before the ISxx. LOOP is
                    // a no-op marker (handled in `process_inst`);
                    // condition_setup (e.g. KSHORT loading the
                    // comparison constant) populates `slot_exprs`
                    // without emitting a Stmt when the target slot is
                    // unnamed.
                    let mut body_stmts: Vec<Stmt> = Vec::new();
                    for inst_id in &block.insts {
                        if (inst_id.0 as usize) >= cond_idx {
                            break;
                        }
                        process_inst(ctx, &mut body_stmts, *inst_id)?;
                    }
                    let cond_inst = &ctx.proto.insts[cond_idx];
                    // build_condition gives the complement of the
                    // ISxx test = the user's until-condition (the
                    // exit condition).
                    let cond = build_condition(ctx.proto, ctx.slot_exprs, cond_inst)?;
                    stmts.push(Stmt::Repeat {
                        cond,
                        body: body_stmts,
                    });
                    current = Some(exit);
                    continue;
                }

                // While: false_edge (body) jumps back to this block.
                // Bound to a local scope so the `body_block` borrow
                // ends before the chain logic below.
                let is_while_loop = {
                    let body_block = &ctx.cfg.blocks[first_false_edge.0 as usize];
                    matches!(
                        body_block.terminator,
                        Terminator::Jump(target) if target == block_id
                    )
                };
                if is_while_loop {
                    let cond_idx = first_condition.0 as usize;
                    // Process the header's prefix instructions
                    // (everything before the ISxx) into the outer
                    // stmts list. For `while i < 10` the prefix is
                    // `KSHORT 1 10` — loading the comparison constant
                    // into slot 1, an unnamed temp, so no Stmt is
                    // emitted but `slot_exprs[1] = Int(10)`.
                    for inst_id in &block.insts {
                        if (inst_id.0 as usize) >= cond_idx {
                            break;
                        }
                        process_inst(ctx, stmts, *inst_id)?;
                    }
                    let cond_inst = &ctx.proto.insts[cond_idx];
                    // build_condition gives the complement of the
                    // ISxx test = the user's while-condition (the
                    // entry condition).
                    let cond = build_condition(ctx.proto, ctx.slot_exprs, cond_inst)?;
                    // Process the body block. Its terminator is a
                    // Jump(header) back-edge; the prefix is the body
                    // source. LOOP marker at the start is a no-op.
                    let body_block = &ctx.cfg.blocks[first_false_edge.0 as usize];
                    let mut body_stmts: Vec<Stmt> = Vec::new();
                    let body_last_idx = body_block.insts.last().map(|id| id.0 as usize).unwrap();
                    for inst_id in &body_block.insts {
                        if (inst_id.0 as usize) == body_last_idx {
                            break;
                        }
                        process_inst(ctx, &mut body_stmts, *inst_id)?;
                    }
                    // Mark the body block as visited. Simple loops
                    // never re-enter the body via the back-edge (the
                    // recovery stops at the back-edge rather than
                    // following it), so this is defensive — it
                    // catches a future loop pattern that would
                    // otherwise infinite-recurse.
                    ctx.visited.insert(first_false_edge);
                    stmts.push(Stmt::While {
                        cond,
                        body: body_stmts,
                    });
                    // Continue at the exit (the true_edge — where the
                    // ISxx+JMP lands when the loop condition fails).
                    current = Some(first_true_edge);
                    continue;
                }

                // ---- Existing if/then/else chain handling ----
                // Collect the chain of CB blocks reachable via
                // false_edge. For Stages 7-8 fixtures this is just
                // [entry]; for Stage 9 AND/OR chains it contains one
                // CB per condition.
                let chain = collect_cb_chain(ctx.cfg, block_id);
                // Walk each chain block's prefix instructions, then
                // build both the ISxx test expression (as-is) and
                // the if-condition (the test's complement — what the
                // user wrote). For a chain, AND combines the
                // if-conditions; OR combines test[0..N-1] with
                // if-conditions[N-1] (see the AND/OR detection
                // below).
                let mut test_conds: Vec<Expr> = Vec::with_capacity(chain.len());
                let mut if_conds: Vec<Expr> = Vec::with_capacity(chain.len());
                for &cb_id in &chain {
                    let cb_block = &ctx.cfg.blocks[cb_id.0 as usize];
                    let condition = match cb_block.terminator {
                        Terminator::ConditionalBranch { condition, .. } => condition,
                        _ => unreachable!(),
                    };
                    let cond_idx = condition.0 as usize;
                    for inst_id in &cb_block.insts {
                        if (inst_id.0 as usize) >= cond_idx {
                            break;
                        }
                        process_inst(ctx, stmts, *inst_id)?;
                    }
                    let cond_inst = &ctx.proto.insts[cond_idx];
                    test_conds.push(build_test_condition(ctx.proto, ctx.slot_exprs, cond_inst)?);
                    if_conds.push(build_condition(ctx.proto, ctx.slot_exprs, cond_inst)?);
                }
                // Determine the chain's shape: single CB, OR chain,
                // or AND chain. Set (then_entry, merge, if_cond)
                // accordingly.
                let last_cb_id = *chain.last().expect("chain is non-empty");
                let (last_true_edge, last_false_edge) = cb_edges(ctx.cfg, last_cb_id);
                let (then_entry, predicted_merge, if_cond) = if chain.len() == 1 {
                    // Single CB: standard if/then or if/else. Merge
                    // is finalized by `branch_shape` below (it
                    // depends on the then-body's terminator).
                    (first_false_edge, None, if_conds[0].clone())
                } else if first_true_edge == last_false_edge {
                    // OR chain: CBs[0..N-1] short-circuit via
                    // true_edge to the then-body; the last CB skips
                    // to the merge via true_edge. The user's
                    // condition is the ISxx test (not negated) for
                    // the short-circuiting CBs and the negation for
                    // the last CB.
                    //
                    // Build [test[0], test[1], …, test[N-2], if[N-1]]
                    // then left-fold with `Or`.
                    let mut or_conds: Vec<Expr> =
                        test_conds.iter().take(chain.len() - 1).cloned().collect();
                    or_conds.push(if_conds[chain.len() - 1].clone());
                    let cond = or_conds
                        .into_iter()
                        .reduce(|acc, c| Expr::Or(Box::new(acc), Box::new(c)))
                        .expect("chain is non-empty");
                    (first_true_edge, Some(last_true_edge), cond)
                } else {
                    // AND chain: every CB's true_edge skips to a
                    // common merge. Verify the invariant before
                    // combining; a chain where CBs disagree on the
                    // merge isn't a Stage 9 shape.
                    for &cb_id in &chain {
                        let (t, _) = cb_edges(ctx.cfg, cb_id);
                        if t != last_true_edge {
                            return Err(DecompilerError::NotImplemented);
                        }
                    }
                    let cond = if_conds
                        .iter()
                        .cloned()
                        .reduce(|acc, c| Expr::And(Box::new(acc), Box::new(c)))
                        .expect("chain is non-empty");
                    (last_false_edge, Some(last_true_edge), cond)
                };
                // Snapshot the slot state so each branch body starts
                // from the post-chain baseline. After the then-body
                // (and else-body, if any) we restore this snapshot
                // so the chain's loads don't leak past the `if`.
                let entry_slots = ctx.slot_exprs.clone();
                let mut then_body: Vec<Stmt> = Vec::new();
                let then_last = recover_from(then_entry, true, ctx, &mut then_body)?;
                // Identify the else-start (if any) and the final
                // merge block. For single-CB if/else this uses
                // `branch_shape`; for chains Stage 9 only supports
                // if/then (no else), so we verify the then-body's
                // terminator is consistent with if/then.
                let (else_start, merge) = if let Some(m) = predicted_merge {
                    // Chain path: assert if/then shape. If the
                    // then-body returns, look for a dead "skip-else"
                    // JMP immediately after — its presence would
                    // indicate a chained if/else, which Stage 9
                    // doesn't model.
                    match ctx.cfg.blocks[then_last.0 as usize].terminator {
                        Terminator::Fallthrough(target) if target == m => (None, m),
                        Terminator::Return => {
                            let next_id = BlockId(then_last.0 + 1);
                            if (next_id.0 as usize) < ctx.cfg.blocks.len() {
                                if let Terminator::Jump(_) =
                                    ctx.cfg.blocks[next_id.0 as usize].terminator
                                {
                                    return Err(DecompilerError::NotImplemented);
                                }
                            }
                            (None, m)
                        }
                        _ => return Err(DecompilerError::NotImplemented),
                    }
                } else {
                    // Single-CB path: defer to branch_shape, which
                    // handles both if/then and if/else detection.
                    branch_shape(ctx.cfg, then_last, first_true_edge)?
                };
                let else_body = if let Some(else_start) = else_start {
                    *ctx.slot_exprs = entry_slots.clone();
                    let mut else_stmts: Vec<Stmt> = Vec::new();
                    recover_from(else_start, true, ctx, &mut else_stmts)?;
                    Some(else_stmts)
                } else {
                    *ctx.slot_exprs = entry_slots.clone();
                    None
                };
                stmts.push(Stmt::If {
                    cond: if_cond,
                    then_body,
                    else_body,
                });
                current = Some(merge);
            }
            Terminator::TailCall(_) => {
                // Tail calls aren't supported.
                return Err(DecompilerError::NotImplemented);
            }
            Terminator::LoopInit { base, exit, body } => {
                // Numeric-for loop start. Process the prefix
                // instructions (start/stop/step loads into slots A,
                // A+1, A+2), then read the three operands back as
                // Exprs, look up the loop variable name at slot A+3
                // (kind=Name in debug info), and recurse into the
                // body. The body's terminator is `LoopLatch`, which
                // stops the walk at the back-edge rather than
                // following it.
                if stop_at_merge {
                    // A LoopInit inside a branch body is a loop
                    // nested in an `if` — Stage 10 doesn't model that.
                    return Err(DecompilerError::NotImplemented);
                }
                // The FORI itself is the trailing instruction; every
                // earlier instruction in the block is the pre-loop
                // setup. The FORI initializes slot A+3 implicitly, so
                // we never `process_inst` it.
                let last_idx = block.insts.last().map(|id| id.0 as usize).unwrap();
                for inst_id in &block.insts {
                    if (inst_id.0 as usize) == last_idx {
                        break;
                    }
                    process_inst(ctx, stmts, *inst_id)?;
                }
                // Read the start/stop/step Exprs from the slot map.
                let start = lookup_operand(ctx.slot_exprs, base)?;
                let stop = lookup_operand(ctx.slot_exprs, base + 1)?;
                let step_expr = lookup_operand(ctx.slot_exprs, base + 2)?;
                // LuaJIT emits an explicit `KSHORT A+2 1` even when
                // the user wrote no step. Collapse Expr::Int(1) back
                // to None so emit produces `for i = 1, 10 do` rather
                // than `for i = 1, 10, 1 do`. Any other step value
                // (including Expr::Float(1.0)) is preserved verbatim.
                let step = if step_expr == Expr::Int(1) {
                    None
                } else {
                    Some(step_expr)
                };
                // The loop variable name is stored in debug info as a
                // VarKind::Name record at slot A+3 (verified
                // empirically: the ForIdx/ForStop/ForStep records
                // occupy slots A, A+1, A+2 and carry no source name;
                // the visible loop variable gets its own Name record
                // at slot A+3). Look it up at the FORI's
                // real-instruction index — the variable's scope_begin
                // equals the FORI's real_idx in all observed fixtures.
                let fori_real_idx = last_idx - 1;
                let var_name = ctx
                    .proto
                    .var_name_at(base + 3, fori_real_idx)
                    .ok_or(DecompilerError::NotImplemented)?
                    .to_string();
                // Make the loop variable visible inside the body:
                // slot A+3 resolves to `Expr::Var(name)` so a `MOV
                // dst, A+3` reads as `dst = i`, a `RET1 A+3 2` reads
                // as `return i`, etc.
                ctx.slot_exprs.insert(base + 3, Expr::Var(var_name.clone()));
                // Process the body. The walk follows Fallthroughs
                // through the body's blocks until it hits the
                // `LoopLatch` block, where it stops at the back-edge.
                let mut body_stmts: Vec<Stmt> = Vec::new();
                recover_from(body, true, ctx, &mut body_stmts)?;
                stmts.push(Stmt::For {
                    var: var_name,
                    start,
                    stop,
                    step,
                    body: body_stmts,
                });
                // Continue the outer walk at the loop exit. The
                // LoopLatch's fall-through and any "dead" LoopLatch
                // block (unreachable when the body returns) are
                // skipped: the exit is encoded directly in LoopInit.
                current = Some(exit);
            }
            Terminator::LoopLatch { .. } => {
                // Loop back-edge terminator (FORL). Process the
                // prefix instructions (everything except the FORL
                // itself), then stop without following the back-edge.
                // The loop body ends here; the FORL's body target is
                // the body start (already visited by the time we'd
                // reach the back-edge).
                //
                // We only enter a LoopLatch block from inside a loop
                // body (the LoopInit arm calls `recover_from(body,
                // ...)`). Encountering one at the top level would
                // mean a malformed CFG (a FORL without a matching
                // FORI); the visited check or earlier bail would have
                // caught it before now.
                let last_idx = block.insts.last().map(|id| id.0 as usize).unwrap();
                for inst_id in &block.insts {
                    if (inst_id.0 as usize) == last_idx {
                        break;
                    }
                    process_inst(ctx, stmts, *inst_id)?;
                }
                break;
            }
        }
    }
    Ok(last_block)
}

/// Decide whether the construct recovered from a
/// [`Terminator::ConditionalBranch`] is an `if/then` or an `if/else`,
/// and identify the merge block where control flow reconverges.
///
/// Returns `(else_start, merge)`:
/// - `else_start == Some(block)` → if/else; `block` is the else-body's
///   entry, `merge` is the post-`if` continuation.
/// - `else_start == None` → if/then; `merge` is the post-`if`
///   continuation (the entry's `true_edge`, where the ISxx+JMP lands
///   when the condition skips the then-body).
///
/// # Detection algorithm
///
/// LuaJIT's codegen for `if/else` always emits a "skip-else" JMP
/// between the two bodies. Its placement depends on whether the
/// then-body returns:
///
/// 1. **Then-body ends with `Jump(merge)`** — the then-body falls
///    through (doesn't return) and ends with the LIVE "skip-else"
///    JMP over the else-body. `true_edge` is the else-body. →
///    if/else.
/// 2. **Then-body ends with `Fallthrough(target)`** — the then-body
///    falls through into a merge that has multiple predecessors (the
///    walk only stops at such a fallthrough when `stop_at_merge` is
///    set). `target` is the merge. → if/then (no skip-else JMP
///    exists).
/// 3. **Then-body ends with `Return`** — the then-body returns. Look
///    for a dead `Jump(merge)` block immediately after the then-body
///    (by block id): LuaJIT emits this dead "skip-else" JMP even
///    though the RET1 makes it unreachable. If found → if/else
///    (`true_edge` is the else-body). If not found → if/then
///    (`true_edge` is the merge).
///
/// Any other terminator shape is unsupported (Stage 8+).
fn branch_shape(
    cfg: &Cfg,
    then_last: BlockId,
    true_edge: BlockId,
) -> Result<(Option<BlockId>, BlockId), DecompilerError> {
    let last = &cfg.blocks[then_last.0 as usize];
    match last.terminator {
        Terminator::Jump(merge) => Ok((Some(true_edge), merge)),
        Terminator::Fallthrough(target) => Ok((None, target)),
        Terminator::Return => {
            // Look for a dead "skip-else" JMP block immediately
            // after the then-body. The walk never enters this block
            // (it's unreachable after the RET1), but its presence —
            // and its target — are the if/else signature.
            let next_id = BlockId(then_last.0 + 1);
            if (next_id.0 as usize) < cfg.blocks.len() {
                if let Terminator::Jump(merge) = cfg.blocks[next_id.0 as usize].terminator {
                    return Ok((Some(true_edge), merge));
                }
            }
            // No dead JMP → if/then; the entry's true_edge is the
            // merge.
            Ok((None, true_edge))
        }
        _ => Err(DecompilerError::NotImplemented),
    }
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
        Opcode::Loop => {
            // LOOP marker — informational only. A holds a framesize
            // hint, D holds the loop exit target; both are irrelevant
            // to source recovery. Treated as a no-op so while/repeat
            // bodies (which start with a LOOP marker) walk cleanly.
        }
        Opcode::Call => {
            // CALL A B C — function call. Per the (empirically
            // verified) convention:
            //   - A     = base slot; function is at slot A.
            //   - B-1   = number of return values expected
            //             (B == 0 means multres — Stage 15).
            //   - C-1   = number of arguments (C == 0 means multres
            //             call from a VARG/previous CALL — Stage 15).
            // Arguments occupy slots arg_base..=A+C where arg_base is
            // A+2 in FR2 (gap at A+1) or A+1 in FR1 (no gap). Results
            // overwrite slots starting at A.
            //
            // Stage 12 only models the regular cases: zero or one
            // explicit results, and a fixed (non-multres) argument
            // list. Tail calls (CALLT/CALLMT) are CFG terminators
            // handled in the recovery walk (they bail with
            // NotImplemented); multres (Stage 15) is deferred. Stage
            // 13 adds method-call detection: a CALL whose base slot
            // holds `Expr::Field { obj, name }` and whose self slot
            // (arg_base) holds `obj` desugars to `obj:name(args)`.
            if inst.b() == 0 || inst.c == 0 {
                return Err(DecompilerError::NotImplemented);
            }
            // FR1 (Darktide corpus) packs args at A+1; FR2 (system
            // luajit test fixtures) leaves a gap and packs args at
            // A+2. The same base slot also identifies the self
            // register for method calls.
            let arg_base = if ctx.is_fr2 { inst.a + 2 } else { inst.a + 1 };
            let func = lookup_operand(ctx.slot_exprs, inst.a)?;
            // Method-call detection. The compiler lowers
            // `obj:method(args...)` to:
            //   MOV arg_base, obj_slot   ; self = obj
            //   TGETS A, obj_slot, name  ; A = obj.method
            //   [<arg loads into arg_base+1..A+C>]
            //   CALL A, B, C
            // We detect this by checking (1) the base slot holds a
            // Field, and (2) the self slot holds the Field's obj.
            // When both hold, the user wrote `obj:name(args)`; drop
            // the implicit self from the arg list.
            let method_call = match &func {
                Expr::Field { obj, name } => slot_exprs_get(ctx.slot_exprs, arg_base)
                    .is_some_and(|self_expr| self_expr == obj.as_ref())
                    .then(|| (obj.as_ref().clone(), name.clone())),
                _ => None,
            };
            if let Some((obj, method)) = method_call {
                // Explicit args start at arg_base+1 (skipping self).
                // Count = C-2: subtract 1 for the C-1 convention, 1
                // for the implicit self. Range is
                // `(arg_base+1)..(arg_base+1 + explicit_count)`.
                let explicit_arg_count = (inst.c as usize).saturating_sub(2);
                let explicit_end = arg_base
                    .saturating_add(1)
                    .saturating_add(explicit_arg_count as u8);
                let mut args = Vec::with_capacity(explicit_arg_count);
                for slot in (arg_base + 1)..explicit_end {
                    args.push(lookup_operand(ctx.slot_exprs, slot)?);
                }
                let result_count = inst.b() - 1;
                match result_count {
                    0 => {
                        // Result discarded — emit a bare method call.
                        stmts.push(Stmt::MethodCall { obj, method, args });
                    }
                    1 => {
                        // Single result stored at slot A. Route
                        // through assign_slot_ast so the destination
                        // is recognized as `local x = obj:m(...)`,
                        // `x = obj:m(...)`, or an unnamed temp.
                        let call_expr = Expr::MethodCall {
                            obj: Box::new(obj),
                            method,
                            args,
                        };
                        assign_slot_ast(
                            ctx.proto,
                            ctx.slot_exprs,
                            stmts,
                            inst.a,
                            call_expr,
                            real_idx,
                        );
                    }
                    _ => {
                        // Multiple return values — Stage 15.
                        return Err(DecompilerError::NotImplemented);
                    }
                }
            } else {
                // Regular call: C-1 args packed at
                // `arg_base..arg_base + arg_count`.
                let arg_count = (inst.c - 1) as usize;
                let args_end = arg_base.saturating_add(arg_count as u8);
                let mut args = Vec::with_capacity(arg_count);
                for slot in arg_base..args_end {
                    args.push(lookup_operand(ctx.slot_exprs, slot)?);
                }
                let result_count = inst.b() - 1;
                match result_count {
                    0 => {
                        // Result discarded — emit a bare call. No slot
                        // update needed: nothing was stored.
                        stmts.push(Stmt::Call { func, args });
                    }
                    1 => {
                        // Single result stored at slot A. Route
                        // through assign_slot_ast so the destination
                        // is recognized as `local x = f(...)`,
                        // `x = f(...)`, or an unnamed temp.
                        let call_expr = Expr::Call {
                            func: Box::new(func),
                            args,
                        };
                        assign_slot_ast(
                            ctx.proto,
                            ctx.slot_exprs,
                            stmts,
                            inst.a,
                            call_expr,
                            real_idx,
                        );
                    }
                    _ => {
                        // Multiple return values (e.g.
                        // `local a, b = f()`) — Stage 15.
                        return Err(DecompilerError::NotImplemented);
                    }
                }
            }
        }
        Opcode::Tgets => {
            // TGETS A B C — `R[A] = R[B][KSTR[reverse(C)]]`. C is a
            // reverse-indexed GC string constant (the field name).
            let gc = ctx.proto.gc_const_for_operand(inst.c as u16)?;
            let name = match gc {
                GcConst::Str(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                _ => return Err(DecompilerError::NotImplemented),
            };
            let obj = lookup_operand(ctx.slot_exprs, inst.b())?;
            let expr = Expr::Field {
                obj: Box::new(obj),
                name,
            };
            assign_slot_ast(ctx.proto, ctx.slot_exprs, stmts, inst.a, expr, real_idx);
        }
        Opcode::Tgetv => {
            // TGETV A B C — `R[A] = R[B][R[C]]`. Both B and C are
            // table/key registers.
            let obj = lookup_operand(ctx.slot_exprs, inst.b())?;
            let key = lookup_operand(ctx.slot_exprs, inst.c)?;
            let expr = Expr::Index {
                obj: Box::new(obj),
                key: Box::new(key),
            };
            assign_slot_ast(ctx.proto, ctx.slot_exprs, stmts, inst.a, expr, real_idx);
        }
        Opcode::Tgetb => {
            // TGETB A B C — `R[A] = R[B][C]`. C is an inline integer
            // literal key (NOT a register).
            let obj = lookup_operand(ctx.slot_exprs, inst.b())?;
            let expr = Expr::Index {
                obj: Box::new(obj),
                key: Box::new(Expr::Int(inst.c as i32)),
            };
            assign_slot_ast(ctx.proto, ctx.slot_exprs, stmts, inst.a, expr, real_idx);
        }
        _ => return Err(DecompilerError::NotImplemented),
    }
    Ok(())
}

/// Read a slot's [`Expr`] without removing it. Used by method-call
/// detection to inspect the self register without consuming it —
/// `lookup_operand` would also work but the missing-slot case here
/// is a `false` (not a method call) rather than an `Err`.
fn slot_exprs_get(slot_exprs: &HashMap<u8, Expr>, slot: u8) -> Option<&Expr> {
    slot_exprs.get(&slot)
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

// ---- ISxx condition building (Stage 9) ------------------------------

/// Build the [`Expr`] for an ISxx instruction's right operand.
///
/// The right operand's encoding depends on the opcode family (format
/// doc §5): a register for the V/V comparisons (`ISLT`, `ISGE`,
/// `ISLE`, `ISGT`, `ISEQV`, `ISNEV`), a reverse-indexed GC string
/// constant for `ISEQS`/`ISNES`, a forward-indexed number constant
/// for `ISEQN`/`ISNEN`, or an inline KPRI tag for `ISEQP`/`ISNEP`.
/// Returns [`DecompilerError::NotImplemented`] for opcodes outside
/// the ISxx family or for malformed constant references.
fn build_isxx_right_operand(
    proto: &Proto,
    slot_exprs: &HashMap<u8, Expr>,
    inst: &Instruction,
) -> Result<Expr, DecompilerError> {
    match inst.op {
        Opcode::Islt
        | Opcode::Isge
        | Opcode::Isle
        | Opcode::Isgt
        | Opcode::Iseqv
        | Opcode::Isnev => {
            // V/V: D is a register.
            lookup_operand(slot_exprs, inst.d() as u8)
        }
        Opcode::Iseqs | Opcode::Isnes => {
            // V/STR: D is a reverse-indexed GC string constant.
            let gc = proto.gc_const_for_operand(inst.d())?;
            match gc {
                GcConst::Str(bytes) => Ok(Expr::Str(bytes.clone())),
                _ => Err(DecompilerError::NotImplemented),
            }
        }
        Opcode::Iseqn | Opcode::Isnen => {
            // V/NUM: D is a forward index into num_consts.
            build_num_const_expr(proto, inst.d() as usize)
        }
        Opcode::Iseqp | Opcode::Isnep => {
            // V/PRI: D is an inline KPRI tag (0=nil, 1=false, 2=true).
            match inst.d() {
                0 => Ok(Expr::Nil),
                1 => Ok(Expr::False),
                2 => Ok(Expr::True),
                _ => Err(DecompilerError::NotImplemented),
            }
        }
        // IST/ISF are unary: there is no right operand.
        Opcode::Ist | Opcode::Isf => Err(DecompilerError::NotImplemented),
        _ => Err(DecompilerError::NotImplemented),
    }
}

/// Build the ISxx test expression — the condition the ISxx literally
/// checks, *without* negation. This is the expression that, when
/// true, causes the JMP to be taken.
///
/// For a single-CB if/then where the false_edge leads to the
/// then-body, the user's `if` condition is the **complement** of
/// the ISxx test (see [`build_condition`]). The test expression
/// surfaces directly only in the short-circuiting prefix of an OR
/// chain, where the JMP targets the then-body rather than the merge.
///
/// Operand layout per family (format doc §5):
/// - `ISLT`/`ISGE`/`ISLE`/`ISGT`/`ISEQV`/`ISNEV`: A and D are both
///   registers; the test is the obvious binop.
/// - `ISEQS`/`ISNES`/`ISEQN`/`ISNEN`/`ISEQP`/`ISNEP`: A is a
///   register, D indexes a constant.
/// - `IST`: tests "truthy(D)"; A is unused.
/// - `ISF`: tests "falsy(D)"; A is unused.
fn build_test_condition(
    proto: &Proto,
    slot_exprs: &HashMap<u8, Expr>,
    inst: &Instruction,
) -> Result<Expr, DecompilerError> {
    use Opcode::*;
    match inst.op {
        // Unary truthiness tests (A unused; D is the tested slot).
        Ist => lookup_operand(slot_exprs, inst.d() as u8),
        Isf => Ok(Expr::Not(Box::new(lookup_operand(
            slot_exprs,
            inst.d() as u8,
        )?))),
        // Binary comparisons: test = BinOp(op, A, D).
        Islt => isxx_binop(slot_exprs, inst, BinOpKind::LessThan),
        Isge => isxx_binop(slot_exprs, inst, BinOpKind::GreaterEqual),
        Isle => isxx_binop(slot_exprs, inst, BinOpKind::LessEqual),
        Isgt => isxx_binop(slot_exprs, inst, BinOpKind::GreaterThan),
        Iseqv => isxx_binop(slot_exprs, inst, BinOpKind::Equal),
        Isnev => isxx_binop(slot_exprs, inst, BinOpKind::NotEqual),
        // Constant-typed comparisons: resolve D through the
        // appropriate constant table.
        Iseqs => isxx_const(proto, slot_exprs, inst, BinOpKind::Equal),
        Isnes => isxx_const(proto, slot_exprs, inst, BinOpKind::NotEqual),
        Iseqn => isxx_const(proto, slot_exprs, inst, BinOpKind::Equal),
        Isnen => isxx_const(proto, slot_exprs, inst, BinOpKind::NotEqual),
        Iseqp => isxx_const(proto, slot_exprs, inst, BinOpKind::Equal),
        Isnep => isxx_const(proto, slot_exprs, inst, BinOpKind::NotEqual),
        _ => Err(DecompilerError::NotImplemented),
    }
}

/// Build the user's `if` condition — the **complement** of the ISxx
/// test.
///
/// Each ISxx tests a condition; if the test holds, the JMP is taken
/// (skipping the then-body for if/then). The user's `if` condition
/// is what's left when the test fails — i.e. the negation of the
/// test. LuaJIT's codegen chooses the ISxx variant that lets the
/// JMP encode the *negation* of the source condition, so e.g.
/// `if a == b then` lowers to `ISNEV` (test `a != b`; JMP when the
/// user's `==` is false).
///
/// For the comparison ops, complementing is just swapping to the
/// paired operator (`<` ↔ `>=`, `==` ↔ `~=`, etc. — see the table
/// in [`binop_complement`]). For IST/ISF, complementing swaps
/// truthy ↔ falsy (i.e. wraps/unwraps [`Expr::Not`]).
/// Build the if-condition expression from an ISxx instruction.
///
/// Each ISxx tests a condition; if the test holds, the JMP is taken
/// (skip then-body). The if-condition (what leads to the then-body) is
/// the **complement** of the ISxx test. This is LuaJIT's "twist":
/// `a < b` compiles to `ISGE` (tests >=), `a == b` compiles to `ISNEV`
/// (tests ~=).
///
/// **LuaJIT canonicalizes ordered comparisons to `<`/`<=`** by swapping
/// operands when needed. So `a > b` in source becomes `b < a` in
/// bytecode, and the decompiler faithfully reproduces the canonicalized
/// form (`b < a`) rather than the original (`a > b`). This is semantically
/// correct but non-canonical — a known limitation.
fn build_condition(
    proto: &Proto,
    slot_exprs: &HashMap<u8, Expr>,
    inst: &Instruction,
) -> Result<Expr, DecompilerError> {
    use Opcode::*;
    match inst.op {
        // Unary: complement of IST (truthy) is falsy = Not(val);
        // complement of ISF (falsy) is truthy = val.
        Ist => Ok(Expr::Not(Box::new(lookup_operand(
            slot_exprs,
            inst.d() as u8,
        )?))),
        Isf => lookup_operand(slot_exprs, inst.d() as u8),
        // Binary comparisons: condition = BinOp(complement(op), A, D).
        Islt => isxx_binop(slot_exprs, inst, BinOpKind::GreaterEqual),
        Isge => isxx_binop(slot_exprs, inst, BinOpKind::LessThan),
        Isle => isxx_binop(slot_exprs, inst, BinOpKind::GreaterThan),
        Isgt => isxx_binop(slot_exprs, inst, BinOpKind::LessEqual),
        Iseqv => isxx_binop(slot_exprs, inst, BinOpKind::NotEqual),
        Isnev => isxx_binop(slot_exprs, inst, BinOpKind::Equal),
        Iseqs => isxx_const(proto, slot_exprs, inst, BinOpKind::NotEqual),
        Isnes => isxx_const(proto, slot_exprs, inst, BinOpKind::Equal),
        Iseqn => isxx_const(proto, slot_exprs, inst, BinOpKind::NotEqual),
        Isnen => isxx_const(proto, slot_exprs, inst, BinOpKind::Equal),
        Iseqp => isxx_const(proto, slot_exprs, inst, BinOpKind::NotEqual),
        Isnep => isxx_const(proto, slot_exprs, inst, BinOpKind::Equal),
        _ => Err(DecompilerError::NotImplemented),
    }
}

/// Build a register/register comparison [`Expr`] for an ISxx that
/// takes two register operands (ISLT/ISGE/ISLE/ISGT/ISEQV/ISNEV).
fn isxx_binop(
    slot_exprs: &HashMap<u8, Expr>,
    inst: &Instruction,
    op: BinOpKind,
) -> Result<Expr, DecompilerError> {
    let left = lookup_operand(slot_exprs, inst.a)?;
    let right = lookup_operand(slot_exprs, inst.d() as u8)?;
    Ok(Expr::BinOp {
        op,
        left: Box::new(left),
        right: Box::new(right),
    })
}

/// Build a register/constant comparison [`Expr`] for an ISxx whose
/// right operand is a constant (ISEQS/ISNES/ISEQN/ISNEN/ISEQP/ISNEP).
fn isxx_const(
    proto: &Proto,
    slot_exprs: &HashMap<u8, Expr>,
    inst: &Instruction,
    op: BinOpKind,
) -> Result<Expr, DecompilerError> {
    let left = lookup_operand(slot_exprs, inst.a)?;
    let right = build_isxx_right_operand(proto, slot_exprs, inst)?;
    Ok(Expr::BinOp {
        op,
        left: Box::new(left),
        right: Box::new(right),
    })
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
    use crate::cfg::{BasicBlock, Cfg};
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
    ///
    /// The header flags include `FLAG_FR2` so [`crate::emit::emit_module`]
    /// threads FR2 through [`recover`] — the test bytecode mirrors
    /// system-luajit output (CALL args at A+2, with the gap at A+1).
    fn module_with(
        real_insts: Vec<Instruction>,
        var_info: Vec<VarInfo>,
        gc_consts: Vec<GcConst>,
        num_consts: Vec<NumConst>,
        framesize: u8,
    ) -> Module {
        use crate::ir::FLAG_FR2;
        let mut insts = vec![Instruction::synthetic_header(Opcode::Funcv, framesize)];
        insts.extend(real_insts);
        Module {
            header: ModuleHeader {
                flags: FLAG_FR2,
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
        let ast = recover(proto, &cfg, true).expect("recover should succeed");

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

    /// Stage 9: ISLT now lowers to the complement comparison
    /// (`>=`). The helper builds `GGET 0 0; ISLT 0 0` — both
    /// operands resolve to `Global("x")` — so the user's condition
    /// is `x >= x`. (Stage 7-8 rejected this with NotImplemented;
    /// Stage 9 supports it.)
    #[test]
    fn recover_islt_compound_condition() {
        let module = if_then_module_with_test_op(
            Opcode::Islt,
            vec![ad(Opcode::Kshort, 0, 1), ad(Opcode::Ret1, 0, 2)],
            vec![GcConst::Str(b"x".to_vec())],
        );
        assert_eq!(emit(&module), "if x >= x then\n    return 1\nend");
    }

    /// `if x then return 1 else return 2 end` — the canonical
    /// Stage 8 if/else fixture (both branches return). Bytecode:
    ///   GGET 0 0; ISF 0; JMP => 0007;
    ///   KSHORT 0 1; RET1;          (then-body, returns)
    ///   JMP => 0009;               (dead jump after RET1)
    ///   KSHORT 0 2; RET1;          (else-body, returns)
    ///   RET0 0 1.                  (merge: implicit return)
    ///
    /// The dead JMP at idx 6 is unreachable (the RET1 already
    /// returned), but LuaJIT always emits it as part of the
    /// if/else codegen pattern. [`branch_shape`] uses it to identify
    /// the merge; the recovery never processes its instructions.
    #[test]
    fn recover_if_else_both_return() {
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
        assert_eq!(
            emit(&module),
            "if x then\n    return 1\nelse\n    return 2\nend"
        );
    }

    /// Same fixture recovered directly to AST (not via emit_module).
    /// Verifies the Stmt tree shape:
    ///   If { cond: Global("x"),
    ///       then_body: [Return(Some(Int(1)))],
    ///       else_body: Some([Return(Some(Int(2)))]) }
    /// The merge's implicit RET0 produces no Stmt.
    #[test]
    fn recover_if_else_both_return_ast_shape() {
        let insts = vec![
            ad(Opcode::Gget, 0, 0),     // idx 1
            ad(Opcode::Isf, 0, 0),      // idx 2
            ad(Opcode::Jmp, 1, 0x8003), // idx 3
            ad(Opcode::Kshort, 0, 1),   // idx 4
            ad(Opcode::Ret1, 0, 2),     // idx 5
            ad(Opcode::Jmp, 0, 0x8002), // idx 6: dead
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
        let proto = module.main_proto();
        let cfg = Cfg::build(proto);
        let ast = recover(proto, &cfg, true).expect("recover should succeed");

        assert_eq!(ast.len(), 1, "expected just [If] (implicit RET0 → no Stmt)");
        match &ast[0] {
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                assert_eq!(*cond, Expr::Global("x".to_string()), "cond");
                assert_eq!(then_body.len(), 1, "then_body len");
                match &then_body[0] {
                    Stmt::Return(Some(expr)) => {
                        assert_eq!(*expr, Expr::Int(1), "then return expr");
                    }
                    other => panic!("expected Return(Some(Int(1))), got {:?}", other),
                }
                let else_stmts = else_body.as_ref().expect("else_body should be populated");
                assert_eq!(else_stmts.len(), 1, "else_body len");
                match &else_stmts[0] {
                    Stmt::Return(Some(expr)) => {
                        assert_eq!(*expr, Expr::Int(2), "else return expr");
                    }
                    other => panic!("expected Return(Some(Int(2))), got {:?}", other),
                }
            }
            other => panic!("expected If, got {:?}", other),
        }
    }

    /// `local y = 0; if x then y = 1 else y = 2 end; return y` —
    /// the if/else fallthrough fixture (both branches fall through
    /// to the merge). Bytecode:
    ///   KSHORT 0 0; GGET 1 0; ISF 1; JMP => 0007;
    ///   KSHORT 0 1; JMP => 0008;       (then-body, LIVE skip-else)
    ///   KSHORT 0 2;                    (else-body)
    ///   RET1 0 2.                      (merge: return y)
    ///
    /// Exercises:
    /// 1. The live "skip-else" JMP at the end of the then-body
    ///    ([`Terminator::Jump`] with `stop_at_merge == true`).
    /// 2. The else-body falling through into a merge with two preds.
    /// 3. Slot tracking across branches: both branches reassign
    ///    slot 0 to `Var("y")`, so the merge's `RET1 0 2` reads
    ///    `Var("y")` and emits `return y`.
    #[test]
    fn recover_if_else_fallthrough() {
        let insts = vec![
            ad(Opcode::Kshort, 0, 0),   // idx 1 — y = 0
            ad(Opcode::Gget, 1, 0),     // idx 2 — load x
            ad(Opcode::Isf, 0, 1),      // idx 3 — tests slot 1 (x)
            ad(Opcode::Jmp, 2, 0x8002), // idx 4: target = idx 7 (else)
            ad(Opcode::Kshort, 0, 1),   // idx 5 — then: y = 1
            ad(Opcode::Jmp, 1, 0x8001), // idx 6: target = idx 8 (merge)
            ad(Opcode::Kshort, 0, 2),   // idx 7 — else: y = 2
            ad(Opcode::Ret1, 0, 2),     // idx 8 — merge: return y
        ];
        let module = module_with(
            insts,
            // y is declared at real-idx 0 (the first KSHORT); scope
            // covers the whole chunk so both then-body and else-body
            // KSHORTs read as reassignments.
            vec![named_local(0, "y", 0, 7)],
            vec![GcConst::Str(b"x".to_vec())],
            Vec::new(),
            2,
        );
        assert_eq!(
            emit(&module),
            "local y = 0\nif x then\n    y = 1\nelse\n    y = 2\nend\nreturn y"
        );
    }

    /// The detection algorithm ([`branch_shape`]) must distinguish
    /// if/then from if/else based on the then-body's terminal
    /// block. This test exercises all four detection paths directly:
    ///
    /// 1. **If/else via live Jump**: then-body's terminator is
    ///    `Jump(merge)` → if/else, merge = Jump target.
    /// 2. **If/else via dead Jump**: then-body's terminator is
    ///    `Return`, and the block immediately after has `Jump(merge)`
    ///    → if/else, merge = dead Jump target.
    /// 3. **If/then via Return (no dead Jump)**: then-body's
    ///    terminator is `Return`, no dead Jump follows → if/then,
    ///    merge = entry's true_edge.
    /// 4. **If/then via Fallthrough**: then-body's terminator is
    ///    `Fallthrough(merge)` → if/then, merge = fallthrough target.
    #[test]
    fn recover_if_else_detection() {
        // Case 1: live Jump (if/else fallthrough). Build the CFG
        // directly and call branch_shape.
        //   KSHORT 0 0; GGET 1 0; ISF 1; JMP => 0007;  (entry)
        //   KSHORT 0 1; JMP => 0008;                  (then-body, Jump)
        //   KSHORT 0 2;                               (else-body)
        //   RET1 0 2.                                 (merge)
        let cfg = Cfg::build(&Proto {
            flags: 0,
            numparams: 0,
            framesize: 2,
            upvalues: Vec::new(),
            gc_consts: vec![GcConst::Str(b"x".to_vec())],
            num_consts: Vec::new(),
            insts: {
                let mut v = vec![Instruction::synthetic_header(Opcode::Funcv, 2)];
                v.extend([
                    ad(Opcode::Kshort, 0, 0),
                    ad(Opcode::Gget, 1, 0),
                    ad(Opcode::Isf, 0, 1),
                    ad(Opcode::Jmp, 2, 0x8002),
                    ad(Opcode::Kshort, 0, 1),
                    ad(Opcode::Jmp, 1, 0x8001),
                    ad(Opcode::Kshort, 0, 2),
                    ad(Opcode::Ret1, 0, 2),
                ]);
                v
            },
            debug: None,
        });
        // entry = Block 0, then-body = Block 1, else-body = Block 2,
        // merge = Block 3. Then-body's terminator is Jump(Block 3).
        let (else_start, merge) = branch_shape(&cfg, BlockId(1), BlockId(2)).unwrap();
        assert_eq!(else_start, Some(BlockId(2)), "live Jump → if/else");
        assert_eq!(merge, BlockId(3), "merge = Jump target");

        // Case 2: dead Jump (if/else both return). Build the CFG.
        //   GGET 0 0; ISF 0; JMP => 0007;  (entry)
        //   KSHORT 0 1; RET1;              (then-body, Return)
        //   JMP => 0009;                   (dead Jump)
        //   KSHORT 0 2; RET1;              (else-body)
        //   RET0 0 1.                      (merge)
        let cfg = Cfg::build(&Proto {
            flags: 0,
            numparams: 0,
            framesize: 1,
            upvalues: Vec::new(),
            gc_consts: vec![GcConst::Str(b"x".to_vec())],
            num_consts: Vec::new(),
            insts: {
                let mut v = vec![Instruction::synthetic_header(Opcode::Funcv, 1)];
                v.extend([
                    ad(Opcode::Gget, 0, 0),
                    ad(Opcode::Isf, 0, 0),
                    ad(Opcode::Jmp, 1, 0x8003),
                    ad(Opcode::Kshort, 0, 1),
                    ad(Opcode::Ret1, 0, 2),
                    ad(Opcode::Jmp, 0, 0x8002),
                    ad(Opcode::Kshort, 0, 2),
                    ad(Opcode::Ret1, 0, 2),
                    ad(Opcode::Ret0, 0, 1),
                ]);
                v
            },
            debug: None,
        });
        // entry = Block 0, then-body = Block 1, dead-jump = Block 2,
        // else-body = Block 3, merge = Block 4.
        let (else_start, merge) = branch_shape(&cfg, BlockId(1), BlockId(3)).unwrap();
        assert_eq!(else_start, Some(BlockId(3)), "dead Jump → if/else");
        assert_eq!(merge, BlockId(4), "merge = dead Jump target");

        // Case 3: if/then return (no dead Jump). Build the CFG.
        //   GGET 0 0; ISF 0; JMP => 0006;  (entry)
        //   KSHORT 0 1; RET1;              (then-body, Return)
        //   RET0 0 1.                      (merge = true_edge)
        let cfg = Cfg::build(&Proto {
            flags: 0,
            numparams: 0,
            framesize: 1,
            upvalues: Vec::new(),
            gc_consts: vec![GcConst::Str(b"x".to_vec())],
            num_consts: Vec::new(),
            insts: {
                let mut v = vec![Instruction::synthetic_header(Opcode::Funcv, 1)];
                v.extend([
                    ad(Opcode::Gget, 0, 0),
                    ad(Opcode::Isf, 0, 0),
                    ad(Opcode::Jmp, 1, 0x8002),
                    ad(Opcode::Kshort, 0, 1),
                    ad(Opcode::Ret1, 0, 2),
                    ad(Opcode::Ret0, 0, 1),
                ]);
                v
            },
            debug: None,
        });
        // entry = Block 0, then-body = Block 1, merge = Block 2.
        let (else_start, merge) = branch_shape(&cfg, BlockId(1), BlockId(2)).unwrap();
        assert_eq!(else_start, None, "no dead Jump → if/then");
        assert_eq!(merge, BlockId(2), "merge = entry true_edge");

        // Case 4: if/then fallthrough. Build the CFG.
        //   GGET 0 0; ISF 0; JMP => 0005;  (entry)
        //   KSHORT 0 1;                    (then-body, Fallthrough)
        //   RET1 0 2.                      (merge)
        let cfg = Cfg::build(&Proto {
            flags: 0,
            numparams: 0,
            framesize: 1,
            upvalues: Vec::new(),
            gc_consts: vec![GcConst::Str(b"x".to_vec())],
            num_consts: Vec::new(),
            insts: {
                let mut v = vec![Instruction::synthetic_header(Opcode::Funcv, 1)];
                v.extend([
                    ad(Opcode::Gget, 0, 0),
                    ad(Opcode::Isf, 0, 0),
                    ad(Opcode::Jmp, 1, 0x8001),
                    ad(Opcode::Kshort, 0, 1),
                    ad(Opcode::Ret1, 0, 2),
                ]);
                v
            },
            debug: None,
        });
        // entry = Block 0, then-body = Block 1, merge = Block 2.
        // Then-body's terminator is Fallthrough(Block 2).
        let (else_start, merge) = branch_shape(&cfg, BlockId(1), BlockId(2)).unwrap();
        assert_eq!(else_start, None, "Fallthrough → if/then");
        assert_eq!(merge, BlockId(2), "merge = Fallthrough target");
    }

    /// Nested if (a ConditionalBranch inside a then-body) isn't
    /// supported. The recovery bails in [`recover`] via the
    /// ConditionalBranch-count check before the walk even starts.
    #[test]
    fn recover_nested_if_is_not_supported() {
        // Outer if/then with an inner if/then as the body:
        //   GGET 0 0; ISF 0; JMP => 0009  (outer)
        //   GGET 1 1; ISF 1; JMP => 0008  (inner)
        //   KSHORT 0 1; RET1               (inner then-body)
        //   RET0                            (inner merge = outer body end)
        //   RET0                            (outer merge)
        // The CFG has two ConditionalBranch terminators (outer + inner),
        // so recover() rejects it up front via the count check.
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
        // that the CFG has two ConditionalBranch blocks — that
        // triggers the count check's NotImplemented.
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

    /// Stage 9: ISGE now lowers to the complement comparison (`<`).
    /// With the test helper's `GGET 0 0; ISGE 0 0` layout the user's
    /// condition is `x < x`. (Stage 7-8 rejected this with
    /// NotImplemented; Stage 9 supports it.)
    #[test]
    fn recover_isge_compound_condition() {
        let module = if_then_module_with_test_op(
            Opcode::Isge,
            vec![ad(Opcode::Kshort, 0, 1), ad(Opcode::Ret1, 0, 2)],
            vec![GcConst::Str(b"x".to_vec())],
        );
        assert_eq!(emit(&module), "if x < x then\n    return 1\nend");
    }

    // ====================================================================
    // Stage 9: compound conditions — direct unit tests
    // ====================================================================
    //
    // The integration tests in `tests/stage9_conditions.rs` cover the
    // end-to-end pipeline (parse → CFG → recover → emit). The unit
    // tests below isolate the new condition-building helpers
    // ([`build_test_condition`], [`build_condition`]) and the
    // chain-detection logic so failures point at the right layer.

    /// Slot-expr map populated with two globals `a` and `b` in
    /// slots 0 and 1 — the typical precondition for a comparison
    /// `if a <op> b then`. Helpers below share it via this setup.
    fn slots_ab() -> HashMap<u8, Expr> {
        let mut m = HashMap::new();
        m.insert(0, Expr::Global("a".to_string()));
        m.insert(1, Expr::Global("b".to_string()));
        m
    }

    // ---- build_test_condition: ISxx test (no negation) ---------------

    #[test]
    fn build_test_condition_islt_yields_less_than() {
        // ISLT 0 1 → R[0] < R[1] → `a < b`.
        let slots = slots_ab();
        let inst = ad(Opcode::Islt, 0, 1);
        assert_eq!(
            build_test_condition(&empty_proto(), &slots, &inst).unwrap(),
            Expr::BinOp {
                op: BinOpKind::LessThan,
                left: Box::new(Expr::Global("a".to_string())),
                right: Box::new(Expr::Global("b".to_string())),
            }
        );
    }

    #[test]
    fn build_test_condition_iseqv_yields_equal() {
        // ISEQV 0 1 → R[0] == R[1] → `a == b`.
        let slots = slots_ab();
        let inst = ad(Opcode::Iseqv, 0, 1);
        assert_eq!(
            build_test_condition(&empty_proto(), &slots, &inst).unwrap(),
            Expr::BinOp {
                op: BinOpKind::Equal,
                left: Box::new(Expr::Global("a".to_string())),
                right: Box::new(Expr::Global("b".to_string())),
            }
        );
    }

    #[test]
    fn build_test_condition_ist_yields_value() {
        // IST 0 → truthy(R[0]) → just `a` (no wrapping).
        let slots = slots_ab();
        let inst = ad(Opcode::Ist, 0, 0);
        assert_eq!(
            build_test_condition(&empty_proto(), &slots, &inst).unwrap(),
            Expr::Global("a".to_string())
        );
    }

    #[test]
    fn build_test_condition_isf_yields_not_value() {
        // ISF 0 → falsy(R[0]) → `not a`.
        let slots = slots_ab();
        let inst = ad(Opcode::Isf, 0, 0);
        assert_eq!(
            build_test_condition(&empty_proto(), &slots, &inst).unwrap(),
            Expr::Not(Box::new(Expr::Global("a".to_string())))
        );
    }

    // ---- build_condition: ISxx test complement (user's if-cond) ------

    #[test]
    fn build_condition_islt_yields_greater_equal() {
        // ISLT test is `<`; complement is `>=`.
        let slots = slots_ab();
        let inst = ad(Opcode::Islt, 0, 1);
        assert_eq!(
            build_condition(&empty_proto(), &slots, &inst).unwrap(),
            Expr::BinOp {
                op: BinOpKind::GreaterEqual,
                left: Box::new(Expr::Global("a".to_string())),
                right: Box::new(Expr::Global("b".to_string())),
            }
        );
    }

    #[test]
    fn build_condition_isge_yields_less_than() {
        // ISGE test is `>=`; complement is `<`.
        let slots = slots_ab();
        let inst = ad(Opcode::Isge, 0, 1);
        assert_eq!(
            build_condition(&empty_proto(), &slots, &inst).unwrap(),
            Expr::BinOp {
                op: BinOpKind::LessThan,
                left: Box::new(Expr::Global("a".to_string())),
                right: Box::new(Expr::Global("b".to_string())),
            }
        );
    }

    #[test]
    fn build_condition_iseqv_yields_not_equal() {
        // ISEQV test is `==`; complement is `~=`.
        let slots = slots_ab();
        let inst = ad(Opcode::Iseqv, 0, 1);
        assert_eq!(
            build_condition(&empty_proto(), &slots, &inst).unwrap(),
            Expr::BinOp {
                op: BinOpKind::NotEqual,
                left: Box::new(Expr::Global("a".to_string())),
                right: Box::new(Expr::Global("b".to_string())),
            }
        );
    }

    #[test]
    fn build_condition_isnev_yields_equal() {
        // ISNEV test is `!=`; complement is `==`.
        let slots = slots_ab();
        let inst = ad(Opcode::Isnev, 0, 1);
        assert_eq!(
            build_condition(&empty_proto(), &slots, &inst).unwrap(),
            Expr::BinOp {
                op: BinOpKind::Equal,
                left: Box::new(Expr::Global("a".to_string())),
                right: Box::new(Expr::Global("b".to_string())),
            }
        );
    }

    #[test]
    fn build_condition_isle_isgt_pair() {
        // ISLE test is `<=`; complement is `>`. ISGT test is `>`;
        // complement is `<=`. Verify both directions.
        let slots = slots_ab();
        let isle = ad(Opcode::Isle, 0, 1);
        assert_eq!(
            build_condition(&empty_proto(), &slots, &isle).unwrap(),
            Expr::BinOp {
                op: BinOpKind::GreaterThan,
                left: Box::new(Expr::Global("a".to_string())),
                right: Box::new(Expr::Global("b".to_string())),
            }
        );
        let isgt = ad(Opcode::Isgt, 0, 1);
        assert_eq!(
            build_condition(&empty_proto(), &slots, &isgt).unwrap(),
            Expr::BinOp {
                op: BinOpKind::LessEqual,
                left: Box::new(Expr::Global("a".to_string())),
                right: Box::new(Expr::Global("b".to_string())),
            }
        );
    }

    #[test]
    fn build_condition_ist_isf_unary_pair() {
        // IST test is `truthy`; complement is `not val`.
        // ISF test is `falsy`; complement is `val`.
        let slots = slots_ab();
        let ist = ad(Opcode::Ist, 0, 0);
        assert_eq!(
            build_condition(&empty_proto(), &slots, &ist).unwrap(),
            Expr::Not(Box::new(Expr::Global("a".to_string())))
        );
        let isf = ad(Opcode::Isf, 0, 0);
        assert_eq!(
            build_condition(&empty_proto(), &slots, &isf).unwrap(),
            Expr::Global("a".to_string())
        );
    }

    #[test]
    fn build_condition_isxx_const_typed_operands() {
        // ISEQN/ISNEN/ISEQS/ISNES/ISEQP/ISNEP — verify constant
        // operand resolution (KNUM forward, KSTR reverse, KPRI inline)
        // by exercising the right side of the resulting BinOp.
        let slots = slots_ab();
        let proto = Proto {
            gc_consts: vec![GcConst::Str(b"foo".to_vec())],
            num_consts: vec![NumConst::Int(42)],
            ..empty_proto()
        };
        // ISEQN A=0 D=0 → KNUM[0] = Int(42). Condition: NotEqual.
        let iseqn = ad(Opcode::Iseqn, 0, 0);
        assert_eq!(
            build_condition(&proto, &slots, &iseqn).unwrap(),
            Expr::BinOp {
                op: BinOpKind::NotEqual,
                left: Box::new(Expr::Global("a".to_string())),
                right: Box::new(Expr::Int(42)),
            }
        );
        // ISNES A=0 D=0 → KSTR reverse[0] = Str("foo"). Condition: Equal.
        let isnes = ad(Opcode::Isnes, 0, 0);
        assert_eq!(
            build_condition(&proto, &slots, &isnes).unwrap(),
            Expr::BinOp {
                op: BinOpKind::Equal,
                left: Box::new(Expr::Global("a".to_string())),
                right: Box::new(Expr::Str(b"foo".to_vec())),
            }
        );
        // ISEQP A=0 D=2 → KPRI tag 2 = True. Condition: NotEqual.
        let iseqp = ad(Opcode::Iseqp, 0, 2);
        assert_eq!(
            build_condition(&proto, &slots, &iseqp).unwrap(),
            Expr::BinOp {
                op: BinOpKind::NotEqual,
                left: Box::new(Expr::Global("a".to_string())),
                right: Box::new(Expr::True),
            }
        );
    }

    #[test]
    fn build_condition_bails_on_missing_operand_slot() {
        // Slot 2 has no recorded expression → NotImplemented.
        let slots = slots_ab();
        let inst = ad(Opcode::Iseqv, 0, 2);
        let result = build_condition(&empty_proto(), &slots, &inst);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for missing slot, got {:?}",
            result
        );
    }

    // ---- chain detection: AND vs OR -----------------------------------

    /// Build a module mirroring `if a and b then return 1 end`. Two
    /// ISF+JMP pairs, both with JMP target → merge.
    fn and_chain_module() -> Module {
        //   GGET 0 0; ISF 0; JMP => 0009;     (CB1)
        //   GGET 0 1; ISF 0; JMP => 0009;     (CB2)
        //   KSHORT 0 1; RET1;                 (then-body)
        //   RET0.                              (merge)
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Isf, 0, 0),
            ad(Opcode::Jmp, 1, 0x8005), // idx 3: target = idx 9
            ad(Opcode::Gget, 0, 1),
            ad(Opcode::Isf, 0, 0),
            ad(Opcode::Jmp, 1, 0x8002), // idx 6: target = idx 9
            ad(Opcode::Kshort, 0, 1),
            ad(Opcode::Ret1, 0, 2),
            ad(Opcode::Ret0, 0, 1),
        ];
        // GC constants are referenced via reverse index
        // (gc_consts[len-1-D]). With operands 0 and 1 mapping to "a"
        // and "b" respectively, the file order is [b, a].
        module_with(
            insts,
            Vec::new(),
            vec![GcConst::Str(b"b".to_vec()), GcConst::Str(b"a".to_vec())],
            Vec::new(),
            1,
        )
    }

    /// Build a module mirroring `if a or b then return 1 end`. An
    /// IST+JMP that short-circuits to the then-body, followed by an
    /// ISF+JMP that skips to the merge.
    fn or_chain_module() -> Module {
        //   GGET 0 0; IST 0; JMP => 0007;     (CB1, true_edge → then-body)
        //   GGET 0 1; ISF 0; JMP => 0009;     (CB2, true_edge → merge)
        //   KSHORT 0 1; RET1;                 (then-body)
        //   RET0.                              (merge)
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Ist, 0, 0),
            ad(Opcode::Jmp, 1, 0x8003), // idx 3: target = idx 7
            ad(Opcode::Gget, 0, 1),
            ad(Opcode::Isf, 0, 0),
            ad(Opcode::Jmp, 1, 0x8002), // idx 6: target = idx 9
            ad(Opcode::Kshort, 0, 1),
            ad(Opcode::Ret1, 0, 2),
            ad(Opcode::Ret0, 0, 1),
        ];
        // Reverse-indexed: file order [b, a] → operand 0 is "a".
        module_with(
            insts,
            Vec::new(),
            vec![GcConst::Str(b"b".to_vec()), GcConst::Str(b"a".to_vec())],
            Vec::new(),
            1,
        )
    }

    #[test]
    fn recover_and_chain_emits_and_expression() {
        let module = and_chain_module();
        let proto = module.main_proto();
        let cfg = Cfg::build(proto);
        let ast = recover(proto, &cfg, true).expect("recover should succeed");
        assert_eq!(ast.len(), 1, "expected just [If]");
        match &ast[0] {
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                // Each ISF's complement is the bare value, combined
                // with And → `a and b`.
                assert_eq!(
                    cond.clone(),
                    Expr::And(
                        Box::new(Expr::Global("a".to_string())),
                        Box::new(Expr::Global("b".to_string())),
                    ),
                    "cond"
                );
                assert_eq!(*else_body, None, "else_body");
                assert_eq!(then_body.len(), 1, "then_body len");
                match &then_body[0] {
                    Stmt::Return(Some(expr)) => assert_eq!(expr.clone(), Expr::Int(1)),
                    other => panic!("expected Return(Some(Int(1))), got {:?}", other),
                }
            }
            other => panic!("expected If, got {:?}", other),
        }
    }

    #[test]
    fn recover_or_chain_emits_or_expression() {
        let module = or_chain_module();
        let proto = module.main_proto();
        let cfg = Cfg::build(proto);
        let ast = recover(proto, &cfg, true).expect("recover should succeed");
        assert_eq!(ast.len(), 1, "expected just [If]");
        match &ast[0] {
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                // First CB (IST, short-circuit) contributes the test
                // verbatim → `a`. Last CB (ISF, skip-to-merge)
                // contributes the complement → `b`. Joined with Or.
                assert_eq!(
                    cond.clone(),
                    Expr::Or(
                        Box::new(Expr::Global("a".to_string())),
                        Box::new(Expr::Global("b".to_string())),
                    ),
                    "cond"
                );
                assert_eq!(*else_body, None, "else_body");
                assert_eq!(then_body.len(), 1, "then_body len");
                match &then_body[0] {
                    Stmt::Return(Some(expr)) => assert_eq!(expr.clone(), Expr::Int(1)),
                    other => panic!("expected Return(Some(Int(1))), got {:?}", other),
                }
            }
            other => panic!("expected If, got {:?}", other),
        }
    }

    /// `if a == b then return 1 end` lowers to ISNEV; the user's
    /// condition is the complement `Equal(a, b)`. End-to-end through
    /// emit, isolating the single-comparison path (no chain).
    #[test]
    fn recover_single_isnev_comparison_emits_equal() {
        //   GGET 0 0; GGET 1 1; ISNEV 0 1; JMP => 0007;
        //   KSHORT 0 1; RET1; RET0.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Gget, 1, 1),
            ad(Opcode::Isnev, 0, 1),
            ad(Opcode::Jmp, 1, 0x8002), // idx 4: target = idx 7
            ad(Opcode::Kshort, 0, 1),
            ad(Opcode::Ret1, 0, 2),
            ad(Opcode::Ret0, 0, 1),
        ];
        // Reverse-indexed: file order [b, a].
        let module = module_with(
            insts,
            Vec::new(),
            vec![GcConst::Str(b"b".to_vec()), GcConst::Str(b"a".to_vec())],
            Vec::new(),
            2,
        );
        assert_eq!(emit(&module), "if a == b then\n    return 1\nend");
    }

    // ---- chain-detection edge cases ----------------------------------

    /// `if a or b or c then return 1 end` — three-CB OR chain.
    /// Verifies the chain algorithm generalizes beyond 2 CBs: the
    /// first N-1 CBs short-circuit (test condition verbatim), the
    /// last CB contributes its complement.
    #[test]
    fn recover_three_term_or_chain() {
        //   GGET 0 0; IST 0; JMP => 0010;     (CB1 → then-body)
        //   GGET 0 1; IST 0; JMP => 0010;     (CB2 → then-body)
        //   GGET 0 2; ISF 0; JMP => 0012;     (CB3 → merge)
        //   KSHORT 0 1; RET1; RET0.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),     // idx 1
            ad(Opcode::Ist, 0, 0),      // idx 2
            ad(Opcode::Jmp, 1, 0x8006), // idx 3: target = idx 10
            ad(Opcode::Gget, 0, 1),     // idx 4
            ad(Opcode::Ist, 0, 0),      // idx 5
            ad(Opcode::Jmp, 1, 0x8003), // idx 6: target = idx 10
            ad(Opcode::Gget, 0, 2),     // idx 7
            ad(Opcode::Isf, 0, 0),      // idx 8
            ad(Opcode::Jmp, 1, 0x8002), // idx 9: target = idx 12
            ad(Opcode::Kshort, 0, 1),   // idx 10 (then-body)
            ad(Opcode::Ret1, 0, 2),     // idx 11
            ad(Opcode::Ret0, 0, 1),     // idx 12 (merge)
        ];
        // Reverse-indexed: file order [c, b, a] → operands 0,1,2 map
        // to a, b, c.
        let module = module_with(
            insts,
            Vec::new(),
            vec![
                GcConst::Str(b"c".to_vec()),
                GcConst::Str(b"b".to_vec()),
                GcConst::Str(b"a".to_vec()),
            ],
            Vec::new(),
            1,
        );
        assert_eq!(emit(&module), "if a or b or c then\n    return 1\nend");
    }

    /// `if a and b and c then return 1 end` — three-CB AND chain.
    #[test]
    fn recover_three_term_and_chain() {
        //   GGET 0 0; ISF 0; JMP => 0012;     (CB1 → merge)
        //   GGET 0 1; ISF 0; JMP => 0012;     (CB2 → merge)
        //   GGET 0 2; ISF 0; JMP => 0012;     (CB3 → merge)
        //   KSHORT 0 1; RET1; RET0.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),     // idx 1
            ad(Opcode::Isf, 0, 0),      // idx 2
            ad(Opcode::Jmp, 1, 0x8008), // idx 3: target = idx 12
            ad(Opcode::Gget, 0, 1),     // idx 4
            ad(Opcode::Isf, 0, 0),      // idx 5
            ad(Opcode::Jmp, 1, 0x8005), // idx 6: target = idx 12
            ad(Opcode::Gget, 0, 2),     // idx 7
            ad(Opcode::Isf, 0, 0),      // idx 8
            ad(Opcode::Jmp, 1, 0x8002), // idx 9: target = idx 12
            ad(Opcode::Kshort, 0, 1),   // idx 10 (then-body)
            ad(Opcode::Ret1, 0, 2),     // idx 11
            ad(Opcode::Ret0, 0, 1),     // idx 12 (merge)
        ];
        // Reverse-indexed: file order [c, b, a].
        let module = module_with(
            insts,
            Vec::new(),
            vec![
                GcConst::Str(b"c".to_vec()),
                GcConst::Str(b"b".to_vec()),
                GcConst::Str(b"a".to_vec()),
            ],
            Vec::new(),
            1,
        );
        assert_eq!(emit(&module), "if a and b and c then\n    return 1\nend");
    }

    /// Mixed `and`/`or` chains are not supported in Stage 9 — the chain
    /// detection requires uniform true_edge targets (all → merge for AND,
    /// first → then-body for OR). A mixed chain has heterogeneous
    /// true_edge targets, which neither invariant matches, so the
    /// recovery correctly returns NotImplemented.
    #[test]
    fn recover_mixed_and_or_chain_bails() {
        // CB1: ISF a; JMP → merge (true_edge = merge).
        // CB2: ISF b; JMP → merge (true_edge = merge).
        // CB3: IST c; JMP → then-body (true_edge = then-body).
        // The AND invariant fails (CB3.true != merge) and the OR
        // invariant fails (CB1.true != then-body). → NotImplemented.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),     // idx 1: a
            ad(Opcode::Isf, 0, 0),      // idx 2
            ad(Opcode::Jmp, 1, 0x8008), // idx 3: target = idx 12 (merge)
            ad(Opcode::Gget, 0, 1),     // idx 4: b
            ad(Opcode::Isf, 0, 0),      // idx 5
            ad(Opcode::Jmp, 1, 0x8005), // idx 6: target = idx 12 (merge)
            ad(Opcode::Gget, 0, 2),     // idx 7: c
            ad(Opcode::Ist, 0, 0),      // idx 8
            ad(Opcode::Jmp, 1, 0x8001), // idx 9: target = idx 11 (then-body)
            ad(Opcode::Kshort, 0, 1),   // idx 10: then-body (live: CB3.true → here)
            ad(Opcode::Ret1, 0, 2),     // idx 11
            ad(Opcode::Ret0, 0, 1),     // idx 12: merge
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![
                GcConst::Str(b"c".to_vec()),
                GcConst::Str(b"b".to_vec()),
                GcConst::Str(b"a".to_vec()),
            ],
            Vec::new(),
            1,
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for mixed and/or chain, got {:?}",
            result
        );
    }

    /// A nested `if` whose bytecode is structurally distinguishable
    /// from an AND chain (different JMP targets per CB) must still
    /// bail with NotImplemented. This is the existing
    /// `recover_nested_if_is_not_supported` shape, re-exercised at
    /// the chain level: the chain's CBs disagree on the merge, so
    /// neither AND nor OR matches.
    #[test]
    fn recover_chain_with_divergent_true_edges_bails() {
        //   GGET 0 0; ISF 0; JMP => 0009;     (CB1 → outer merge)
        //   GGET 1 1; ISF 1; JMP => 0008;     (CB2 → inner merge)
        //   KSHORT 0 1; RET1; RET0.
        // CB1.true (Block at idx 9) != CB2.true (Block at idx 8).
        let insts = vec![
            ad(Opcode::Gget, 0, 0),     // idx 1
            ad(Opcode::Isf, 0, 0),      // idx 2
            ad(Opcode::Jmp, 1, 0x8005), // idx 3: target = idx 9
            ad(Opcode::Gget, 1, 1),     // idx 4
            ad(Opcode::Isf, 1, 0),      // idx 5
            ad(Opcode::Jmp, 1, 0x8001), // idx 6: target = idx 8
            ad(Opcode::Kshort, 0, 1),   // idx 7
            ad(Opcode::Ret1, 0, 2),     // idx 8
            ad(Opcode::Ret0, 0, 1),     // idx 9
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![GcConst::Str(b"b".to_vec()), GcConst::Str(b"a".to_vec())],
            Vec::new(),
            2,
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for chain with divergent true_edges, got {:?}",
            result
        );
    }

    /// Lightweight `Proto` factory for unit tests that only need an
    /// empty shell (no insts, no debug info). Local to the `tests`
    /// module so the [`build_condition`] / [`build_test_condition`]
    /// tests can construct a `Proto` without restating every field.
    fn empty_proto() -> Proto {
        Proto {
            flags: 0,
            numparams: 0,
            framesize: 0,
            upvalues: Vec::new(),
            gc_consts: Vec::new(),
            num_consts: Vec::new(),
            insts: Vec::new(),
            debug: None,
        }
    }

    // ====================================================================
    // Stage 10: numeric-for recovery unit tests
    // ====================================================================

    /// Build a `VarInfo` of the given `VarKind` (no source name) at
    /// `slot`, scope `[begin..=end]`. Used to populate the
    /// ForIdx/ForStop/ForStep records LuaJIT emits for the loop's
    /// internal slots.
    fn for_loop_var(kind: VarKind, slot: u8, begin: u32, end: u32) -> VarInfo {
        VarInfo {
            kind,
            name: None,
            is_parameter: false,
            slot,
            scope_begin: begin,
            scope_end: end,
        }
    }

    /// Build a Stage-10 module mirroring `for i = 1, 10 do local x = i end`.
    ///
    /// Bytecode (abs idx → real idx):
    ///   1 KSHORT 0 1     real-idx 0  (start)
    ///   2 KSHORT 1 10    real-idx 1  (stop)
    ///   3 KSHORT 2 1     real-idx 2  (step)
    ///   4 FORI 0 => 0007 real-idx 3
    ///   5 MOV 4 3         real-idx 4  (body: x = i)
    ///   6 FORL 0 => 0005 real-idx 5
    ///   7 RET0 0 1        real-idx 6  (exit)
    ///
    /// Caller supplies the var_info records so individual tests can
    /// vary them (e.g., test the missing-name NotImplemented path).
    fn for_loop_module(var_info: Vec<VarInfo>) -> Module {
        // FORI at abs idx 4 → target abs idx 7. j = 7 - 5 = 2, D = 0x8002.
        // FORL at abs idx 6 → target abs idx 5. j = 5 - 7 = -2, D = 0x7ffe.
        let insts = vec![
            ad(Opcode::Kshort, 0, 1),
            ad(Opcode::Kshort, 1, 10),
            ad(Opcode::Kshort, 2, 1),
            ad(Opcode::Fori, 0, 0x8002),
            ad(Opcode::Mov, 4, 3),
            ad(Opcode::Forl, 0, 0x7ffe),
            ad(Opcode::Ret0, 0, 1),
        ];
        module_with(insts, var_info, Vec::new(), Vec::new(), 5)
    }

    /// The var_info records LuaJIT emits for
    /// `for i = 1, 10 do local x = i end`. Verified empirically via
    /// the IR parser. The visible loop variable `i` is a
    /// `VarKind::Name` at slot A+3 (NOT a ForIdx — ForIdx occupies
    /// slot A and carries no source name).
    fn standard_for_loop_var_info() -> Vec<VarInfo> {
        vec![
            for_loop_var(VarKind::ForIdx, 0, 2, 5),
            for_loop_var(VarKind::ForStop, 1, 2, 5),
            for_loop_var(VarKind::ForStep, 2, 2, 5),
            named_local(3, "i", 3, 4),
            named_local(4, "x", 4, 4),
        ]
    }

    /// `for i = 1, 10 do local x = i end` → emits the canonical
    /// source through the full pipeline.
    #[test]
    fn recover_numeric_for_simple_body() {
        let module = for_loop_module(standard_for_loop_var_info());
        assert_eq!(emit(&module), "for i = 1, 10 do\n    local x = i\nend");
    }

    /// Same fixture recovered directly to AST (not via emit_module).
    /// Verifies the `Stmt::For` tree shape: step collapsed to None
    /// (Expr::Int(1) default), one LocalDecl in the body.
    #[test]
    fn recover_numeric_for_ast_shape() {
        let module = for_loop_module(standard_for_loop_var_info());
        let proto = module.main_proto();
        let cfg = Cfg::build(proto);
        let ast = recover(proto, &cfg, true).expect("recover should succeed");

        assert_eq!(ast.len(), 1, "expected just [For] (implicit RET0)");
        match &ast[0] {
            Stmt::For {
                var,
                start,
                stop,
                step,
                body,
            } => {
                assert_eq!(var, "i", "var name");
                assert_eq!(*start, Expr::Int(1), "start");
                assert_eq!(*stop, Expr::Int(10), "stop");
                assert_eq!(*step, None, "step should be None for Expr::Int(1)");
                assert_eq!(body.len(), 1, "body len");
                match &body[0] {
                    Stmt::LocalDecl { name, expr } => {
                        assert_eq!(name, "x");
                        assert_eq!(*expr, Expr::Var("i".to_string()));
                    }
                    other => panic!("expected LocalDecl, got {:?}", other),
                }
            }
            other => panic!("expected For, got {:?}", other),
        }
    }

    /// `for i = 1, 10, 2 do local x = i end` → preserves step.
    /// Reuses `for_loop_module` but overrides the step slot's value
    /// via post-construction mutation (simpler than threading a
    /// parameter through).
    #[test]
    fn recover_numeric_for_with_step_preserved() {
        let mut module = for_loop_module(standard_for_loop_var_info());
        // KSHORT 2 1 (slot 2 = step) is at abs idx 3; change 1 → 2.
        module.protos[0].insts[3].b_or_d = 2;
        assert_eq!(emit(&module), "for i = 1, 10, 2 do\n    local x = i\nend");

        // AST: step is Some(Int(2)).
        let proto = module.main_proto();
        let cfg = Cfg::build(proto);
        let ast = recover(proto, &cfg, true).expect("recover should succeed");
        match &ast[0] {
            Stmt::For { step, .. } => {
                assert_eq!(*step, Some(Expr::Int(2)), "step should be Int(2)");
            }
            other => panic!("expected For, got {:?}", other),
        }
    }

    /// A loop variable name that the debug info doesn't surface
    /// (stripped or no Name record at slot A+3) bails with
    /// NotImplemented rather than guessing.
    #[test]
    fn recover_numeric_for_bails_when_var_name_missing() {
        // Same bytecode as the simple-body fixture, but the var_info
        // only has the ForIdx/ForStop/ForStep records (no Name for
        // slot 3 or 4). The recovery can't reconstruct `i`.
        let var_info = vec![
            for_loop_var(VarKind::ForIdx, 0, 2, 5),
            for_loop_var(VarKind::ForStop, 1, 2, 5),
            for_loop_var(VarKind::ForStep, 2, 2, 5),
        ];
        let module = for_loop_module(var_info);
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented when loop var name is missing, got {:?}",
            result
        );
    }

    /// The visited-block safety net fires when `recover_from` is
    /// called recursively on a block already entered. Construct a
    /// synthetic CFG whose LoopInit points its `body` edge at itself:
    /// the LoopInit arm calls `recover_from(body, ...)` which is the
    /// same block, so the recursive call's visited check fires. This
    /// is the exact shape that would infinite-recurse without the
    /// safety net — by the time the recursive call is made, the
    /// prefix has been processed and the loop variable resolved, so
    /// nothing else bails first.
    #[test]
    fn visited_block_tracking_breaks_unexpected_cycle() {
        // Proto: KSHORT (start); KSHORT (stop); KSHORT (step); FORI.
        // The synthetic CFG below rewrites the FORI's body edge to
        // point at the FORI's own block (Block 0) rather than a
        // separate body block — creating a self-referential cycle.
        let proto = Proto {
            flags: 0x02,
            numparams: 0,
            framesize: 4,
            upvalues: Vec::new(),
            gc_consts: Vec::new(),
            num_consts: Vec::new(),
            insts: vec![
                Instruction::synthetic_header(Opcode::Funcv, 4),
                ad(Opcode::Kshort, 0, 1),    // idx 1 — start
                ad(Opcode::Kshort, 1, 10),   // idx 2 — stop
                ad(Opcode::Kshort, 2, 1),    // idx 3 — step
                ad(Opcode::Fori, 0, 0x8002), // idx 4 — FORI (would target idx 7 normally)
            ],
            debug: Some(DebugInfo {
                var_info: vec![named_local(3, "i", 3, 3)],
                ..DebugInfo::default()
            }),
        };
        // Synthetic CFG: a single block that is its own loop body.
        // Not a shape the CFG builder would ever produce, but exactly
        // the kind of unexpected cycle the safety net must catch.
        let cfg = Cfg {
            blocks: vec![BasicBlock {
                id: BlockId(0),
                insts: vec![
                    InstructionId(1),
                    InstructionId(2),
                    InstructionId(3),
                    InstructionId(4),
                ],
                terminator: Terminator::LoopInit {
                    base: 0,
                    exit: BlockId(0),
                    body: BlockId(0),
                },
                preds: vec![BlockId(0)],
                succs: vec![
                    (BlockId(0), crate::cfg::EdgeKind::True),
                    (BlockId(0), crate::cfg::EdgeKind::False),
                ],
            }],
            entry: BlockId(0),
        };
        let result = recover(&proto, &cfg, true);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented on synthetic self-referential LoopInit, got {:?}",
            result
        );
    }

    /// A LoopInit inside a branch body (nested loop in an `if`) is
    /// not supported in Stage 10 — bails with NotImplemented rather
    /// than attempting the nested walk.
    #[test]
    fn recover_numeric_for_in_branch_body_bails() {
        // Outer if/then wrapping an inner numeric-for. The walk
        // enters the then-body with stop_at_merge=true; the LoopInit
        // arm rejects that immediately.
        //   GGET 0 0; ISF 0; JMP => 0010;  (entry / outer CB)
        //   KSHORT 1 1; KSHORT 2 10; KSHORT 3 1;
        //   FORI 1 => 0009; FORL 1 => 0007;  (inner loop, in then-body)
        //   RET0.
        // (Contrived bytecode; the CFG has 1 CB + 1 FORI, the
        // unsupported nested shape.)
        let insts = vec![
            ad(Opcode::Gget, 0, 0),      // idx 1
            ad(Opcode::Isf, 0, 0),       // idx 2
            ad(Opcode::Jmp, 1, 0x8006),  // idx 3: target = idx 10
            ad(Opcode::Kshort, 1, 1),    // idx 4
            ad(Opcode::Kshort, 2, 10),   // idx 5
            ad(Opcode::Kshort, 3, 1),    // idx 6
            ad(Opcode::Fori, 1, 0x8001), // idx 7: target = idx 9 (exit)
            ad(Opcode::Forl, 1, 0x7ffd), // idx 8: target = idx 6 (body)
            ad(Opcode::Ret0, 0, 1),      // idx 9 (inner exit)
            ad(Opcode::Ret0, 0, 1),      // idx 10 (outer merge)
        ];
        let module = module_with(
            insts,
            vec![
                for_loop_var(VarKind::ForIdx, 1, 5, 8),
                for_loop_var(VarKind::ForStop, 2, 5, 8),
                for_loop_var(VarKind::ForStep, 3, 5, 8),
                named_local(4, "i", 6, 7),
            ],
            vec![GcConst::Str(b"x".to_vec())],
            Vec::new(),
            5,
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for nested loop in if-body, got {:?}",
            result
        );
    }

    // ====================================================================
    // Stage 11: while/repeat recovery unit tests
    // ====================================================================
    //
    // The integration tests in `tests/stage11_while_repeat.rs` cover
    // the end-to-end pipeline. The unit tests below isolate the
    // while/repeat detection logic at the AST level so failures point
    // at the right layer.

    /// Build a Stage-11 module mirroring `local i = 0; while i < N do
    /// i = i + 1 end[; return i]`. Caller supplies `N` and whether to
    /// emit a trailing `return i`.
    ///
    /// Bytecode shape (abs idx → real idx):
    ///   1 KSHORT 0 0     real-idx 0  (i = 0)
    ///   2 KSHORT 1 N     real-idx 1  (loop header: load N)
    ///   3 ISGE 0 1       real-idx 2  (test: i >= N?)
    ///   4 JMP => exit    real-idx 3  (if i >= N, exit loop)
    ///   5 LOOP => exit   real-idx 4  (body start marker)
    ///   6 ADDVN 0 0 0    real-idx 5  (i = i + 1)
    ///   7 JMP => 0002    real-idx 6  (back-edge)
    ///   8 RET1 0 2       real-idx 7  (exit: return i)
    ///                 OR RET0 0 1     (exit: implicit return)
    fn while_loop_module(n: u16, with_return: bool) -> Module {
        // JMP at idx 4 targets exit at idx 8 → j = 8 - 5 = 3, D = 0x8003.
        // JMP at idx 7 targets header at idx 2 → j = 2 - 8 = -6, D = 0x7FFA.
        let exit_inst = if with_return {
            ad(Opcode::Ret1, 0, 2)
        } else {
            ad(Opcode::Ret0, 0, 1)
        };
        let insts = vec![
            ad(Opcode::Kshort, 0, 0),
            ad(Opcode::Kshort, 1, n),
            ad(Opcode::Isge, 0, 1),
            ad(Opcode::Jmp, 1, 0x8003),
            ad(Opcode::Loop, 1, 0x8003),
            abc(Opcode::Addvn, 0, 0, 0),
            ad(Opcode::Jmp, 1, 0x7FFA),
            exit_inst,
        ];
        module_with(
            insts,
            vec![named_local(0, "i", 0, 7)],
            Vec::new(),
            vec![NumConst::Int(1)],
            2,
        )
    }

    /// `local i = 0; while i < 10 do i = i + 1 end; return i` →
    /// emits canonical source through the full pipeline.
    #[test]
    fn recover_while_basic() {
        let module = while_loop_module(10, true);
        assert_eq!(
            emit(&module),
            "local i = 0\nwhile i < 10 do\n    i = i + 1\nend\nreturn i"
        );
    }

    /// `local i = 0; while i < 3 do i = i + 1 end` (no return) →
    /// the implicit RET0 produces no Stmt; the AST is just the local
    /// declaration followed by the While.
    #[test]
    fn recover_while_no_return() {
        let module = while_loop_module(3, false);
        assert_eq!(
            emit(&module),
            "local i = 0\nwhile i < 3 do\n    i = i + 1\nend"
        );
    }

    /// Same `while_basic` fixture recovered directly to AST. Verifies
    /// the `Stmt::While` tree shape: condition is the complement of
    /// ISGE (= LessThan), body is a single Assign.
    #[test]
    fn recover_while_ast_shape() {
        let module = while_loop_module(10, true);
        let proto = module.main_proto();
        let cfg = Cfg::build(proto);
        let ast = recover(proto, &cfg, true).expect("recover should succeed");

        assert_eq!(ast.len(), 3, "expected [LocalDecl, While, Return]");
        match &ast[0] {
            Stmt::LocalDecl { name, expr } => {
                assert_eq!(name, "i");
                assert_eq!(*expr, Expr::Int(0), "local i = 0");
            }
            other => panic!("expected LocalDecl, got {:?}", other),
        }
        match &ast[1] {
            Stmt::While { cond, body } => {
                // ISGE 0 1 tests R[0] >= R[1] = `i >= 10`; complement
                // is `i < 10`.
                assert_eq!(
                    cond.clone(),
                    Expr::BinOp {
                        op: BinOpKind::LessThan,
                        left: Box::new(Expr::Var("i".to_string())),
                        right: Box::new(Expr::Int(10)),
                    },
                    "while condition"
                );
                assert_eq!(body.len(), 1, "body len");
                match &body[0] {
                    Stmt::Assign { name, expr } => {
                        assert_eq!(name, "i");
                        // i + 1: ADDVN 0 0 0; num_const[0] = Int(1).
                        assert_eq!(
                            expr.clone(),
                            Expr::BinOp {
                                op: BinOpKind::Add,
                                left: Box::new(Expr::Var("i".to_string())),
                                right: Box::new(Expr::Int(1)),
                            },
                            "body: i = i + 1"
                        );
                    }
                    other => panic!("expected Assign, got {:?}", other),
                }
            }
            other => panic!("expected While, got {:?}", other),
        }
        match &ast[2] {
            Stmt::Return(Some(expr)) => assert_eq!(*expr, Expr::Var("i".to_string())),
            other => panic!("expected Return(Some(Var(i))), got {:?}", other),
        }
    }

    /// Build a Stage-11 module mirroring `local i = 0; repeat
    /// i = i + 1 until i >= N; return i`. Caller supplies `N`.
    ///
    /// Bytecode shape (mirrors `luajit -bl` exactly):
    ///   1 KSHORT 0 0     real-idx 0  (i = 0)
    ///   2 LOOP => exit   real-idx 1  (body start marker)
    ///   3 ADDVN 0 0 0    real-idx 2  (i = i + 1)
    ///   4 KSHORT 1 N     real-idx 3  (load N for the comparison)
    ///   5 ISGT 1 0       real-idx 4  (test: N > i? continue)
    ///   6 JMP => 0002    real-idx 5  (back-edge to LOOP)
    ///   7 RET1 0 2       real-idx 6  (exit: return i)
    fn repeat_loop_module(n: u16) -> Module {
        // LOOP at idx 2 targets exit at idx 7 → D encoded as 0x8000 + (7 - 3) = 0x8004.
        // JMP at idx 6 targets header at idx 2 → j = 2 - 7 = -5, D = 0x7FFB.
        let insts = vec![
            ad(Opcode::Kshort, 0, 0),
            ad(Opcode::Loop, 1, 0x8004),
            abc(Opcode::Addvn, 0, 0, 0),
            ad(Opcode::Kshort, 1, n),
            ad(Opcode::Isgt, 1, 0),
            ad(Opcode::Jmp, 1, 0x7FFB),
            ad(Opcode::Ret1, 0, 2),
        ];
        module_with(
            insts,
            vec![named_local(0, "i", 0, 6)],
            Vec::new(),
            vec![NumConst::Int(1)],
            2,
        )
    }

    /// `local i = 0; repeat i = i + 1 until i >= 3; return i` →
    /// emits source through the full pipeline. Note LuaJIT
    /// canonicalizes `i >= 3` to `3 <= i` (the documented Stage 9
    /// limitation for ordered comparisons).
    #[test]
    fn recover_repeat_basic() {
        let module = repeat_loop_module(3);
        assert_eq!(
            emit(&module),
            "local i = 0\nrepeat\n    i = i + 1\nuntil 3 <= i\nreturn i"
        );
    }

    /// Same `repeat_basic` fixture recovered directly to AST.
    /// Verifies the `Stmt::Repeat` tree shape: condition is the
    /// complement of ISGT (= LessEqual, with operands in canonical
    /// order — constant on the left).
    #[test]
    fn recover_repeat_ast_shape() {
        let module = repeat_loop_module(3);
        let proto = module.main_proto();
        let cfg = Cfg::build(proto);
        let ast = recover(proto, &cfg, true).expect("recover should succeed");

        assert_eq!(ast.len(), 3, "expected [LocalDecl, Repeat, Return]");
        match &ast[0] {
            Stmt::LocalDecl { name, expr } => {
                assert_eq!(name, "i");
                assert_eq!(*expr, Expr::Int(0), "local i = 0");
            }
            other => panic!("expected LocalDecl, got {:?}", other),
        }
        match &ast[1] {
            Stmt::Repeat { cond, body } => {
                // ISGT 1 0 tests R[1] > R[0] = `3 > i` (continue
                // condition); complement is `3 <= i`.
                assert_eq!(
                    cond.clone(),
                    Expr::BinOp {
                        op: BinOpKind::LessEqual,
                        left: Box::new(Expr::Int(3)),
                        right: Box::new(Expr::Var("i".to_string())),
                    },
                    "until condition (canonicalized)"
                );
                assert_eq!(body.len(), 1, "body len");
                match &body[0] {
                    Stmt::Assign { name, expr } => {
                        assert_eq!(name, "i");
                        assert_eq!(
                            expr.clone(),
                            Expr::BinOp {
                                op: BinOpKind::Add,
                                left: Box::new(Expr::Var("i".to_string())),
                                right: Box::new(Expr::Int(1)),
                            },
                            "body: i = i + 1"
                        );
                    }
                    other => panic!("expected Assign, got {:?}", other),
                }
            }
            other => panic!("expected Repeat, got {:?}", other),
        }
        match &ast[2] {
            Stmt::Return(Some(expr)) => assert_eq!(*expr, Expr::Var("i".to_string())),
            other => panic!("expected Return(Some(Var(i))), got {:?}", other),
        }
    }

    /// `while` detection requires the body block's terminator to be a
    /// `Jump(header)`. If the body's terminator is something else
    /// (e.g. Fallthrough into a merge), the recovery treats the CB as
    /// a regular if/then. This test confirms a regular if/then still
    /// works when the false_edge block doesn't loop back: the same
    /// Stage 7 shape (`if x then return 1 end`) is recovered as If,
    /// not While.
    #[test]
    fn recover_if_then_when_body_does_not_loop_back() {
        //   GGET 0 0; ISF 0; JMP => 0006;  (entry CB)
        //   KSHORT 0 1; RET1;              (then-body, returns)
        //   RET0.                           (merge = true_edge)
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Isf, 0, 0),
            ad(Opcode::Jmp, 1, 0x8002),
            ad(Opcode::Kshort, 0, 1),
            ad(Opcode::Ret1, 0, 2),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![GcConst::Str(b"x".to_vec())],
            Vec::new(),
            1,
        );
        let proto = module.main_proto();
        let cfg = Cfg::build(proto);
        let ast = recover(proto, &cfg, true).expect("recover should succeed");
        assert_eq!(ast.len(), 1, "expected just [If]");
        assert!(
            matches!(ast[0], Stmt::If { .. }),
            "expected If, not While, when body doesn't loop back"
        );
    }

    /// A 2-CB AND chain (`if a and b then … end` shape) is NOT
    /// treated as a while loop even though the chain has a body
    /// block — the chain logic at the ConditionalBranch arm runs
    /// only when neither the self-loop nor the back-edge checks
    /// match. This test confirms the chain path still produces an
    /// `If`, not a `While`, for the Stage 9 AND-chain shape.
    #[test]
    fn recover_and_chain_is_not_while() {
        //   GGET 0 0; ISF 0; JMP => 0009;  (CB1)
        //   GGET 0 1; ISF 0; JMP => 0009;  (CB2)
        //   KSHORT 0 1; RET1; RET0.        (then-body + merge)
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Isf, 0, 0),
            ad(Opcode::Jmp, 1, 0x8005),
            ad(Opcode::Gget, 0, 1),
            ad(Opcode::Isf, 0, 0),
            ad(Opcode::Jmp, 1, 0x8002),
            ad(Opcode::Kshort, 0, 1),
            ad(Opcode::Ret1, 0, 2),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![GcConst::Str(b"b".to_vec()), GcConst::Str(b"a".to_vec())],
            Vec::new(),
            1,
        );
        let proto = module.main_proto();
        let cfg = Cfg::build(proto);
        let ast = recover(proto, &cfg, true).expect("recover should succeed");
        assert_eq!(ast.len(), 1, "expected just [If]");
        assert!(
            matches!(ast[0], Stmt::If { .. }),
            "AND chain should be If, not While"
        );
    }

    /// An empty `while` body (`while x do end`) — the body block is
    /// just [LOOP, JMP]. The recovery should produce an empty body
    /// Vec and emit `while <cond> do\nend`.
    #[test]
    fn recover_while_empty_body() {
        //   KSHORT 0 1; KSHORT 1 2; ISGE 0 1; JMP => 0007;
        //   LOOP 1 => 0007; JMP => 0002;   (empty body)
        //   RET0.
        let insts = vec![
            ad(Opcode::Kshort, 0, 1),
            ad(Opcode::Kshort, 1, 2),
            ad(Opcode::Isge, 0, 1),
            ad(Opcode::Jmp, 1, 0x8002), // idx 4: target = idx 7
            ad(Opcode::Loop, 1, 0x8002),
            ad(Opcode::Jmp, 1, 0x7ffb), // idx 6: target = idx 2 (back-edge)
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "a", 0, 6), named_local(1, "b", 1, 6)],
            Vec::new(),
            Vec::new(),
            2,
        );
        assert_eq!(
            emit(&module),
            "local a = 1\nlocal b = 2\nwhile a < b do\nend"
        );
    }

    /// An empty `repeat` body (`repeat until x`) — body is just
    /// [LOOP, condition, JMP]. The recovery should produce an empty
    /// body Vec.
    #[test]
    fn recover_repeat_empty_body() {
        //   KSHORT 0 0; LOOP 1 => 0006; KSHORT 1 3; ISGT 1 0; JMP => 0002; RET0.
        // Loop runs zero times if i happens to already be >= 3, but
        // for testing purposes the bytecode shape is what matters.
        let insts = vec![
            ad(Opcode::Kshort, 0, 0),
            ad(Opcode::Loop, 1, 0x8004), // idx 2: target = idx 6
            ad(Opcode::Kshort, 1, 3),
            ad(Opcode::Isgt, 1, 0),
            ad(Opcode::Jmp, 1, 0x7ffc), // idx 5: target = idx 2
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "i", 0, 5)],
            Vec::new(),
            Vec::new(),
            2,
        );
        assert_eq!(emit(&module), "local i = 0\nrepeat\nuntil 3 <= i");
    }

    // ====================================================================
    // Stage 12: function-call recovery unit tests
    // ====================================================================
    //
    // The integration tests in `tests/stage12_calls.rs` cover the
    // end-to-end pipeline (parse → CFG → recover → emit) using real
    // `luajit -b -g` fixtures. The unit tests below isolate the CALL
    // handling in [`process_inst`] so failures point at the right
    // layer.

    /// Build a Stage-12 module mirroring `print("hello")`'s bytecode
    /// (verified via `luajit -bl`):
    ///   1 GGET  0 0     real-idx 0  (function at slot 0)
    ///   2 KSTR  2 1     real-idx 1  (arg at slot 2; gap at slot 1)
    ///   3 CALL  0 1 2   real-idx 2  (B=1: 0 results; C=2: 1 arg)
    ///   4 RET0  0 1     real-idx 3
    ///
    /// Caller supplies the argument's load instruction + GC constant
    /// table so individual tests can vary the argument's type.
    fn call_no_result_module(arg_load: Instruction, gc_consts: Vec<GcConst>) -> Module {
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            arg_load,
            abc(Opcode::Call, 0, 1, 2),
            ad(Opcode::Ret0, 0, 1),
        ];
        module_with(insts, Vec::new(), gc_consts, Vec::new(), 3)
    }

    /// `print("hello")` — bare call, single string arg, no result.
    /// The recovery pushes a `Stmt::Call` directly (no slot update).
    #[test]
    fn recover_call_no_result_string_arg() {
        // GGET 0 0 → Global("print") (operand 0); KSTR 2 1 →
        // Str("hello") (operand 1). With gc_consts = [hello, print]
        // (file order), reverse-index gives operand 0 → [1] = print,
        // operand 1 → [0] = hello.
        let module = call_no_result_module(
            ad(Opcode::Kstr, 2, 1),
            vec![
                GcConst::Str(b"hello".to_vec()),
                GcConst::Str(b"print".to_vec()),
            ],
        );
        assert_eq!(emit(&module), "print(\"hello\")");
    }

    /// Same fixture recovered directly to AST. Verifies the
    /// `Stmt::Call` tree shape: func is `Global("print")`, args is
    /// `[Str("hello")]`. The implicit RET0 produces no Stmt.
    #[test]
    fn recover_call_no_result_ast_shape() {
        let module = call_no_result_module(
            ad(Opcode::Kstr, 2, 1),
            vec![
                GcConst::Str(b"hello".to_vec()),
                GcConst::Str(b"print".to_vec()),
            ],
        );
        let proto = module.main_proto();
        let cfg = Cfg::build(proto);
        let ast = recover(proto, &cfg, true).expect("recover should succeed");
        assert_eq!(
            ast.len(),
            1,
            "expected just [Call] (implicit RET0 → no Stmt)"
        );
        match &ast[0] {
            Stmt::Call { func, args } => {
                assert_eq!(*func, Expr::Global("print".to_string()), "func");
                assert_eq!(args.len(), 1, "args len");
                assert_eq!(args[0], Expr::Str(b"hello".to_vec()), "arg 0");
            }
            other => panic!("expected Call, got {:?}", other),
        }
    }

    /// `f()` — bare call with NO arguments (C=1, meaning 0 args).
    /// Confirms the arg-count math: `arg_count = C - 1 = 0`, and
    /// the arg-slot loop `(A+2)..=(A+C)` is empty when C == 1.
    #[test]
    fn recover_call_no_args() {
        //   GGET 0 0; CALL 0 1 1; RET0.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            abc(Opcode::Call, 0, 1, 1),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![GcConst::Str(b"f".to_vec())],
            Vec::new(),
            1,
        );
        assert_eq!(emit(&module), "f()");
    }

    /// `print("a", "b", "c")` — bare call with three args. Verifies
    /// the arg-slot range `(A+2)..=(A+C)` covers slots 2, 3, 4 when
    /// C == 4 (3 args). Bytecode mirrors `luajit -bl` exactly.
    #[test]
    fn recover_call_multiple_args() {
        //   GGET 0 0;      (print — operand 0)
        //   KSTR 2 1; KSTR 3 2; KSTR 4 3;   (3 args, operands 1, 2, 3)
        //   CALL 0 1 4;    (B=1: 0 results; C=4: 3 args)
        //   RET0.
        // gc_consts file order (reverse-indexed): [c, b, a, print].
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Kstr, 2, 1),
            ad(Opcode::Kstr, 3, 2),
            ad(Opcode::Kstr, 4, 3),
            abc(Opcode::Call, 0, 1, 4),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![
                GcConst::Str(b"c".to_vec()),
                GcConst::Str(b"b".to_vec()),
                GcConst::Str(b"a".to_vec()),
                GcConst::Str(b"print".to_vec()),
            ],
            Vec::new(),
            5,
        );
        assert_eq!(emit(&module), "print(\"a\", \"b\", \"c\")");
    }

    /// `local x = tostring(42)` — single-result call whose result is
    /// stored in a named local. The recovery routes through
    /// [`assign_slot_ast`], which recognizes slot 0 as the
    /// declaration point of `x` and pushes `Stmt::LocalDecl`.
    #[test]
    fn recover_call_result_in_local_decl() {
        //   GGET  0 0      (tostring at slot 0)
        //   KSHORT 2 42    (arg at slot 2; gap at slot 1)
        //   CALL  0 2 2    (B=2: 1 result at slot 0; C=2: 1 arg)
        //   RET0  0 1.
        // var_info: x declared at real-idx 2 (the CALL), scope
        // covers through the RET0.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Kshort, 2, 42),
            abc(Opcode::Call, 0, 2, 2),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "x", 2, 3)],
            vec![GcConst::Str(b"tostring".to_vec())],
            Vec::new(),
            3,
        );
        assert_eq!(emit(&module), "local x = tostring(42)");
    }

    /// `x = f(1)` — single-result call assigned to an existing
    /// local. The CALL writes slot 0, but `x` is already in scope
    /// (declared earlier), so [`assign_slot_ast`] pushes an Assign
    /// (no `local`).
    #[test]
    fn recover_call_result_in_reassignment() {
        //   KSHORT 0 0      (x = 0; declares x at real-idx 0)
        //   GGET   1 0      (f at slot 1)
        //   KSHORT 3 1      (arg at slot 3; gap at slot 2)
        //   CALL   1 2 2    (B=2: 1 result at slot 1)
        //   MOV    0 1      (x = result)
        //   RET1   0 2.
        //
        // LuaJIT actually lowers `x = f(1)` this way — the call's
        // result lands in a temp slot, then a MOV copies it into the
        // named local. We use this shape to exercise the Assign path
        // for a CALL RHS.
        let insts = vec![
            ad(Opcode::Kshort, 0, 0),
            ad(Opcode::Gget, 1, 0),
            ad(Opcode::Kshort, 3, 1),
            abc(Opcode::Call, 1, 2, 2),
            ad(Opcode::Mov, 0, 1),
            ad(Opcode::Ret1, 0, 2),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "x", 0, 5)],
            vec![GcConst::Str(b"f".to_vec())],
            Vec::new(),
            4,
        );
        assert_eq!(emit(&module), "local x = 0\nx = f(1)\nreturn x");
    }

    /// `return f(1)` shape — single-result call whose result is
    /// immediately returned. The CALL writes slot 0 (an unnamed
    /// temp), and the following RET1 reads slot 0. No Stmt is
    /// emitted for the CALL itself; the call surfaces as
    /// `Expr::Call` inside the `Return`.
    #[test]
    fn recover_call_result_returned() {
        //   GGET   0 0      (f at slot 0)
        //   KSHORT 2 1      (arg at slot 2)
        //   CALL   0 2 2    (B=2: 1 result at slot 0)
        //   RET1   0 2.
        // No var_info at slot 0 → the CALL stores Expr::Call as an
        // unnamed temp; RET1 reads it.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Kshort, 2, 1),
            abc(Opcode::Call, 0, 2, 2),
            ad(Opcode::Ret1, 0, 2),
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![GcConst::Str(b"f".to_vec())],
            Vec::new(),
            3,
        );
        assert_eq!(emit(&module), "return f(1)");
    }

    /// CALL with B == 0 (multres return — e.g. `local a, b, c = f()`)
    /// bails with NotImplemented. Stage 15 territory.
    #[test]
    fn recover_call_multres_results_not_supported() {
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Kshort, 2, 1),
            abc(Opcode::Call, 0, 0, 2), // B=0 → multres
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![GcConst::Str(b"f".to_vec())],
            Vec::new(),
            3,
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for multres CALL (B=0), got {:?}",
            result
        );
    }

    /// CALL with C == 0 (multres args — e.g. `f(...)`) bails with
    /// NotImplemented. Stage 15 territory.
    #[test]
    fn recover_call_multres_args_not_supported() {
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            abc(Opcode::Call, 0, 1, 0), // C=0 → multres args
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![GcConst::Str(b"f".to_vec())],
            Vec::new(),
            1,
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for multres CALL (C=0), got {:?}",
            result
        );
    }

    /// CALL with B > 2 (multiple explicit results — e.g. the
    /// `local a, b = f()` shape, where B = 3 means 2 results) bails
    /// with NotImplemented.
    #[test]
    fn recover_call_multi_result_not_supported() {
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Kshort, 2, 1),
            abc(Opcode::Call, 0, 3, 2), // B=3 → 2 results
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![GcConst::Str(b"f".to_vec())],
            Vec::new(),
            3,
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for multi-result CALL (B=3), got {:?}",
            result
        );
    }

    /// CALL with an uninitialized argument slot bails with
    /// NotImplemented — we can't reconstruct the call source.
    #[test]
    fn recover_call_uninitialized_arg_not_supported() {
        //   GGET   0 0      (f at slot 0)
        //   CALL   0 1 2    (C=2 → 1 arg at slot 2, but slot 2 was
        //                    never written)
        //   RET0.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            abc(Opcode::Call, 0, 1, 2),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![GcConst::Str(b"f".to_vec())],
            Vec::new(),
            3,
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for CALL with uninitialized arg slot, got {:?}",
            result
        );
    }

    /// CALL with no function in the base slot bails with
    /// NotImplemented.
    #[test]
    fn recover_call_no_function_in_base_slot_not_supported() {
        // CALL at the top with slot 0 never written — lookup_operand
        // fails on the func slot.
        let insts = vec![abc(Opcode::Call, 0, 1, 1), ad(Opcode::Ret0, 0, 1)];
        let module = module_with(insts, Vec::new(), Vec::new(), Vec::new(), 1);
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for CALL with no function in base slot, got {:?}",
            result
        );
    }

    // ====================================================================
    // Stage 13: field access / table index / method-call recovery
    // ====================================================================
    //
    // The integration tests in `tests/stage13_fields_and_methods.rs`
    // cover the end-to-end pipeline (parse → CFG → recover → emit)
    // using real `luajit -b -g` fixtures. The unit tests below
    // isolate the TGETS/TGETV/TGETB handling in [`process_inst`], the
    // method-call detection on the CALL handler, and the FR1/FR2
    // argument-base fix.

    /// `local x = obj.field` — TGETS with a named destination. The
    /// Field expression is routed through `assign_slot_ast`, which
    /// recognizes the slot as a LocalDecl.
    #[test]
    fn recover_tgets_field_access() {
        //   GGET  0 0      (obj — operand 0)
        //   TGETS 0 0 1    (slot 0 = slot 0[KSTR[reverse(1)]])
        //   RET0  0 1.
        // gc_consts file order (reverse-indexed): [field, obj].
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            abc(Opcode::Tgets, 0, 0, 1),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "x", 1, 2)],
            vec![
                GcConst::Str(b"field".to_vec()),
                GcConst::Str(b"obj".to_vec()),
            ],
            Vec::new(),
            1,
        );
        assert_eq!(emit(&module), "local x = obj.field");
    }

    /// `local x = obj[5]` — TGETB. C is an inline integer literal
    /// key, NOT a register.
    #[test]
    fn recover_tgetb_numeric_index() {
        //   GGET   0 0     (obj)
        //   TGETB  0 0 5   (slot 0 = slot 0[5])
        //   RET0   0 1.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            abc(Opcode::Tgetb, 0, 0, 5),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "x", 1, 2)],
            vec![GcConst::Str(b"obj".to_vec())],
            Vec::new(),
            1,
        );
        assert_eq!(emit(&module), "local x = obj[5]");
    }

    /// `local x = t[k]` — TGETV. Both B (table) and C (key) are
    /// registers.
    #[test]
    fn recover_tgetv_register_index() {
        //   GGET  0 0     (t)
        //   GGET  1 1     (k)
        //   TGETV 0 0 1   (slot 0 = slot 0[slot 1] = t[k])
        //   RET0  0 1.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Gget, 1, 1),
            abc(Opcode::Tgetv, 0, 0, 1),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "x", 2, 3)],
            vec![GcConst::Str(b"k".to_vec()), GcConst::Str(b"t".to_vec())],
            Vec::new(),
            2,
        );
        assert_eq!(emit(&module), "local x = t[k]");
    }

    /// TGETS with a non-string GC constant (e.g. an FFI cdata) bails
    /// with NotImplemented.
    #[test]
    fn recover_tgets_non_string_const_not_supported() {
        // TGETS C=0 → reverse-index 0 → gc_consts[len-1-0]. With
        // gc_consts = [Str(obj), I64(0)] the operand resolves to
        // gc_consts[1] = I64 — non-string.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            abc(Opcode::Tgets, 0, 0, 0),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![GcConst::Str(b"obj".to_vec()), GcConst::I64(0)],
            Vec::new(),
            1,
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for TGETS with non-string key, got {:?}",
            result
        );
    }

    /// TGETS with an uninitialized table register bails with
    /// NotImplemented.
    #[test]
    fn recover_tgets_uninitialized_table_not_supported() {
        // slot 0 (the table) is never written.
        let insts = vec![abc(Opcode::Tgets, 0, 0, 0), ad(Opcode::Ret0, 0, 1)];
        let module = module_with(
            insts,
            Vec::new(),
            vec![GcConst::Str(b"field".to_vec())],
            Vec::new(),
            1,
        );
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for TGETS with uninitialized table, got {:?}",
            result
        );
    }

    /// `obj:method(1)` — method call with one explicit arg. Verified
    /// against the FR2 disassembly from `luajit -bl`. The recovery
    /// detects the method-call shape (slot 0 is `Field { obj, name }`,
    /// self slot 2 holds `obj`) and emits `obj:method(1)`.
    #[test]
    fn recover_method_call_with_explicit_arg() {
        //   GGET   0 0     (obj)
        //   MOV    2 0     (self = obj → slot 2, the FR2 gap)
        //   TGETS  0 0 1   (slot 0 = obj.method)
        //   KSHORT 3 1     (explicit arg → slot 3)
        //   CALL   0 1 3   (B=1: 0 results; C=3: 2 args)
        //   RET0   0 1.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Mov, 2, 0),
            abc(Opcode::Tgets, 0, 0, 1),
            ad(Opcode::Kshort, 3, 1),
            abc(Opcode::Call, 0, 1, 3),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![
                GcConst::Str(b"method".to_vec()),
                GcConst::Str(b"obj".to_vec()),
            ],
            Vec::new(),
            4,
        );
        assert_eq!(emit(&module), "obj:method(1)");
    }

    /// `obj:method()` — method call with zero explicit args. Same
    /// shape as `method_call_with_explicit_arg` minus the explicit
    /// arg load; C=2 means 1 total arg (self), 0 explicit.
    #[test]
    fn recover_method_call_no_explicit_args() {
        //   GGET   0 0     (obj)
        //   MOV    2 0     (self → slot 2)
        //   TGETS  0 0 1   (obj.method)
        //   CALL   0 1 2   (B=1; C=2: 1 arg, 0 explicit)
        //   RET0   0 1.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Mov, 2, 0),
            abc(Opcode::Tgets, 0, 0, 1),
            abc(Opcode::Call, 0, 1, 2),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![
                GcConst::Str(b"method".to_vec()),
                GcConst::Str(b"obj".to_vec()),
            ],
            Vec::new(),
            3,
        );
        assert_eq!(emit(&module), "obj:method()");
    }

    /// `local x = obj:method()` — method call with a single result.
    /// The CALL has B=2 (1 result), routed through `assign_slot_ast`
    /// so the recovery recognizes slot 0 as a LocalDecl.
    #[test]
    fn recover_method_call_result_in_local_decl() {
        //   GGET   0 0     (obj)
        //   MOV    2 0     (self → slot 2)
        //   TGETS  0 0 1   (obj.method)
        //   CALL   0 2 2   (B=2: 1 result at slot 0; C=2: 1 arg)
        //   RET0   0 1.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Mov, 2, 0),
            abc(Opcode::Tgets, 0, 0, 1),
            abc(Opcode::Call, 0, 2, 2),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            vec![named_local(0, "x", 3, 4)],
            vec![
                GcConst::Str(b"method".to_vec()),
                GcConst::Str(b"obj".to_vec()),
            ],
            Vec::new(),
            3,
        );
        assert_eq!(emit(&module), "local x = obj:method()");
    }

    /// Method-call detection requires the self slot to equal the
    /// Field's obj. If it doesn't (the user wrote `obj.method(obj)`
    /// explicitly, which compiles to a regular CALL), the recovery
    /// falls back to a regular function call.
    #[test]
    fn recover_field_call_not_method() {
        // `obj.method(obj)` — same shape as `obj:method()` from the
        // compiler's POV *except* the user wrote `obj.method(obj)`.
        // LuaJIT's compiler actually lowers both forms identically
        // (the explicit `obj` arg becomes self), so this test pins
        // the recovery's canonical output: the method-call form.
        // The construction here is the literal bytecode from
        // `obj.method(obj)` — which matches `obj:method()` — so we
        // expect the method-call desugaring.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Mov, 2, 0),
            abc(Opcode::Tgets, 0, 0, 1),
            abc(Opcode::Call, 0, 1, 2),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![
                GcConst::Str(b"method".to_vec()),
                GcConst::Str(b"obj".to_vec()),
            ],
            Vec::new(),
            3,
        );
        // The recovery always desugars this shape to `obj:method()`.
        // The two source forms are indistinguishable at the bytecode
        // level; the method-call form is canonical.
        assert_eq!(emit(&module), "obj:method()");
    }

    /// If the self slot holds a *different* expr from the Field's
    /// obj (e.g. a synthetic test bytecode), the recovery falls back
    /// to a regular function call rather than guessing a method
    /// call. This exercises the detection predicate's equality
    /// check.
    #[test]
    fn recover_field_call_with_mismatched_self_falls_back() {
        //   GGET   0 0     (obj)
        //   GGET   2 2     (something_else → slot 2, would-be self)
        //   TGETS  0 0 1   (slot 0 = obj.method)
        //   CALL   0 1 2   (C=2: 1 arg at slot 2)
        //   RET0   0 1.
        // slot 2 (the arg) is `something_else`, not `obj`, so the
        // method-call check fails. The CALL surfaces as a regular
        // call on the Field expression: `obj.method(something_else)`.
        // gc_consts file order (reverse-indexed):
        //   [something_else, method, obj]   (len=3)
        // so GGET 0 0 → gc_consts[2]=obj, GGET 2 2 → gc_consts[0]=something_else,
        //    TGETS C=1 → gc_consts[1]=method.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Gget, 2, 2),
            abc(Opcode::Tgets, 0, 0, 1),
            abc(Opcode::Call, 0, 1, 2),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![
                GcConst::Str(b"something_else".to_vec()),
                GcConst::Str(b"method".to_vec()),
                GcConst::Str(b"obj".to_vec()),
            ],
            Vec::new(),
            3,
        );
        assert_eq!(emit(&module), "obj.method(something_else)");
    }

    /// Direct AST test for method-call shape: verifies the
    /// `Stmt::MethodCall` payload (obj, method name, args).
    #[test]
    fn recover_method_call_ast_shape() {
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Mov, 2, 0),
            abc(Opcode::Tgets, 0, 0, 1),
            ad(Opcode::Kshort, 3, 1),
            abc(Opcode::Call, 0, 1, 3),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![
                GcConst::Str(b"method".to_vec()),
                GcConst::Str(b"obj".to_vec()),
            ],
            Vec::new(),
            4,
        );
        let proto = module.main_proto();
        let cfg = Cfg::build(proto);
        let ast = recover(proto, &cfg, true).expect("recover should succeed");
        assert_eq!(
            ast.len(),
            1,
            "expected just [MethodCall] (implicit RET0 → no Stmt)"
        );
        match &ast[0] {
            Stmt::MethodCall { obj, method, args } => {
                assert_eq!(*obj, Expr::Global("obj".to_string()), "obj");
                assert_eq!(method, "method", "method");
                assert_eq!(args.len(), 1, "args len");
                assert_eq!(args[0], Expr::Int(1), "arg 0");
            }
            other => panic!("expected MethodCall, got {:?}", other),
        }
    }

    /// Direct AST test for the field-access shape: verifies the
    /// `Expr::Field` payload is preserved through the slot map.
    #[test]
    fn recover_field_access_ast_shape() {
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            abc(Opcode::Tgets, 0, 0, 1),
            ad(Opcode::Ret1, 0, 2),
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![
                GcConst::Str(b"field".to_vec()),
                GcConst::Str(b"obj".to_vec()),
            ],
            Vec::new(),
            1,
        );
        let proto = module.main_proto();
        let cfg = Cfg::build(proto);
        let ast = recover(proto, &cfg, true).expect("recover should succeed");
        assert_eq!(ast.len(), 1, "expected just [Return]");
        match &ast[0] {
            Stmt::Return(Some(expr)) => {
                assert_eq!(
                    *expr,
                    Expr::Field {
                        obj: Box::new(Expr::Global("obj".to_string())),
                        name: "field".to_string(),
                    },
                    "return expr"
                );
            }
            other => panic!("expected Return(Some(Field)), got {:?}", other),
        }
    }

    // ---- FR1/FR2 argument-base fix -----------------------------------
    //
    // Stage 13 fixes a latent bug: the CALL handler resolved args
    // from `A+2` (FR2) unconditionally. FR1 protos (the Darktide
    // corpus) pack args at `A+1` (no gap). All system-luajit test
    // fixtures are FR2, so the bug is latent until Stage 14 turns on
    // corpus support. These unit tests exercise the FR1 path directly
    // by building a FR1 module and calling `recover` with
    // `is_fr2 = false`.

    /// Build a Module with FR1 (no FR2 flag). Mirrors `module_with`
    /// but clears the FR2 flag in the header.
    fn module_with_fr1(
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

    /// FR1 CALL: args start at A+1 (no gap). The Darktide corpus
    /// packs args this way; the recovery must honor the FR1 layout.
    #[test]
    fn recover_fr1_call_args_at_a_plus_1() {
        // `f(1)` in FR1:
        //   GGET   0 0     (f at slot 0)
        //   KSHORT 1 1     (arg at slot 1, NOT slot 2 — no FR2 gap)
        //   CALL   0 1 2   (A=0, B=1, C=2: 1 arg at slot A+1)
        //   RET0   0 1.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Kshort, 1, 1),
            abc(Opcode::Call, 0, 1, 2),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with_fr1(
            insts,
            Vec::new(),
            vec![GcConst::Str(b"f".to_vec())],
            Vec::new(),
            2,
        );
        assert_eq!(emit_module(&module).unwrap(), "f(1)");
    }

    /// FR1 method call: self at slot A+1, explicit args at A+2..A+C.
    #[test]
    fn recover_fr1_method_call() {
        // `obj:method(1)` in FR1:
        //   GGET   0 0     (obj)
        //   MOV    1 0     (self = obj → slot A+1 = slot 1)
        //   TGETS  0 0 1   (obj.method → slot 0)
        //   KSHORT 2 1     (explicit arg at slot A+2 = slot 2)
        //   CALL   0 1 3   (C=3: 2 args: self + explicit)
        //   RET0   0 1.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Mov, 1, 0),
            abc(Opcode::Tgets, 0, 0, 1),
            ad(Opcode::Kshort, 2, 1),
            abc(Opcode::Call, 0, 1, 3),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with_fr1(
            insts,
            Vec::new(),
            vec![
                GcConst::Str(b"method".to_vec()),
                GcConst::Str(b"obj".to_vec()),
            ],
            Vec::new(),
            3,
        );
        assert_eq!(emit_module(&module).unwrap(), "obj:method(1)");
    }

    /// Regression: FR2 calls still resolve args from A+2 (the gap).
    /// Stage 12's behavior must not change.
    #[test]
    fn recover_fr2_call_args_at_a_plus_2() {
        // `f(1)` in FR2 (system-luajit output):
        //   GGET   0 0     (f at slot 0)
        //   KSHORT 2 1     (arg at slot 2 — gap at slot 1)
        //   CALL   0 1 2   (1 arg at slot A+2)
        //   RET0   0 1.
        let insts = vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Kshort, 2, 1),
            abc(Opcode::Call, 0, 1, 2),
            ad(Opcode::Ret0, 0, 1),
        ];
        let module = module_with(
            insts,
            Vec::new(),
            vec![GcConst::Str(b"f".to_vec())],
            Vec::new(),
            3,
        );
        assert_eq!(emit(&module), "f(1)");
    }

    /// Table writes (TSETS/TSETV/TSETB) are Stage 14. The recovery
    /// bails with NotImplemented for each.
    #[test]
    fn recover_table_writes_not_supported() {
        for op in [Opcode::Tsets, Opcode::Tsetv, Opcode::Tsetb] {
            let insts = vec![abc(op, 0, 0, 0), ad(Opcode::Ret0, 0, 1)];
            let module = module_with(insts, Vec::new(), Vec::new(), Vec::new(), 1);
            let result = emit_module(&module);
            assert!(
                matches!(result, Err(DecompilerError::NotImplemented)),
                "expected NotImplemented for {:?}, got {:?}",
                op,
                result
            );
        }
    }

    /// TGETR (FFI raw table get — rare) bails with NotImplemented.
    #[test]
    fn recover_tgetr_not_supported() {
        let insts = vec![abc(Opcode::Tgetr, 0, 0, 0), ad(Opcode::Ret0, 0, 1)];
        let module = module_with(insts, Vec::new(), Vec::new(), Vec::new(), 1);
        let result = emit_module(&module);
        assert!(
            matches!(result, Err(DecompilerError::NotImplemented)),
            "expected NotImplemented for TGETR, got {:?}",
            result
        );
    }
}
