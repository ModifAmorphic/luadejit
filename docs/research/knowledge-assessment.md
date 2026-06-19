# Knowledge Assessment — Phase A Decision Point

**Audience**: the user, who will decide whether to move to
Phase B or loop back for more Phase A work.

**Purpose**: Step 5 of Phase A. The gate. After Steps 1-4, do we have
what we need to architect and plan a LuaJIT decompiler from scratch?
This doc answers that honestly — including naming what we *don't* have,
which is at least as important as what we do.

**Status**: Step 5 of Phase A. The decision point.

---

## The short answer

**Yes, we're ready to move to Phase B, with two specific caveats worth
considering addressing first.** The caveats are:

1. Structural recovery (loops, irregular control flow) — we've read
   one paper (isec2016) covering the boolean-expression corner. The
   broader structural-recovery literature (No More Gotos, etc.) we've
   cited but not read primary. This is the second-most-important
   architectural area after the SSA substrate, and we have less
   primary-source depth here than I'd like.

2. The SSA-to-source bridge — how we go from SSA + analyses to clean
   emitted Lua source, specifically phi-elimination strategies for
   decompilers. The Cytron paper covers SSA *construction* but not
   phi *elimination* for source emission. The SSA Book would cover
   this; we haven't read those sections.

These aren't blockers — we can move to Phase B and address them
during architecture work. But addressing them first via short Phase A+
loops would reduce risk. The user's call.

## What we have, by assessment criterion

The plan called out seven questions the assessment should be able to
answer. Going through them honestly:

### 1. Can we describe the full pipeline of a LuaJIT decompiler, phase by phase?

**Yes.** The primer established the canonical 7-phase pipeline
(frontend → IR/CFG → control-flow recovery → structural analysis →
data-flow analysis → type recovery → code generation). The LuaJIT
deep-dive mapped LuaJIT specifics to each phase. The dataflow survey
and SSA assessment established what Phase 5 looks like in detail.
We can sketch:

```
LuaJIT bytecode
   → frontend (parse format)              [well-understood, format spec'd]
   → CFG construction                     [mechanical]
   → SSA construction                     [Cytron algorithm]
   → analyses (liveness, etc.)            [sparse on SSA, cheap]
   → structural recovery                  [isec2016 for booleans; broader lit for loops]
   → transformations (CSE, copy prop, DCE) [clean on SSA]
   → phi elimination + source emission    [partial coverage]
   → Lua source
```

### 2. Can we name the analyses a serious implementation needs and why?

**Yes for the core set.** Liveness, reaching definitions (free under
SSA), CSE/value numbering (clean on SSA), copy propagation, dead code
elimination. We know what each is for and what it buys.

**Partial for LuaJIT-specific extensions.** Multres tracking, upvalue
resolution across function boundaries, debug-info-driven scope
recovery. We know these exist and matter; we don't have proven
approaches for them.

### 3. Can we identify the genuinely hard problems and known techniques for each?

**Mixed.**

| Hard problem | Known technique | Coverage |
|--------------|-----------------|----------|
| Boolean expression recovery | isec2016 algorithm | **Strong** (primary read) |
| Loop / irregular control flow recovery | No More Gotos, structural analysis | **Weak** (cited, not read primary) |
| Expression recovery from register reuse | CSE / GVN on SSA | **Strong** |
| Variable naming | Debug info + SSA congruence | **Moderate** (concepts clear, details fuzzy) |
| Phi elimination for source emission | SSA Book Ch. on elimination | **Weak** (not studied) |
| Multres handling | Custom (no standard reference) | **Weak** |
| Upvalue resolution across closures | Custom interprocedural | **Moderate** |

### 4. Do we understand the tradeoffs between architectural choices?

**Yes on the big two:**
- SSA vs. traditional dataflow (settled: SSA)
- Pattern-matching forward-pass vs. analysis-based (settled: analysis-based)

**Less settled:**
- Implementation methodology (incremental Ghuloum-style vs. design-
  first-then-build). Ghuloum's paper is input but we haven't committed.
- Library vs. hand-roll for IR infrastructure (Cranelift's
  `cranelift-frontend` exists; using it trades learning value for
  implementation speed).

### 5. Have we studied enough prior art?

**This is the weakest area.** What we've read primary:
- isec2016 (one paper, one corner of structural recovery)
- Cytron 1991 (one paper, SSA construction)

What we've read at survey level:
- The Dragon Book / Cooper & Torczon chapters on dataflow (cited but
  not deep-read for this engagement)
- Cifuentes, No More Gotos, SSA Book (cited, not primary-read)

What we haven't studied at all:
- Marsinator's actual implementation (deferred, spot-check only)
- Other register-VM decompilers (we deliberately scoped this out)
- Empirical studies of LuaJIT decompiler quality at scale

**Honest characterization**: we have enough theoretical grounding to
*start* Phase B, but we'd be designing without having seen how anyone
else has actually built a LuaJIT decompiler. That's a non-trivial gap.

### 6. Do we know what Lua idioms are hard to recover and why?

**Empirically, partially.** Step 2 verified the bytecode shapes of
several idioms: arithmetic, calls, multi-return, varargs, closures,
concat chains, nested calls, basic conditionals.

**What we haven't done**: systematic corpus-wide analysis to find the
long tail of weird patterns. We know FR1/FR2, multres, and register
reuse matter, but we don't have a ranked list of "here are the top
N idioms that cause decompiler bugs."

### 7. Can we sketch, at a high level, how we'd structure the code?

**Yes at pipeline level** (per #1).

**No at module/data-type/function-signature level.** That's Phase B's
job, but we could have done more pre-sketching. Specifically: what
does the SSA IR look like as a Rust data structure? What's the
interface between passes? Where does debug info plug in?

## What we don't have (the gaps, prioritized)

In order of how much they'd benefit from more Phase A work:

1. **Structural recovery beyond boolean expressions.** Specifically:
   loop recovery (while/repeat), if/elseif/else chains, irregular
   control flow. No More Gotos (Yakdan et al. 2015) is the standard
   reference. Reading it primary would firm up the second-most-
   important architectural area.

2. **Phi elimination for source emission.** Specifically: when SSA
   names need to be collapsed back to source-level names, what's the
   algorithm? SSA Book chapters on this would help. Without it, Phase
   B will be designing this from scratch.

3. **Marsinator spot-check.** One working LuaJIT decompiler exists
   and is open-source. We've deliberately deferred looking at it.
   Even a few-hour spot-check ("how do they structure their pipeline?
   Do they use SSA?") would give a concrete data point.

4. **Variable naming strategy details.** When SSA names collapse,
   how do we pick the source-level name? Use debug info preferentially?
   Synthesize when missing? Handle conflicts? This is mechanical but
   has many small decisions.

5. **Multres empirical investigation.** Verify the multres edge cases
   (CALLM, CALLT, ITERC, VARG) against real bytecode so we know what
   we're designing for.

6. **LuaJIT-specific hard cases ranked.** A corpus analysis to find
   which patterns actually dominate the long tail of weird shapes.

## Options

**Option A — Move to Phase B now, learn as we go.** Use Ghuloum's
incremental methodology to start with a trivial decompiler and grow
it. Each growth step is a learning step; gap-filling happens during
implementation rather than during more reading.

- *Cost:* none up front
- *Risk:* architectural choices made without full information may need
  revision during Phase B. Some rework likely.
- *Benefit:* momentum; concrete artifacts sooner; learning is
  contextualized by actual implementation work.

**Option B — Short Phase A+ loop on the top 1-2 gaps, then Phase B.**
Specifically: read No More Gotos primary, and read SSA Book chapters
on phi elimination. Skip the others (defer to Phase B).

- *Cost:* ~1 session each (so ~2 sessions of additional Phase A work)
- *Risk:* low — addresses the two highest-value gaps
- *Benefit:* Phase B starts with stronger architectural grounding in
  the two areas most likely to cause rework.

**Option C — More thorough Phase A+ loop covering all 6 gaps.**

- *Cost:* ~5-6 sessions
- *Risk:* very low
- *Benefit:* Phase B starts with maximum information
- *Downside:* diminishing returns; feels like procrastination; many
  of these gaps are better filled contextually during Phase B

## Recommendation

**Option B.** Address the two highest-value gaps (structural recovery
via No More Gotos primary; phi elimination via SSA Book chapters)
with short focused Phase A+ loops, then move to Phase B.

Reasoning:

- The SSA substrate is the biggest architectural decision and it's
  settled. The next-biggest is the structural-recovery approach, and
  we have only one paper's coverage of it.
- Phi elimination for source emission is the decompiler-specific
  wrinkle on SSA. Standard compiler phi-elimination targets machine
  code; we need the version that targets readable source.
- The other gaps (multres, naming strategy, marsinator spot-check,
  corpus analysis) are real but better filled contextually during
  Phase B when we have concrete design questions to answer.
- Ghuloum's incremental methodology means Phase B naturally surfaces
  unknowns as we grow the decompiler. We don't need to front-load
  everything.

**But the user should make this call.** The trade-off is real:
Option A is faster; Option B is safer; Option C is overkill. The
user's risk tolerance and time horizon should decide.

## What Phase B would look like

(For context; not detailed here — that's Phase B's job.)

If we proceed, Phase B produces:

- A clean-slate architecture document (pipeline, modules, data types,
  pass structure).
- An implementation plan organized incrementally (per Ghuloum), with
  the smallest first step that produces a working decompiler for
  trivial input.
- Test strategy and corpus selection.

The incremental plan likely starts: "decompile a function that's just
`return 5`" — then `return a` — then `return a + b` — then local
variables — then if/then — etc., growing one capability at a time,
each step testable, each step producing a working decompiler.

## Calibration notes

I've been honest about gaps but I should also flag uncertainty about
my own assessment:

- I might be overestimating our readiness because we've covered the
  concepts I'm most comfortable with (SSA, dataflow). Areas I'm less
  grounded in (structural recovery, decompiler-specific emission) may
  have more depth than I'm aware of.
- I might be underestimating the value of just diving into Phase B and
  discovering gaps empirically. Implementation often teaches faster
  than reading.
- The user's risk tolerance is the deciding factor, and I don't have
  strong calibration on it. The recommendation above assumes moderate
  risk tolerance (Option B rather than A or C).
