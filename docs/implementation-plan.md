# Architecture v2 — Incremental Implementation Plan

**Audience**: whoever does the implementation (could be the user
learning Rust, could be a subagent, could be a future contributor).

**Purpose**: Phase B Step 3. Concrete implementation order, organized
per Ghuloum's incremental methodology. Each stage produces a *working
decompiler* for some subset of inputs. We don't build ahead; we build
exactly what each stage needs.

**Status**: Phase B Step 3 of 4. Builds on `architecture-v2.md`
(Step 1) and `architecture-v2-data-structures.md` (Step 2). Phase B
Step 4 will be the testing strategy.

**Methodology source**: Ghuloum 2006 (per `.agents/references.md`).
Local copy: `/tmp/opencode/step4_dataflow/11-ghuloum.pdf`.

---

## 1. How to read this plan

The plan is a sequence of **stages**. Each stage:

- Has a **target capability** — what kind of input it handles.
- Has a **forcing function** — what architectural piece it forces
  into existence.
- Has a **test** — a specific Lua input → expected output that
  confirms the stage works.
- Produces a **working decompiler** — at the end of the stage, the
  decompiler handles the target input correctly and doesn't regress
  on earlier stages.

**The plan is a starting point, not a contract.** Stages will
surface surprises. We adjust as we go. The principle is "always
working, always growing," not "follow these 17 steps in order."

**Sizing guidance**: each stage is meant to be roughly one focused
work session. Some will be shorter (an hour); some will be longer
(a few sessions). If a stage feels too big, split it. If too small,
merge with the next.

## 2. Pre-implementation: project skeleton (Stage 0)

Before any capability work: stand up the project structure.

- Cargo workspace with two crates: `luadejit-core` (library) and
  `luadejit-cli` (binary), per `architecture-v2.md` §3.
- Module directories created but mostly empty: `frontend/`, `cfg/`,
  `ssa/`, `analysis/`, `transform/`, `structure/`, `emit/`, `ir/`.
- `lib.rs` exposes a stub `decompile(bytes: &[u8]) -> Result<String>`
  that returns `Err(NotImplemented)`.
- CLI binary calls `decompile` on an input file.
- CI: `cargo build` and `cargo test` succeed.

**Test for Stage 0**: `cargo test` passes; `luadejit some_file.bc`
runs and exits with a clear "not implemented" error.

## 3. The capability stages

Stages 1–17 below. Each is summarized with target, forcing function,
and test case. Detailed implementation notes follow in §4.

### Stage 1 — `return` (the trivial case)

**Target**: decompile a chunk that's just `return` (RET0).

**Forces**: full pipeline end-to-end at its simplest. Frontend parses
the bytecode. CFG is a single block. SSA construction is trivial
(no real values). AST is empty or just an implicit return. Emitter
produces empty source or just nothing.

**Test**:
- Input Lua source: empty file or `return`
- Compiled bytecode
- Expected output: empty string or just a trailing newline

**Notes**: this is the Ghuloum "compile an integer constant" stage
— the simplest thing that exercises the whole pipeline. Don't skip
it; it surfaces plumbing issues (does the CLI handle the I/O? does
the pipeline orchestrator work? does the emitter terminate?).

### Stage 2 — `return <const>`

**Target**: `return 5`, `return "foo"`, `return true`, `return nil`.

**Forces**: constant load instructions (KSHORT, KNUM, KSTR, KPRI).
SSA has one def per instruction. AST has a Return with an Expr.
Emitter handles each constant type.

**Test**:
```lua
-- input
return 5
-- expected output
return 5
```

And similarly for string, bool, nil.

### Stage 3 — `local x = <const>; return x`

**Target**: simple local variable declaration and use.

**Forces**: SSA renaming within a block (each assignment gets a new
SSA name). Debug-info-driven name lookup (the variable's source name
"x" comes from var_info). Emitter produces `local x = ...` for
freshly declared locals.

**Test**:
```lua
-- input
local x = 5
return x
-- expected output
local x = 5
return x
```

### Stage 4 — Arithmetic

**Target**: `return 1 + 2`, `return a - b`, etc.

**Forces**: binary op instructions (ADDVV and the *VN/*NV variants).
AST has BinOp nodes. Emitter handles operator precedence (initial
case — no parenthesization needed yet because we have single binops).

**Test**:
```lua
-- input
return 1 + 2
-- expected output
return 1 + 2
```

### Stage 5 — Multi-statement sequences

**Target**: `local x = 1; local y = 2; return x + y`.

**Forces**: multiple SSA defs and uses within a single block. AST
has a sequence of statements. Emitter handles newlines / statement
separation.

**Test**:
```lua
-- input
local x = 1
local y = 2
return x + y
-- expected output
local x = 1
local y = 2
return x + y
```

### Stage 6 — Local reassignment (SSA renaming under one block)

**Target**: `local x = 1; x = 2; return x`.

**Forces**: SSA renaming where the same source variable maps to
multiple SSA names. No phi functions yet (still single block). Phi
elimination has to collapse x_1 and x_2 back to source name "x"
during emission.

**Test**:
```lua
-- input
local x = 1
x = 2
return x
-- expected output
local x = 1
x = 2
return x
```

**Notes**: this stage validates the debug-info ↔ SSA mapping (Q3
from Step 2). The forward map `ssa_to_debug` records x_1 →
DebugVarId(0), x_2 → DebugVarId(0). The reverse map
`debug_to_ssa` records DebugVarId(0) → [x_1, x_2]. Emitter picks
the source name "x" for all of them.

### Stage 7 — `if/then/end` (first real control flow)

**Target**: `if x then return 1 end`.

**Forces**: **CFG construction** (first time). Multiple basic blocks.
Conditional branches. **SSA with phi functions** (first time) — at
the merge point after the if. **Structural recovery** (first time) —
the No More Gotos algorithm's simplest acyclic case. **Phi
elimination** at the merge point.

This is the biggest stage so far. Consider splitting if needed:
7a = build CFG, 7b = extend SSA with phi functions, 7c = structural
recovery for if/then, 7d = phi elimination at merge points.

**Test**:
```lua
-- input
if x then
    return 1
end
-- expected output
if x then
    return 1
end
```

### Stage 8 — `if/then/else`

**Target**: `if x then return 1 else return 2 end`.

**Forces**: structural recovery for if/then/else (the condition-
based refinement produces the else branch). Phi elimination where
both branches define the same variable.

**Test**:
```lua
-- input
if x then
    return 1
else
    return 2
end
```

### Stage 9 — Compound conditions (`and`, `or`)

**Target**: `if a and b then ... end`, `if a or b then ... end`,
`if not a then ... end`.

**Forces**: ISxx + JMP chains. isec2016-style boolean expression
recovery. **First real exercise of the condition-builder logic**.
Short-circuit patterns (ISTC/ISFC vs ISF+JMP — both shapes per Phase
A LuaJIT deep-dive §4.2).

**Test**:
```lua
-- input
if a and b then
    return 1
end
-- expected output
if a and b then
    return 1
end
```

### Stage 10 — Numeric-for loops

**Target**: `for i = 1, 10 do print(i) end`.

**Forces**: FORI/FORL handling. **First loop structural recovery**.
Loop-variable scope (the `__for*` markers in debug info, per Phase A
LuaJIT deep-dive). Loop-body extraction.

**Test**:
```lua
-- input
for i = 1, 10 do
    print(i)
end
-- expected output
for i = 1, 10 do
    print(i)
end
```

### Stage 11 — While and repeat loops

**Target**: `while x do ... end`, `repeat ... until x`.

**Forces**: **real exercise of No More Gotos' cyclic region
structuring**. LOOP+JMP without dedicated loop opcodes. Loop-type
inference (while vs do-while vs endless, per the paper's inference
rules). This is where the structural recovery algorithm has to
actually work, not just handle the easy case.

**Test**:
```lua
-- input
while x > 0 do
    x = x - 1
end
-- expected output
while x > 0 do
    x = x - 1
end
```

### Stage 12 — Function calls

**Target**: `print("hello")`, `f(x, y, z)`.

**Forces**: CALL instruction. Argument setup. Function-call AST
node. Emitter for function calls. **Multres introduction** (B=0 or
C=0 cases) — but we can defer full multres handling to Stage 15 and
emit a `-- TODO` for those cases here.

**Test**:
```lua
-- input
print("hello")
-- expected output
print("hello")
```

### Stage 13 — Method calls and field access

**Target**: `obj:method(args)`, `obj.field`, `obj[key]`.

**Forces**: TGETS/TGETV/TGETB instructions. Method-call detection
(structural equality check per Phase A LuaJIT deep-dive §2.5).
Method-call syntax in emitter.

**Test**:
```lua
-- input
obj:method("arg")
-- expected output
obj:method("arg")
```

### Stage 14 — Tables

**Target**: `local t = {1, 2, 3}`, `local t = {a = 1, b = 2}`,
`local t = {}; t.x = 1`.

**Forces**: TNEW, TDUP, TSET* instructions. Table literal
reconstruction (TDUP templates + TSET merging). Array part vs hash
part of tables.

**Test**:
```lua
-- input
local t = { 1, 2, 3 }
-- expected output
local t = { 1, 2, 3 }
```

### Stage 15 — Multi-return values and varargs

**Target**: `local a, b, c = f()`, `function(...) return ... end`,
`return f()`.

**Forces**: **full multres handling**. The Q4 open question from
Step 2 gets resolved here by implementation. Stage 12 introduced
multres; this stage implements it properly.

**Test**:
```lua
-- input
local a, b, c = f()
-- expected output
local a, b, c = f()
```

### Stage 16 — Closures and upvalues

**Target**: `local function() ... end` (with captured variables),
`local function outer() local x = 1; return function() return x end end`.

**Forces**: FNEW, UGET, USETx, UCLO. Cross-proto upvalue resolution
(Q5 from Step 2). Nested function AST. Children-first post-order
proto traversal.

**Test**:
```lua
-- input
local function outer()
    local x = 42
    return function() return x end
end
-- expected output
local function outer()
    local x = 42
    return function()
        return x
    end
end
```

### Stage 17 — FR2 mode + final hardening

**Target**: decompile a corpus of FR2-mode bytecode correctly.

**Forces**: testing across both FR1 and FR2 modes. Test corpus
selection (Q6 from Step 1) gets resolved here. Polish, edge cases,
graceful-degradation testing (intentionally malformed bytecode,
panic recovery, etc.).

**Test**: full Darktide corpus (or selected v1 subset per Step 4)
decompiles without crashes; output is syntactically valid Lua.

## 4. Implementation notes for the bigger stages

The bigger stages (7, 11, 15, 16) deserve more detail. Here's
additional guidance for each.

### Stage 7 (if/then/end) — detailed

This stage introduces a lot at once. Recommended sub-stages:

- **7a**: Build CFG construction. For the simplest if/then input,
  identify basic blocks (entry, then-body, merge). Compute
  predecessor/successor edges. Don't compute dominators yet.
- **7b**: Compute dominator tree (Lengauer-Tarjan or iterative).
  This is needed for SSA construction.
- **7c**: Extend SSA construction to handle phi functions. Use the
  Cytron algorithm with dominance frontiers (per Phase A Step 4
  addendum). First time we have a real merge point.
- **7d**: Structural recovery for the simplest if/then case. The
  No More Gotos algorithm's condition-based refinement should
  identify the if/then from the reaching conditions of the merge
  block.
- **7e**: Phi elimination at merge points. For the simple case
  (one variable defined in the then-branch, used after), emit it
  as the source-level name in both places.

Each sub-stage should produce a working decompiler for some subset.
7a's working decompiler still handles only Stage 6 inputs (no if),
but the CFG infrastructure is in place.

### Stage 11 (while/repeat) — detailed

The No More Gotos algorithm's cyclic region structuring. Recommended
approach:

- Detect back-edges (JMP to earlier instruction).
- Compute loop nodes via graph slicing.
- Apply the loop-type inference rules (while vs do-while vs endless).
- For while: identify the continuation condition.
- For repeat: identify the exit condition.

If the algorithm has bugs, they'll show up here. Allocate extra time.

### Stage 15 (multres) — detailed

This is where the Q4 design decision gets validated. Implementation:

- For producers (CALL/CALLM with C=0, VARG with B=0): create an
  `SsaValue::Multres` with `MultresCount::Known(n)` if n is
  statically determinable from the consumer, else `Dynamic`.
- For consumers: if the count is known, materialize n SSA names
  that reference the multres tuple with indices. If dynamic,
  reference the tuple opaquely.
- For emission: known counts are easy (just emit the N locals or
  whatever). Dynamic counts need a fallback (TODO comment or
  best-effort emission).

If `Dynamic` turns out to be common (corpus analysis during this
stage), we'll need to revisit the design.

### Stage 16 (closures) — detailed

Upvalue resolution is the key new piece:

- Read upvalue descriptors from the bytecode (per Phase A LuaJIT
  deep-dive §2.7).
- For each UGET/USETx in a proto, resolve to either parent's
  register (open) or parent's upvalue (closed).
- During emission, look up the source name via the descriptor chain.

This is conceptually independent of SSA — upvalues are a separate
mechanism. The challenge is threading the resolution through the
emission pipeline.

## 5. How the Step 2 open questions get resolved

The five open questions from `architecture-v2-data-structures.md` §5:

| Open question | Resolved in stage | Notes |
|---------------|-------------------|-------|
| ExprKey representation | Stage 9+ (when LVN/CSE comes in) | Actually defer to a later pass — Stage 9 is for compound conditions, not LVN. LVN comes when we add the `transform` module's first real pass, probably after Stage 11. |
| Debug-info collision policy | Stage 6 | First time we have to pick a policy. |
| Multres `Dynamic` frequency | Stage 15 | Corpus analysis during this stage validates the Q4 design. |
| AST ↔ SSA name threading | Stage 7 | First time structural recovery produces AST referencing SSA names; first time phi elimination has to thread through. |
| Recursion depth limits | Stage 11 | First time we may have deeply nested control flow. Add stack guards then. |

## 6. Risk register

Risks I can see now, with mitigation:

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| SSA construction has subtle bugs | Medium | Test the construction algorithm directly (independent of the rest of the pipeline) with fixtures where we know the correct SSA form. |
| No More Gotos algorithm is harder to implement than the paper suggests | Medium-high | The paper has a reference implementation (DREAM) we can consult for algorithmic questions. Stage 11 is where this risk materializes; budget extra time. |
| Multres design (Q4) is wrong | Medium | Stage 15 is the validation point. Fallback: simplify to known-counts-only with TODOs for dynamic. |
| AST ↔ SSA threading is more complex than expected | Low-medium | Stage 7 surfaces this. Design judgment in Step 2 was to defer; we'll know more after 7. |
| Pipeline ordering is wrong (some step needs to be reordered) | Low | The pipeline orchestrator is small; reordering is mechanical if needed. |
| Marsinator spot-check missed something we should have caught | Low | We treated Marsinator as one data point, not authoritative. If we discover we missed something, that's a Phase A+ loop. |

## 7. What this plan does NOT cover

- **Concrete code**. This is a plan, not an implementation. Each
  stage's actual code gets written when we work the stage.
- **Module-internal design**. We've defined types and interfaces;
  we haven't designed internal helper functions, error types,
  logging, etc. Those emerge during implementation.
- **Phase B Step 4 (testing strategy)**. That's the next doc. This
  plan references tests per stage; Step 4 systematizes the testing
  approach.
- **Estimated timeline**. Per user clarification: there's no
  deadline. The plan is ordered by dependency, not by calendar.

## 8. Calibration

Confidence levels on the plan itself:

- **The stage sequence**: high confidence. Ghuloum's methodology is
  well-established; the ordering follows naturally from what each
  stage forces.
- **The "roughly one work session per stage" sizing**: low
  confidence. Some stages will be bigger than expected (especially
  7, 11, 15, 16). Plan to adjust sizing after we've done a few.
- **The risk register**: medium confidence. I can see the risks I
  can see; the risks I can't see are by definition not on the list.

The medium-confidence items in Step 2 (Q4 multres, Q7 hand-roll)
both have explicit resolution points in this plan (Stage 15 for
multres; hand-roll is no longer flagged as a risk per the no-deadline
context).

## 9. Next step

Phase B Step 4: testing strategy. This systematizes the per-stage
testing referenced above. Covers:

- Test fixture format and organization.
- Snapshot testing infrastructure.
- Corpus regression testing approach.
- Test corpus v1 selection (Q6 from Step 1).
- What invariants the corpus regression checks.

Same review request: I'd like your review on this plan before Step
4. Specifically:

- Are the stages at the right granularity? (Too coarse? Too fine?)
- Is the ordering right? Anything obvious missing or out of order?
- The four "bigger stages" (7, 11, 15, 16) — comfortable with the
  sub-staging approach for Stage 7, or want more detail on the
  others too?
