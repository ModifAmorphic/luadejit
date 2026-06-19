# Loop 2 Notes — SSA Phi Elimination (SSA Book + Marsinator spot-check)

**Audience**: same as prior Phase A docs.

**Purpose**: Phase A+ Loop 2 (per knowledge-assessment.md). Fill the
phi-elimination gap by reading SSA Book chapters on destruction, then
spot-checking Marsinator's source-emission approach for contrast.

**Methodology**:
- SSA Book: read Chapter 3 §3.2 (Destruction) and §3.3 (SSA property
  transformations) in detail. Surveyed TOC for Chapters 5
  (Reconstruction), 6 (Functional Representations, including
  destruction), 15 (Code Generation), and 17 (SSA Destruction for
  Machine Code). Did not read Ch 17 line-by-line; surveyed its scope.
- Marsinator: read `lua/lua.cpp` emission entry points and the
  `Ast::Variable` / `SlotScope` interaction. The `SlotScope` mechanism
  in `ast/function.h` is the relevant contrast.

**Status**: Loop 2 complete. With Loops 1 and 2 done, Phase A+ is
complete and the project is ready for Phase B.

---

## 1. The headline finding

**SSA destruction for source emission is dramatically simpler than
the SSA Book's primary treatment suggests.** The book's complexity
(almost all of Chapter 17, much of Chapter 3 §3.2) is about
preserving register-allocation quality for machine-code emission.
For a decompiler that emits source, none of those concerns apply.

The decompiler version of SSA destruction is essentially:
1. Find φ-webs (union-find on φ operands).
2. For each φ-web, pick a representative source-level name.
3. Replace all SSA names in the web with the representative name.
4. Drop the φ-functions.

That's it. No parallel copies, no critical-edge splitting, no
sequentialization, no coalescing. The complexity simply doesn't
apply to our use case.

This **reinforces the SSA substrate decision** (locked in Step 4
addendum). The main concern flagged at the time — phi elimination
is a real cost — turns out to be much smaller for decompilers than
for optimizing compilers.

## 2. What the SSA Book covers

### 2.1 The basic destruction algorithm (Ch 3 §3.2)

**Conventional SSA form**: φ-webs are free from interference (the
live-ranges of variables in the same web don't overlap). Freshly
constructed SSA is conventional. For conventional SSA, destruction is
trivial:

> One simply has to rename all φ-related variables (source and
> destination operands of the same φ-function) into a unique
> representative variable. Then, each φ-function should have
> syntactically identical names for all its operands, and thus can be
> removed to coalesce the related live-ranges.

The φ-web discovery is union-find (Algorithm 3.4 in the book, ~5
lines of pseudocode). The destruction itself is rename-and-drop.

**Transformed SSA**: after optimizations like copy propagation, SSA
may become non-conventional (φ-web live-ranges interfere). Then
destruction requires more work — critical-edge splitting, parallel
copies, sequentialization. This is where the complexity lives.

### 2.2 The complex case (parallel copies, critical edges, etc.)

For non-conventional SSA, the destruction algorithm (Algorithm 3.5)
does:
1. Split critical edges (edges from multi-successor to multi-
   predecessor blocks) by inserting fresh basic blocks.
2. Replace each φ-function with parallel copies at the end of
   predecessor blocks.
3. Sequentialize the parallel copies (Algorithm 3.6) — non-trivial
   because cycles in the copy graph require temporary variables.

The book notes these complexities are *for machine code emission*:
the parallel copies have to be sequentialized in a way that
preserves register-allocation quality, doesn't introduce bad
interferences, etc. Chapter 17 goes deep on this.

**For source emission, none of this matters.** We don't allocate
registers; we emit names. Two assignments that "interfere" in
register-allocation terms are perfectly fine in source — they're
just two `local` declarations or two assignments to the same name.

### 2.3 Pruned SSA and dead φ-elimination (Ch 3 §3.3)

**Pruned SSA**: don't insert φ-functions for variables not live at
the join point. Reduces φ count significantly with no loss of
analysis power. Construction: insert φ-functions only where variable
is live (requires liveness info as a pre-pass, or do dead-φ
elimination after construction).

**Dead φ-elimination** (Algorithm 3.7): mark φ-functions whose
results are used by non-φ instructions; propagate usefulness
backward through φ-chains; delete φ-functions never marked useful.

**Redundant φ-elimination** (Algorithm 3.8): identity φ-functions
(`a_i = φ(a_j, ..., a_j)` where all sources are the same) can be
removed via rewriting rules.

These are all cheap, well-understood optimizations. For a
decompiler, pruned SSA is the right starting point — fewer φs to
reason about, fewer to eliminate.

### 2.4 What I didn't read closely

- **Chapter 17 (SSA Destruction for Machine Code)** — the dedicated
  chapter. Surveyed its scope; it's about register allocation
  interactions, parallel-copy sequentialization with temporaries,
  and aggressive coalescing. Not relevant to source emission.
- **Chapter 6 §6.2.4 (Functional Destruction of SSA)** — destruction
  via λ-dropping in functional representations. Different paradigm;
  not relevant unless we choose a functional IR substrate (we won't
  for a LuaJIT decompiler).
- **Chapter 15 §15.3 (SSA form destruction algorithms)** — code-
  generation-flavored destruction. Again, machine-code focused.

## 3. The decompiler-specific destruction algorithm

Based on the SSA Book material, here's what SSA destruction looks
like for a LuaJIT decompiler specifically:

### Step 1: Find φ-webs

Use union-find on φ operands (Algorithm 3.4):
```
for each variable v: phiweb(v) ← {v}
for each φ-function a_dest = φ(a_1, ..., a_n):
    for each source operand a_i:
        union(phiweb(a_dest), phiweb(a_i))
```

This is ~10 lines of Rust. Linear in the number of φ-operands.

### Step 2: Pick a representative name per web

For each φ-web, choose a source-level name to use for all variables
in the web. Candidate sources, in priority order:

1. **Debug info** — if any variable in the web has a debug-info
   name (from LuaJIT's `var_info`), prefer it. If multiple distinct
   names appear (rare but possible if the compiler merged scopes),
   pick the most frequently occurring or the one with the longest
   scope.
2. **Synthesized name** — if no debug info, generate one. Pattern:
   `var_N` where N is a counter, or (better) something derived from
   the value's content (e.g., a value computed by `require("foo")`
   could be named `foo_module` or similar).

This step is where decompiler-specific heuristics live. The SSA
Book doesn't cover it (machine-code decompilers don't have the
luxury of debug info usually).

### Step 3: Rename and drop

For each variable in each φ-web, replace its name with the
representative name. Drop the φ-functions. Done.

No critical edges to split (source-level control flow already
expresses the structure). No parallel copies (source-level
assignment is already sequential). No sequentialization concerns
(no registers to worry about).

### Total implementation surface

Maybe 50-100 lines of Rust for the basic algorithm. Plus the
name-picking heuristics, which is open-ended but doesn't need to be
perfect for v1.

## 4. Marsinator spot-check: source emission without SSA

Marsinator doesn't use SSA. Their emission (`lua/lua.cpp`)
references variables via `Ast::Variable` of type `AST_VARIABLE_SLOT`,
which carries a `SlotScope**` — a pointer-to-pointer to a scope
record. The scope record has a `name` field. To emit a variable,
they dereference: `(*variable.slotScope)->name`.

The `SlotScope` mechanism (in `ast/function.h`) tracks register
scopes directly:
- Each scope has a `scopeBegin` and `scopeEnd` (positions in the
  bytecode stream).
- Scopes can be merged when the same source-level variable spans
  multiple slots over its lifetime.
- The mechanism handles the merge-point naming problem by
  essentially pre-computing "what name should this slot have at this
  point" before emission.

### What this tells us

1. **Marsinator avoids SSA's construction cost** by not using SSA.
   They solve the merge-point naming problem a different way:
   direct scope tracking per slot, with scope merging.
2. **They also avoid SSA's destruction cost** — there's nothing to
   destroy.
3. **But they lose SSA's analytical power.** Without use-def chains
   baked into the IR, every analysis (CSE, copy propagation,
   liveness) has to be re-derived from the slot-scope information.
   Their `SlotScope` is sophisticated but it's not a substitute for
   SSA when you want to do real dataflow analysis.

4. **The SlotScope mechanism is worth studying for our project** as
   a concrete example of how to track register-to-name mappings. Even
   with SSA, we need something similar: a way to map SSA names back
   to source-level names during emission. Marsinator's heuristics
   for scope merging may inform our name-picking step (Loop 2 §3
   above).

### The trade-off, made concrete

| Approach | Construction | Analyses | Destruction | Implementation risk |
|----------|--------------|----------|-------------|---------------------|
| SSA (our choice) | Cytron (~200-400 lines) | Trivial (use-def is free) | ~50-100 lines + name picking | Construction algorithm correctness |
| SlotScope (Marsinator's) | Direct during forward pass | Each analysis re-derives chains | None (nothing to destroy) | Slot-merging heuristics, analysis correctness |

Marsinator chose lower upfront cost; we're choosing higher upfront
cost for cleaner ongoing analyses. The trade-off is real; the SSA
substrate decision (Step 4 addendum) is the bet that the ongoing
benefits outweigh the upfront cost.

## 5. Implications for Phase B

For the SSA destruction side of the decompiler:

1. **Plan for simple destruction** — φ-web union-find + representative
   name + rename + drop. The machine-code-focused complexities in
   the SSA Book are not our problem.

2. **Invest in the name-picking step** — this is where decompiler-
   specific heuristics pay off. Inputs:
   - LuaJIT `var_info` debug info (preferred source of names).
   - SlotScope-style scope tracking (for cases where debug info is
     incomplete or where one source variable maps to multiple SSA
     names legitimately).
   - Content-based heuristics (e.g., `require("foo")` → `foo`).

3. **Use pruned SSA** — fewer φs to reason about, fewer to destroy.
   Requires a liveness pre-pass but that's a cheap analysis.

4. **Use identity φ-elimination** (Algorithm 3.8) as cleanup before
   destruction. Trivially removes redundant φs that copy
   propagation or other transformations may have left behind.

5. **Don't bother with Chapter 17's complexity** unless we hit a
   specific case that needs it. We probably won't.

## 6. Revised confidence

Before Loop 2: phi elimination was flagged as a "weak" area in the
knowledge assessment. After Loop 2: I'd characterize it as **solid,
and simpler than expected**. The decompiler use case lets us skip
most of the textbook complexity.

Combined with Loop 1 (structural recovery, upgraded from weak to
solid), both Phase A+ target areas are now adequately covered.

## 7. Open questions for Phase B

- **Variable naming strategy details**: how exactly do we pick a
  representative name when a φ-web contains variables with multiple
  distinct debug-info names? The SlotScope merging heuristics from
  Marsinator are a starting point but not a complete answer.
- **Mapping `var_info` scope records to φ-webs**: LuaJIT debug info
  records source-level scopes (start-pc, end-pc per variable). How
  do these compose with SSA φ-webs? Sometimes they align cleanly;
  sometimes they don't. Phase B question.
- **Handling lost debug info**: when input is stripped, all our
  name-picking falls back to synthesis. Worth designing the
  synthesis to produce readable names (`require("foo")` → `foo`)
  rather than `var_N`.
- **phi-functions in decompiler output**: should any φ-functions
  survive into the emitted source as explicit constructs (e.g., as
  `x = cond and a or b` for ternary-like patterns)? Probably some
  should — but this is an emission-strategy question for Phase B.

## Sources

- Rastello & Bouchez Tichadou (eds.), *Static Single Assignment Book*.
  Local: `/tmp/opencode/step5_prep/ssabook.pdf`. Chapters consulted:
  Ch 3 §3.2 (Destruction), Ch 3 §3.3 (SSA property transformations).
  TOC consulted for Ch 5, 6, 15, 17.
- Marsinator / Aussiemon LuaJIT-decompiler-v2: local clone at
  `research-repos/marsinator/`. Files referenced: `lua/lua.cpp`,
  `lua/lua.h`, `ast/function.h`, `ast/building_blocks.h`.
- Cross-references: `ssa-substrate-assessment.md` (substrate
  decision this loop reinforces); `dataflow-techniques-survey.md`
  (broader dataflow context); `structural-recovery-notes.md`
  (Loop 1, complementary structural-recovery coverage).
