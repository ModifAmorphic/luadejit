# Notes on Nanda & Arun-Kumar (ISEC 2016) — Decompiling Boolean Expressions from Java Bytecode

**Audience**: same as the primer and LuaJIT deep-dive. Read after both.

**Purpose**: Step 3 of Phase A. Capture the technique presented in the
one theory paper marsinator's README cites, in terms accessible to a
non-specialist, and assess its relevance to LuaJIT decompilation.

**Bibliography**:
- *Decompiling Boolean Expressions from Java™ Bytecode*, Mangala Gowri
  Nanda (IBM Research) & S. Arun-Kumar (IIT Delhi), ISEC 2016 (ACM).
- DOI: 10.1145/2856636.2856651
- PDF: https://www.cse.iitd.ac.in/~sak/reports/isec2016-paper.pdf

**Status**: paper read in full. Notes below. Caveat: the paper is
about Java bytecode, not LuaJIT. The technique is structural — it
operates on the CFG, which is largely language-agnostic — so it
should translate, but the translation requires thought. Specific
LuaJIT mapping discussed in §6.

---

## 1. The problem they're solving

Java source has `&&`, `||`, and `?:` (ternary) operators. Java bytecode
has none of these — it has only conditional and unconditional jumps.
So a simple source expression like `if (n % 9 == 0 || m % 9 == 1) {
... }` becomes a small subgraph of conditional jumps in the bytecode.

The hard observation: **the same source expression can compile to
several different but equivalent CFGs**. Their Figure 1 shows four
distinct CFGs for a simple two-clause `||` expression, depending on
which conditions the compiler chose to negate. A decompiler that
recognizes only one of these shapes will fail on the other three.

Their goal: regenerate Java source with no `goto` and no labeled
`break` statements, using `&&`/`||`/`?:` to express the recovered
boolean structure. They motivate this by their actual use case —
program slicing for debugging, where unreadable code defeats the
purpose.

This problem is *exactly* the boolean-expression recovery sub-problem
of structural analysis (Phase 4 of the canonical pipeline in the
primer). It is not the whole structural-recovery problem (it doesn't
address loop recovery, `if/else` structure, etc.) and it is not the
dataflow problem at all.

## 2. The formal setup

A few definitions they rely on:

- **CFG**: nodes are basic blocks; edges have a color attribute —
  *green* (true edge from a conditional), *red* (false edge), or
  *black* (unconditional). A conditional block has exactly one green
  and one red outgoing edge.
- **Dominator** (Def 1): node *S<sub>i</sub>* dominates *S<sub>j</sub>*
  iff every path from Entry to *S<sub>j</sub>* goes through
  *S<sub>i</sub>*.
- **Backedge** (Def 2): an edge whose destination dominates its source
  (i.e., a loop back-edge).
- **DAsG** (maximal directed-acyclic subgraph): the CFG with back-edges
  removed. The algorithm operates on DAsGs.
- **Topological order**: a DFS-based ordering of basic blocks within
  the DAsG (Algorithm 1 in the paper) — they require a specific
  invariant: at every conditional, the true-edge destination precedes
  the false-edge destination in the ordering.

The topological-order invariant matters because it makes the
"direction" of expressions deterministic during the recursive walk.

## 3. The two key theorems

### The Monochromatic Theorem (Theorem 1)

> For each basic block that participates in a boolean expression, all
> the incoming edges must be the same color.

This is the necessary condition for a subgraph to be expressible as a
single boolean expression without gotos. The intuition: if a block
has both a green (true-path) and a red (false-path) incoming edge,
then "arriving at this block" doesn't tell you anything about the
condition's outcome — you can't write `&&`/`||`/`?:` for it without
restructuring.

They prove this by induction on the structure of non-negative
conditional expressions (Lemma 1).

### The Anchor Theorem (Theorem 2)

> At the root *N<sub>root</sub>* of an expression consisting of `||`s
> and `&&`s only, let *N<sub>G</sub>* be the destination of the green
> edge and *N<sub>R</sub>* be the destination of the red edge. Either
> *N<sub>G</sub>* is the destination of multiple green edges, or
> *N<sub>R</sub>* is the destination of multiple red edges, but not
> both.

This tells you the *scope* of a boolean expression. The
highest-topological-order predecessor of the convergence point
*Z* is the **anchor** — the last node that participates in the
expression. Everything from the root to the anchor gets reduced to a
single equivalent node.

## 4. The algorithm

Two phases.

### Phase A — Make the CFG monochromatic (Algorithm 2)

For every basic block with multiple colored (true/false) incoming
edges:

1. Find a predecessor whose color matches the majority (or any marked
   predecessor's color).
2. For each other predecessor whose edge color differs, **twist** that
   predecessor: negate its condition (e.g., `<` → `>=`, `==` → `!=`)
   and swap its outgoing edge colors.
3. Mark twisted predecessors.

Repeat until every node's incoming colored edges are uniform.

The "twist" works because Java comparisons come in complementary
pairs (`==`/`!=`, `<`/>=`, `>`/`<=`). Negating the condition is just
swapping to the complement and exchanging edge colors.

### Phase B — Recognize patterns and reduce

After Phase A, walk the (now-monochromatic) DAsG in topological order
matching four patterns (their Figure 2):

| Pattern | Shape | Recovery |
|---------|-------|----------|
| (a) `c0 && c1` | c0 —green→ c1 —green→ S; c0 —red→ S; c1 —red→ S | `c0 && c1` |
| (b) `c0 \|\| c1` | c0 —green→ S; c0 —red→ c1; c1 —red→ S; c1 —green→ S | `c0 \|\| c1` |
| (c) `c0 ? c1 : c2` | c0 —green→ c1; c0 —red→ c2; both c1, c2 → S<sub>T</sub> on green and S<sub>F</sub> on red | `c0 ? c1 : c2` |
| (d) `(c0 ? i1 : i2) == val` | like (c), but the join node compares a variable assigned in c1/c2 | special-case |

**Critical ordering**: ternaries are processed *first* (they're
"stand-alone atoms" that can be part of an AND/OR chain). Then AND/OR
chains.

When a pattern is recognized, the participating nodes are collapsed
into a single equivalent node whose conditional is the recovered
expression, with the predecessor edges of the root and the successor
edges of the last node. The walk then continues with the reduced
graph.

The reduction is repeated until no more patterns match.

## 5. The Untwistable DAG (Section 6)

Some CFGs *cannot* be made monochromatic by twisting. Their Figure 5
shows an example: a node *V* with both an incoming green and an
incoming red edge, where the predecessor conditions can't be negated
to make both edges the same color.

The fix is **node duplication**: split *V* into two copies — one
receiving only green edges, one receiving only red edges — and
continue. They observe that this pattern typically arises from
common-subexpression elimination at the bytecode level, where the
same boolean sub-expression is shared between two paths.

This is the one place where the algorithm has to make a structural
change to the CFG rather than just relabeling. It's also the place
that produces the most "synthesized" output — the recovered source
may have the shared computation written twice where the original had
it once.

## 6. Relevance to LuaJIT decompilation

**Direct translation.** LuaJIT bytecode has the same fundamental
shape as Java bytecode for conditionals: ISxx instructions are
conditional jumps (test a condition, jump if true or false), JMP is
the unconditional jump. The CFG construction is mechanical and
identical in structure to the Java case.

| Java concept | LuaJIT analog |
|--------------|---------------|
| Conditional jump with true/false edges | ISxx + JMP — the JMP is taken when ISxx *fails* (a non-obvious semantic; see `docs/decompilation-patterns.md` for the negation issue) |
| Comparison op (`<`, `==`, etc.) | ISLT/ISGE/ISLE/ISGT/ISEQV/ISNEV/ISEQS/ISNES/ISEQN/ISNEN/ISEQP/ISNEP — twelve flavors, but each has its complementary pair for twist |
| Truthiness test | IST (jump if truthy) / ISF (jump if falsy) — no direct Java analog |
| Conditional assignment (ternary-like) | ISTC (`if truthy: assign and jump`) / ISFC (`if falsy: assign and jump`) — directly pattern (c) in their Figure 2 |
| Ternary expression in source | Lua has no `?:` operator; the idiom is `a and b or c`, which compiles to ISTC/ISFC chains |

**What this algorithm gives us, concretely:**

- A formal correctness criterion for boolean-expression recovery: the
  monochromatic theorem. If our recovered CFG violates it for some
  node, we know we can't emit a clean boolean expression there
  without twisting or node duplication.
- A specific algorithm for the ISxx+JMP recovery problem: identify
  the scope via the anchor theorem, walk topologically, twist as
  needed, recognize the four patterns.
- A principled handling of negation (the "twist" — exactly the
  semantic that tripped up our own pattern-matcher per the
  "wait, this seems contradictory" passage in
  `docs/decompilation-patterns.md`'s condition-recovery section).
- A recognition that ISTC/ISFC are the ternary pattern, which gives
  us `x = a and b or c` (Lua's ternary idiom) recovery for free if
  we implement the algorithm faithfully.

**What this algorithm does NOT give us:**

- Loop recovery (while/repeat/for). This is a different problem —
  structural recovery on back-edges, not boolean-expression recovery
  on DAsGs.
- `if/elseif/else` chain recovery as a unit. Each `if` is recovered
  independently; the chaining is a separate structural step.
- Data-flow analysis (Phase 5 of the canonical pipeline). Bugs 1, 2,
  and 4 from our earlier (Phase-C) bug list are not addressed by
  this paper at all.
- Variable name or type recovery. Assumes those are handled
  elsewhere.

**One important caveat**: the paper's "twist" relies on having
complementary comparisons. LuaJIT's ISxx family has them in pairs
(ISLT/ISGE, ISLE/ISGT, ISEQV/ISNEV, etc.), so twisting works. But
truthiness tests (IST/ISF) are *self-complementary* — IST is "jump if
truthy" and ISF is "jump if falsy" — so twisting an IST just replaces
it with an ISF on the same operand. That's clean.

## 7. Empirical results (for calibration)

They implemented this in a tool called JinxGo, built on IBM's WALA
analysis infrastructure. Tested on:

| Subject | Classes | Methods | Bytecode instructions |
|---------|---------|---------|-----------------------|
| Antlr | 507 | 2,582 | 103,797 |
| Xerces | 51 | 341 | 14,585 |
| Dao | 3 | 20 | 930 |
| App A (commercial) | 29 | 195 | 2,588 |
| App B (commercial) | 243 | 2,134 | 36,997 |
| App C (commercial) | 94 | 749 | 39,769 |

Findings worth noting:

- **Performance**: decompilation analysis (excluding WALA's parsing
  and pointer analysis) runs in under 1 second even on Antlr. The
  expensive part is the front-end, not the algorithm.
- **Expression complexity**: real-world Java has plenty of AND/OR
  chains (Antlr had 367, max size 19 clauses). Ternaries are rarer
  (Antlr had 14, all simple).
- **Comparison with other Java decompilers** (Table 5): Jode,
  JReversePro, Procyon, FernFlower, CFR, Soot. Most produced correct
  but "messed up" code (heavy use of labeled breaks). CFR sometimes
  produced *incorrect* code on OR variants of expressions it handled
  correctly with AND — an algorithmic bug, they suspect.

The empirical data is reassuring on two points: (a) the analysis is
fast, (b) real-world code genuinely exercises this — it's not a
purely academic problem.

## 8. Limitations and open questions

**Per the paper's own admission** (Section 6, Section 7):

- The Untwistable DAG case requires node duplication, which can
  produce recovered source that's structurally different from the
  original (computations written twice). They note this typically
  comes from CSE optimization at the bytecode level.
- They don't guarantee syntactic match to the original source —
  only functional equivalence and readability.
- Obfuscated bytecode can defeat the algorithm; they suggest
  running dead-code elimination first.

**For our LuaJIT translation**:

- We need to confirm that LuaJIT's ISxx chains are always twistable
  in practice. Java bytecode from a sane compiler is; LuaJIT
  bytecode may or may not always be. (The paper's Section 6 case
  arose manually; it's worth checking whether real LuaJIT compilers
  produce such shapes.)
- Lua's `and`/`or` ternary idiom (`x = a and b or c`) is *not*
  exactly the same as Java's `?:` — short-circuit semantics differ
  subtly when `b` can be falsy. The pattern recognition needs to
  account for this.
- The algorithm doesn't help with compound conditions across nested
  scopes — each scope's conditions are recovered independently.
- This addresses only the boolean-expression-recovery corner of
  structural analysis. Loops, early returns, `break`/`continue`,
  multi-branch `if` all need separate handling.

## 9. What this paper tells us about Phase A overall

The paper is encouraging on one front and clarifying on another:

**Encouraging**: there *is* a formal, algorithmic handle on the
boolean-expression-recovery problem. It's not just pattern-matching
hopeful guessing — there are theorems, an algorithm, and empirical
evidence it works at scale. We can stand on this.

**Clarifying**: this paper addresses *one* of the open problems.
Specifically, it's a Phase 4 (structural analysis) technique for the
boolean-expression sub-problem. It doesn't touch Phase 5 (dataflow),
which is where the harder problems live (expression recovery from
register-reuse, multres handling, scope lifetime, etc.). The
dataflow literature (Step 4 in `phase-A-plan.md`) is what addresses
*those*.

For the architecture-and-planning work in Phase B, this paper gives
us:

- A concrete algorithm for boolean-expression recovery we can either
  implement directly or use as a reference design.
- A formal correctness criterion (monochromaticity) we can test our
  output against.
- Confidence that the structural-recovery side of the decompiler has
  a foundation to stand on.

For Phase A's remaining steps (Step 4 = dataflow literature, Step 5 =
knowledge assessment), this paper sets the scope: we know structural
analysis is tractable, so the open question becomes whether dataflow
analysis on register-based bytecode is similarly tractable. That's
what Step 4 should answer.

## Sources

- Primary: Nanda & Arun-Kumar, ISEC 2016. PDF at the URL above. Local
  copy: `/tmp/opencode/step3_paper/isec2016-paper.pdf`. Extracted
  text: `/tmp/opencode/step3_paper/isec2016-paper.txt`.
- Cross-references in this project:
  - `decompilation-primer.md` — Phase 4 (structural analysis)
    context.
  - `luajit-deep-dive.md` §2.4 — the ISxx instruction class.
  - The current project's `docs/decompilation-patterns.md` — has a
    candid "wait, this seems contradictory, let me re-examine"
    passage on ISxx+JMP semantics that this paper's "twist" operation
    formalizes cleanly. (Phase C reference, not Phase A.)
