# Dataflow Techniques Survey

**Audience**: same as prior Phase A docs. Read after the primer, the
LuaJIT deep-dive, and the isec2016 paper notes.

**Purpose**: Step 4 of Phase A. Survey the dataflow-analysis techniques
relevant to decompiling LuaJIT bytecode, identify which ones matter
for which decompiler problems, and assess the substrate choice
(traditional iterative dataflow vs. SSA) for a clean-slate design.

**Methodology**: this survey draws on standard compiler-theory
references (Dragon Book Ch. 9, Cooper & Torczon, the SSA Book, Cytron
et al. 1991). For the survey level we need, the textbook material is
authoritative; I'm not reproducing derivations but citing where they
live. Where I'm reasoning about *which technique fits our problem*,
that's synthesis on my part — flagged as such.

**Concrete questions this survey must answer** (set in Step 3):

1. Does a serious LuaJIT decompiler need full SSA, or is traditional
   iterative dataflow enough?
2. What's the minimum subset of analyses that addresses the
   expression-recovery problem class?
3. Are there register-VM-specific considerations the textbook
   treatments (which mostly use stack VMs or RISC binary as examples)
   gloss over?
4. What's the implementation cost of each candidate analysis?

**Status**: Step 4 of Phase A. Clean-slate domain study — does not
reference any existing decompiler implementation.

---

## 1. What dataflow analysis is, and why it dominates decompiler effort

Dataflow analysis is the static computation of facts about program
values as they flow through a program. "What value can this register
hold at this point?" "Is this variable's value still needed later?"
"Does this expression get recomputed with the same operands?" These
are all dataflow questions.

The canonical pipeline (primer §4) puts dataflow analysis at Phase 5,
after control-flow recovery. That ordering matters: you need
structured control flow (basic blocks, CFG) before dataflow analysis
is meaningful, because data flows *through* the CFG.

For decompilation specifically, dataflow is where the expression-
recovery problem lives. When a compiler flattens `f(g(x))` into a
sequence of instructions, recovering the source-level expression
requires understanding which instructions produce values consumed by
which other instructions. That's reaching definitions. When a value
computed once is read multiple times downstream and we want to emit
`local x = ...; use(x); use(x)` instead of `use(...); use(...);`,
that's common subexpression elimination (CSE) or value numbering.
When a register slot is reused for different logical variables over
time, distinguishing them is liveness analysis.

The primer's framing: dataflow is "the core effort" of decompiling
LuaJIT. The LuaJIT deep-dive confirmed it: register reuse, multres
chaining, and expression-shape flattening are all dataflow problems.

## 2. The mathematical framework (briefly)

A dataflow analysis is defined by:

- A **domain** of facts (e.g., "the set of variables that are live").
- A **direction**: forward (facts propagate from entry to exit) or
  backward (facts propagate from exit to entry).
- A **meet operator** (∧): how facts from multiple predecessors (or
  successors) combine. Usually set union ("may" analyses — any path
  could make this true) or set intersection ("must" analyses — all
  paths must make this true).
- A **transfer function** for each instruction: how the instruction
  transforms its input facts into output facts.
- A **fixpoint iteration**: repeatedly apply transfer functions until
  no facts change. The result is the *maximal fixed point* (MFP),
  which is the most precise solution obtainable without knowing
  actual run-time paths.

The fixpoint property is what makes dataflow tractable: you iterate
until stable, and you're guaranteed to terminate (because the fact
domain is finite and the operations are monotone).

Two implications worth highlighting:

- **Conservative approximation.** "May" analyses over-approximate
  (they say "this *could* be true"); "must" analyses under-approximate
  (they say "this *definitely* is true"). For decompilation, we
  usually want *may* analyses (better to over-emit than to lose
  information).
- **Iteration through loops.** Fixpoint iteration handles loops
  naturally — you just keep iterating until the loop body's facts
  stabilize. This is what makes dataflow able to reason about loop
  accumulators and similar patterns that defeat single-pass analysis.

For a thorough treatment, Dragon Book Ch. 9 is canonical; Cooper &
Torczon's *Engineering a Compiler* is more accessible on the
implementation mechanics.

## 3. The foundational analyses

### 3.1 Liveness analysis (backward, may)

**Computes**: at each program point, the set of variables/registers
whose values will be read later (before being overwritten).

**Direction**: backward — live-out at a point depends on what's read
*after* it.

**Transfer function (informal)**: for an instruction `R[x] = R[y] + R[z]`:
- Remove R[x] from the live set (it's being defined — its previous
  value is now dead).
- Add R[y] and R[z] to the live set (they're being read).

**Meet operator**: union of successor live sets.

**Why for decompilation**: this is what tells us "this register
assignment is dead — don't emit it." Without liveness, every
producer instruction emits a `local` declaration, even if nothing
ever reads the result. With liveness, we can suppress dead stores
and avoid polluting the output with `local var_N = ...` for values
that go nowhere.

**Cost**: cheap. Bitvector implementation, O(blocks × variables)
per iteration, converges quickly. The most basic dataflow analysis
and the highest-value-for-effort one.

### 3.2 Reaching definitions (forward, may)

**Computes**: at each program point, the set of definitions
(assignments) that could reach this point without being overwritten.

**Direction**: forward.

**Transfer function (informal)**: for an instruction `R[x] = ...`:
- Remove all prior definitions of R[x] (they're killed).
- Add this definition of R[x] (it's generated).

**Meet operator**: union of predecessor reaching-definitions sets.

**Why for decompilation**: this is what links a register read to its
producing instruction. To recover `f(g(x))` from a sequence of
instructions, we need to know that R[10] at the f-call site is the
result of the g-call two instructions earlier. Reaching definitions
gives us that link.

It's also the basis for use-def chains — explicit edges from each use
of a value to its (possibly multiple) definitions. Use-def chains are
the data structure that expression-recovery passes traverse.

**Cost**: cheap. Same shape as liveness (bitvectors, fixpoint). For
register-based bytecode where each register has a finite definition
count per function, the analysis is well-bounded.

### 3.3 Available expressions (forward, must)

**Computes**: at each program point, the set of expressions that have
definitely been computed already and whose operands haven't been
modified since.

**Direction**: forward.

**Meet operator**: intersection (must-analysis — all paths must have
computed it).

**Why for decompilation**: the basis for CSE. If `R[a] + R[b]` was
computed earlier and neither R[a] nor R[b] has been redefined, the
second computation is redundant — we can refer to the first result.

**Cost**: similar to liveness and reaching definitions. The fact
domain is "set of expressions," which can be larger than "set of
variables" but still finite and bitvector-encodable for typical
function sizes.

## 4. Transformations built on the foundational analyses

The foundational analyses are *information*. To improve decompiler
output, we apply transformations that consume that information.

### 4.1 Dead code elimination (DCE)

Uses liveness. Any instruction whose result is never live can be
removed. For decompilation, "removed" means "not emitted in the
output."

This is the single most impactful transformation for output
readability in a decompiler. Without it, every producer instruction
emits a local, and the output is full of `local var_N = ...` for
values nothing reads.

### 4.2 Copy propagation

Uses reaching definitions. If `R[x] = R[y]` (a copy) is the unique
reaching definition of R[x] at some use, replace the use of R[x]
with R[y]. Effectively undoes the compiler's register-shuffling.

For decompilation: this is what lets us emit `f(y)` instead of
`local x = y; f(x)` when the bytecode has a MOV instruction. Critical
for clean output.

### 4.3 Constant propagation

Similar to copy propagation, but for constants: if `R[x] = 5` reaches
a use, replace the use with `5`. Less commonly needed for LuaJIT
decompilation (the bytecode typically carries constants inline
already via KSHORT/KNUM/KSTR), but useful in some patterns.

### 4.4 Common subexpression elimination (CSE)

Uses available expressions. If an expression `R[a] + R[b]` is
available (computed earlier, operands unchanged), replace the second
computation with a reference to the first.

For decompilation, this is the technique for the "register-read-
multiple-times" pattern. If `require("...")` is computed once into
R[5] and then R[5] is read in two places, CSE recognition lets us
emit `local x = require("..."); use(x); use(x)` instead of
re-expanding `require("...")` at each read.

Implementation choice: when we recognize a CSE, do we hoist to a
local, or just duplicate the source expression? Hoisting produces
cleaner output but requires inventing a variable name (or using the
debug info's name). Duplicating is simpler but loses the CSE
information. Real decompilers hoist; the question is *when*.

### 4.5 Value numbering (local and global)

A more powerful alternative to CSE. Assign each unique expression a
"value number" such that two expressions get the same number iff
they're guaranteed to compute the same value. Includes algebraic
identities (`a + b` == `b + a` for commutative ops, `a + 0` == `a`,
etc.).

- **Local value numbering (LVN)**: within a single basic block.
  Simple hash table; very cheap.
- **Global value numbering (GVN)**: across the whole function.
  Harder; typically requires SSA form to do cleanly.

For decompilation, LVN handles most cases. GVN's extra power (catching
equivalent expressions across basic blocks) is useful but adds
significant complexity.

## 5. SSA form: substrate choice

### 5.1 What SSA is

Static Single Assignment form: an IR where each variable is assigned
exactly once (statically — dynamically the same variable can be
assigned many times in a loop). Phi functions at merge points
reconcile values from different paths.

A phi function `R[x_3] = φ(R[x_1], R[x_2])` at a join node says "R[x_3]
takes the value of R[x_1] if control came from predecessor 1, R[x_2]
if control came from predecessor 2."

### 5.2 Why SSA exists

SSA was invented (Cytron et al. 1991) to make dataflow analyses
simpler and optimizations more effective. With SSA:

- Reaching definitions becomes trivial — each use has exactly one
  reaching definition by construction.
- Liveness is simpler — no need to track kills via definitions
  because there's only one definition per name.
- GVN, code motion, and many other optimizations have clean
  algorithms on SSA.

Modern compilers (LLVM, GCC's GIMPLE, the Rust compiler's MIR, the
Julia compiler) all use SSA as their primary IR.

### 5.3 The construction algorithm (Cytron et al.)

The classic algorithm has three phases:

1. **Compute dominators** of the CFG. (A node d dominates n if every
   path from entry to n goes through d.) This is the Lengauer-Tarjan
   algorithm in efficient implementations, but a simpler iterative
   algorithm works for small CFGs.
2. **Compute dominance frontiers**. The dominance frontier of n is
   the set of nodes where n's dominance stops — i.e., nodes that are
   just-finished being dominated by n, but have a predecessor that
   isn't dominated by n. Insert phi functions at dominance frontiers.
3. **Rename variables**. Walk the dominator tree, giving each
   assignment a fresh subscripted name, and rewriting uses to match.

The Cytron paper's main contribution is the dominance-frontier
characterization, which makes phi placement efficient and principled
(was previously ad-hoc).

### 5.4 Trade-offs

**Pros of SSA**:
- Clean algorithms for many optimizations.
- Modern compiler literature assumes SSA — leveraging it brings the
  literature to bear directly.
- Strong tooling (e.g., the Cranelift and LLVM IR-building libraries).

**Cons of SSA**:
- Construction is non-trivial (the three-phase algorithm above).
- Phi functions complicate the IR — code generation has to handle
  them, debug-info mapping gets harder.
- For *analyses* that don't need the full power of SSA, the
  construction cost may not pay off.
- Phi-function elimination (when going back to executable code) is
  itself a non-trivial pass.

**The SSA Book** (Rastello & Bouchez Tichadou, free at
https://ssabook.gforge.inria.fr/) is the comprehensive reference if
we decide SSA is the right substrate.

## 6. Register-VM-specific considerations

Most textbook dataflow treatments use stack VMs (JVM, CPython) or
RISC binary as examples. LuaJIT is register-based. The analyses
translate cleanly, but a few specifics differ:

### 6.1 Registers vs. variables

In a stack VM, the "variables" are stack slots, and dataflow has to
model the stack's behavior. In a register VM, registers are named
locations, so dataflow is direct: "what value is in R[5] at this
point?" is a concrete question about a concrete location.

This makes the *analyses* easier — no stack simulation. But it makes
the *register-reuse problem* more visible: the same slot holds
different logical variables over time, and without liveness we
conflate them.

### 6.2 Multi-value results and multres

LuaJIT's CALL/CALLM/CALLT/CALLMT/VARG/RETM/TSETM all have multres
variants where the count is "from previous" or "to top." These chain
across instructions: a CALL producing multres followed by a TSETM
consuming multres is a single source-level operation spread over two
instructions.

Dataflow-wise, this means the analysis needs to track "multres state"
as a fact: at each point, is there a pending multres result, and if
so, how many values? This isn't hard per se but is
LuaJIT-specific — the standard analyses don't handle it out of the
box.

### 6.3 Debug-info-driven scope

When debug info is present, LuaJIT's `var_info` records give us
source-level scope boundaries (start-pc, end-pc for each named
variable). This is a gift: we can use these directly as ground truth
for variable scopes, *bypassing* the need to infer scopes from
dataflow.

But there's a subtlety: `var_info` scopes are about *source-level*
variables, not *register-level* lifetimes. A single source-level
variable may live in multiple registers over its lifetime (the
compiler shuffles it around), and a single register may hold multiple
source-level variables (the compiler reuses slots). Using debug info
well requires understanding this mapping.

### 6.4 Upvalues

Closures introduce cross-function dataflow: a local captured by a
nested function is read inside that function as an upvalue, not as a
register. The dataflow analysis for the parent function tracks the
local; the child function's analysis tracks the upvalue; linking them
is a separate "interprocedural-lite" step.

For a decompiler, this matters when you want to render a closure and
need to name its captured variables correctly. The bytecode's
upvalue descriptors give the link; the analysis uses them.

## 7. Substrate recommendation — superseded

**This section has been superseded by `ssa-substrate-assessment.md`
in this directory.** That doc was written after primary-source
reading of Cytron et al. 1991 and revises the recommendation.

The short version: **SSA is the right substrate**, not traditional
iterative dataflow. The reasoning I gave here ("decompilers don't
optimize," "SSA's payoff is in optimization clarity," "SSA
construction is non-trivial") was under-justified and doesn't
survive a careful read of the primary source.

The text below is kept for context but should be read alongside
the revised assessment.

---

**Original (pre-revision) text follows.**

For a LuaJIT decompiler specifically:

**Recommendation: traditional iterative dataflow + value numbering,
NOT full SSA.**

Reasoning:

- A decompiler's job is *understanding* code, not *optimizing* it.
  The dataflow analyses we need (liveness, reaching definitions, some
  CSE/value numbering for expression recovery) all have well-known
  non-SSA implementations.
- SSA's main payoff is making optimizations cleaner. Decompilers
  don't optimize — they emit source. The payoff is reduced for us.
- SSA construction is non-trivial and adds complexity to every other
  pass. The avoided complexity is worth more than the optimizations
  we'd enable.
- The textbook references (Dragon Book Ch. 9, Cooper & Torczon)
  cover traditional dataflow thoroughly; we'd be working from
  well-trodden material.

**Where SSA could still be worth it**:
- If we want GVN (full algebraic-identity-aware CSE across blocks),
  SSA is the natural substrate. LVN may be sufficient for our needs;
  if not, GVN-with-SSA becomes more attractive.
- If we want to do code motion (moving computations across blocks
  for readability), SSA makes it much cleaner.
- If we decide to lean on an existing IR library (Cranelift's
  `cranelift-frontend`, or similar) rather than building our own,
  SSA comes with the library.

This is a Phase B decision, not a Phase A conclusion. Phase A's
knowledge assessment (Step 5) should record this as a question to
resolve during Phase B's architectural work.

## 8. Concrete illustrations

These are clean-slate illustrations of the technique-problem mapping,
not references to any specific decompiler. (The "our bugs" framing
lives in Phase C.)

| Source pattern | Bytecode shape | Dataflow technique |
|----------------|----------------|--------------------|
| `local x = require("foo")` then `x.bar` then `x.baz` | CALL into R[x], then two TGETS reading R[x] | CSE/value numbering: R[x] is computed once and read twice; hoist requires emitting `local x = ...` |
| `f(g(x))` | CALL g → R[t]; CALL f reading R[t] | Reaching definitions + copy propagation: R[t] at f-call is the g-call result, inline it |
| `local function() local a = 1; a = a + 1; return a end` | KSHORT R[a] 1; ADDVN R[a] R[a] 1; RET1 R[a] | Liveness: R[a] is live throughout; no dead-store elimination. Reaching definitions: inside the loop body, R[a]'s definition is the ADDVN (the KSHORT is killed). |
| `local t = {}; t.x = 1; t.y = 2` | TNEW R[t]; KSHORT R[tmp] 1; TSETS R[tmp] R[t] "x"; similar for y | Mostly mechanical; dataflow confirms R[t] is the same table across both stores. |
| `for i = 1, 10 do sum = sum + i end` | FORI/FORL with ADD inside | Liveness with fixpoint iteration: `sum` is live across the loop back-edge; without iteration, the analysis would miss the self-reference. |

The pattern: dataflow is what lets the decompiler go from "a sequence
of register operations" to "a coherent source-level expression or
statement."

## 9. Open questions for Step 5 (knowledge assessment)

The survey leaves several questions for the decision-point artifact
to address:

- **Is LVN sufficient for our expression-recovery needs, or do we
  need GVN?** Answer requires studying what kinds of duplicate
  computations real LuaJIT code actually has. An empirical probe on
  a corpus (independent of any specific decompiler) could answer
  this in Phase B.
- **Multres tracking: ad-hoc or as a dataflow fact?** The cleanest
  treatment is a small extension to standard reaching-definitions
  (track "multres in flight" as a fact), but a more ad-hoc
  treatment (specific pattern matching on CALL+TSETM sequences) is
  also viable. Implementation choice for Phase B.
- **Debug-info scope as ground truth or as a hint?** The mapping
  between `var_info` scopes and register lifetimes isn't 1:1; we
  need a clear policy. Probably: use debug info when available,
  fall back to dataflow when stripped.
- **Interprocedural analysis depth.** Most decompiler-relevant
  analyses are intraprocedural (within one function). Upvalue
  resolution is the main exception. How deep do we go? My initial
  read: only as far as upvalue resolution requires; full
  interprocedural analysis is overkill.

## 10. What's next

Step 5 (knowledge assessment / decision point). After Steps 1-4, do
we have enough to architect a LuaJIT decompiler from scratch? The
assessment writes that down explicitly and either certifies readiness
or identifies specific gaps.

## Sources

- Aho, Lam, Sethi, Ullman, *Compilers: Principles, Techniques, and
  Tools* (the Dragon Book), Chapter 9 — the canonical dataflow
  reference.
- Cooper & Torczon, *Engineering a Compiler* — more accessible on
  implementation mechanics; Chapters 8-10 cover dataflow.
- Cytron, Ferrante, Rosen, Wegman, Zadeck (1991), *Efficiently
  Computing Static Single Assignment Form* — the foundational SSA
  paper. (Reference; my survey-level treatment relies on the
  standard textbook summary of its contribution.)
- Rastello & Bouchez Tichadou, *Static Single Assignment Book* —
  free at https://ssabook.gforge.inria.fr/. The comprehensive SSA
  reference; consult if Phase B decides SSA is the right substrate.
- Cross-references in this project: `decompilation-primer.md` §4
  (the canonical pipeline), `luajit-deep-dive.md` §6 (LuaJIT-specific
  challenges), `isec2016-paper-notes.md` (what's *not* a dataflow
  problem).
