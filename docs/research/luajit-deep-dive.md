# LuaJIT in depth — synthesis with the canonical pipeline

**Audience**: same as the primer — experienced developer, not a compiler
specialist. Read after `decompilation-primer.md`.

**Purpose**: Step 2 of Phase A. Ties the LuaJIT bytecode format to the
canonical decompiler pipeline established in the primer. Synthesis, not
redocumentation — for the structural byte format, the format spec is
the reference; for what decompilation *is*, the primer is the
reference. This doc sits between them: it says what LuaJIT's specific
shape *means* for anyone trying to decompile it.

**Methodology**: empirical verification matters here. Where the
operational semantics of an opcode are non-obvious (operand roles,
idiom shapes), the claims in this doc are checked by compiling small
Lua fixtures with system `luajit` (LuaJIT 2.1) and dumping the
resulting bytecode with `luajit -bl`. Sources cited inline.

**Status**: Step 2 of Phase A. Clean-slate domain study — does not
reference any existing decompiler implementation.

---

## 1. LuaJIT in the canonical pipeline

Mapping the 7 canonical phases from the primer to LuaJIT-specific
realities:

| Phase | Generic task | LuaJIT-specific note |
|-------|--------------|----------------------|
| 1. Frontend | Decode bytes → instructions | **Trivial**: 4-byte fixed instructions, format fully specified, debug info usually present. |
| 2. IR / CFG | Group into basic blocks | **Mechanical**: jump targets are explicit, no computed jumps. |
| 3. Control-flow recovery | Jumps → if/while/for | **Mixed**: numeric-for and generic-for have dedicated opcodes (easy); while/repeat use arbitrary jumps like binary (harder). |
| 4. Structural analysis | Compound conditions, etc. | Recoverable; the isec2016 paper is one technique here. |
| 5. Data-flow analysis | Liveness, reaching defs, expressions | **The core effort** — register-based, so analyses are concrete, but register reuse is the difficulty. |
| 6. Type recovery | Infer types | **Largely N/A** for Lua (dynamic). |
| 7. Code generation | Emit source | Standard, with Lua-specific syntax (method-call sugar, multi-return, varargs, table literals). |

The key takeaway from the primer — that LuaJIT lets us skip much of
phases 1-2 that dominates binary decompilation — means decompiler
effort concentrates in phases 3-5.

## 2. Instruction classes mapped to analyses

Organized by the role they play in the canonical analyses. Opcode
numbers and bit-layout details belong in a format spec; the point here
is *what kind of analysis each class participates in*.

### 2.1 Constants and literals (KSTR, KSHORT, KNUM, KPRI, KNIL)

**Role**: leaf expressions — the atoms of any expression tree.

**Note**: trivial to recover; the value is in the instruction itself.
The interesting question for decompilation is *when* a constant is
inline-rendered vs. hoisted to a named local — that's a data-flow
question, not a constant-decoding question.

### 2.2 Loads and moves (MOV, GGET, UGET)

**Role**: value movement — the wiring of the data-flow graph.

**Note**: MOV is the canonical "copy propagation" candidate (replacing
`local x = y; use(x)` with `use(y)`). GGET (read global) is
interesting: `local x = require("...")` is a GGET in a chain, and
recognizing that the result is reused downstream is a data-flow
problem. UGET (read upvalue) appears in closures (§4.5 below).

### 2.3 Arithmetic (ADDVN, ADDNV, ADDVV, SUB*, MUL*, DIV*, MOD*, POW, CAT, UNM, LEN, NOT)

**Role**: building blocks of expression trees.

**Note**: the VN/NV/VV suffixes encode operand type (register vs
number-constant) and order. ADDVN is `R[A] = R[B] + KNUM[C]`; ADDNV is
`R[A] = KNUM[C] + R[B]` (operands swapped — matters for non-commutative
ops like SUB). ADDVV is register+register.

CAT is unusual: it's a *range* concat over R[B..C], not a single binary
op. `a..b..c..d` is one instruction. The decompiler has to expand the
register range into the source-level expression chain.

### 2.4 Comparisons and tests (ISLT..ISF, ISTC, ISFC)

**Role**: condition recovery (Phase 3-4 of the canonical pipeline).

**Note**: the ISxx family tests a condition and jumps based on the
result. The JMP-after-ISxx is taken when the test *fails* — a
non-obvious semantic that's easy to get backwards (the source condition
is the *negation* of what ISxx tests).

ISTC/ISFC are *conditional assignment*: `ISTC A D` means "if D is
truthy, assign R[D] to R[A] and jump." These look like they should
implement the `x and y` / `x or y` assignment pattern, but empirically
the compiler often uses ISF + JMP + MOV instead (see §4.2). Knowing
when each shape appears is itself a research question.

### 2.5 Tables (TNEW, TDUP, TGETV, TGETS, TGETB, TSETV, TSETS, TSETB, TSETM, TSETR)

**Role**: Lua's central data structure. Two sub-patterns:

- **Literal table construction**: TNEW (empty table) + TSET* (set
  fields), or TDUP (template-clone with pre-built array/hash parts).
- **Table access**: TGET* (read) and TSET* (write). TGETS with a
  string key is the `t.field` case; TGETB with an integer key is
  `t[i]`; TGETV with a register key is `t[k]`.

TSETM (multres set, for `t = {f()}` patterns) is hard — it depends on
a previous CALL's multres output, tying table construction to call
semantics.

### 2.6 Calls (CALL, CALLM, CALLT, CALLMT, ITERC, ITERN, VARG)

**Role**: function invocation — the most semantically complex
instruction class.

**Note**: this class has the most subtlety for decompilation:

- **Operand semantics** (see §4.1 below for empirical verification):
  for CALL/CALLM in ABC format, **B = result count +1, C = argument
  count +1**. Either can be 0 meaning multres.
- **FR1 vs FR2 frame layout** (see §5): the arg-base offset depends on
  a header flag.
- **CALLM / CALLMT** consume multres args from a previous CALL/VARG —
  the decompiler has to thread the previous call's result count
  through to know what's being passed.
- **ITERC / ITERN** are call-shaped (they invoke the iterator function
  in a generic-for loop) but with a different layout (iterator at
  A-3..A-1).
- **VARG** reads `...` into a register range; multres variant is used
  for `return ...` and forward-multi-value contexts.

### 2.7 Functions and upvalues (FNEW, UGET, USETV, USETS, USETN, USETP, UCLO)

**Role**: closures and captured variables.

**Note**: FNEW creates a closure over a child prototype. Children are
emitted depth-first post-order, so the parent's `gc_consts` references
to child protos are resolved by counting backwards.

Upvalues are the more interesting part: a `local x` captured by a
nested function becomes an upvalue, accessed via UGET inside the inner
function and (if assigned) USETx. UCLO closes upvalues when the
capturing scope exits.

For decompilation, recognizing that `UGET 0 0` inside a closure means
"the variable named by upvalue[0]" requires resolving the upvalue
descriptor — which points back to either a parent register (open
upvalue) or a parent upvalue (closed upvalue). The bytecode preserves
this structure unambiguously via the upvalue descriptor table.

### 2.8 Returns (RETM, RET, RET0, RET1)

**Role**: function exit, possibly with multi-value return.

**Note**: RET0 and RET1 are trivial. RET with `D = n+1` returns
R[A..A+n-1]. RETM is multres (for `return f()` style — passes through
all results from a previous call).

### 2.9 Control (JMP, LOOP)

**Role**: flow redirection; the raw material of structural recovery.

**Note**: JMP is the universal branch (conditional when following an
ISxx, unconditional otherwise). LOOP is a marker instruction the
interpreter uses for loop bookkeeping; it doesn't directly affect
control flow but signals a loop entry — useful as a loop-detection hint
during structural recovery.

### 2.10 Loops (FORI, FORL, ISNEXT, ITERL)

**Role**: dedicated loop opcodes — the *easiest* loops to recover
because the structure is explicit.

**Note**: LuaJIT has dedicated opcodes for numeric-for (FORI/FORL) and
generic-for (ISNEXT/ITERL, with ITERC/ITERN as the step). The hard
loops for decompilation are while/repeat, which use LOOP+JMP without
dedicated opcodes and require structural recovery from arbitrary jump
patterns — same problem class as binary-decompiler loop recovery.

## 3. What LuaJIT's compiler preserves and discards

| Information | Preserved? | Where | Implication for decompilation |
|------------|------------|-------|-------------------------------|
| Instruction list | Always | `proto.insts` | Source of truth; unambiguous |
| Constant pool | Always | `proto.gc_consts`, `proto.num_consts` | Unambiguous |
| Upvalue structure | Always | `proto.upvalues` | Lets a decompiler resolve captured-variable references |
| Function nesting | Always | Children-first post-order | Lets a decompiler reconstruct `local function` nesting |
| Prototype flags (vararg, child, ffi) | Always | `proto.flags` | Distinguishes `function(...)` from named args |
| Variable names | **If not stripped** | `debug.var_info` | Critical for readable output |
| Variable scope (start/end pc) | **If not stripped** | `debug.var_info` | Lets a decompiler resolve `local` vs assignment |
| Line numbers | **If not stripped** | `debug.line_info` | Useful for diagnostics; not directly for source recovery |
| Upvalue names | **If not stripped** | `debug.upvalue_names` | Helpful but redundant with upvalue descriptors |
| Chunkname | **If not stripped** | `header.chunkname` | File path recovery |
| Comments | Never | — | Genuinely lost |
| Source formatting | Never | — | Genuinely lost |
| Exact spelling of equivalent constructs | Never | — | `not a == b` vs `a ~= b` indistinguishable |
| Expression structure | Never | — | **The core data-flow recovery target** |
| Control structure (if/while/for) | Implicitly via jumps | `insts` | Recoverable via Phase 3 analysis |
| Local-vs-global distinction | Implicitly via debug info | `debug.var_info` | Recoverable when debug info present |
| Optimization artifacts | Often invisible | — | Sometimes ambiguous (was this inlined?) |

The pattern: LuaJIT preserves **structure** (the format and the
instruction graph) and **debug metadata** (when not stripped); it
discards **intent** (comments, formatting, exact source spelling) and
**expression shape** (sequences of ops collapse to expressions only
via analysis).

When the input is non-stripped (common for game-script bundles,
including the Darktide corpus the broader project works with), the
decompiler gets debug info — a major advantage over binary
decompilation.

## 4. Common Lua-to-bytecode idioms (empirically verified)

Verified by compiling small fixtures with system `luajit` and dumping
with `luajit -bl`. For each idiom, the bytecode shape and what
recovery is needed.

### 4.1 CALL operand semantics

For CALL and CALLM in ABC format, the operand roles are:

- **A** = base register (function and first result)
- **B** = number of results wanted +1 (B=0 means multres — pass all up)
- **C** = number of arguments +1 (C=0 means multres — from previous
  CALL/VARG)

This is verifiable with simple test cases. Source:
`local function f() return 1 end` plus a sequence of varying-arity
calls; dump with `luajit -bl`.

| Source | Bytecode | Decoded |
|--------|----------|---------|
| `f()` (0 args, 0 results used) | `CALL 3 1 1` | A=3, B=1 (0 results), C=1 (0 args) |
| `r1 = f()` (0 args, 1 result) | `CALL 3 2 1` | A=3, B=2 (1 result), C=1 (0 args) |
| `ra, rb = f()` (0 args, 2 results) | `CALL 4 3 1` | A=4, B=3 (2 results), C=1 (0 args) |
| `r2 = g(7)` (1 arg, 1 result) | `CALL 6 2 2` | A=6, B=2 (1 result), C=2 (1 arg) |
| `r3 = h(1, 2, 3)` (3 args, 1 result) | `CALL 7 2 4` | A=7, B=2 (1 result), C=4 (3 args) |

The pattern is unambiguous. **Implication for decompilation**: the
fixed result count wanted by the source is recoverable from B (when
B>0), which is what lets a decompiler emit `local p, q, r = f()`
rather than `local p = f()` — it can know the source expected exactly
three values. Multres (B=0) is the harder case, requiring threading
of the previous instruction's result count.

### 4.2 `and`/`or` short-circuit (the `local a = x and y` pattern)

Source: `local a = x and y` — compiled to (abbreviated):

```
GGET 0 0   ; x        -- load x
ISF 0                -- if x is falsy, jump
JMP 1 => 0005        -- skip y load
GGET 0 1   ; y        -- overwrite R0 with y
```

So `local a = x and y` is the **ISF + JMP + load** pattern, not the
ISTC/ISFC pattern one might expect. The compiler chose to express it
as a conditional jump rather than a conditional assignment.

`local b = x or y` is symmetric (IST + JMP + load).

**Implication**: ISTC/ISFC aren't the primary shapes for `and/or`
assignment-to-local in current LuaJIT. They surface elsewhere —
probably in argument-passing contexts like `f(x and y)`, where the
conditional assignment avoids an extra register. When each shape
appears is a question for Step 3 (reference implementation study) and
Step 4 (dataflow literature).

### 4.3 Multi-return consumption (`local p, q, r = f()`)

Per §4.1, this compiles to `CALL 2 4 1` — B=4 means 3 results wanted.
The compiler sets B to encode exactly the number of values the source
expects. This is recoverable; the decompiler doesn't have to guess.

### 4.4 Varargs (`function(...) return ... end`)

```
VARG 0 0 0   -- load ... into R0..top (multres)
RETM 0 0     -- return multres from R0
```

VARG with B=0 is multres (read all varargs); VARG with B>0 reads a
fixed count. The VARG + RETM multres combination is the
"pass-through" pattern (`function(...) return ... end`) that benefits
from recognition as a unit rather than as two independent operations.

### 4.5 Closures with upvalue capture

Source:
```lua
local function outer()
    local captured = 42
    local function inner()
        return captured + 1
    end
    return inner
end
```

Bytecode shape (inner proto):
```
UGET 0 0      ; captured — read from upvalue 0
ADDVN 0 0 0   ; + 1
RET1 0 2
```

Outer proto:
```
KSHORT 0 42   ; local captured = 42
FNEW 1 0      ; local inner = closure over child proto 0
UCLO 0 => 4   ; close upvalue, jump
RET1 1 2      ; return inner
```

The inner proto has `numuv = 1` and an upvalue descriptor pointing to
the parent's R0 (`captured`). The UGET in the inner proto resolves
through that descriptor. A decompiler that wants to render the source
faithfully has to walk the upvalue descriptors in the bytecode header
of each proto to resolve UGET/USETx to the actual captured variable
name (or to a synthesized name if debug info is stripped).

### 4.6 String concat chain

Source: `local s = "a" .. "b" .. "c" .. "d"`:

```
KSTR 6 4   ; "a"
KSTR 7 5   ; "b"
KSTR 8 6   ; "c"
KSTR 9 7   ; "d"
CAT 6 6 9  -- R[6] = R[6] .. ... .. R[9]
```

CAT takes a *range* (B and C are register endpoints) and concatenates
everything in between. The decompiler expands the range to the chain
of operands.

### 4.7 Nested function call expression

Source: `local r = f(g(x) + h(y))`:

```
GGET 8 2   ; f
GGET 10 9  ; g
GGET 12 0  ; x
CALL 10 2 2  -- g(x) → R10   (B=2: 1 result; C=2: 1 arg)
GGET 11 10 ; h
GGET 13 1  ; y
CALL 11 2 2  -- h(y) → R11
ADDVV 10 10 11  -- R10 = g(x) + h(y)
CALL 8 2 2   -- f(R10) → R8
```

This is the canonical case for expression-recovery via dataflow: the
intermediate calls' results are read exactly once and could in principle
be inlined into the outer expression. The harder case is when an
intermediate result is read *multiple times* downstream — then the
decompiler has to decide whether to hoist to a named local or
re-expand, which is a value/CSE-style decision.

## 5. The FR1/FR2 distinction

The frame-layout question: when a function is called, where do its
arguments live relative to the CALL instruction's A operand?

- **FR1 (one-slot)**: function at R[A], args at R[A+1], R[A+2], ...
  The default in 32-bit builds and most desktop LuaJIT.
- **FR2 (two-slot)**: function at R[A], a "continuation/frame" slot at
  R[A+1], args starting at R[A+2]. Used in 64-bit GC mode (`LJ_FR2`).
  Required for some LuaJIT 2.1 builds.

The mode is recorded in the bytecode header (`BCDUMP_F_FR2 = 0x08`).
A decompiler reads this flag and applies the appropriate offset to
every call-shaped operation.

**Why it matters for decompiler design**: every analysis that touches
call argument layout — CALL, CALLM, CALLT, CALLMT, ITERC, ITERN, VARG
— needs to apply the correct offset consistently. The format spec
spells out the offset for CALL/CALLM explicitly; for the
tail-call/iterator variants the layout is less clearly documented and
worth empirical verification during any implementation effort.

**Practical note**: different LuaJIT builds default to different modes.
A decompiler tested only against FR2 (or only against FR1) can pass its
tests while failing on the other mode. Whatever reference corpus is
used during development should include both, or the decompiler should
be exercised against both modes synthetically.

## 6. LuaJIT-specific design considerations

Pulling the above into design-shape implications for *any* LuaJIT
decompiler:

### Advantages LuaJIT gives a decompiler

1. **Clean frontend.** The format spec is finite, parsing is
   deterministic. This is significant — binary decompilers spend
   enormous effort on instruction decoding.
2. **Debug info usually present** in non-stripped dumps. Variable names
   and scopes are recoverable. This collapses Phase 6 (name recovery)
   into "read debug info" — a major win vs. binary decompilation.
3. **Explicit control flow.** No computed jumps; every jump target is
   a static offset. Basic-block construction is mechanical.
4. **Dedicated loop opcodes.** Numeric-for and generic-for have
   FORI/FORL/ISNEXT/ITERL — the structure is *in* the bytecode. Only
   while/repeat require structural recovery from arbitrary jumps.

### Challenges LuaJIT poses

1. **Register reuse.** The same slot holds different values over time.
   Without liveness analysis, a decompiler will conflate them — reading
   the same register at two different points as if it were one value
   when the source actually had distinct variables.
2. **Multres is pervasive.** CALL/CALLM/CALLT/CALLMT/VARG/RETM/TSETM
   all have multres variants where the count is "from previous" or
   "fill to top." These chain across instructions, so single-pass
   analysis misses them.
3. **Idioms have multiple valid encodings.** `local a = x and y`
   compiles to ISF+JMP+load *or* (in other contexts) ISTC. The
   decompiler needs to recognize both shapes and produce the same
   output.
4. **Expression shape is entirely gone.** LuaJIT's compiler flattens
   expression trees into instruction sequences. The structure is
   recoverable via data-flow analysis, but the analysis has to be
   non-trivial — a single-pass forward walk isn't enough.
5. **Frame-layout ambiguity.** FR1 vs FR2 changes operand offsets and
   has to be threaded through every call-shaped operation, not just
   CALL/CALLM.
6. **while/repeat still require structural recovery.** LuaJIT gives us
   dedicated opcodes for `for`, but `while`/`repeat` use LOOP+JMP and
   require the same structural analysis binary decompilers do.

## 7. Open domain questions

Things this synthesis surfaced but didn't resolve. They're questions
about LuaJIT or about decompilation technique — not about any specific
existing decompiler:

- **When does ISTC/ISFC vs ISF+JMP appear?** The compiler emits
  different shapes for the same source construct in different contexts.
  What are the rules? Step 3 (reference implementation study) may
  illuminate.
- **Do CALLT/CALLMT/ITERC/ITERN need FR2 offset adjustment?** The
  format spec is explicit for CALL/CALLM but less clear on the others.
  Worth empirical verification.
- **What are the precise semantics of VARG's B and C operands?** The
  format spec mentions B=0→multres, C=results, but the full behavior
  deserves empirical confirmation across usage patterns.
- **What's the right data-flow analysis substrate for a register-VM
  decompiler?** SSA? Traditional use-def chains? Value numbering?
  Step 4 is where this gets real.
- **What's the relationship between debug-info `var_info` scope
  records and actual liveness?** They *should* correspond but the
  exact correspondence (and where they diverge) is worth understanding
  for any decompiler that wants to use debug info as ground truth.

## 8. What's next

Per `phase-A-plan.md`:

- **Step 3** — walk marsinator's source as a working LuaJIT decompiler
  in its own right; read the isec2016 paper. (Framing discussion
  required before starting.)
- **Step 4** — selective survey of the relevant dataflow analyses.
- **Step 5** — knowledge assessment / decision point.

## Sources

- `docs/luajit-bytecode-format.md` — structural format reference.
- `decompilation-primer.md` (this directory) — the canonical pipeline
  framework this doc maps to.
- Cifuentes (1994), *Reverse Compilation Techniques* — pipeline origin.
- Yakdan et al. (NDSS 2015), *No More Gotos* — modern structural
  recovery.
- Empirical bytecode dumps from system `luajit` 2.1.1774896198.
  Fixtures used: `/tmp/opencode/step2_probe/{idioms.lua,
  call_verify.lua}` — reproducible by re-running `luajit -bg` and
  `luajit -bl`.
