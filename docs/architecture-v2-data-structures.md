# Architecture v2 — Data Structures and Interfaces

**Audience**: an experienced developer who isn't a Rust or
decompiler specialist, and any future contributor. Rust-specific
concepts (ownership, lifetimes, traits) get brief explanations where
they appear.

**Purpose**: Phase B Step 2. Resolve the seven open architectural
questions from `architecture-v2.md` §8. Define concrete Rust types
for each module's inputs and outputs. Define the public interface
(function signatures) each module exposes.

**Status**: Phase B Step 2 of 4. Builds on `architecture-v2.md`
(Step 1). Phase B Step 3 will be the incremental implementation
plan; Step 4 will be the testing strategy.

**Calibration**: this doc has design judgments, not just factual
claims. Where I'm making a judgment call, I say so explicitly.
Where I'm asserting something empirical, I either source it or flag
it as unverified.

---

## 1. Resolving the seven open questions

### Q1: SSA name representation → newtyped integer + side table

**Decision**: `SsaName` is a newtype around `u32`. Metadata about
each SSA name (defining instruction, debug-info linkage, type hints)
lives in a side table owned by `SsaFunction`.

```rust
#[derive(Copy, Clone,Eq, PartialEq, Hash, Debug)]
pub struct SsaName(pub u32);
```

**Reasoning**:
- SSA names appear everywhere in the IR — in every instruction, every
  φ-function, every analysis fact. They need to be cheap to copy and
  compare. `u32` (4 bytes, copy-semantic) is the right primitive.
- The newtype wrapper (`struct SsaName(u32)`) gives us type safety:
  we can't accidentally pass a register number where an SSA name is
  expected, or vice versa.
- Metadata that's looked up less frequently (def location, debug-info
  mapping, etc.) goes in a side table. This keeps the hot path (the
  SSA values themselves) compact.

**What `Derives` means** (Rust): `#[derive(...)]` tells the compiler
to auto-implement those traits. `Copy` means the value is bit-wise
copyable (no ownership transfer). `Clone` is the explicit-copy
version. `Eq`/`PartialEq` enable equality comparison. `Hash` enables
use as a `HashMap` key. `Debug` enables pretty-printing.

### Q2: AST vs. CFG-annotated → AST

**Decision**: structural recovery produces a brand-new `Ast` data
structure. The CFG is consumed (read-only) by structural recovery;
the AST is what emission walks.

**Reasoning**:
- ASTs are the conventional output of structural recovery. Both
  papers we read in Phase A (isec2016, No More Gotos) produce ASTs.
- ASTs naturally represent nesting (an `if` inside a `while` is a
  tree, not a graph). CFGs represent flat control flow.
- Emission walks a tree cleanly (recursive descent). Emission from a
  CFG-annotated-with-structure would require reconstructing the
  nesting on the fly.
- The transform is one-way (CFG → AST). We don't need to maintain
  consistency between the two after structural recovery runs.

**Trade-off acknowledged**: the AST duplicates some info from the
CFG (specifically: which basic blocks participate in which
constructs). That's acceptable because the duplication is one-way
and we discard the CFG after structural recovery (in memory terms:
the CFG can be dropped once the AST is built).

### Q3: Debug-info ↔ SSA-name mapping → bidirectional maps

**Decision**: `DebugInfo` lookup is bidirectional. Forward map
(`SsaName → DebugVarId`) is for emission ("what source name should
this SSA value get?"). Reverse map (`DebugVarId → Vec<SsaName>`)
is for analyses that need to know "which SSA values correspond to
this source variable?"

```rust
pub struct DebugMapping {
    ssa_to_debug: HashMap<SsaName, DebugVarId>,
    debug_to_ssa: HashMap<DebugVarId, Vec<SsaName>>,
    debug_names: HashMap<DebugVarId, String>,
}

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct DebugVarId(pub u32);
```

**Reasoning**:
- The forward direction is the common case at emission time.
- The reverse direction is needed for things like "is this source
  variable still live anywhere?" (a query that some transformations
  may want).
- The "one debug var → many SSA names" case is the *common* case
  (a source-level variable reassigned multiple times becomes
  multiple SSA names). The reverse map handles this naturally.
- The "many debug vars → one SSA name" case — flagged in Phase A+
  Loop 2 as "frequency unknown" — would manifest as collisions in
  the forward map. We need to decide on a policy: first-write-wins,
  last-write-wins, or error. **Open question for Step 4**: which
  policy is right, validated by corpus analysis.

**What still needs investigation**: the actual frequency of the
"many debug vars → one SSA name" case in real LuaJIT bytecode. We
flagged this in the v2 architecture audit; it stays flagged here.

### Q4: Multres representation in SSA → dual representation

**Decision**: most values are single SSA names. Multres values
(results of a CALL/CALLM with C=0, or VARG with B=0) get a special
`SsaValue::Multres` variant that carries either a known count or
`MultresCount::Dynamic`.

```rust
pub enum SsaValue {
    Single { def: InstructionId },
    Multres {
        def: InstructionId,
        count: MultresCount,
    },
}

pub enum MultresCount {
    Known(u32),    // e.g., local a, b, c = f() → Known(3)
    Dynamic,       // e.g., return f(...) → Dynamic
}
```

**Reasoning**:
- For the common case (`local a, b, c = f()`), the count is
  statically determinable from the consumer's operand. We can
  materialize three SSA names — `f_result_0`, `f_result_1`,
  `f_result_2` — and use them directly.
- For the dynamic case (`return f(...)`, `print(f())`), the count
  isn't known statically. We represent the multres as a single
  `Multres` value with `Dynamic` count, and consumers reference it
  opaquely.
- This is a **synthesis decision**, not a standard technique — I
  haven't found a textbook treatment of multres-in-SSA. The dual
  representation is the simplest thing I can see that handles both
  cases without forcing every value to be a potential tuple.

**Open question for Step 4**: validate this design by enumerating
the actual multres patterns in the corpus. If `Dynamic` is rare
(my intuition but unverified — flagging per sourcing discipline),
we might be able to use `Known` exclusively and emit a TODO comment
for the dynamic case.

### Q5: Closure / upvalue representation → per-proto SSA + upvalue refs

**Decision**: SSA is per-proto. Cross-function captures use a
separate `UpvalueRef` type, resolved at emission via the bytecode's
upvalue descriptor table.

```rust
pub enum ValueRef {
    Local(SsaName),          // SSA value within current proto
    Upvalue(UpvalueIndex),   // reference to parent scope
}

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct UpvalueIndex(pub u32);
```

The upvalue descriptors (from the bytecode format spec, §7 of
`docs/luajit-bytecode-format.md`) tell us whether each upvalue is
*open* (a register in the parent) or *closed* (an upvalue of the
parent). Emission walks these descriptors to name upvalues
correctly.

**Reasoning**:
- Cross-procedural SSA is overkill for our needs. We're not
  optimizing across function boundaries; we just need to emit the
  right source-level name for each upvalue.
- The bytecode format already has the upvalue descriptor table.
  Using it directly is simpler than inventing a cross-procedural IR.
- This matches the approach the current attempt takes (and which
  works for it); the change is using SSA within each proto, not
  cross-procedurally.

### Q6: Test corpus selection → deferred to Step 4

This isn't a data-structures question. Step 4 will address it.

### Q7: Library vs. hand-roll → hand-roll for v1

**Decision**: hand-roll dominator-tree computation and SSA
construction. Don't use Cranelift or other IR libraries for v1.

**Reasoning**:
- The Cytron algorithm is ~200-400 lines of Rust (per Phase A
  Step 4 addendum). Dominator tree is another ~100-200. Manageable.
- Hand-rolling gives complete understanding, which is part of the
  project's goal.
- Cranelift's `cranelift-frontend` is designed for compiler IR with
  optimization in mind; we don't need most of its features.
- We can swap in a library later if hand-rolling turns out to be a
  time sink.

**Risk acknowledged**: if SSA construction has subtle bugs, we own
them. Mitigation: test the construction algorithm explicitly with
 fixtures where we know the correct SSA form.

## 2. Core data types

Module-by-module. Showing the public types only; private helpers
stay private.

### `ir` module (shared types)

```rust
/// SSA name — the fundamental value identifier in the IR.
/// Cheap to copy (4 bytes), type-safe via the newtype wrapper.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct SsaName(pub u32);

/// Identifier for a basic block in a CFG.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct BlockId(pub u32);

/// Identifier for an instruction (original bytecode index).
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct InstructionId(pub u32);

/// Identifier for a debug-info variable (from var_info records).
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct DebugVarId(pub u32);

/// Upvalue index within a proto's upvalue descriptor table.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct UpvalueIndex(pub u32);
```

### `frontend` module

Reuses types similar to the current attempt's reader (the bytecode
format hasn't changed):

```rust
pub struct Module {
    pub header: ModuleHeader,
    pub protos: Vec<Proto>,    // children-first post-order; main chunk last
}

pub struct Proto {
    pub flags: u8,
    pub numparams: u8,
    pub framesize: u8,
    pub upvalues: Vec<UpvalDesc>,
    pub gc_consts: Vec<GcConst>,
    pub num_consts: Vec<NumConst>,
    pub insts: Vec<Instruction>,
    pub debug: Option<DebugInfo>,
}

// (other reader types: ModuleHeader, Instruction, Opcode, GcConst,
// NumConst, DebugInfo, VarInfo, UpvalDesc — as in current attempt)
```

Public interface:
```rust
impl Module {
    pub fn from_bytes(bytes: &[u8]) -> Result<Module, ReaderError>;
}
```

### `cfg` module

```rust
pub struct Cfg {
    pub blocks: Vec<BasicBlock>,
    pub entry: BlockId,
    pub dominator_tree: DominatorTree,
}

pub struct BasicBlock {
    pub id: BlockId,
    pub insts: Vec<InstructionId>,
    pub terminator: Terminator,
    pub preds: Vec<BlockId>,
    pub succs: Vec<(BlockId, EdgeKind)>,
}

pub enum Terminator {
    Fallthrough(BlockId),
    Jump(BlockId),
    ConditionalBranch {
        condition: InstructionId,  // the ISxx instruction
        true_edge: BlockId,
        false_edge: BlockId,
    },
    Return,
    TailCall(InstructionId),
}

#[derive(Copy, Clone, Debug)]
pub enum EdgeKind {
    True,
    False,
    Unconditional,
}

pub struct DominatorTree {
    // parent in dominator tree, indexed by BlockId
    pub immediate_doms: Vec<Option<BlockId>>,
}
```

Public interface:
```rust
impl Cfg {
    pub fn build(proto: &Proto) -> Cfg;
}
```

### `ssa` module

```rust
pub struct SsaFunction {
    pub cfg: Cfg,                          // owns the CFG after construction
    pub values: Vec<SsaValue>,             // indexed by SsaName
    pub phis: HashMap<BlockId, Vec<Phi>>,  // φ-functions per block
    pub debug_mapping: DebugMapping,
}

pub enum SsaValue {
    Single { def: InstructionId },
    Multres { def: InstructionId, count: MultresCount },
}

pub enum MultresCount {
    Known(u32),
    Dynamic,
}

pub struct Phi {
    pub target: SsaName,
    pub inputs: Vec<(BlockId, SsaName)>,   // (predecessor block, value)
}

pub struct DebugMapping {
    pub ssa_to_debug: HashMap<SsaName, DebugVarId>,
    pub debug_to_ssa: HashMap<DebugVarId, Vec<SsaName>>,
    pub debug_names: HashMap<DebugVarId, String>,
}
```

Public interface:
```rust
impl SsaFunction {
    pub fn build(cfg: Cfg, proto: &Proto) -> SsaFunction;
    pub fn use_def_chain(&self, name: SsaName) -> Option<&SsaValue>;
    pub fn def_use_chain(&self, name: SsaName) -> impl Iterator<Item = InstructionId> + '_;
}
```

**What `impl Iterator<Item = ...> + '_` means** (Rust): the
function returns something that implements the `Iterator` trait,
yielding items of type `InstructionId`. The `'_` is a lifetime —
the iterator borrows from `self` for as long as it lives. This is
Rust's way of saying "an iterator over data I own."

### `analysis` module

```rust
pub struct AnalysisFacts {
    pub liveness: LivenessFacts,
    // reaching definitions are free under SSA — no separate fact needed
    pub available_expressions: AvailableExprFacts,
}

pub struct LivenessFacts {
    // for each block, the set of SSA names live at entry/exit
    pub live_in: HashMap<BlockId, HashSet<SsaName>>,
    pub live_out: HashMap<BlockId, HashSet<SsaName>>,
}

pub struct AvailableExprFacts {
    // for each block, expressions available at entry
    pub available_in: HashMap<BlockId, HashSet<ExprKey>>,
}

// ExprKey: a canonicalized form of an expression for equality
// checking. Implementation TBD; likely a hash of (op, operands).
pub struct ExprKey(u64);
```

Public interface:
```rust
impl AnalysisFacts {
    pub fn compute(ssa: &SsaFunction) -> AnalysisFacts;
    pub fn is_live_at(&self, name: SsaName, block: BlockId) -> bool;
}
```

### `transform` module

Transforms mutate `SsaFunction` in place. Each transform is a
function (not a method on SsaFunction — keeps them modular):

```rust
pub fn copy_propagate(ssa: &mut SsaFunction, facts: &AnalysisFacts);
pub fn constant_propagate(ssa: &mut SsaFunction, facts: &AnalysisFacts);
pub fn eliminate_dead_code(ssa: &mut SsaFunction, facts: &AnalysisFacts);
pub fn local_value_number(ssa: &mut SsaFunction);
```

### `structure` module

```rust
pub struct Ast {
    pub root: Vec<Statement>,
}

pub enum Statement {
    LocalAssign {
        names: Vec<String>,
        values: Vec<Expr>,
    },
    Assign {
        targets: Vec<Expr>,
        values: Vec<Expr>,
    },
    Call(Expr),
    Return(Vec<Expr>),
    If {
        branches: Vec<(Expr, Vec<Statement>)>,  // (condition, body)
        else_body: Option<Vec<Statement>>,
    },
    While {
        condition: Expr,
        body: Vec<Statement>,
    },
    Repeat {
        body: Vec<Statement>,
        condition: Expr,
    },
    ForNumeric {
        var: String,
        start: Expr,
        stop: Expr,
        step: Option<Expr>,
        body: Vec<Statement>,
    },
    ForGeneric {
        vars: Vec<String>,
        iterators: Vec<Expr>,
        body: Vec<Statement>,
    },
    Break,
}

pub enum Expr {
    Number(f64),
    Str(String),
    Bool(bool),
    Nil,
    Var(String),
    BinOp { left: Box<Expr>, op: BinOpKind, right: Box<Expr> },
    UnOp { op: UnOpKind, operand: Box<Expr> },
    Index { table: Box<Expr>, key: Box<Expr> },
    Field { table: Box<Expr>, field: String },
    Call { func: Box<Expr>, args: Vec<Expr>, method: Option<String> },
    Table(Vec<TableField>),
    Function(Box<FunctionExpr>),
    Vararg,
}

// (BinOpKind, UnOpKind, TableField, FunctionExpr — straightforward)
```

Public interface:
```rust
impl Ast {
    pub fn build(cfg: &Cfg, ssa: &SsaFunction) -> Ast;
}
```

**Design note**: the AST mirrors Lua source structure. Emission
walks it recursively. The AST carries SSA names via `Expr::Var` (the
string is the resolved source name post-phi-elimination) or via
debug-info lookup if names haven't been resolved yet.

### `emit` module

```rust
pub fn emit_module(module: &Module, asts: &[Ast]) -> String;
pub fn emit_proto(ast: &Ast, debug_mapping: &DebugMapping) -> String;

// Phi elimination (Step 7 of pipeline)
pub fn eliminate_phis(ast: &mut Ast, ssa: &SsaFunction);
```

The emit module exposes functions, not methods — emission is a
transform from AST to string.

## 3. Module interfaces summary

The pipeline orchestrator (in `lib.rs`) calls these in order:

```rust
pub fn decompile(bytes: &[u8]) -> Result<String, DecompilerError> {
    let module = Module::from_bytes(bytes)?;

    let mut asts = Vec::with_capacity(module.protos.len());
    for proto in &module.protos {
        let cfg = Cfg::build(proto);
        let mut ssa = SsaFunction::build(cfg, proto);
        let facts = AnalysisFacts::compute(&ssa);

        transform::copy_propagate(&mut ssa, &facts);
        transform::constant_propagate(&mut ssa, &facts);
        transform::eliminate_dead_code(&mut ssa, &facts);
        transform::local_value_number(&mut ssa);

        let mut ast = Ast::build(&ssa.cfg, &ssa);
        emit::eliminate_phis(&mut ast, &ssa);

        asts.push(ast);
    }

    Ok(emit::emit_module(&module, &asts))
}
```

Each step takes the previous step's output. Borrows are transient.
No module reaches into another's internals.

## 4. Ownership and lifetimes

For the non-Rust reader: Rust's ownership system tracks at compile
time who "owns" each piece of data and how long borrows last. Quick
rules:

- Each value has exactly one owner; when the owner goes out of
  scope, the value is dropped.
- You can have either one mutable borrow (`&mut`) or any number of
  immutable borrows (`&`) at a time, but not both.
- Lifetimes (`'a`) track how long borrows are valid.

For our pipeline:

- `Module` is owned by `decompile`. It lives for the whole call.
- `Cfg` is owned by `SsaFunction` (passed in by value during
  construction). When SSA construction is done, the SSA function
  owns the CFG.
- `SsaFunction` is owned by the loop iteration. After `Ast::build`,
  it's still borrowed for `emit::eliminate_phis`.
- `Ast` is owned by `asts: Vec<Ast>`.
- `AnalysisFacts` is owned by the loop iteration; lives only as
  long as the transforms need it.

This means: no `Rc`/`Arc` (reference counting) needed for the IR
itself. Each value has a clear owner. Borrows are short.

**Design judgment**: I'm choosing clear single-ownership over
shared ownership. This makes the code easier to reason about. If we
hit a case where shared ownership is genuinely needed (e.g., a
value referenced from multiple places that all outlive each other),
we can introduce `Rc` at that point. Don't speculatively add it.

## 5. What Step 2 surfaced but didn't resolve

Step 2 produced new open questions to address in Step 3 or Step 4:

1. **`ExprKey` representation** for available-expressions analysis.
   Hash-based? Structural? Needs design when we implement
   `available_expressions`.

2. **Policy for "many debug vars → one SSA name" collisions**
   (Phase A+ Loop 2 / Q3 above). First-write-wins, last-write-wins,
   or error? Step 4 with corpus analysis.

3. **Frequency of dynamic-count multres** (Q4 above). My intuition:
   rare. Needs corpus validation. Step 4.

4. **AST ↔ SSA name threading**. The AST carries source-level names
   post-phi-elimination, but during structural recovery, AST
   expressions reference SSA names. Do we have two AST flavors
   (SSA-name-bearing and source-name-bearing), or one AST with a
   name-resolution phase? Design judgment; defer to Step 3.

5. **Recursion depth limits**. The AST can be deeply nested
   (Lua programs can have arbitrarily nested control flow). Rust's
   default stack may overflow on pathological inputs. Need either
   explicit iteration or a stack-size guard. Step 3 question.

## 6. Calibration

Confidence levels on the design decisions in this doc:

| Decision | Confidence | Notes |
|----------|------------|-------|
| Q1: SsaName as newtype u32 | **High** | Standard pattern; well-justified by performance needs |
| Q2: AST over CFG-annotated | **High** | Convention from both papers we read |
| Q3: Bidirectional debug mapping | **Medium-high** | Structure is right; collision policy unresolved |
| Q4: Multres dual representation | **Medium** | Synthesis on my part; no textbook treatment found. Corpus validation needed |
| Q5: Per-proto SSA + UpvalueRef | **High** | Matches bytecode format; standard approach |
| Q7: Hand-roll SSA | **Medium** | Right call for learning; could be wrong call if time becomes critical |

The two medium-confidence decisions (Q4, Q7) are the ones most
likely to need revision during implementation. Both have fallbacks
(Q4 → simpler representation if Dynamic is rare; Q7 → use library).

## 7. Next step

Phase B Step 3: incremental implementation plan. Concrete order of
implementation, organized per Ghuloum's methodology. Each step
produces a working decompiler for some subset of inputs. The open
questions in §5 get addressed as they come up in the plan.

Same review request as before: I'd like your review on this before
Step 3. Specifically:

- Do the type definitions look right? Anything obviously missing
  or wrong?
- The pipeline orchestrator in §3 — does the data flow make sense?
- The medium-confidence items in §6 (Q4 multres, Q7 hand-roll) —
  are you comfortable proceeding with both as proposed, or do you
  want to revisit either?
