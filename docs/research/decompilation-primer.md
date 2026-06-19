# Decompilation Primer

**Audience**: an experienced developer who is not a compiler or
decompiler specialist. Written as the foundational reference for the
luadejit project's Phase A work (see `.agents/phase-A-plan.md`).

**Purpose**: level-set on what decompilation *is* as a discipline — the
canonical pipeline, what compilation throws away, what is and isn't
recoverable, and where LuaJIT fits — before we get into technique-specific
reading or any redesign of our decompiler.

**Status**: Step 1 of Phase A. Subsequent artifacts (a LuaJIT synthesis
and a diagnosis of our decompiler) build on the framework established
here. Sources are cited at the section level; full references are in
`.agents/references.md`.

---

## 1. What decompilation is

Decompilation is the inverse of compilation: given a low-level
representation (machine code or bytecode), reconstruct a high-level
source program that would compile to the observed low-level form.

Important framing points that are easy to miss:

- **It is not "running the program backwards."** Decompilation is a
  *static analysis* — it reasons about the code without executing it.
  It reconstructs source that *would have produced* the observed
  bytecode; it does not observe the program's runtime behavior.
- **The reconstruction is never unique.** Many different source programs
  compile to identical bytecode. `x = x + 1` and `x += 1` compile to the
  same instructions in most languages. So the decompiler isn't
  "recovering the original source" — it's producing *a* plausible source
  that round-trips. The goal is readability and round-trip fidelity,
  not archaeological reconstruction.
- **It is fundamentally harder than compilation.** Compilation is a
  many-to-one function (lots of source → one bytecode). Decompilation
  has to pick one output from many candidates, and the candidates it
  prefers (idiomatic, readable source) require recovering structure the
  compiler deliberately threw away.

Why do it:

- Recovering lost source (the original Lua source for a compiled LuaJIT
  file is gone — our use case for Darktide modding).
- Understanding obfuscated or proprietary code.
- Security analysis (malware reverse engineering).
- Compiler-internals education (us).

## 2. What compilation throws away

A compiler takes high-level source and produces low-level code. In the
process it discards (selectively, by category):

| Lost | Sometimes / always | Recoverable? |
|------|---------------------|--------------|
| Variable names | Always (replaced by registers or stack slots) | Only if debug info preserves them |
| Types | Often (replaced by raw bits or tagged values) | Partially; harder in dynamic languages |
| Expression structure | Always (`a + b * c` becomes load/mul/add sequence) | Mostly — *the core dataflow problem* |
| Control-flow structure | Always (`if`/`while`/`for` become conditional + unconditional jumps) | Mostly — *the core structuring problem* |
| Scope boundaries | Always (a `local` becomes a reusable register slot) | Mostly — tied to dataflow |
| Comments and formatting | Always | Never |
| Source-level idioms | Always (`x += 1` vs `x = x + 1`, sugar) | Never (compiler already normalized) |
| Optimization artifacts | Depends | Often ambiguous (was this inlined? unrolled?) |

The two rows marked "core" are what most decompiler engineering effort
goes into. Our luadejit is no exception — the bugs in
`.agents/known-bugs.md` are expression-structure and (to a lesser
extent) control-flow recovery failures.

## 3. What's recoverable, and how hard

Grouping roughly by difficulty:

**Trivially recoverable** (mechanical, deterministic):
- Literal constants (numbers, strings, nil/true/false)
- Direct function call structure (call an address with N args, M results)
- Basic arithmetic (one ADD instruction → one `+`)
- Load and store of named globals

**Recoverable with structured effort** (the meat of decompilation):
- Control flow: arbitrary jump patterns → nested `if`/`while`/`for`/`repeat`
- Expression trees: sequences of simple ops → nested expressions like
  `f(g(x) + h(y))`
- Variable scopes and lifetimes
- Method-call syntax (`obj:m()` vs `obj.m(obj, ...)`) when the bytecode
  passes `self` as the first argument

**Recoverable only when present**:
- Variable names — only if debug info preserves them. LuaJIT bytecode
  often does; stripped binaries never do.
- Line numbers and source positions — same.
- Upvalue closure structure — present in the bytecode format.

**Recoverable only with inference / heuristics**:
- Type information — very limited in dynamic languages; the bytecode
  knows "this is a number" at the moment of an arithmetic op but not
  much more.
- Variable name *guessing* when debug info is stripped (e.g.,
  naming a loop variable `i` based on convention).

**Genuinely unrecoverable**:
- Comments
- Source formatting (indentation, blank lines, line wrapping)
- The exact spelling of equivalent constructs (`x = x + 1` vs `x += 1` —
  though Lua doesn't have `+=`, the principle holds for `not a == b`
  vs `a ~= b`)
- Intent. If the original author wrote `if x then` with no else, the
  compiler may emit the same bytecode as `if x then ... else end`.
  Decompilers pick one.

The decompiler's job is to maximize the "recoverable with structured
effort" tier without producing wrong output in the process.

## 4. The canonical decompiler pipeline

Established by Cristina Cifuentes' 1994 PhD thesis ("Reverse Compilation
Techniques") and refined by every serious decompiler since. A typical
decompiler has these phases, roughly in order:

```
  raw bytes
     │
     ▼
[1] Frontend ............... parse bytes → instruction list
     │
     ▼
[2] IR construction ........ instructions → basic blocks → CFG
     │
     ▼
[3] Control-flow recovery .. arbitrary jumps → if/while/for/repeat
     │
     ▼
[4] Structural analysis .... identify loops, conditionals, compound conditions
     │
     ▼
[5] Data-flow analysis ..... liveness, reaching definitions, use-def chains
     │
     ▼
[6] Type recovery .......... infer types from operations  (limited for dynamic langs)
     │
     ▼
[7] Code generation ........ emit source
```

Phase by phase:

1. **Frontend** — decode the raw format into a list of instructions. For
   us this is `src/reader/`, and `docs/luajit-bytecode-format.md` is the
   spec. This phase is mechanical and well-understood.

2. **IR construction** — group the flat instruction list into *basic
   blocks* (straight-line sequences with a single entry and single exit)
   and connect them into a *control-flow graph* (CFG) where edges
   represent possible flow of control. Basic blocks are bounded by jump
   targets and jump instructions. This phase is also fairly mechanical.

3. **Control-flow recovery** — the CFG has arbitrary edges (any jump can
   go anywhere). Source code has *structured* control flow (loops nest,
   conditionals don't overlap arbitrarily). This phase looks for
   patterns in the CFG that match structured constructs — a back-edge is
   a loop, a diamond-shaped branch is an `if/else`, etc. The "No More
   Gotos" paper (Yakdan et al., NDSS 2015) is the modern reference.

4. **Structural analysis** — refine the recovered control flow into
   cleaner forms. E.g., a chain of `if not X then jump` becomes
   `if X and Y then` via compound-condition recovery. This is where the
   boolean-expression paper marsinator cites (isec2016) lives.

5. **Data-flow analysis** — reason about *what values are where, when*.
   Liveness analysis: which variables are read later (so we don't emit
   dead assignments). Reaching definitions: which assignment of a
   variable reaches a given use. Use-def chains: link each use of a
   value to its definition. **This is the phase our decompiler is
   weakest at** — it's what Bugs 1 and 2 in `known-bugs.md` are about.

6. **Type recovery** — try to infer types from operations. Mostly
   relevant for static languages; less so for Lua where everything is
   dynamic by default.

7. **Code generation** — walk the analyzed IR and emit source. For us
   this is `src/codegen/`.

Decompilers split, merge, or reorder these phases. The order is roughly:
*structural recovery first* (give the code shape), *then data-flow
recovery* (give the code meaning). Doing them in the other order is
possible but harder — data-flow analysis on unstructured code tends to
over-approximate.

This pipeline framing is the single most useful thing to internalize
from this primer. The diagnosis in Step 5 maps our decompiler onto it
phase-by-phase, which makes the gaps immediately visible.

## 5. Stack-based vs register-based VMs (and why it matters)

Virtual machines come in two broad flavors. The decompilation literature
mostly assumes one or the other, and the choice affects which analyses
are natural.

**Stack-based VMs** (JVM, CPython bytecode, PostScript):
- Operands are pushed and popped from an implicit stack.
- Each instruction is short (`ADD` takes no operands — it pops two and
  pushes one).
- "What value is on top of the stack at instruction K?" requires
  *simulating the stack* to answer.

**Register-based VMs** (LuaJIT, Dalvik, Parrot, LLVM IR semi-):
- Operands live in named slots (registers).
- Each instruction names its sources and destination explicitly
  (`ADD R[3] R[1] R[2]`).
- "What value is in R[3] at instruction K?" is a direct data-flow
  question about a named location.

LuaJIT is register-based. This puts it closer to RISC binary (also
register-based) than to stack VMs in shape. **Practical implications:**

- **Good news**: data-flow questions are more concrete. A register is a
  named location; we can ask "where was this defined?" and "what reads
  this?" without simulating a stack. Many of the standard compiler
  analyses (liveness, reaching definitions, SSA) were originally
  formulated for register machines.
- **Bad news**: registers get *reused*. The same slot R[7] may hold a
  function argument at the top of a function, a temporary mid-body, and
  a loop variable later. Without explicit liveness tracking, the
  decompiler will treat every read of R[7] as the same value — which is
  exactly the bug pattern in `weapon_stats.lua` where `R[7]` is read 20
  times as if it were one value (when the source actually computed it
  once and named it `weapon_stats`).

This register-reuse problem is the data-flow version of the stack-
simulation problem. Same underlying issue (the same physical location
holds different logical values over time), different surface.

Most decompilation textbooks use stack VMs or RISC binary as examples.
The analyses translate to register VMs, but the framing requires
adjustment. We'll come back to this in the Step 4 literature survey.

## 6. The standard hard problems

The decompilation literature broadly agrees on what's hard. Listing
them so we can map our symptoms to them precisely in the diagnosis:

### Control-flow recovery

Going from arbitrary jumps to structured `if`/`while`/`for`/`repeat`.
Hard because:

- Compilers can transform structured source into arbitrarily-shaped
  jumps (e.g., loop unrolling, tail-call optimization, early-exit
  patterns).
- The CFG may not match any single structured construct cleanly —
  decompilers use heuristics to pick the closest match.
- `break`, `continue`, and `goto` produce jumps that look like
  structured-control jumps but mean different things.

For us: numeric/generic-for recovery works (Phase 2.5), `if/then/end`
works, but `while`/`repeat` and `if/then/else` are not fully recovered
(per the README's Limitations). The isec2016 paper marsinator cites is
specifically about boolean-expression *recovery* (a sub-problem:
recovering `if a and not b then` from a chain of conditional jumps).

### Expression recovery

Going from a sequence of simple instructions to a nested expression
tree. `f(g(x) + h(y))` becomes `CALL g x; CALL h y; ADD t1 t2; CALL f t3`
in bytecode. The decompiler has to recognize that the ADD's operands
are themselves calls, that the calls' results are used exactly once
here (and so can be inlined), and that the whole thing forms a single
expression.

This is **the core data-flow problem** and the place our decompiler is
weakest. Symptoms:

- `PowerLevelSettings` (Bug 1): a single `require()` call's result is
  read twice; we re-expand the call at each read instead of referring
  to a local.
- `calculate_stats` (Bug 2): a method call's result is read 20 times;
  same pattern at function-body scope.
- Loop accumulator (Bug 4): a variable updated in a loop body resolves
  to its initial value instead of carrying the self-reference.

All three are expression-recovery failures, of slightly different
flavors. The diagnosis will say which standard technique addresses
each.

### Type recovery

Inferring types from operations. Largely out of scope for Lua (the
bytecode doesn't carry rich type information, and Lua's dynamic typing
makes source-level type annotations sparse anyway). We're not going to
spend much time here.

### Name recovery

Recovering variable names. Only possible when debug info is present;
LuaJIT's debug info (the `var_info` records, see
`docs/luajit-bytecode-format.md` §8) usually is. Phase 1 of our work
(see `.agents/engagement-state.md`) made major progress here. When
debug info is absent or stripped, we fall back to synthetic `var_N`
names.

### Scope recovery

Determining where each variable is visible. Tied to data-flow (a
variable's scope is essentially its *live range* — from definition to
last use). For Lua specifically, this includes the `local` vs not-`local`
distinction, which our `post_process_duplicate_locals` pass handles
heuristically.

## 7. How binary decompilation differs from bytecode decompilation

Most of the foundational literature is about *binary* decompilation
(x86, ARM, etc.). LuaJIT is *bytecode* — already a level of abstraction
above machine code. The differences that matter to us:

| Concern | Binary decompilation | LuaJIT bytecode |
|---------|----------------------|------------------|
| Instruction decoding | Hard (variable-length, mode-dependent, ABI) | Trivial (fixed 4-byte instruction, format spec'd) |
| Calling convention | Discoverable but ambiguous (ABI inference) | Specified by the bytecode format |
| Type info | None | Limited but present (numeric vs other) |
| Variable names | None (stripped) | Usually present (debug info) |
| Control flow | Computed via disassembly | Explicit in the bytecode (jumps are explicit) |
| Optimizations | Aggressive (inlining, vectorization, etc.) | Modest (some folding, peephole) |

**Net effect**: bytecode decompilation skips much of the *frontend*
work that dominates binary decompilation (instruction decoding, ABI
inference, control-flow discovery). The remaining hard problems
(structural recovery, expression recovery) translate directly.

So when we read the binary-decompilation literature (Cifuentes, "No
More Gotos", Ghidra docs), the relevant parts are **phases 3-5** of the
canonical pipeline. Phases 1-2 are already done for us by the bytecode
format being explicit. This is a real advantage we should exploit.

## 8. What this means for luadejit

A preview of the Step 5 diagnosis — the full version goes in
`docs/research/luadejit-diagnosis.md`:

- **Phase 1 (frontend)**: solid. `src/reader/` parses the format
  correctly per `docs/luajit-bytecode-format.md`.
- **Phase 2 (IR construction)**: implemented but **mostly unused**.
  `src/ir/cfg.rs` builds basic blocks and a CFG; `src/ir/loop_detection.rs`,
  `src/ir/scope.rs`, `src/ir/upvalue.rs` exist but `codegen` doesn't
  consume their output. This is the architectural admission in
  `docs/architecture.md`'s closing line: *"The codegen layer uses a
  forward pass approach rather than CFG-based analysis."*
- **Phase 3 (control-flow recovery)**: ad-hoc, inside codegen. Each
  loop / conditional pattern is matched directly from the instruction
  stream. Works for common cases; misses some.
- **Phase 4 (structural analysis)**: minimal. The condition module
  (`src/ir/condition.rs`) does some boolean-expression recovery.
- **Phase 5 (data-flow analysis)**: **largely absent**. `reg_exprs`
  (`src/codegen/tracking.rs`) is a single-block, single-pass shadow of
  real data-flow. No liveness, no reaching definitions, no use-def
  chains. **This is where Bugs 1, 2, and 4 live.**
- **Phase 6 (type recovery)**: N/A for Lua; correctly skipped.
- **Phase 7 (code generation)**: works; the codegen layer produces
  readable output within the limits of what phases 3-5 hand it.

The diagnosis step will quantify this and prioritize the gaps. But the
going-in picture is clear: phases 1 and 7 are fine; phases 3-5 are
where the work is, with phase 5 (data-flow) being the most impactful
single gap and phase 2 (the unused CFG/scope/upvalue IR) being the
most obvious architectural debt.

---

## What's next

This primer is Step 1 of Phase A. The next artifacts are:

- **`docs/research/luajit-deep-dive.md`** (Step 2) — tie the LuaJIT
  bytecode format to the pipeline above. Most of the source material is
  already in `docs/luajit-bytecode-format.md`; the synthesis work is
  mapping it to the canonical analyses.
- **`docs/research/reference-implementation-notes.md`** (Step 3) — notes
  from walking marsinator's source and reading the isec2016 paper.
- **`docs/research/luadejit-diagnosis.md`** (Step 5) — the full
  phase-by-phase mapping of our decompiler to the canonical pipeline,
  with prioritized gaps. (Section 8 above is a preview.)
- **`docs/research/<topic>-design.md`** (Step 6) — design memo for
  whatever the diagnosis identifies as the top-priority gap.

## Sources

- Cifuentes, C. (1994). *Reverse Compilation Techniques.* PhD thesis,
  Queensland University of Technology. — the canonical decompiler
  pipeline.
- Yakdan, K., Elsner, N., Duregard, D., Knierim, T., Gersch, P., &
  Holz, T. (2015). *No More Gotos: Decompilation Using Pattern-
  Independent Control-Flow Structuring and Semantics-Preserving
  Transformations.* NDSS 2015. — modern control-flow recovery.
- Aho, Lam, Sethi, Ullman. *Compilers: Principles, Techniques, and Tools*
  (the Dragon Book). — Chapters 8-9 cover data-flow analyses.
- Cooper & Torczon. *Engineering a Compiler.* — accessible data-flow
  chapters.
- Project-internal: `docs/architecture.md`,
  `docs/luajit-bytecode-format.md`, `docs/decompilation-patterns.md`,
  `.agents/engagement-state.md`, `.agents/known-bugs.md`.
