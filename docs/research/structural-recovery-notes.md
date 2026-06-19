# Loop 1 Notes — Structural Recovery (No More Gotos + Marsinator spot-check)

**Audience**: same as prior Phase A docs.

**Purpose**: Phase A+ Loop 1 (per knowledge-assessment.md). Fill the
structural-recovery gap by reading No More Gotos primary, then
spot-checking Marsinator's implementation as the concrete data point.

**Methodology**:
- No More Gotos: read primary, focused on abstract, intro,
  background, the pattern-independent structuring algorithm for
  acyclic and cyclic regions, and the overall approach. Skimmed
  semantics-preserving transformations and post-structuring
  optimizations.
- Marsinator: read `ast/conditionBuilder.h` fully (433 lines — the
  boolean-expression recovery); grepped `ast/ast.cpp` (3,751 lines)
  for structural-recovery patterns. Did not read end-to-end.

**Status**: Loop 1 complete. Loop 2 (SSA phi elimination) follows.

---

## 1. What No More Gotos actually contributes

The paper's full title: *No More Gotos: Decompilation Using Pattern-
Independent Control-Flow Structuring and Semantics-Preserving
Transformations* (Yakdan, Eschweiler, Gerhards-Padilla, Smith; NDSS
2015). Implemented in a decompiler called DREAM.

### The problem they target

State-of-the-art decompilers (Hex-Rays, Phoenix) use **structural
analysis** — a pattern-matching approach over the CFG. They have a
predefined set of region schemas (if-then-else, while loop, etc.) and
iteratively match subgraphs against them. When no match is found,
they fall back to `goto` statements.

Empirically (their numbers): Hex-Rays produces ~1 goto per 32 lines
on a Zeus sample. That's a lot of gotos. Gotos make decompiled code
harder to read and less suitable for automated analysis.

### Their core insight

Two observations:
1. High-level control constructs have a **single entry point** and a
   **single successor point**.
2. The type and nesting of high-level constructs is reflected by the
   **logical conditions** that determine when CFG nodes are reached.

So instead of matching shapes, they reason about *conditions*. They
call this **pattern-independent control-flow structuring**.

### The algorithm, sketchily

For each region (single-entry, single-successor subgraph):

1. **Compute reaching conditions** for every node. The reaching
   condition `cr(h, n)` is the boolean expression that's true at
   the region header `h` if and only if control reaches node `n`.
   Computed via graph slicing + topological traversal:
   `cr(h, n) = ∨(cr(h, v) ∧ τ(v, n))` over predecessors `v`, where
   `τ(v, n)` is the edge tag (the condition labeling the CFG edge
   from `v` to `n`).

2. **Build the initial AST** as a sequence of nodes in topological
   order, each annotated with its reaching condition. This gives
   `if (cr(h, n_1)) {n_1}; if (cr(h, n_2)) {n_2}; ...` — verbose
   but correct.

3. **Refine the AST** via three passes:
   - **Condition-based refinement**: find pairs of nodes with
     complementary conditions (c, ¬c) and group them into if-then-else.
   - **Condition-aware refinement**: find switch constructs by
     looking for nodes whose reaching conditions are comparisons of
     the same variable with constants. (Less relevant for Lua, which
     has no switch — but the same logic applies to cascading elseif.)
   - **Reachability-based refinement**: group nodes into cascading
     if-else chains when they have non-overlapping reachability and
     their OR is `true`.

4. For cyclic regions (loops): find loop nodes via graph slicing,
   refine loop membership, transform to single-entry single-successor
   if needed, structure the loop body as an acyclic region (with
   exits represented as `break` statements), then infer loop type
   (while/do-while/endless) via formal inference rules.

### The hard cases — semantics-preserving transformations

Some CFGs can't be cleanly structured because they have multiple
entries or multiple successors. For these, DREAM uses
**semantics-preserving transformations**:
- Compute the unique condition under which the region is entered at
  each entry node (or exited to each successor).
- Redirect all entries to a single synthetic header that dispatches
  via conditional checks.
- Similar for exits to a single synthetic successor.

This is the part that gets a region with weird shape into a form the
pattern-independent algorithm can handle. It's analogous to the
"untwistable DAG" case in isec2016 — sometimes you have to
restructure the graph to make it amenable to clean source emission.

### Empirical results

Tested on GNU coreutils: zero gotos (vs. Hex-Rays and Phoenix both
producing many). More compact code than Hex-Rays in 72.7% of
functions, than Phoenix in 98.8%. Also tested on three real malware
samples (Cridex, ZeusP2P, SpyEye).

## 2. What this gives us for LuaJIT decompilation

**Direct translation.** The algorithm operates on the CFG, which is
largely language-agnostic. Once we have a CFG built from LuaJIT
bytecode (mechanical, per the LuaJIT deep-dive doc), the algorithm
applies.

The LuaJIT-specific mapping:
- ISxx instructions are the conditional nodes; their JMP targets
  define the true/false edges.
- LOOP marker instructions and back-edge JMPs identify cyclic regions.
- FORI/FORL/ISNEXT/ITERL are dedicated loop opcodes, so for-loops are
  even easier than the general case (the structure is explicit in
  the bytecode).
- The hard cases (while/repeat with break, irregular control flow)
  are exactly what the algorithm is designed to handle.

**What we'd skip from the paper**: the binary-decompiler-specific
concerns (instruction decoding, switch recovery from jump tables,
etc.). The control-flow structuring algorithm itself translates
directly.

**Combining with isec2016**: the two papers cover complementary
corners. isec2016 gives a clean algorithm for boolean-expression
recovery (the `&&`/`||`/`?:` patterns within a single condition).
No More Gotos gives the broader algorithm for structural recovery
(if/else/while/for structure across the CFG). Together they cover
the structural-analysis phase comprehensively.

## 3. Marsinator spot-check: structural recovery

### What they do

Marsinator's structural recovery is **pattern-based**, in the
traditional structural-analysis style that No More Gotos explicitly
outperforms. From the source:

- **`ast/conditionBuilder.h`** (433 lines): A custom boolean-expression
  builder. Collects ISxx nodes with their jump targets, links them
  into a graph by matching labels, then merges nodes with
  matching/complementary targets into AND/OR trees. Uses a
  `TYPE_PREFERENCE` table and careful inversion logic to handle
  negation. This is **neither isec2016 nor No More Gotos** — it's a
  bespoke approach that pattern-matches on chains of conditional
  jumps in the instruction stream.

- **`ast/ast.cpp`** (3,751 lines): Heavy pattern matching on
  bytecode sequences. Repeated checks like
  `block[i-3]->type == AST_STATEMENT_GOTO` to recognize if/else,
  while, repeat, and for-loop shapes. Falls back to
  `AST_STATEMENT_GOTO` + `AST_STATEMENT_LABEL` when no pattern
  matches.

### What this tells us

1. **Marsinator does NOT implement anything like No More Gotos.**
   They do structural recovery the old way (pattern matching on
   bytecode sequences), with gotos as fallback. This is exactly the
   approach No More Gotos shows produces unreadable code on irregular
   control flow.

2. **Marsinator is NOT authoritative for structural recovery.** Their
   approach is one data point — a working implementation that handles
   common cases but produces gotos on hard cases. We should not
   inherit their approach.

3. **The conditionBuilder is interesting in its own right.** It's a
   real attempt at boolean-expression recovery, with carefully
   worked-out inversion logic. But it operates on linear instruction
   sequences, not on a CFG, which limits what it can handle. It's
   closer in spirit to our current luadejit's forward-pass
   approach than to a CFG-based algorithm.

4. **For our clean-slate design, we should use the No More Gotos
   approach, not mimic Marsinator.** This is the most important
   takeaway from the spot-check. The contrast between the two
   validates that No More Gotos is genuinely better, not just
   theoretically cleaner.

### What Marsinator does well (worth noting)

- The `SlotScope` mechanism (`ast/function.h`) for tracking register
  scoping is sophisticated. They explicitly model scopes per slot,
  with merging and upvalue tracking. This is the kind of
  register-scope-aware analysis our current luadejit lacks.
- Their handling of multres (`isMultres`, `multresIndex`,
  `multresArgument`, etc. throughout the AST) is more thorough than
  ours. Worth a closer look in Phase B when we design multres
  handling.
- The overall pipeline structure (bytecode → AST → emission) is
  clean, even if the AST construction is pattern-heavy.

## 4. Implications for Phase B

The structural recovery architecture for our clean-slate decompiler
should:

1. **Build a proper CFG first**, with explicit basic blocks,
   colored edges (true/false), and a dominator tree. We need this
   as the substrate for No More Gotos' algorithm. (Our current
   attempt has CFG-construction code that's *computed but unused*;
   Phase B starts from making it load-bearing.)

2. **Implement reaching-condition computation** as the core primitive.
   Every node in a region gets a boolean expression representing
   when it's reached from the region header. This is what feeds
   everything else.

3. **Implement the three refinement passes** (condition-based,
   condition-aware, reachability-based) for acyclic regions.

4. **Implement cyclic region structuring** with the loop-type
   inference rules. The general approach: any loop can be represented
   as `while (1) { ... break ... }`, then refined to while/do-while/
   endless by analyzing where the breaks are.

5. **For the hard cases (multi-entry/multi-successor regions)**,
   implement the semantics-preserving transformations. We may be
   able to defer this initially — Ghuloum's incremental methodology
   suggests starting with the common cases and adding the
   transformations when we hit a case that needs them.

6. **Combine with isec2016** for the boolean-expression corner.
   The two algorithms compose: No More Gotos recovers the overall
   structure; isec2016 operates within the conditions that emerge.

7. **Do NOT inherit Marsinator's pattern-based approach.** Their
   conditionBuilder is interesting as a concrete attempt but is
   fundamentally less powerful than the algorithm-based approaches.

### What we still don't know (and should treat as Phase B risks)

- **Real LuaJIT vs. the binary code the paper targets.** The paper
  validates on x86 binary from GNU coreutils. LuaJIT bytecode may
  have different distributions of control-flow shapes. Worth
  empirical validation early in Phase B.
- **The semantics-preserving transformations are complex.** The
  paper describes them; I read the description lightly. Implementing
  them may surface surprises.
- **Combining with SSA.** No More Gotos assumes a CFG, not
  specifically an SSA-form CFG. We'll be doing structural recovery
  on top of SSA — need to think about how phi functions interact
  with the structural-recovery process. (Phi functions live at
  merge points; structural recovery identifies the merge points;
  there's interaction.)

## 5. Revised confidence

Before Loop 1: I'd characterized structural recovery as a "weak"
area in the knowledge assessment, with only isec2016 as primary
coverage. After Loop 1: I'd characterize it as **solid**. We have:
- Primary-source depth on boolean-expression recovery (isec2016,
  read in Step 3).
- Primary-source depth on broader structural recovery (No More Gotos,
  read in this loop).
- A concrete contrast point (Marsinator) that validates the
  algorithm-based approaches are genuinely better than pattern-based.

This is enough to architect the structural-recovery side of the
decompiler in Phase B.

## 6. Open questions worth flagging for Phase B

- Should we use No More Gotos' approach directly, or adopt the more
  recent follow-up work (the paper has been extended by the same
  group and others)? A literature check might be worth doing before
  committing to a specific algorithm version.
- How does the algorithm compose with SSA? Specifically, do we
  build SSA first and then structurally recover, or structurally
  recover first and then build SSA per region? Phase B question.
- Empirical: what fraction of real LuaJIT control flow is "well-
  behaved" (handles cleanly with the basic algorithm) vs. needs the
  semantics-preserving transformations? Phase B can probe this on
  the corpus.

## Sources

- Yakdan, Eschweiler, Gerhards-Padilla, Smith (NDSS 2015), *No More
  Gotos: Decompilation Using Pattern-Independent Control-Flow
  Structuring and Semantics-Preserving Transformations*. Local:
  `/tmp/opencode/step5_prep/no_more_gotos.pdf`.
- Marsinator / Aussiemon LuaJIT-decompiler-v2: local clone at
  `research-repos/marsinator/`. Files referenced:
  `ast/conditionBuilder.h`, `ast/building_blocks.h`, `ast/function.h`,
  `ast/ast.cpp`.
- Cross-references: `isec2016-paper-notes.md` (complementary
  algorithm for boolean-expression recovery);
  `dataflow-techniques-survey.md` (Phase 5 of the canonical pipeline);
  `decompilation-primer.md` §4 (canonical pipeline framing).
