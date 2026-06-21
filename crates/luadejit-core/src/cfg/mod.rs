//! Control Flow Graph (CFG) construction from a [`Proto`]'s
//! instructions.
//!
//! **Stage 7a**: builds basic blocks and wires predecessor/successor
//! edges. **Stage 7b**: adds dominator-tree computation
//! ([`Cfg::compute_dominators`]) via the Cooper-Harvey-Kennedy
//! iterative algorithm. **Stage 7c-7e**: the emit pipeline now
//! builds a CFG for every proto and feeds it to
//! [`crate::structure::recover`]; the dominator tree remains
//! on-demand infrastructure for the SSA / phi-elimination stages
//! still ahead.
//!
//! See `docs/architecture-v2-data-structures.md` §2 for the type
//! specifications and `docs/luajit-bytecode-format.md` §4.2 for the
//! jump-target encoding.
//!
//! ## Jump-target formula
//!
//! Per format doc §4.2, a JMP at internal index `i` (the index into
//! [`Proto::insts`], where 0 is the synthetic FUNC* header and 1 is
//! the first real instruction) has D-field target
//!
//! ```text
//! j = (D as i32) - 0x8000     // subtract the bias (NOT a bit reinterpret)
//! target = i + 1 + j          // relative to the instruction *after* the JMP
//! ```
//!
//! **Important**: the bias subtraction `(D as i32) - 0x8000` is *not*
//! the same as `(D as u16) as i16`. The former yields the signed
//! offset (e.g. D=0x8002 → j=2; D=0x7ffe → j=-2); the latter
//! reinterprets the bits as a signed 16-bit integer and would give
//! 0x8002 → -32766 — wrong.
//!
//! This was verified empirically against `luajit -bl` output for
//! both `if x then return 1 end` (JMP at idx 3, D=0x8002 → target 6)
//! and `if x then return 1 else return 2 end` (two JMPs: idx 3
//! D=0x8003 → 7; idx 6 D=0x8002 → 9). The internal index equals the
//! `luajit -bl` display number, because both number real instructions
//! starting at 1.

use std::collections::{BTreeSet, HashMap};

use crate::ir::{Opcode, Proto};

/// Identifier for a basic block (index into [`Cfg::blocks`]).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct BlockId(pub u32);

/// Identifier for an instruction (index into [`Proto::insts`]).
///
/// Uses the same indexing as the instruction vector: 0 is the
/// synthetic FUNC* header (excluded from all basic blocks), 1+ are
/// real instructions.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct InstructionId(pub u32);

/// A control flow graph built from a [`Proto`]'s instructions.
///
/// Stage 7a populates [`blocks`](Cfg::blocks) and wires
/// [`preds`](BasicBlock::preds) / [`succs`](BasicBlock::succs).
/// Stage 7b adds [`Cfg::compute_dominators`] for on-demand
/// dominator-tree construction; the tree is computed by the caller
/// rather than cached on the CFG, so SSA construction (Stage 7c) can
/// decide when and whether to materialize it.
#[derive(Clone, Debug)]
pub struct Cfg {
    pub blocks: Vec<BasicBlock>,
    pub entry: BlockId,
}

/// A basic block: a maximal sequence of instructions with no branches
/// in (except at the start) and no branches out (except at the end).
#[derive(Clone, Debug)]
pub struct BasicBlock {
    pub id: BlockId,
    /// Indices into [`Proto::insts`]. Always non-empty for blocks
    /// produced by [`Cfg::build`]; the FUNC* header at index 0 is
    /// never included.
    pub insts: Vec<InstructionId>,
    pub terminator: Terminator,
    /// Predecessor block ids, in ascending order.
    pub preds: Vec<BlockId>,
    /// Successor (block, edge-kind) pairs.
    pub succs: Vec<(BlockId, EdgeKind)>,
}

/// How a basic block exits.
#[derive(Clone, Debug)]
pub enum Terminator {
    /// Falls through to the next block (no explicit branch).
    Fallthrough(BlockId),
    /// Unconditional jump (standalone JMP not preceded by ISxx).
    Jump(BlockId),
    /// Conditional branch: an ISxx instruction immediately followed
    /// by a JMP. When the ISxx condition is true the JMP is taken
    /// (`true_edge`); when false execution falls through to
    /// `false_edge`.
    ConditionalBranch {
        /// The ISxx instruction's index.
        condition: InstructionId,
        /// JMP target (condition held).
        true_edge: BlockId,
        /// Fall-through target (condition failed).
        false_edge: BlockId,
    },
    /// Return instruction (RET0, RET1, RET, RETM).
    Return,
    /// Tail call (CALLT, CALLMT) — treated as a return for CFG
    /// purposes. The payload is the CALLT/CALLMT instruction's index.
    TailCall(InstructionId),
    // Note on UCLO (close upvalues): UCLO can be followed by a JMP or
    // a RET. For Stage 7a we treat UCLO as a regular instruction
    // within its block; the *following* instruction determines the
    // terminator. Refinement (e.g. a UCLO-specific terminator variant
    // that records the close-set) is deferred to later stages.
}

/// Labels a successor edge.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EdgeKind {
    /// Edge taken when the ISxx condition is true (jump taken).
    True,
    /// Edge taken when the ISxx condition is false (fall-through).
    False,
    /// Unconditional edge (from [`Terminator::Jump`] or
    /// [`Terminator::Fallthrough`]).
    Unconditional,
}

/// The immediate-dominator tree for a CFG. Built by
/// [`Cfg::compute_dominators`] using the Cooper-Harvey-Kennedy
/// iterative dominance algorithm (Cooper, Harvey, Kennedy. "A Simple,
/// Fast Dominance Algorithm." Rice University, 2001).
///
/// `immediate_doms[i]` gives the immediate dominator of block `i`,
/// or `None` for the entry block (which dominates itself by
/// convention but has no *strict* dominator) and for unreachable
/// blocks (which are not dominated by any block in the reachable
/// CFG).
///
/// Stage 7b: pure infrastructure. Not yet consumed by the emit
/// pipeline; SSA construction (Stage 7c) will derive dominance
/// frontiers from this tree via Cytron's algorithm.
#[derive(Clone, Debug)]
pub struct DominatorTree {
    /// `immediate_doms[block_id]` = that block's immediate dominator.
    /// The entry block is `None` (no strict dominator). Unreachable
    /// blocks are also `None`.
    pub immediate_doms: Vec<Option<BlockId>>,
}

impl DominatorTree {
    /// The immediate dominator of `b`, or `None` for the entry block
    /// and unreachable blocks. Out-of-range ids return `None`
    /// defensively (treated like an unreachable block).
    pub fn idom(&self, b: BlockId) -> Option<BlockId> {
        self.immediate_doms.get(b.0 as usize).copied().flatten()
    }

    /// Whether `a` dominates `b`: every control-flow path from the
    /// entry to `b` passes through `a`. A block dominates itself
    /// (`a == b` returns `true`) even if `a` is unreachable.
    ///
    /// Implemented as a walk up the dominator tree from `b` to the
    /// entry: if `a` is encountered the walk returns `true`; if the
    /// walk reaches `None` (entry or an unreachable block) it returns
    /// `false`.
    pub fn dominates(&self, a: BlockId, b: BlockId) -> bool {
        let mut cur = Some(b);
        while let Some(c) = cur {
            if c == a {
                return true;
            }
            cur = self.idom(c);
        }
        false
    }

    /// Whether `a` strictly dominates `b`: `a` dominates `b` and
    /// `a != b`.
    pub fn strictly_dominates(&self, a: BlockId, b: BlockId) -> bool {
        a != b && self.dominates(a, b)
    }
}

impl Cfg {
    /// Build a control flow graph from a proto's instruction stream.
    ///
    /// Construction proceeds in four steps:
    /// 1. Identify block leaders (entry, JMP targets, instructions
    ///    following a block-ending opcode).
    /// 2. Group instructions between consecutive leaders into basic
    ///    blocks.
    /// 3. Classify each block's terminator from its trailing
    ///    instruction(s) and populate successor edges.
    /// 4. Collect predecessor edges by walking the successor lists.
    ///
    /// The FUNC* header at index 0 is never part of any block.
    pub fn build(proto: &Proto) -> Cfg {
        // Real instructions live at indices 1..n. Index 0 is the
        // synthetic FUNC* header and is excluded from every block.
        let n = proto.insts.len();
        if n <= 1 {
            // No real instructions. Well-formed bytecode always has at
            // least one (e.g. RET0), but guard anyway so a malformed
            // proto produces an empty CFG rather than a panic.
            return Cfg {
                blocks: Vec::new(),
                entry: BlockId(0),
            };
        }

        // ---- Step 1: identify block leaders -----------------------------
        let mut leaders: BTreeSet<usize> = BTreeSet::new();
        leaders.insert(1); // entry: first real instruction.

        for i in 1..n {
            let op = proto.insts[i].op;
            if Self::is_unconditional_terminator(op) {
                // RET*/CALLT/CALLMT: the instruction after them (if
                // any) starts a new block.
                if i + 1 < n {
                    leaders.insert(i + 1);
                }
            } else if op == Opcode::Jmp {
                // JMP: its target is a leader, and the instruction
                // after the JMP is a leader (the false-edge target
                // for an ISxx+JMP pair, or unreachable for a
                // standalone JMP).
                if let Some(target) = Self::jmp_target(proto, i) {
                    leaders.insert(target);
                }
                if i + 1 < n {
                    leaders.insert(i + 1);
                }
            }
            // ISxx is *not* a leader source on its own: an ISxx never
            // ends a block. The ISxx+JMP pair is held in the same
            // block, with the JMP as the trailing instruction.
        }

        // ---- Step 2: build blocks ---------------------------------------
        // Each block contains instructions [leader, next_leader).
        let leaders_vec: Vec<usize> = leaders.iter().copied().collect();
        let leader_to_block: HashMap<usize, BlockId> = leaders_vec
            .iter()
            .enumerate()
            .map(|(block_idx, &leader)| (leader, BlockId(block_idx as u32)))
            .collect();

        let mut blocks: Vec<BasicBlock> = Vec::with_capacity(leaders_vec.len());
        for (block_idx, &leader) in leaders_vec.iter().enumerate() {
            let next_leader = if block_idx + 1 < leaders_vec.len() {
                leaders_vec[block_idx + 1]
            } else {
                n
            };
            let insts: Vec<InstructionId> = (leader..next_leader)
                .map(|i| InstructionId(i as u32))
                .collect();
            blocks.push(BasicBlock {
                id: BlockId(block_idx as u32),
                insts,
                terminator: Terminator::Return, // placeholder; step 3 overwrites
                preds: Vec::new(),
                succs: Vec::new(),
            });
        }

        // ---- Step 3: classify terminators + wire successors -------------
        for block_idx in 0..blocks.len() {
            let next_block_id =
                (block_idx + 1 < blocks.len()).then_some(BlockId((block_idx + 1) as u32));
            let last_inst_idx = blocks[block_idx].insts.last().map(|id| id.0 as usize);
            let last_op = last_inst_idx.map(|i| proto.insts[i].op);

            let (terminator, succs) = Self::classify_terminator(
                proto,
                last_op,
                last_inst_idx,
                next_block_id,
                &leader_to_block,
            );

            blocks[block_idx].terminator = terminator;
            blocks[block_idx].succs = succs;
        }

        // ---- Step 4: wire predecessors ----------------------------------
        // Walk each block's successor list and add the current block
        // to each successor's preds. Iterating block ids in ascending
        // order yields `preds` lists in ascending predecessor-id
        // order, which keeps CFG construction deterministic.
        for block_idx in 0..blocks.len() {
            // Snapshot successor ids before the mutable iteration so
            // the borrow checker is happy.
            let succ_ids: Vec<BlockId> =
                blocks[block_idx].succs.iter().map(|(id, _)| *id).collect();
            let this_id = BlockId(block_idx as u32);
            for succ_id in succ_ids {
                let succ_idx = succ_id.0 as usize;
                if succ_idx < blocks.len() && !blocks[succ_idx].preds.contains(&this_id) {
                    blocks[succ_idx].preds.push(this_id);
                }
            }
        }

        let entry = leader_to_block.get(&1).copied().unwrap_or(BlockId(0));
        Cfg { blocks, entry }
    }

    /// Compute the immediate-dominator tree using the
    /// Cooper-Harvey-Kennedy iterative algorithm.
    ///
    /// Returns a [`DominatorTree`] whose `immediate_doms` vector is
    /// indexed by `BlockId`:
    /// - The entry block maps to `None` (no strict dominator).
    /// - Reachable non-entry blocks map to `Some(idom)`.
    /// - Unreachable blocks (not on any path from the entry) map to
    ///   `None`.
    ///
    /// # Algorithm
    ///
    /// 1. **Reverse postorder (RPO)** of the reachable blocks via DFS
    ///    from the entry. The entry receives RPO number 0; each block
    ///    is (mostly) processed after its predecessors in RPO, which
    ///    speeds convergence to a fixpoint.
    /// 2. **Fixpoint iteration** over RPO (skipping entry):
    ///    `new_idom = first processed pred`, then for each other
    ///    processed pred `p`, `new_idom = intersect(p, new_idom)`.
    /// 3. **`intersect(b1, b2)`** walks both nodes up the dom tree
    ///    toward the entry via RPO-number comparisons (the "finger"
    ///    technique) until they meet at their lowest common ancestor.
    ///
    /// The algorithm converges in at most `N+1` iterations for a CFG
    /// of `N` reachable blocks; in practice it converges in 2-3 for
    /// typical LuaJIT CFGs.
    ///
    /// # Entry convention
    ///
    /// Internally the entry's idom is a self-loop (`Some(entry)`) so
    /// that `intersect` never encounters `None` on a reachable node;
    /// this is converted to `None` in the public result.
    pub fn compute_dominators(&self) -> DominatorTree {
        let n = self.blocks.len();
        if n == 0 {
            return DominatorTree {
                immediate_doms: Vec::new(),
            };
        }

        let entry = self.entry;
        let entry_idx = entry.0 as usize;

        // ---- Step 1: reverse postorder of reachable blocks ---------
        let rpo = self.reverse_postorder();

        // RPO numbers: entry = 0, increasing along DFS tree edges.
        // Unreachable blocks get u32::MAX; intersect never reads them
        // (only reachable blocks have idom != None).
        let mut rpo_number = vec![u32::MAX; n];
        for (i, &b) in rpo.iter().enumerate() {
            rpo_number[b.0 as usize] = i as u32;
        }

        // ---- Step 2: CHK fixpoint ---------------------------------
        // Internal entry convention: idom[entry] = Some(entry) (self-
        // loop) so intersect can walk any reachable node all the way
        // up without hitting None. Converted to None in the result.
        let mut idom: Vec<Option<BlockId>> = vec![None; n];
        idom[entry_idx] = Some(entry);

        let mut changed = true;
        while changed {
            changed = false;
            // Process reachable blocks in RPO, skipping entry.
            for &b in &rpo {
                let b_idx = b.0 as usize;
                if b_idx == entry_idx {
                    continue;
                }

                // Seed new_idom from the first pred whose idom is set.
                let preds = &self.blocks[b_idx].preds;
                let mut new_idom: Option<BlockId> = None;
                for &p in preds {
                    if idom[p.0 as usize].is_some() {
                        new_idom = Some(p);
                        break;
                    }
                }
                let Some(mut new_idom) = new_idom else {
                    // No processed pred yet this iteration (e.g. a
                    // loop header whose back-edge pred is still
                    // unset). Defer to a later iteration.
                    continue;
                };
                // Fold in the remaining processed preds.
                for &p in preds {
                    if p == new_idom {
                        continue;
                    }
                    if idom[p.0 as usize].is_some() {
                        new_idom = intersect(new_idom, p, &idom, &rpo_number);
                    }
                }

                if idom[b_idx] != Some(new_idom) {
                    idom[b_idx] = Some(new_idom);
                    changed = true;
                }
            }
        }

        // ---- Step 3: publish entry as None -------------------------
        idom[entry_idx] = None;
        DominatorTree {
            immediate_doms: idom,
        }
    }

    /// Compute the reverse postorder of the reachable blocks via an
    /// iterative DFS from the entry.
    ///
    /// Returns only blocks reachable from the entry; unreachable
    /// blocks (no path from entry) are absent. The entry is first in
    /// the result, so it receives RPO number 0.
    fn reverse_postorder(&self) -> Vec<BlockId> {
        let n = self.blocks.len();
        let entry_idx = self.entry.0 as usize;
        let mut visited = vec![false; n];
        let mut postorder: Vec<BlockId> = Vec::with_capacity(n);
        // Iterative DFS to avoid stack overflow on deep CFGs. Each
        // frame is (block, next-succ-index); a block is appended to
        // postorder once all its successors have been explored.
        let mut stack: Vec<(BlockId, usize)> = Vec::new();
        visited[entry_idx] = true;
        stack.push((self.entry, 0));
        while let Some(&(block, succ_i)) = stack.last() {
            let succs = &self.blocks[block.0 as usize].succs;
            if succ_i < succs.len() {
                let next = succs[succ_i].0;
                stack.last_mut().unwrap().1 = succ_i + 1;
                if !visited[next.0 as usize] {
                    visited[next.0 as usize] = true;
                    stack.push((next, 0));
                }
            } else {
                postorder.push(block);
                stack.pop();
            }
        }
        postorder.reverse();
        postorder
    }

    /// Compute a JMP's target index in [`Proto::insts`], returning
    /// `None` if the computed index falls outside `[1, insts.len())`.
    ///
    /// Per format doc §4.2: `j = (D as i32) - 0x8000` (subtracting the
    /// bias), and `target = i + 1 + j` where `i` is the JMP's own
    /// index. See the module docs for empirical verification and the
    /// important distinction between bias subtraction and bit
    /// reinterpretation.
    fn jmp_target(proto: &Proto, i: usize) -> Option<usize> {
        let d = proto.insts[i].d();
        // Bias subtraction (NOT `(d as u16) as i16` bit reinterpret).
        let j = i32::from(d) - 0x8000;
        let target = i as i32 + 1 + j;
        if target >= 1 && (target as usize) < proto.insts.len() {
            Some(target as usize)
        } else {
            None
        }
    }

    /// Decide a block's terminator and successor edges from its
    /// trailing instruction(s).
    fn classify_terminator(
        proto: &Proto,
        last_op: Option<Opcode>,
        last_inst_idx: Option<usize>,
        next_block_id: Option<BlockId>,
        leader_to_block: &HashMap<usize, BlockId>,
    ) -> (Terminator, Vec<(BlockId, EdgeKind)>) {
        match last_op {
            Some(Opcode::Ret0 | Opcode::Ret1 | Opcode::Ret | Opcode::Retm) => {
                (Terminator::Return, Vec::new())
            }
            Some(Opcode::Callt | Opcode::Callmt) => (
                Terminator::TailCall(InstructionId(last_inst_idx.unwrap() as u32)),
                Vec::new(),
            ),
            Some(Opcode::Jmp) => {
                let jmp_idx = last_inst_idx.unwrap();
                let target_block =
                    Self::jmp_target(proto, jmp_idx).and_then(|t| leader_to_block.get(&t).copied());
                // Look at the proto-level previous instruction: in
                // well-formed bytecode an ISxx is always immediately
                // followed by its consuming JMP and lives in the same
                // block (ISxx is not a terminator and only becomes a
                // leader if it's another JMP's target — rare). The
                // proto-level check is robust to that rare split too.
                let prev_is_isxx = jmp_idx >= 2 && Self::is_isxx(proto.insts[jmp_idx - 1].op);
                match (prev_is_isxx, target_block, next_block_id) {
                    (true, Some(true_edge), Some(false_edge)) => (
                        Terminator::ConditionalBranch {
                            condition: InstructionId((jmp_idx - 1) as u32),
                            true_edge,
                            false_edge,
                        },
                        vec![(true_edge, EdgeKind::True), (false_edge, EdgeKind::False)],
                    ),
                    _ => {
                        // Standalone JMP, or an ISxx+JMP whose target
                        // or fall-through we couldn't resolve (out of
                        // range). Fall back to an unconditional Jump;
                        // if the target was unresolvable there are no
                        // successors.
                        let succs = match target_block {
                            Some(t) => vec![(t, EdgeKind::Unconditional)],
                            None => Vec::new(),
                        };
                        (Terminator::Jump(target_block.unwrap_or(BlockId(0))), succs)
                    }
                }
            }
            _ => match next_block_id {
                // No explicit terminator: fall through to the next block.
                Some(next) => (
                    Terminator::Fallthrough(next),
                    vec![(next, EdgeKind::Unconditional)],
                ),
                // Trailing block with no recognized terminator and no
                // next block to fall through to. Well-formed bytecode
                // always ends with RET*/CALLT so this branch is
                // unreachable in practice; we return Return to keep
                // the CFG well-formed rather than panicking.
                None => (Terminator::Return, Vec::new()),
            },
        }
    }

    /// Whether an opcode is a block-ending "unconditional" terminator
    /// in the leader-detection sense (RET* / CALLT / CALLMT). JMP is
    /// also a terminator but is handled with separate logic because
    /// its leader set depends on the jump target; ISxx is *not* a
    /// terminator (it's consumed by the following JMP).
    fn is_unconditional_terminator(op: Opcode) -> bool {
        matches!(
            op,
            Opcode::Ret0
                | Opcode::Ret1
                | Opcode::Ret
                | Opcode::Retm
                | Opcode::Callt
                | Opcode::Callmt
        )
    }

    /// Whether an opcode is an ISxx test instruction. Every ISxx is
    /// immediately followed by a JMP in well-formed LuaJIT bytecode;
    /// the pair together forms a [`Terminator::ConditionalBranch`].
    fn is_isxx(op: Opcode) -> bool {
        matches!(
            op,
            Opcode::Islt
                | Opcode::Isge
                | Opcode::Isle
                | Opcode::Isgt
                | Opcode::Iseqv
                | Opcode::Isnev
                | Opcode::Iseqs
                | Opcode::Isnes
                | Opcode::Iseqn
                | Opcode::Isnen
                | Opcode::Iseqp
                | Opcode::Isnep
                | Opcode::Istc
                | Opcode::Isfc
                | Opcode::Ist
                | Opcode::Isf
        )
    }
}

/// Cooper-Harvey-Kennedy `intersect`: lowest common ancestor of two
/// reachable nodes in the dominator tree, found by walking both
/// "fingers" up the tree using RPO numbers.
///
/// Walks the node with the *larger* RPO number up (toward the entry,
/// which has RPO number 0) until the two meet. Because the entry's
/// idom is a self-loop internally and both args are reachable, the
/// walk always terminates — at worst both fingers reach the entry.
///
/// Both `b1` and `b2` must have `idom != None` (i.e. be reachable).
fn intersect(
    mut b1: BlockId,
    mut b2: BlockId,
    idom: &[Option<BlockId>],
    rpo_number: &[u32],
) -> BlockId {
    while b1 != b2 {
        while rpo_number[b1.0 as usize] > rpo_number[b2.0 as usize] {
            b1 = idom[b1.0 as usize].expect("intersect arg must be reachable");
        }
        while rpo_number[b2.0 as usize] > rpo_number[b1.0 as usize] {
            b2 = idom[b2.0 as usize].expect("intersect arg must be reachable");
        }
    }
    b1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Instruction, Opcode, Proto};

    // ---- proto / instruction builders ----------------------------------

    /// Build an AD-format instruction.
    fn ad(op: Opcode, a: u8, d: u16) -> Instruction {
        Instruction {
            op,
            a,
            b_or_d: u32::from(d),
            c: 0,
        }
    }

    /// Build a proto with the FUNC* header prepended to the given real
    /// instruction stream. Main-chunk conventions: VARARG, no params,
    /// framesize 1 (sufficient for the test fixtures).
    fn proto_with_real_insts(real_insts: Vec<Instruction>) -> Proto {
        let mut insts = Vec::with_capacity(real_insts.len() + 1);
        insts.push(Instruction::synthetic_header(Opcode::Funcv, 1));
        insts.extend(real_insts);
        Proto {
            flags: 0x02, // VARARG (main chunk convention)
            numparams: 0,
            framesize: 1,
            upvalues: Vec::new(),
            gc_consts: Vec::new(),
            num_consts: Vec::new(),
            insts,
            debug: None,
        }
    }

    /// Assert two BlockIds match, with a readable panic.
    fn assert_block_id(actual: BlockId, expected: u32) {
        assert_eq!(actual, BlockId(expected), "expected BlockId({})", expected);
    }

    // ---- Test 1: linear sequence (no branches) -------------------------

    // Source: `return 5` → KSHORT 0 5; RET1 0 2.
    #[test]
    fn linear_sequence_produces_single_return_block() {
        let proto = proto_with_real_insts(vec![
            ad(Opcode::Kshort, 0, 5), // idx 1
            ad(Opcode::Ret1, 0, 2),   // idx 2
        ]);
        let cfg = Cfg::build(&proto);

        assert_eq!(cfg.blocks.len(), 1, "linear code should be one block");
        assert_block_id(cfg.entry, 0);

        let entry = &cfg.blocks[0];
        assert_eq!(
            entry.insts,
            vec![InstructionId(1), InstructionId(2)],
            "entry should contain KSHORT and RET1"
        );
        assert!(
            matches!(entry.terminator, Terminator::Return),
            "entry should be a Return"
        );
        assert!(entry.succs.is_empty(), "Return has no successors");
        assert!(entry.preds.is_empty(), "entry has no predecessors");
    }

    // ---- Test 4: multi-statement linear sequence -----------------------

    // Source: `local x = 1; return x` → KSHORT 0 1; RET1 0 2.
    // Structurally identical to Test 1.
    #[test]
    fn multi_statement_linear_produces_single_return_block() {
        let proto = proto_with_real_insts(vec![
            ad(Opcode::Kshort, 0, 1), // idx 1
            ad(Opcode::Ret1, 0, 2),   // idx 2
        ]);
        let cfg = Cfg::build(&proto);

        assert_eq!(cfg.blocks.len(), 1);
        assert!(matches!(cfg.blocks[0].terminator, Terminator::Return));
        assert_eq!(
            cfg.blocks[0].insts,
            vec![InstructionId(1), InstructionId(2)]
        );
    }

    // ---- Test 2: if/then -----------------------------------------------

    // Source: `if x then return 1 end`. Bytecode (from `luajit -bl`):
    //   0001 GGET  0 0      ; "x"
    //   0002 ISF   0
    //   0003 JMP   1 => 0006   (D=0x8002 → j=2 → target=3+1+2=6)
    //   0004 KSHORT 0 1
    //   0005 RET1  0 2
    //   0006 RET0  0 1
    #[test]
    fn if_then_produces_conditional_branch_with_three_blocks() {
        let proto = proto_with_real_insts(vec![
            ad(Opcode::Gget, 0, 0),     // idx 1
            ad(Opcode::Isf, 0, 0),      // idx 2  (D unused for ISxx)
            ad(Opcode::Jmp, 1, 0x8002), // idx 3: target = idx 6
            ad(Opcode::Kshort, 0, 1),   // idx 4
            ad(Opcode::Ret1, 0, 2),     // idx 5
            ad(Opcode::Ret0, 0, 1),     // idx 6
        ]);
        let cfg = Cfg::build(&proto);

        assert_eq!(cfg.blocks.len(), 3, "if/then should produce 3 blocks");
        assert_block_id(cfg.entry, 0);

        // Block 0 (entry): [GGET, ISF, JMP] → ConditionalBranch.
        let entry = &cfg.blocks[0];
        assert_eq!(
            entry.insts,
            vec![InstructionId(1), InstructionId(2), InstructionId(3)]
        );
        match &entry.terminator {
            Terminator::ConditionalBranch {
                condition,
                true_edge,
                false_edge,
            } => {
                assert_eq!(*condition, InstructionId(2), "condition is ISF at idx 2");
                // JMP target is idx 6 (the RET0 / merge block).
                assert_block_id(*true_edge, 2);
                // Fall-through is idx 4 (the then-body).
                assert_block_id(*false_edge, 1);
            }
            other => panic!(
                "entry terminator should be ConditionalBranch, got {:?}",
                other
            ),
        }
        assert_eq!(
            entry.succs,
            vec![(BlockId(2), EdgeKind::True), (BlockId(1), EdgeKind::False)]
        );
        assert!(entry.preds.is_empty(), "entry has no predecessors");

        // Block 1 (then-body): [KSHORT, RET1] → Return.
        let then_body = &cfg.blocks[1];
        assert_eq!(then_body.insts, vec![InstructionId(4), InstructionId(5)]);
        assert!(
            matches!(then_body.terminator, Terminator::Return),
            "then-body should Return"
        );
        assert!(then_body.succs.is_empty());
        assert_eq!(then_body.preds, vec![BlockId(0)]);

        // Block 2 (merge): [RET0] → Return.
        let merge = &cfg.blocks[2];
        assert_eq!(merge.insts, vec![InstructionId(6)]);
        assert!(
            matches!(merge.terminator, Terminator::Return),
            "merge should Return"
        );
        assert!(merge.succs.is_empty());
        assert_eq!(merge.preds, vec![BlockId(0)]);
    }

    // ---- Test 3: if/then/else ------------------------------------------

    // Source: `if x then return 1 else return 2 end`. Bytecode:
    //   0001 GGET  0 0
    //   0002 ISF   0
    //   0003 JMP   1 => 0007   (D=0x8003 → j=3 → target=3+1+3=7)
    //   0004 KSHORT 0 1
    //   0005 RET1  0 2
    //   0006 JMP   0 => 0009   (D=0x8002 → j=2 → target=6+1+2=9)
    //   0007 KSHORT 0 2
    //   0008 RET1  0 2
    //   0009 RET0  0 1
    //
    // Note: the JMP at 0006 follows the then-body's RET1 and is dead
    // code (the RET1 already returned). LuaJIT still emits it as part
    // of its standard if/else codegen pattern, so the CFG correctly
    // surfaces it as its own block. This yields 5 blocks (entry,
    // then-body, dead-jump, else-body, merge) rather than the 4 the
    // spec text anticipated — the 4-block count would only arise for
    // `if/else` shapes where neither branch returns. The block-edge
    // structure is what matters and is verified below.
    #[test]
    fn if_then_else_produces_conditional_branch_and_five_blocks() {
        let proto = proto_with_real_insts(vec![
            ad(Opcode::Gget, 0, 0),     // idx 1
            ad(Opcode::Isf, 0, 0),      // idx 2
            ad(Opcode::Jmp, 1, 0x8003), // idx 3: target = idx 7 (else-body)
            ad(Opcode::Kshort, 0, 1),   // idx 4
            ad(Opcode::Ret1, 0, 2),     // idx 5
            ad(Opcode::Jmp, 0, 0x8002), // idx 6: target = idx 9 (merge)
            ad(Opcode::Kshort, 0, 2),   // idx 7
            ad(Opcode::Ret1, 0, 2),     // idx 8
            ad(Opcode::Ret0, 0, 1),     // idx 9
        ]);
        let cfg = Cfg::build(&proto);

        assert_eq!(cfg.blocks.len(), 5);
        assert_block_id(cfg.entry, 0);

        // Block 0 (entry): [GGET, ISF, JMP] → ConditionalBranch.
        let entry = &cfg.blocks[0];
        assert_eq!(
            entry.insts,
            vec![InstructionId(1), InstructionId(2), InstructionId(3)]
        );
        match &entry.terminator {
            Terminator::ConditionalBranch {
                condition,
                true_edge,
                false_edge,
            } => {
                assert_eq!(*condition, InstructionId(2));
                // JMP target idx 7 → Block 3 (else-body).
                assert_block_id(*true_edge, 3);
                // Fall-through idx 4 → Block 1 (then-body).
                assert_block_id(*false_edge, 1);
            }
            other => panic!("expected ConditionalBranch, got {:?}", other),
        }
        assert_eq!(
            entry.succs,
            vec![(BlockId(3), EdgeKind::True), (BlockId(1), EdgeKind::False)]
        );

        // Block 1 (then-body): [KSHORT, RET1] → Return.
        let then_body = &cfg.blocks[1];
        assert_eq!(then_body.insts, vec![InstructionId(4), InstructionId(5)]);
        assert!(matches!(then_body.terminator, Terminator::Return));
        assert!(then_body.succs.is_empty());
        assert_eq!(then_body.preds, vec![BlockId(0)]);

        // Block 2 (dead jump after then-body): [JMP] → Jump(merge).
        // RET1 at idx 5 ended the then-body block, so the JMP at
        // idx 6 becomes its own block. Its proto-level predecessor
        // (idx 5) is RET1, not ISxx, so it classifies as a standalone
        // Jump.
        let dead_jump = &cfg.blocks[2];
        assert_eq!(dead_jump.insts, vec![InstructionId(6)]);
        match &dead_jump.terminator {
            Terminator::Jump(target) => assert_block_id(*target, 4),
            other => panic!("expected Jump, got {:?}", other),
        }
        assert_eq!(dead_jump.succs, vec![(BlockId(4), EdgeKind::Unconditional)]);

        // Block 3 (else-body): [KSHORT, RET1] → Return.
        let else_body = &cfg.blocks[3];
        assert_eq!(else_body.insts, vec![InstructionId(7), InstructionId(8)]);
        assert!(matches!(else_body.terminator, Terminator::Return));
        assert!(else_body.succs.is_empty());
        assert_eq!(else_body.preds, vec![BlockId(0)]);

        // Block 4 (merge): [RET0] → Return.
        let merge = &cfg.blocks[4];
        assert_eq!(merge.insts, vec![InstructionId(9)]);
        assert!(matches!(merge.terminator, Terminator::Return));
        assert!(merge.succs.is_empty());
        // Merge has two predecessors: the dead-jump (Block 2). The
        // else-body returns, so it does not flow into the merge.
        assert_eq!(merge.preds, vec![BlockId(2)]);
    }

    // ---- Standalone unconditional jump ---------------------------------

    // A standalone JMP not preceded by ISxx should classify as
    // `Terminator::Jump`, not `ConditionalBranch`. Source shape:
    // `goto label` style, but constructed directly to keep the test
    // focused. Bytecode:
    //   0001 KSHORT 0 1
    //   0002 JMP    0 => 0004   (D=0x8001 → j=1 → target=2+1+1=4)
    //   0003 KSHORT 0 2
    //   0004 RET0   0 1
    #[test]
    fn standalone_jmp_classifies_as_jump() {
        let proto = proto_with_real_insts(vec![
            ad(Opcode::Kshort, 0, 1),   // idx 1
            ad(Opcode::Jmp, 0, 0x8001), // idx 2: target = idx 4
            ad(Opcode::Kshort, 0, 2),   // idx 3
            ad(Opcode::Ret0, 0, 1),     // idx 4
        ]);
        let cfg = Cfg::build(&proto);

        // Leaders: {1 (entry), 3 (after JMP at 2), 4 (target of JMP)}.
        assert_eq!(cfg.blocks.len(), 3);

        // Block 0: [KSHORT, JMP] → Jump(target=Block 2).
        let b0 = &cfg.blocks[0];
        assert_eq!(b0.insts, vec![InstructionId(1), InstructionId(2)]);
        match &b0.terminator {
            Terminator::Jump(target) => assert_block_id(*target, 2),
            other => panic!("expected Jump, got {:?}", other),
        }
        assert_eq!(b0.succs, vec![(BlockId(2), EdgeKind::Unconditional)]);

        // Block 1: [KSHORT] → Fallthrough. Structurally a predecessor
        // of Block 2 even though it is unreachable from the entry
        // (Block 0 jumps over it). CFG edges are syntactic, not
        // reachability-based; unreachable-block elimination is a
        // later stage's concern.
        let b1 = &cfg.blocks[1];
        assert_eq!(b1.insts, vec![InstructionId(3)]);
        match &b1.terminator {
            Terminator::Fallthrough(target) => assert_block_id(*target, 2),
            other => panic!("expected Fallthrough, got {:?}", other),
        }
        assert!(b1.preds.is_empty(), "Block 1 is unreachable from entry");

        // Block 2: [RET0] → Return. Two structural predecessors:
        // Block 0 (via Jump) and Block 1 (via Fallthrough), in
        // ascending id order.
        let b2 = &cfg.blocks[2];
        assert_eq!(b2.insts, vec![InstructionId(4)]);
        assert!(matches!(b2.terminator, Terminator::Return));
        assert_eq!(b2.preds, vec![BlockId(0), BlockId(1)]);
    }

    // ---- Tail call terminator ------------------------------------------

    // CALLT (tail call) should classify as `Terminator::TailCall`
    // with no successors. Bytecode:
    //   0001 GGET  0 0      ; f
    //   0002 CALLT 0 2      ; tail call f()
    #[test]
    fn callt_classifies_as_tail_call() {
        let proto = proto_with_real_insts(vec![
            ad(Opcode::Gget, 0, 0),  // idx 1
            ad(Opcode::Callt, 0, 2), // idx 2
        ]);
        let cfg = Cfg::build(&proto);

        assert_eq!(cfg.blocks.len(), 1);
        match &cfg.blocks[0].terminator {
            Terminator::TailCall(InstructionId(id)) => assert_eq!(*id, 2),
            other => panic!("expected TailCall, got {:?}", other),
        }
        assert!(cfg.blocks[0].succs.is_empty(), "TailCall has no successors");
    }

    // ---- Fallthrough terminator ----------------------------------------

    // A block whose last instruction is not a terminator should
    // fall through to the next block. Constructed directly: a JMP
    // in the middle (whose presence makes the post-JMP instruction
    // a leader) followed by non-terminator instructions, then RET.
    #[test]
    fn non_terminator_block_falls_through() {
        //   0001 JMP    0 => 0003   (target=3)
        //   0002 KSHORT 0 1         (leader; Fallthrough into next)
        //   0003 RET0   0 1         (leader: JMP target)
        // Block 0 [JMP] → Jump(Block 2). Block 1 [KSHORT] ends with
        // a non-terminator and has a successor → Fallthrough(Block 2).
        // Recompute: we want JMP at idx 1 to target idx 3.
        // target = 1 + 1 + j = 3 → j = 1 → D = 0x8001.
        let proto = proto_with_real_insts(vec![
            ad(Opcode::Jmp, 0, 0x8001), // idx 1: target = idx 3
            ad(Opcode::Kshort, 0, 1),   // idx 2: leader (after JMP)
            ad(Opcode::Ret0, 0, 1),     // idx 3: leader (JMP target)
        ]);
        let cfg = Cfg::build(&proto);

        // Leaders: {1, 2 (after JMP), 3 (JMP target)}.
        assert_eq!(cfg.blocks.len(), 3);

        // Block 1 [KSHORT] ends with a non-terminator → Fallthrough.
        let fallthrough_block = &cfg.blocks[1];
        assert_eq!(fallthrough_block.insts, vec![InstructionId(2)]);
        match &fallthrough_block.terminator {
            Terminator::Fallthrough(target) => assert_block_id(*target, 2),
            other => panic!("expected Fallthrough, got {:?}", other),
        }
        assert_eq!(
            fallthrough_block.succs,
            vec![(BlockId(2), EdgeKind::Unconditional)]
        );
    }

    // ---- JMP target formula verification -------------------------------

    // Direct verification of the jump-target formula against the
    // empirically observed D-field values from `luajit -bl`. These
    // are the same D values the if/then and if/then/else fixtures
    // produce; see the module-level docs for the manual derivation.
    #[test]
    fn jmp_target_formula_matches_luajit_bl_output() {
        // if/then: JMP at internal idx 3, D=0x8002 → target 6.
        let ifthen = proto_with_real_insts(vec![
            ad(Opcode::Gget, 0, 0),
            ad(Opcode::Isf, 0, 0),
            ad(Opcode::Jmp, 1, 0x8002), // idx 3
            ad(Opcode::Kshort, 0, 1),
            ad(Opcode::Ret1, 0, 2),
            ad(Opcode::Ret0, 0, 1), // idx 6
        ]);
        assert_eq!(Cfg::jmp_target(&ifthen, 3), Some(6));

        // if/then/else: two JMPs.
        //   JMP at idx 3, D=0x8003 → target 7.
        //   JMP at idx 6, D=0x8002 → target 9.
        let ifelse = proto_with_real_insts(vec![
            ad(Opcode::Gget, 0, 0),     // idx 1
            ad(Opcode::Isf, 0, 0),      // idx 2
            ad(Opcode::Jmp, 1, 0x8003), // idx 3
            ad(Opcode::Kshort, 0, 1),   // idx 4
            ad(Opcode::Ret1, 0, 2),     // idx 5
            ad(Opcode::Jmp, 0, 0x8002), // idx 6
            ad(Opcode::Kshort, 0, 2),   // idx 7
            ad(Opcode::Ret1, 0, 2),     // idx 8
            ad(Opcode::Ret0, 0, 1),     // idx 9
        ]);
        assert_eq!(Cfg::jmp_target(&ifelse, 3), Some(7));
        assert_eq!(Cfg::jmp_target(&ifelse, 6), Some(9));
    }

    // Backward jump (negative offset). Verifies the sign correction
    // of the biased D encoding works in the negative direction.
    #[test]
    fn jmp_target_handles_backward_jump() {
        // Construct a 3-instruction loop where idx 3 jumps back to
        // idx 2. target = 3 + 1 + j = 2 → j = -2 → D = 0x8000 - 2 = 0x7ffe.
        // (D is u16; (0x7ffe as i16) = -2.)
        let proto = proto_with_real_insts(vec![
            ad(Opcode::Kshort, 0, 0),   // idx 1
            ad(Opcode::Loop, 0, 0),     // idx 2 (LOOP marker; loop target)
            ad(Opcode::Jmp, 0, 0x7ffe), // idx 3: target = idx 2 (back-edge)
        ]);
        assert_eq!(Cfg::jmp_target(&proto, 3), Some(2));
    }

    // ---- Edge case: proto with only the FUNC* header -------------------

    #[test]
    fn empty_proto_produces_empty_cfg() {
        let proto = Proto {
            flags: 0x02,
            numparams: 0,
            framesize: 0,
            upvalues: Vec::new(),
            gc_consts: Vec::new(),
            num_consts: Vec::new(),
            insts: vec![Instruction::synthetic_header(Opcode::Funcv, 0)],
            debug: None,
        };
        let cfg = Cfg::build(&proto);
        assert!(cfg.blocks.is_empty(), "no real instructions → no blocks");
    }

    // ---- Edge case: single RET0 (return-only chunk) --------------------

    #[test]
    fn single_ret0_produces_single_return_block() {
        let proto = proto_with_real_insts(vec![ad(Opcode::Ret0, 0, 1)]);
        let cfg = Cfg::build(&proto);
        assert_eq!(cfg.blocks.len(), 1);
        assert_eq!(cfg.blocks[0].insts, vec![InstructionId(1)]);
        assert!(matches!(cfg.blocks[0].terminator, Terminator::Return));
    }

    // ====================================================================
    // Stage 7b: dominator-tree tests
    // ====================================================================

    /// Pretty-print a dominator tree as "block -> idom" pairs for
    /// readable assertion failures.
    fn dump_dom(tree: &DominatorTree) -> String {
        tree.immediate_doms
            .iter()
            .enumerate()
            .map(|(i, idom)| match idom {
                Some(d) => format!("{}->{}", i, d.0),
                None => format!("{}->None", i),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    // ---- Dom tree: linear sequence (1 block) ---------------------------
    //
    // Source: `return 5`. Single entry block that returns. The entry
    // has no strict dominator (idom = None) but dominates itself.
    #[test]
    fn dom_tree_linear_sequence() {
        let proto = proto_with_real_insts(vec![
            ad(Opcode::Kshort, 0, 5), // idx 1
            ad(Opcode::Ret1, 0, 2),   // idx 2
        ]);
        let cfg = Cfg::build(&proto);
        let tree = cfg.compute_dominators();

        assert_eq!(
            tree.immediate_doms,
            vec![None],
            "single-block CFG: entry has no strict dominator. tree = {}",
            dump_dom(&tree)
        );
        // A block always dominates itself, even the entry.
        assert!(tree.dominates(BlockId(0), BlockId(0)));
        assert!(!tree.strictly_dominates(BlockId(0), BlockId(0)));
        assert_eq!(tree.idom(BlockId(0)), None);
    }

    // ---- Dom tree: if/then (3 blocks, all reachable) -------------------
    //
    // Source: `if x then return 1 end`. CFG:
    //   entry(0) -true->  merge(2)
    //          \-false-> then(1) [returns]
    // Both then-body and merge are direct children of entry; neither
    // dominates the other.
    #[test]
    fn dom_tree_if_then_all_reachable() {
        let proto = proto_with_real_insts(vec![
            ad(Opcode::Gget, 0, 0),     // idx 1
            ad(Opcode::Isf, 0, 0),      // idx 2
            ad(Opcode::Jmp, 1, 0x8002), // idx 3: target = idx 6
            ad(Opcode::Kshort, 0, 1),   // idx 4
            ad(Opcode::Ret1, 0, 2),     // idx 5
            ad(Opcode::Ret0, 0, 1),     // idx 6
        ]);
        let cfg = Cfg::build(&proto);
        let tree = cfg.compute_dominators();

        // entry(0) -> None, then-body(1) -> entry(0), merge(2) -> entry(0).
        assert_eq!(
            tree.immediate_doms,
            vec![None, Some(BlockId(0)), Some(BlockId(0))],
            "tree = {}",
            dump_dom(&tree)
        );

        // Entry dominates every reachable block.
        assert!(tree.dominates(BlockId(0), BlockId(1)));
        assert!(tree.dominates(BlockId(0), BlockId(2)));
        assert!(tree.strictly_dominates(BlockId(0), BlockId(1)));
        assert!(tree.strictly_dominates(BlockId(0), BlockId(2)));

        // Siblings: then-body and merge are children of entry; neither
        // dominates the other.
        assert!(!tree.dominates(BlockId(1), BlockId(2)));
        assert!(!tree.dominates(BlockId(2), BlockId(1)));

        // idom helper round-trips.
        assert_eq!(tree.idom(BlockId(0)), None);
        assert_eq!(tree.idom(BlockId(1)), Some(BlockId(0)));
        assert_eq!(tree.idom(BlockId(2)), Some(BlockId(0)));

        // Self-dominance.
        assert!(tree.dominates(BlockId(1), BlockId(1)));
        assert!(!tree.strictly_dominates(BlockId(1), BlockId(1)));
    }

    // ---- Dom tree: if/else with returns (5 blocks, unreachables) -------
    //
    // Source: `if x then return 1 else return 2 end`. CFG (reusing
    // the Stage 7a fixture):
    //   entry(0)  -true->  else-body(3) [returns]
    //           \-false-> then-body(1) [returns]
    //   dead-jmp(2): no preds (unreachable, follows then-body's RET1).
    //   merge(4): only pred is dead-jmp(2) → also unreachable.
    //
    // Unreachable blocks must have idom = None.
    #[test]
    fn dom_tree_if_else_with_returns_has_unreachable_blocks() {
        let proto = proto_with_real_insts(vec![
            ad(Opcode::Gget, 0, 0),     // idx 1
            ad(Opcode::Isf, 0, 0),      // idx 2
            ad(Opcode::Jmp, 1, 0x8003), // idx 3: target = idx 7
            ad(Opcode::Kshort, 0, 1),   // idx 4
            ad(Opcode::Ret1, 0, 2),     // idx 5
            ad(Opcode::Jmp, 0, 0x8002), // idx 6: dead, target = idx 9
            ad(Opcode::Kshort, 0, 2),   // idx 7
            ad(Opcode::Ret1, 0, 2),     // idx 8
            ad(Opcode::Ret0, 0, 1),     // idx 9
        ]);
        let cfg = Cfg::build(&proto);
        let tree = cfg.compute_dominators();

        // entry(0) -> None
        // then-body(1) -> entry(0)
        // dead-jmp(2) -> None (unreachable)
        // else-body(3) -> entry(0)
        // merge(4) -> None (only pred is unreachable dead-jmp)
        assert_eq!(
            tree.immediate_doms,
            vec![None, Some(BlockId(0)), None, Some(BlockId(0)), None,],
            "tree = {}",
            dump_dom(&tree)
        );

        // Reachable blocks: entry dominates both branches.
        assert!(tree.dominates(BlockId(0), BlockId(1)));
        assert!(tree.dominates(BlockId(0), BlockId(3)));
        // Unreachable blocks are not dominated by entry.
        assert!(!tree.dominates(BlockId(0), BlockId(2)));
        assert!(!tree.dominates(BlockId(0), BlockId(4)));
        // An unreachable block still dominates itself by convention.
        assert!(tree.dominates(BlockId(2), BlockId(2)));
    }

    // ---- Dom tree: if/then with fallthrough merge (3 blocks) -----------
    //
    // Source: `if x then y = 1 end; return y`. Bytecode:
    //   0001 GGET   0 0
    //   0002 ISF    0
    //   0003 JMP    1 => 0005   (D=0x8001: j=1, target=3+1+1=5)
    //   0004 KSHORT 1 1         ; y = 1
    //   0005 RET1   1 2         ; return y
    //
    // CFG: entry(0) [GGET,ISF,JMP] branches; then-body(1) [KSHORT]
    // falls through to merge(2) [RET1]. The merge has two preds
    // (entry via the JMP true-edge, then-body via fallthrough), so
    // idom[merge] = entry (entry is the LCA of both paths).
    #[test]
    fn dom_tree_if_then_with_fallthrough_merge() {
        let proto = proto_with_real_insts(vec![
            ad(Opcode::Gget, 0, 0),     // idx 1
            ad(Opcode::Isf, 0, 0),      // idx 2
            ad(Opcode::Jmp, 1, 0x8001), // idx 3: target = idx 5
            ad(Opcode::Kshort, 1, 1),   // idx 4: y = 1
            ad(Opcode::Ret1, 1, 2),     // idx 5: return y
        ]);
        let cfg = Cfg::build(&proto);

        // Sanity-check the CFG shape first so a Stage 7a regression
        // produces a clear failure rather than a misleading dom-tree
        // mismatch.
        assert_eq!(cfg.blocks.len(), 3, "expected 3 blocks");
        assert_eq!(
            cfg.blocks[2].preds,
            vec![BlockId(0), BlockId(1)],
            "merge should have entry and then-body as preds"
        );

        let tree = cfg.compute_dominators();

        // entry(0) -> None
        // then-body(1) -> entry(0)
        // merge(2) -> entry(0)   [entry dominates both paths into merge]
        assert_eq!(
            tree.immediate_doms,
            vec![None, Some(BlockId(0)), Some(BlockId(0))],
            "tree = {}",
            dump_dom(&tree)
        );
        // Crucially, then-body does NOT dominate merge — the entry's
        // true-edge bypasses then-body entirely and lands in merge.
        assert!(
            !tree.dominates(BlockId(1), BlockId(2)),
            "then-body must not dominate merge (entry bypasses it)"
        );
        assert!(tree.dominates(BlockId(0), BlockId(2)));
    }

    // ---- Dom tree: standalone JMP with unreachable block ---------------
    //
    // Reuses the `standalone_jmp_classifies_as_jump` fixture's shape:
    //   Block 0 [KSHORT, JMP] -> Jump(Block 2)
    //   Block 1 [KSHORT]      -> Fallthrough(Block 2)  [unreachable]
    //   Block 2 [RET0]        -> Return
    // Block 1 is unreachable (no preds). idom[1] = None. Block 2's
    // only *reachable* pred is Block 0, so idom[2] = Block 0.
    #[test]
    fn dom_tree_standalone_jmp_with_unreachable_block() {
        let proto = proto_with_real_insts(vec![
            ad(Opcode::Kshort, 0, 1),   // idx 1
            ad(Opcode::Jmp, 0, 0x8001), // idx 2: target = idx 4
            ad(Opcode::Kshort, 0, 2),   // idx 3 (unreachable)
            ad(Opcode::Ret0, 0, 1),     // idx 4
        ]);
        let cfg = Cfg::build(&proto);
        let tree = cfg.compute_dominators();

        // Block 0 (entry) -> None
        // Block 1 (unreachable) -> None
        // Block 2 -> Block 0 (entry is its only reachable pred)
        assert_eq!(
            tree.immediate_doms,
            vec![None, None, Some(BlockId(0))],
            "tree = {}",
            dump_dom(&tree)
        );
        assert!(tree.dominates(BlockId(0), BlockId(2)));
        assert!(!tree.dominates(BlockId(0), BlockId(1)));
    }

    // ---- Dom tree: empty CFG -------------------------------------------
    //
    // A proto with no real instructions builds an empty CFG; computing
    // dominators must not panic and must return an empty tree.
    #[test]
    fn dom_tree_empty_cfg() {
        let proto = Proto {
            flags: 0x02,
            numparams: 0,
            framesize: 0,
            upvalues: Vec::new(),
            gc_consts: Vec::new(),
            num_consts: Vec::new(),
            insts: vec![Instruction::synthetic_header(Opcode::Funcv, 0)],
            debug: None,
        };
        let cfg = Cfg::build(&proto);
        let tree = cfg.compute_dominators();
        assert!(tree.immediate_doms.is_empty());
        // Out-of-range id lookups are defensive (no panic, returns None).
        assert_eq!(tree.idom(BlockId(0)), None);
        assert_eq!(tree.idom(BlockId(99)), None);
        // dominates(a, b) where neither block exists: entry-vs-block
        // queries must return false (no dominance between ghosts).
        assert!(!tree.dominates(BlockId(0), BlockId(1)));
        assert!(!tree.dominates(BlockId(1), BlockId(0)));
    }

    // ---- Dom tree: idom() out-of-range is defensive --------------------
    //
    // A BlockId past the end of the tree returns None rather than
    // panicking, so callers iterating over block lists don't need to
    // bounds-check before calling idom().
    #[test]
    fn dom_tree_idom_out_of_range_returns_none() {
        let proto = proto_with_real_insts(vec![ad(Opcode::Ret0, 0, 1)]);
        let cfg = Cfg::build(&proto);
        let tree = cfg.compute_dominators();
        assert_eq!(tree.idom(BlockId(99)), None);
        assert!(!tree.dominates(BlockId(0), BlockId(99)));
        assert!(!tree.dominates(BlockId(99), BlockId(0)));
    }
}
