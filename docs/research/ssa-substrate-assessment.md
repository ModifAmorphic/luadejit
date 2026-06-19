# SSA Substrate Assessment (Cytron primary-source reread)

**Audience**: same as prior Phase A docs. Specifically written to be
readable by someone who didn't know what SSA was 30 minutes ago.

**Purpose**: Step 4 follow-up. Reassess the substrate recommendation in
`dataflow-techniques-survey.md` §7 after reading Cytron et al. 1991 in
primary source rather than from general knowledge.

**Honest framing up front**: my prior recommendation ("traditional
iterative dataflow + value numbering, NOT full SSA") was
under-justified. The reasoning I gave made sense on the surface but I
hadn't actually walked through what SSA construction costs or what it
buys. After reading the primary source, the recommendation should
change. This doc explains why, in terms accessible to a non-specialist.

**Status**: revision of the substrate recommendation. The dataflow
survey doc stays; this doc supersedes its §7 conclusion.

---

## 1. What my prior recommendation was, and what was wrong with it

In `dataflow-techniques-survey.md` §7, I recommended *against* SSA,
giving three reasons:

1. *"Decompilers understand code, don't optimize it."*
2. *"SSA's main payoff is making optimizations cleaner."*
3. *"SSA construction is non-trivial and adds complexity."*

Reading these back with primary-source knowledge of SSA in hand, all
three are sloppy:

**Reason 1 is misleading.** Decompilers do transformations — copy
propagation, dead code elimination, common subexpression elimination,
value numbering. These are *exactly* the same transformations that
make SSA valuable in optimizing compilers. The framing "decompilers
don't optimize" elides that expression recovery *is* an optimization
pass on the IR before code generation.

**Reason 2 undersells what SSA gives you.** SSA isn't just about
"making optimizations cleaner." It gives you sparse, explicit use-def
chains as part of the representation itself. That changes the cost of
*every* dataflow question. My prior survey listed the benefits
 dismissively as "cleaner algorithms" — but the difference between
"compute use-def chains via fixpoint iteration every time you need
them" and "have use-def chains baked into the IR" is substantial.

**Reason 3 was asserted, not argued.** I called SSA construction
"non-trivial" without quantifying it. Having now read Cytron, the
construction is more bounded than I implied — call it a few hundred
lines of straightforward code, with linear-in-practice performance.
That's real cost, but not "scary complex."

So the recommendation should change. The rest of this doc explains
what SSA is (from scratch), walks through Cytron's algorithm in
accessible terms, and then gives the revised recommendation with
reasoning you can follow.

## 2. What SSA is, from scratch

**The most important thing to understand up front**: SSA is invisible
to the user of the decompiler. The decompiler's output is clean Lua
source — the same source you'd write by hand. SSA exists only *inside*
the decompiler while it's figuring out what source to emit. It is not
output. It is not written by humans. It is intermediate computational
state, the same way a compiler's register allocation or constant
folding happens internally and never appears in your source code.

If that's clear, the rest of this section is straightforward. If it's
not, treat the rest of this section as the elaboration.

**Static Single Assignment** form is a way of representing a program
*internally* — in what compiler engineers call an **IR**, or
Intermediate Representation. The IR is how the decompiler thinks about
the program while it's working. The input format (LuaJIT bytecode)
and the output format (Lua source) are both fixed; the IR is the
decompiler's choice, and different IRs make different analyses easier
or harder.

In SSA IR, each variable gets assigned exactly once (statically — the
same source-level variable can be reassigned many times at runtime,
but each assignment gets a fresh name in the IR).

A non-SSA representation of a small program:
```
x = 1
print(x)
x = 2
print(x)
```

The same program in SSA IR:
```
x_1 = 1
print(x_1)
x_2 = 2
print(x_2)
```

Each assignment to `x` becomes a fresh name (`x_1`, `x_2`, ...). Now
every use of `x_1` unambiguously refers to the first assignment; every
use of `x_2` unambiguously refers to the second. There's no need to
ask "which assignment of `x` reaches this use?" — the answer is in the
name.

### What about merge points?

Consider control flow that brings different values of a variable
together:
```
if cond:
    x = 1
else:
    x = 2
print(x)  # which x?
```

In SSA IR, each branch assigns a different name:
```
if cond:
    x_1 = 1
else:
    x_2 = 2
print(x_?)  # which one?
```

The answer is a **phi function** (φ). At the merge point, the SSA IR
inserts a notional assignment that selects between the incoming values
based on which branch was taken:
```
if cond:
    x_1 = 1
else:
    x_2 = 2
x_3 = φ(x_1, x_2)  # x_1 if came from then, x_2 if came from else
print(x_3)
```

### What the phi function actually is

The phi function is *not* Lua code. It's not LuaJIT bytecode. It's
not a machine instruction. There's no LuaJIT opcode called `PHI`, no
x86 instruction called `PHI`. The phi function is **only a data
structure inside the decompiler's memory** — a notation compiler
authors agreed on to mean "at this program point, this value is one
of these options depending on which path control took."

Concretely, in the decompiler's source code, a phi might be a Rust
struct like:
```rust
struct Phi {
    target: SsaName,              // e.g., x_3
    inputs: Vec<(BasicBlock, SsaName)>,  // [(then_block, x_1), (else_block, x_2)]
}
```

It exists in the IR data structures. It gets used by analyses that
need to reason about values flowing across merge points. It gets
*eliminated* (transformed back into normal source-level scoping) when
the decompiler emits Lua source. **It is never in the output.**

### The full pipeline, concretely

Take the if/else example above. Here's what actually happens end-to-end:

**Input — LuaJIT bytecode (what's in the file):**
```
ISF R_cond           ; if cond is falsy, jump
JMP else_branch
KSHORT R_x 1         ; R_x = 1
JMP end
else_branch:
KSHORT R_x 2         ; R_x = 2
end:
GGET R_print "print"
MOV R_arg R_x
CALL R_print 2 2
```

Note: the bytecode uses **the same register R_x** in both branches.
There's no SSA in the bytecode; the compiler reused the slot.

**Internal — SSA IR (what the decompiler constructs to reason):**
```
block_then:
  x_1 = 1
  jump merge
block_else:
  x_2 = 2
  jump merge
block_merge:
  x_3 = φ(x_1, x_2)   ; phi function lives only in decompiler memory
  print(x_3)
```

Invisible. The user never sees this. The decompiler uses it to track
facts like "x_1 is read exactly once, by the phi, so we can inline
it," or "x_3 has a unique value across all paths through merge."

**Output — Lua source (what the decompiler produces):**
```lua
if cond then
    x = 1
else
    x = 2
end
print(x)
```

The phi function has vanished. The decompiler recognized that x_1,
x_2, x_3 are all versions of the same source-level variable (debug
info told us its name was "x"), emitted a single source-level `x`
everywhere, and discarded the SSA scaffolding.

### Why is this useful?

The payoff is that **every use has exactly one reaching definition by
construction**. Without SSA, asking "which assignment of `x` reaches
this use?" requires a reaching-definitions analysis — a forward
dataflow pass with fixpoint iteration. With SSA, the answer is the
subscript on the name: `x_3` at this use means the φ-function defined
it, period.

This makes a whole family of analyses and transformations dramatically
simpler:

- **Use-def chains are explicit** (the IR *is* the use-def chains).
- **Liveness analysis is simpler** (no need to track kills — each name
  has one definition).
- **Common subexpression elimination** has cleaner algorithms (two
  expressions with the same operands and operator have the same value
  number trivially).
- **Copy propagation** is trivial (a `MOV` becomes `x_new = y_name`;
  just replace uses of `x_new` with `y_name`).
- **Constant propagation** has cleaner algorithms (sparse conditional
  constant propagation, the canonical algorithm, requires SSA).

### Why doesn't every IR use SSA?

Two reasons, both real:

1. **Construction cost.** Going from arbitrary code to SSA requires
   (a) computing dominators, (b) computing dominance frontiers,
   (c) inserting phi-functions where needed, (d) renaming all
   variables. Cytron et al. 1991 is the paper that made this tractable
   (linear-in-practice algorithms for each step).
2. **Phi-function elimination.** When you want to emit executable code
   (or in our case, source code), you have to lower phi-functions back
   into regular assignments and registers. This is its own pass with
   its own subtleties.

**Readability of the IR is NOT a reason.** SSA IR is uglier than
non-SSA IR, but humans don't read IR — humans read source. The IR's
readability is a concern only for the decompiler's developers
debugging the decompiler itself, and even then the trade is "uglier
but easier to reason about formally."

What IS real: SSA makes the *decompiler's own source code* more
complex. There's more plumbing (the construction algorithm, phi
handling, phi elimination). The trade-off is more code, but each
piece is cleaner and more correct. The alternative (forward-pass
pattern matching without SSA) is less code per piece, but you need
more ad-hoc patches over time — which is exactly the fix-patch-fix
cycle that the current luadejit attempt ran into during Phases 1-3.

Neither construction nor phi-elimination is a deal-breaker. Modern
compilers (LLVM, GCC's GIMPLE, Rust's MIR, Cranelift) all use SSA as
their primary IR. The literature assumes SSA. The cost of building it
has been paid many times over.

## 3. Cytron's construction algorithm in accessible terms

The Cytron paper's central contribution is showing that phi-function
placement can be computed efficiently using a structure called
**dominance frontiers**. Here's the intuition.

### Dominators, briefly

A node `D` in a CFG **dominates** node `N` if every path from the
entry to `N` goes through `D`. So the entry dominates everything; a
loop header dominates everything in the loop body; etc.

Dominators form a tree (the **dominator tree**): the parent of `N` is
its *immediate dominator* — the closest node above it that dominates
it. Computing the dominator tree is well-understood; the
Lengauer-Tarjan algorithm does it in near-linear time, and simpler
iterative algorithms work fine for small CFGs.

### Dominance frontiers — the key idea

The **dominance frontier** of a node `X`, written `DF(X)`, is the set
of nodes where `X`'s dominance "runs out" — nodes that have a
predecessor dominated by `X` but that aren't themselves strictly
dominated by `X`.

Concretely: `Y` is in `DF(X)` if some predecessor of `Y` is dominated
by `X`, but `Y` itself is not strictly dominated by `X`. Intuitively,
these are the points where control can "escape" from `X`'s region of
influence.

Why does this matter for phi placement? Because **phi functions for a
variable V need to be placed exactly at the iterated dominance
frontier of V's assignments**. If `V` is assigned in `X`, then any
node in `DF(X)` is a place where the value of `V` could be ambiguous
(came from `X`'s assignment or from somewhere else), so it needs a
phi.

### The construction in four steps

Step by step, Cytron's algorithm:

1. **Compute dominator tree** of the CFG. (Standard algorithm.)

2. **Compute dominance frontiers** for every node. Cytron's Figure 2
   is a 10-line recursive algorithm: walk the dominator tree
   bottom-up, and for each node propagate its dominance frontier up to
   its parent. (The pseudocode in my reading was about 12 lines.)

3. **Place phi functions.** For each variable `V`, find all the nodes
   that assign to `V`, then compute the iterated dominance frontier
   (DF applied until fixpoint). Each node in the iterated DF needs a
   phi for `V`. This is a worklist algorithm — Cytron's Figure 4 is
   about 20 lines.

4. **Rename variables.** Walk the dominator tree depth-first,
   maintaining a stack of current names per variable. Each assignment
   gets a fresh subscript; each use is rewritten to the top-of-stack
   name. At phi-functions, the operands get renamed according to
   which predecessor the operand came from. Cytron's Figure 5 is
   about 25 lines.

**Total implementation surface**: call it 200-400 lines of real Rust
code for a straightforward implementation. Dominator-tree computation
is another 100-200 lines if you write it yourself (less if you use a
library). This isn't trivial, but it's not "scary complex" either —
it's a couple weeks of careful work for someone who hasn't done it
before, with the literature as a guide.

### Linear in practice

Cytron's worst-case bound is O(E + N²) where E is CFG edges and N is
nodes, because in pathological cases the dominance frontier mapping
can be quadratic. But the paper includes empirical evidence (Section
6) that on real FORTRAN programs, the dominance frontiers are linear
in program size, so the algorithm runs in linear time in practice.
Subsequent decades of compiler implementation have confirmed this —
SSA construction is fast on real code.

## 4. SSA vs traditional dataflow — honest comparison

For our context (decompiler, register-VM bytecode, target = readable
Lua source), the comparison comes down to:

| Concern | Traditional iterative dataflow | SSA |
|---------|-------------------------------|-----|
| Initial cost | Lower (liveness, reaching-defs are simple bitvector passes) | Higher (dominator tree, DF, phi insertion, renaming) |
| Cost per subsequent analysis | Each analysis needs its own fixpoint iteration | Many analyses become trivial (use-def is free) |
| Use-def chains | Computed on demand, O(function size) each time | Built into the IR, free |
| Liveness | Backward dataflow pass | Simpler (sparse) |
| Reaching definitions | Forward dataflow pass | Free (the IR *is* reaching definitions) |
| CSE / value numbering | LVN is easy, GVN is hard | Both clean; GVN was *designed* for SSA |
| Copy propagation | Doable but requires tracking | Trivial (MOV becomes a rename) |
| Code motion | Awkward | Clean |
| Literature coverage | Standard but older | Most modern work assumes SSA |
| Implementation help | Roll your own | Could lean on libraries (Cranelift, etc.) |
| Going back to source | Direct | Need phi-elimination pass |

### Where traditional dataflow still wins

- **Initial simplicity.** If we only needed liveness + reaching
  definitions and never anything more, traditional dataflow is faster
  to implement.
- **No phi-elimination pass needed.** Going from dataflow facts to
  source code is direct. SSA requires lowering phi functions back to
  source-level constructs.

### Where SSA wins

- **Every additional analysis is cheaper.** Once the SSA
  infrastructure exists, every new transformation is easier.
- **Use-def chains are the substrate**, not a query. For a
  decompiler, where the core operation is "given this register read,
  which instruction produced the value?" — having that be free is a
  big deal.
- **GVN (global value numbering)**, which is the strongest form of
  CSE, was literally designed for SSA. Doing it without SSA is
  awkward.
- **Modern literature assumes SSA.** If we want to read papers on
  decompiler-relevant techniques (expression recovery, code motion,
  partial redundancy elimination), they'll assume SSA. We can
  translate, but it's extra work.

### The decompiler-specific concern: phi elimination

For an optimizing compiler going back to machine code, phi-elimination
is a real pass with real subtleties (register allocation has to
handle phis, etc.).

For a decompiler going back to *source*, phi-elimination is different
but not necessarily harder:

- A phi at a merge point corresponds to "this variable has different
  values depending on which branch we came from." In source terms,
  this is just normal control flow — the source-level variable is
  assigned in different branches.
- Most phi functions in decompiler output can be lowered to either
  (a) implicit source-level scoping (the variable is just the same
  name in both branches; we don't introduce subscripts in the
  emitted source), or (b) a synthesized merge assignment if the
  source-level variable needs an explicit value at the merge.

So the phi-elimination concern is real but smaller for a decompiler
than for an optimizing compiler.

## 5. Revised recommendation

**SSA is the right substrate for a clean-slate LuaJIT decompiler.**
The prior recommendation against SSA was under-justified and should
be revised.

Reasoning, plainly:

1. **We're building from scratch.** No sunk cost in traditional
   dataflow infrastructure. The "simplicity" advantage of traditional
   dataflow is mostly a one-time implementation cost; once paid, SSA's
   ongoing advantages dominate.

2. **The core decompiler operation is use-def traversal** — "which
   instruction produced the value in this register at this read?"
   Having use-def chains be the substrate rather than a query is the
   single biggest architectural decision in favor of SSA.

3. **Expression recovery is the central hard problem**, and the
   cleanest expression-recovery algorithms are SSA-based (GVN, sparse
   conditional constant propagation, code motion).

4. **The implementation cost is bounded** — Cytron's algorithm is
   ~200-400 lines, dominator-tree computation is another ~100-200.
   Not trivial, but a small fraction of total decompiler size.

5. **The literature advantage compounds.** Every decompiler-relevant
   paper from the last 25 years assumes SSA. We can translate to
   non-SSA, but it's friction we don't need.

6. **The phi-elimination concern is smaller for decompilers** than
   for optimizing compilers. Going back to source-level scoping is
   natural; we're not constrained by register allocation.

### Where traditional dataflow still has a role

Even with SSA as the substrate, individual passes may use traditional
fixpoint iteration internally (e.g., sparse analyses that need a
prepass). SSA doesn't eliminate dataflow algorithms — it makes most
of them cheaper and a few of them trivial.

### What I'd defer to Phase B

- **Choice of SSA variant** (minimal SSA, pruned SSA, semi-pruned
  SSA). Cytron gives minimal SSA; the variants trade phi count for
  analysis precision. Phase B picks based on the decompiler's needs.
- **Whether to use an existing library** (Cranelift's
  `cranelift-frontend`, etc.) or build our own. Phase B trades off
  learning value vs. implementation speed.
- **How to handle upvalues** (closures cross function boundaries, so
  strict SSA within a function doesn't capture them). Likely a small
  interprocedural extension.

## 6. What this means for Phase A's remaining work

The substrate question was the biggest architectural unknown going
into Step 4. With it resolved (in favor of SSA), the remaining
Phase A work is more focused:

- **Step 5 (knowledge assessment)** should record that the substrate
  question has a preliminary answer (SSA), with the variant and
  library choices deferred to Phase B.
- The dataflow-techniques survey stays as a reference for *what
  analyses* we need; SSA is the substrate on which they run.

## 7. Open questions worth flagging

- **Register-reuse and SSA.** LuaJIT's register reuse (same slot,
  different logical variables) is exactly the kind of thing SSA
  renaming fixes. But how the bytecode's register layout maps onto
  SSA names deserves explicit thought in Phase B.
- **Multres handling under SSA.** Multres values (results passed
  across instructions implicitly) don't fit cleanly into the
  "every value has a name" model. Probably needs a small extension
  (a synthetic "multres" name that's live across the relevant
  instruction range).
- **Debug-info interaction.** LuaJIT's `var_info` gives source-level
  scope. SSA renaming gives IR-level scope. Mapping between them —
  especially when a source-level variable lives in multiple SSA names
  due to branching — is a real concern for emitting readable output.
- **Empirical validation.** At some point in Phase B we should verify
  that real LuaJIT bytecode produces dominance frontiers of expected
  (linear) size. Cytron showed this for FORTRAN; LuaJIT may differ
  in pattern.

## 8. Sources

- **Primary**: Cytron, Ferrante, Rosen, Wegman, Zadeck (1991),
  *Efficiently Computing Static Single Assignment Form*. Local copy:
  `/tmp/opencode/step4_dataflow/cytron_ssa.pdf`. The paper introduces
  dominance frontiers and gives the construction algorithm in
  Figures 2, 4, 5.
- **Comprehensive reference**: Rastello & Bouchez Tichadou, *Static
  Single Assignment Book* — free at https://ssabook.gforge.inria.fr/.
  For deeper treatment of variants (pruned, semi-pruned) and modern
  construction algorithms.
- **Cross-references in this project**: `dataflow-techniques-survey.md`
  (this doc supersedes its §7 recommendation); `decompilation-primer.md`
  §4 (the canonical pipeline — dataflow is Phase 5); `luajit-deep-dive.md`
  §6 (LuaJIT-specific challenges that SSA helps with, particularly
  register reuse).

## 9. Calibration note

I'm revising my prior recommendation in this doc. That's not a sign of
flippancy about recommendations — it's a sign that the prior
recommendation was made without primary-source reading and didn't
survive primary-source reading. The lesson for Phase B: when an
architectural decision rests on a claim about a specific technique's
cost or benefit, that claim should be checked against primary sources
before being committed. Phase A is the right place to do that
checking; Phase B is too late.
