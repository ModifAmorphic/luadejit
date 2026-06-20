# Architecture — Clean-Slate LuaJIT Decompiler (v2)

**Audience**: an experienced developer who isn't a Rust or
decompiler specialist, and any future contributor.

**Purpose**: Phase B Step 1. The architectural skeleton for a clean-
slate LuaJIT decompiler. This doc settles the high-level architecture
— pipeline shape, module decomposition, IR choice, error handling,
testing philosophy. Concrete data structures and interfaces come in
Phase B Step 2; the incremental implementation plan comes in Step 3.

**Status**: Phase B Step 1 of 4. Read alongside the Phase A research
artifacts in `docs/research/`, which ground the major decisions.

---

## 1. Design principles

Five principles, all grounded in Phase A findings:

1. **Incremental development (Ghuloum).** Build the decompiler in
   small steps, each producing a *working* decompiler for some
   subset of inputs. Always-working, test-driven. The current
   attempt's fix-patch-fix cycle (per engagement-state) is what
   this principle exists to avoid.

2. **SSA as the IR substrate.** Locked in Phase A Step 4 addendum.
   Use-def chains are the substrate, not a query. Phi elimination
   for source emission is cheap (~50-100 lines per Phase A+ Loop 2).

3. **Analysis-based, not pattern-based.** Use the No More Gotos
   reaching-condition algorithm for structural recovery (per Phase A+
   Loop 1), not the pattern-matching-on-bytecode approach the current
   attempt and Marsinator use. Analysis-based scales better and
   produces cleaner output on irregular control flow.

4. **Make the CFG load-bearing.** The current attempt computes a CFG
   but doesn't use it for codegen. The clean-slate design uses the
   CFG as the primary substrate for both SSA construction and
   structural recovery. This is the architectural debt we're
   explicitly avoiding.

5. **Pipeline architecture with clear pass boundaries.** Each pass
   has a typed input and a typed output. Passes don't reach into
   each other's internals. This is in contrast to the current
   attempt's monolithic codegen, where instruction analysis, scope
   tracking, and emission are interleaved.

## 2. Pipeline

```
   LuaJIT bytecode (file or bytes)
        │
        ▼
┌──────────────────────────────────────────────┐
│ 1. Frontend                                  │
│    Parse bytecode format → Module            │
│    (per docs/luajit-bytecode-format.md)      │
└──────────────────────────────────────────────┘
        │
        ▼  (Module: protos, constants, debug info)
┌──────────────────────────────────────────────┐
│ 2. CFG construction (per proto)              │
│    Flat instruction list → basic blocks      │
│    → CFG with colored edges (true/false)     │
└──────────────────────────────────────────────┘
        │
        ▼  (CFG: blocks + edges + dominator tree)
┌──────────────────────────────────────────────┐
│ 3. SSA construction (per proto)              │
│    Cytron algorithm: φ-placement + renaming  │
│    Build pruned SSA (liveness pre-pass)      │
└──────────────────────────────────────────────┘
        │
        ▼  (SSA IR: SSA values + φ-functions + def-use links)
┌──────────────────────────────────────────────┐
│ 4. Analyses                                  │
│    Liveness (sparse on SSA)                  │
│    Reaching definitions (free under SSA)     │
│    Available expressions (for CSE)           │
└──────────────────────────────────────────────┘
        │
        ▼  (analysis facts)
┌──────────────────────────────────────────────┐
│ 5. Transformations                           │
│    Copy propagation                          │
│    Constant propagation                      │
│    Dead code elimination                     │
│    CSE / local value numbering               │
└──────────────────────────────────────────────┘
        │
        ▼  (transformed SSA IR)
┌──────────────────────────────────────────────┐
│ 6. Structural recovery                       │
│    No More Gotos reaching-condition alg      │
│    CFG + conditions → structured AST         │
│    (if/elseif/else, while, repeat, for, etc.)│
└──────────────────────────────────────────────┘
        │
        ▼  (structured AST with SSA name references)
┌──────────────────────────────────────────────┐
│ 7. Phi elimination + name resolution         │
│    φ-web union-find + representative names   │
│    Debug info → source names where available │
└──────────────────────────────────────────────┘
        │
        ▼  (structured AST with source-level names)
┌──────────────────────────────────────────────┐
│ 8. Emission                                  │
│    AST → Lua source string                   │
│    Operator precedence, method-call sugar,   │
│    table literals, etc.                      │
└──────────────────────────────────────────────┘
        │
        ▼
   Lua source code
```

### Ordering rationale

**Why SSA before structural recovery**: structural recovery operates
on the CFG, which is independent of SSA. But the AST it produces
references values; we want those to be SSA names (so transformations
like copy propagation have already been applied). Phi elimination
then happens naturally during structural recovery — the merge points
structural recovery identifies are exactly where phis live.

**Why transformations before structural recovery**: transformations
clean up the IR (remove dead code, propagate copies, eliminate
redundant computations). The design judgment here is that doing
this before structural recovery lets us produce an already-simplified
AST, rather than producing a verbose AST and cleaning it up at the
AST level. (This is a judgment call, not a verified claim — the
alternative ordering may turn out to have advantages we don't see
yet.)

**Why structural recovery before phi elimination**: structural
recovery tells us *what kind of merge point* each phi lives at (end
of if/else, loop header, etc.). That context informs how to lower
the phi during elimination. (Most cases are uniform — "use the same
source name in both branches" — but having the structure makes the
edge cases handleable.)

## 3. Module decomposition

Eight modules, one per pipeline stage. Each has a typed input and
typed output; no module reaches into another's internals.

```
luadejit/
├── crates/
│   ├── luadejit-core/         # library
│   │   └── src/
│   │       ├── frontend/      # stage 1: bytecode → Module
│   │       ├── cfg/           # stage 2: Module → CFG
│   │       ├── ssa/           # stage 3: CFG → SSA IR
│   │       ├── analysis/      # stage 4: SSA IR → facts
│   │       ├── transform/     # stage 5: SSA IR + facts → SSA IR
│   │       ├── structure/     # stage 6: CFG + SSA → AST
│   │       ├── emit/          # stages 7+8: AST → source
│   │       ├── ir/            # shared IR types (Module, CFG, SSA, AST)
│   │       └── lib.rs
│   └── luadejit-cli/          # binary (CLI wrapper)
└── tests/
    ├── corpus/                # LuaJIT bytecode fixtures
    └── snapshots/             # expected output for incremental tests
```

### Module responsibilities

| Module | Responsibility | Key types |
|--------|----------------|-----------|
| `frontend` | Parse bytecode format. Validate header, decode instructions, parse debug info. | `Module`, `Proto`, `Instruction`, `DebugInfo` |
| `cfg` | Build CFG per proto. Identify basic blocks, colored edges, dominator tree. | `Cfg`, `BasicBlock`, `Edge`, `DominatorTree` |
| `ssa` | Construct SSA per proto. φ-placement via dominance frontiers, renaming. | `SsaFunction`, `SsaValue`, `Phi` |
| `analysis` | Run dataflow analyses. Liveness, reaching defs (trivial on SSA), available expressions. | `LivenessFacts`, `ReachingDefFacts` |
| `transform` | Apply transformations. Copy/constant propagation, DCE, LVN. | (transforms `SsaFunction` in place) |
| `structure` | Structural recovery via No More Gotos. Produces structured AST. | `Ast`, `Statement`, `Expr` |
| `emit` | φ-elimination + name resolution + source emission. | `str` (Lua source) |
| `ir` | Shared IR types. No logic, just data. | (re-exports from above) |

### What's deliberately NOT here

- No `codegen` module that mixes analysis and emission (the current
  attempt's pattern). Analysis is in `analysis`/`transform`;
  emission is in `emit`.
- No `pattern_match` module. Structural recovery is algorithm-based
  (No More Gotos), not pattern-based.
- No `reg_exprs` HashMap equivalent. SSA use-def chains replace it.

## 4. IR choice

**Pruned SSA**, constructed via Cytron's algorithm with liveness-
based pruning. Rationale:

- **Pruned over minimal**: minimal SSA inserts φs everywhere the
  iterated dominance frontier requires, even for dead variables.
  Pruned SSA skips those, leaving fewer φs to reason about and
  fewer to eliminate. The liveness pre-pass is cheap.
- **Cytron over more recent algorithms**: Cytron is the well-
  understood reference. More recent algorithms (BJ-graphs, loop
  nesting forests — see SSA Book Ch 4) are faster but more complex.
  For our use case (decompiling, not compiling), Cytron's linear-
  in-practice performance is fine. We can swap in a faster
  algorithm later if profiling shows it matters.

Open question for Phase B Step 2: do we want **semi-pruned** (filter
out purely-local variables before φ insertion, cheaper than full
pruned)? Probably yes for v1 — it gets most of the benefit of pruned
at lower cost.

## 5. Error handling philosophy

Three error categories, handled differently:

1. **Malformed bytecode** (reader detects impossible format):
   fail fast with a clear error message naming the offset and what
   was expected. No partial output. The reader is the trust
   boundary; once we have a `Module`, we trust it.

2. **Unsupported bytecode patterns** (legal bytecode we don't yet
   handle): emit a clear `-- TODO: <pattern>` comment in the output
   where the unsupported construct would go, plus a structured
   warning to stderr. The decompiler still produces output for the
   rest of the file. This is the always-working principle from
   Ghuloum applied to error cases.

3. **Decompiler bugs** (we panic): catch at the file boundary in
   batch mode, log the panic, continue with the next file. Never
   crash the whole batch on one bad file.

Graceful degradation is a forward-looking design goal for the clean-
slate decompiler. We're not asserting anything specific about the
current attempt's failure modes — its README claims validation
against ~9,600 files without crashes or hangs, and we have no
evidence contradicting that claim. The point is to make graceful
degradation a first-class requirement from the start, regardless of
how the current attempt performs.

## 6. Testing strategy

Two test layers, both rooted in Ghuloum's incremental methodology:

### Layer 1: Incremental capability tests

Each step in the incremental plan (Phase B Step 3) has dedicated
tests. The first test for each capability is the simplest input
that exercises it. Example progression for arithmetic:

```lua
-- Step "add two constants"
-- input
return 1 + 2
-- bytecode
KSHORT 0 1; KSHORT 1 2; ADDVV 0 0 1; RET1 0 2
-- expected output
return 1 + 2
```

Tests are paired `.source.lua` + `.bc` fixtures, plus expected
decompiler output. We compile fixtures with system `luajit -bg` to
get bytecode; expected output is hand-written to reflect what *good*
decompiled source looks like. Compilers generally normalize some
constructs during compilation (constant folding, evaluation order
fixing, etc.), so decompiler output won't necessarily byte-match a
hypothetical original source even when both are correct.

### Layer 2: Corpus regression tests

A separate test suite runs the decompiler against the full Darktide
corpus (9,649 files external to this repo) and checks:

- No crashes / hangs (timeout per file).
- Output is syntactically valid Lua (parseable by `luajit -bl` or
  similar).
- Specific invariants hold (e.g., no `var_N` names when debug info
  is present).

This is the regression net — it catches us when a transformation
improves one case but breaks another. The current attempt has a
similar suite; we keep the concept, redesign the implementation.

### Snapshot testing

For each incremental capability, we maintain a snapshot of the
*expected* decompiler output for a representative input. When we
intentionally change output (e.g., improve copy propagation to
eliminate a temp), we update the snapshot deliberately. When the
snapshot changes unintentionally, that's a regression.

Snapshot tests are the primary correctness signal during development.
They're fast, deterministic, and catch exactly the kind of "this
used to work, what happened?" regression that pattern-matching
fixes tend to introduce.

## 7. v1 scope

What v1 (the first complete clean-slate release) handles, vs. what
gets deferred:

### In scope for v1

- Full bytecode format support (FR1 and FR2 modes).
- All LuaJIT instruction opcodes (front-end never fails on a
  well-formed file).
- All Lua control constructs: if/elseif/else, while, repeat,
  numeric-for, generic-for (all three LuaJIT shapes).
- Arithmetic, comparison, and boolean expressions.
- Local variables, assignments (single and multi).
- Function calls (regular, method, tail-call).
- Multi-return values and varargs.
- Tables (TNEW, TDUP, TSET*).
- Closures with upvalue capture.
- Variable naming from debug info; sensible synthesis otherwise.
- Batch processing, graceful degradation, structured warnings.

### Deferred past v1

- Type recovery (mostly N/A for Lua, per Phase A primer).
- Aggressive code motion for readability (e.g., hoisting invariant
  computations out of loops).
- Cross-procedural analyses beyond upvalue resolution.
- Source-level comment inference (we can't recover comments; v1
  doesn't try heuristics).

The scope is defined by the 17 stages in
`docs/implementation-plan.md`. When all 17 stages are complete, v1.0
is done. The POC (`luadejit-poc`) is a reference for understanding
what a LuaJIT decompiler can do, but we do not target feature parity
with it. The clean-slate implementation stands on its own; its
success criteria are the stage tests and corpus regression, not a
comparison against the POC's output.

Note: cascading `elseif` chains are common in real Lua and are
*not* a separate feature — they're handled by the No More Gotos
condition-aware refinement pass as a natural case of condition
grouping. (Earlier draft of this doc incorrectly suggested a
separate "switch-style recovery" feature was being deferred; that
was wrong — Lua has no switch syntax and LuaJIT doesn't emit jump
tables for elseif chains, so there's nothing to defer.)

## 8. Open architectural questions for Phase B Step 2

These need resolution during Phase B Step 2 (data structures and
interfaces), not before. Listed here so they're visible:

1. **SSA name representation.** Newtyped integer? Symbol table
   reference? Affects every type that touches SSA.

2. **AST vs. CFG-annotated-with-structure.** Should the structural
   recovery pass produce a brand-new AST data structure, or
   annotate the CFG with structure information that emission
   walks? Both are viable; AST is more conventional.

3. **Debug-info ↔ SSA-name mapping data structure.** Needs to handle
   the case where one debug-info variable maps to multiple SSA names
   (branching) and the case where multiple debug-info variables map
   to one SSA name. I asserted this second case was "rare" in an
   earlier draft without corpus analysis to back it up; treat the
   frequency as unknown until we measure it. Phase A+ Loop 2 flagged
   this mapping as a whole.

4. **Multres representation in SSA.** Phase A deep-dive §6 flagged
   this. The standard SSA model assumes every value has a name;
   multres values are implicit across instructions. Needs a small
   extension.

5. **Closure / upvalue representation in the IR.** Per-proto SSA
   doesn't capture cross-function captures. Need a thin
   interprocedural layer for upvalue resolution.

6. **Test corpus v1 selection.** Darktide corpus is large (~9,600
   files); running the full corpus per test is too slow for a tight
   development loop. We need a smaller v1 subset that exercises the
   capabilities in scope. Selection criteria TBD in Phase B Step 4.

7. **Library vs. hand-roll for dominator-tree computation and SSA
   construction.** Cranelift's `cranelift-frontend` provides SSA
   construction. Trade-off: implementation speed vs. learning value
   vs. dependency footprint. My initial lean: hand-roll for v1,
   because we're building understanding as much as a product.

## 9. What this architecture deliberately improves vs. the current attempt

For Phase C context (where we'll compare clean-slate vs. current),
here's the explicit improvement list:

| Concern | Current attempt | Clean-slate (this doc) |
|---------|-----------------|------------------------|
| IR substrate | Forward-pass `reg_exprs` HashMap | SSA with use-def chains |
| CFG usage | Computed but unused | Load-bearing — primary substrate |
| Structural recovery | Pattern matching on bytecode | No More Gotos reaching-condition algorithm |
| Pass separation | Monolithic codegen | 8 modules with typed interfaces |
| Loop recovery | Special-case `loops.rs` | General algorithm handles all loops uniformly |
| Multres handling | Ad-hoc, with known bugs (per Phase A LuaJIT deep-dive) | Explicit dataflow fact (Phase B Step 2 question 4) |
| Failure mode | (per its own README: validated as crash-free on ~9,600 files) | Graceful degradation designed in from the start |

This isn't a criticism of the current attempt — it was built
without the Phase A grounding. The point is to be explicit about
what the clean-slate design does differently, so the Phase C
comparison is grounded. The "failure mode" row is included to make
explicit that we're not claiming superiority there without
evidence; the current attempt's stated crash-freeness is a real
accomplishment and we'll need to match it.

## 10. Next step

Phase B Step 2: data structures and interfaces. Concrete Rust types
for each module's inputs and outputs. Module signatures. The open
questions in §8 get resolved there.

I'd like your review on this skeleton before going deeper. Specifically:

- Does the pipeline ordering make sense? (§2)
- Are the module boundaries right? (§3)
- Is the v1 scope appropriate — too ambitious, too conservative? (§7)
- Are there architectural concerns I haven't surfaced?

After your review, I'd proceed to Phase B Step 2.
