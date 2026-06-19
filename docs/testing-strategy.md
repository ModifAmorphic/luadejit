# Architecture v2 — Testing Strategy

**Audience**: an experienced developer who isn't a Rust/decompiler
specialist, and any future contributor.

**Purpose**: Phase B Step 4 (final Phase B doc). Systematize the
per-stage testing referenced in `implementation-plan.md`. Resolves
Q6 (test corpus v1 selection) from `architecture-v2.md` Step 1.

**Status**: Phase B Step 4 of 4. After this, Phase B is complete
and Stage 0 of implementation can begin.

---

## 1. Testing layers

Four layers, each with a different purpose:

| Layer | Purpose | Speed | Run frequency |
|-------|---------|-------|---------------|
| Unit tests | Test individual functions/modules in isolation | Fast (seconds) | Every commit |
| Integration tests | Test multi-module interactions via the public API | Fast (seconds) | Every commit |
| Snapshot tests | Catch unintended changes to decompiler output | Fast (seconds) | Every commit |
| Corpus regression | Catch crashes/hangs and gross regressions on real-world code | Slow (minutes) | Periodically + before release |

The first three are the **development loop** — they run continuously
during implementation, give immediate feedback, and gate every
commit. The fourth is the **safety net** — it catches things the
focused tests can't.

## 2. Unit tests

**What they test**: individual functions and modules in isolation.
For example: `cfg::compute_dominator_tree` given a known CFG should
produce a known dominator tree. `ssa::insert_phis` given a known
CFG + variable assignment set should produce a known phi layout.

**Where they live**: inline `#[cfg(test)]` modules in each source
file. Standard Rust convention.

```rust
// in crates/luadejit-core/src/cfg/mod.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dominator_tree_for_simple_if_then() {
        let cfg = build_test_cfg_with_if_then();
        let dom_tree = compute_dominator_tree(&cfg);
        assert_eq!(dom_tree.immediate_doms[3], Some(BlockId(0)));
    }
}
```

**What `#[cfg(test)]` means** (Rust): this code only compiles when
running tests (`cargo test`), not in normal builds. Keeps test code
next to the code it tests without bloating production binaries.

**When to write them**: for any function with non-trivial logic
where a unit test would have caught a regression. The Cytron SSA
construction algorithm is a prime candidate — its correctness is
load-bearing and unit tests can verify specific phi placements.

**When NOT to write them**: for trivial wrappers, getters, or
plumbing. Unit tests should test behavior, not lines of code.

## 3. Integration tests

**What they test**: multi-module interactions via the public API of
`luadejit-core`. Each test takes bytecode bytes as input and asserts
on the emitted Lua source.

**Where they live**: `crates/luadejit-core/tests/` directory. Each
file is a separate integration test binary.

```rust
// in crates/luadejit-core/tests/stage01_return.rs
use luadejit_core::decompile;

#[test]
fn empty_chunk_emits_empty_source() {
    let bytecode = compile_lua("");  // helper
    let result = decompile(&bytecode).unwrap();
    assert_eq!(result.trim(), "");
}
```

**Organization**: one integration test file per implementation
stage. `stage01_return.rs`, `stage02_constants.rs`,
`stage03_locals.rs`, etc. This mirrors the implementation plan and
makes it easy to see which stages are passing.

**Fixture loading helper**: a shared module (in `tests/common/` or
similar) provides:
- `compile_lua(source: &str) -> Vec<u8>` — shells out to `luajit -b`
  to produce bytecode from source.
- `load_fixture(name: &str) -> (Vec<u8>, String)` — loads a paired
  `.source.lua` + `.bc` fixture from `tests/fixtures/`.

## 4. Snapshot tests

**What they test**: that the decompiler's output for a given input
doesn't change unintentionally.

**How they work**:
1. Each test case has a representative input.
2. The first run captures the decompiler's output as a "snapshot"
   and saves it.
3. Subsequent runs compare actual output to the saved snapshot.
4. If they differ, the test fails. If the difference is intentional
   (e.g., we improved copy propagation), the developer updates the
   snapshot explicitly.

**Infrastructure**: I lean toward a simple custom implementation
rather than a library like `insta`. Reasons:
- We control behavior completely.
- No external dependency.
- Snapshot diffing is ~50 lines of code.
- The format is exactly what we want.

A simple version:

```rust
// in tests/common/snapshot.rs
pub fn check_snapshot(name: &str, actual: &str) {
    let snapshot_path = format!("tests/snapshots/{}", name);
    let expected = match std::fs::read_to_string(&snapshot_path) {
        Ok(s) => s,
        Err(_) => {
            // First run — create snapshot
            std::fs::write(&snapshot_path, actual).unwrap();
            panic!("snapshot created; re-run to verify");
        }
    };
    assert_eq!(actual, &expected, "snapshot mismatch for {}", name);
}
```

To update snapshots deliberately, set an env var
(`UPDATE_SNAPSHOTS=1`) and re-run. The helper checks the env var and
overwrites instead of comparing.

**What goes in snapshots**: the decompiler's emitted source for
representative inputs. Each snapshot file is named after the test
case. Snapshots are committed to git and reviewed in PRs.

## 5. Corpus regression testing

**What it tests**: that the decompiler doesn't crash, hang, or
produce grossly broken output on a large corpus of real-world
bytecode.

**The corpus**: the Darktide corpus (~9,600 files, external to this
repo, at `~/repos/ModifAmorphic/Darktide-Magos/extracted/`). Same
corpus the current attempt validates against.

**Test runner**: a separate binary or `tests/corpus.rs` that:
1. Iterates every `.lua` file in the corpus.
2. Runs the decompiler with a per-file timeout (say, 10 seconds).
3. Records: success, timeout, panic, error.
4. For successful runs: checks output invariants (§7).
5. Produces a summary report.

**Failure handling**:
- A file that crashes/panics: logged as a regression. The full
  suite continues (one file's panic doesn't stop the run).
- A file that times out: logged as a hang. Same.
- A file that violates an invariant: logged. Same.

**Frequency**: not run on every commit (too slow). Run before
merging major changes, before releases, and periodically as a
regression check.

**Note**: corpus regression does NOT check that decompiler output is
"correct" — it checks that output is *valid Lua* and doesn't
regress on invariants. Correctness is the snapshot tests' job on
representative inputs.

## 6. Test corpus v1 selection (resolves Q6)

**Problem**: running the full 9,600-file corpus takes too long for
tight dev loops. Need a smaller v1 subset.

**Selection criteria**:
- **Size**: 50-100 files. Runs in under a minute.
- **Coverage**: exercises every capability in scope for v1 (control
  flow, loops, calls, tables, closures, multres).
- **Edge cases**: includes known-tricky files (closures with deep
  nesting, tables with mixed array/hash parts, functions with many
  locals, etc.).
- **Stability**: from the Darktide corpus (which is fixed, not
  changing under us).

**Selection methodology** (Step 4 design; actual selection happens
during Stage 0 or early Stage 1):

1. Run a corpus analysis probe over the full Darktide corpus that
   categorizes each file by features present:
   - Has closures (FNEW > 0)
   - Has loops (FORI/ISNEXT/LOOP > 0)
   - Has tables (TNEW/TDUP > 0)
   - Has multres (CALLM/VARG/RETM with multres operands)
   - Has conditionals (ISxx > 0)
   - File size bucket (small / medium / large)
2. Sample N files from each category, balanced.
3. Manually curate to include known-tricky cases.
4. Commit the v1 subset list (file paths) to the repo. The actual
   bytecode files stay external (they're not in this repo).

**Result**: a `tests/corpus/v1.txt` file listing the selected file
paths (relative to the Darktide corpus root). The corpus runner
reads this list and runs against just those files in v1 mode.

## 7. Invariants checked by corpus regression

Each successful decompilation is checked against these invariants:

| Invariant | What it catches |
|-----------|-----------------|
| Output parses as valid Lua (via `luajit -bl`) | Emitter producing syntactically broken Lua |
| No `var_N` names when debug info present | Name-resolution failures |
| No `-- TODO:` markers in output | Unsupported constructs that should have been handled |
| No "unsupported opcode" warnings | Frontend gaps |
| Output size within bounds (e.g., < 10x input size) | Runaway emission (infinite loops in structuring, etc.) |
| No empty function bodies (unless input is truly empty) | Pipeline dropping instructions |
| All input opcodes accounted for | Silent instruction skipping |

Each invariant failure is reported with the file path and the
specific failure. Aggregate counts per invariant type.

**What's NOT checked** (intentionally):
- Whether the output is *semantically equivalent* to the input.
  That would require running both and comparing behavior, which is
  out of scope for v1.
- Whether the output is "readable" or matches a hypothetical
  original source. Subjective; left to human review of snapshots.

## 8. Test running and CI

**Local dev**:
- `cargo test` — runs unit + integration + snapshot tests. Seconds.
- `cargo test --test corpus -- --v1` — runs the v1 corpus subset. Under a minute.
- `cargo test --test corpus` — runs the full corpus. Minutes.
  Manual, periodic.

**CI** (when we set it up):
- On every PR: `cargo test` (must pass).
- On every PR: `cargo test --test corpus -- --v1` (must not regress
  pass count or invariant counts).
- Nightly: `cargo test --test corpus` (full run, full corpus).
- Before release: manual full-corpus run + snapshot review.

**Snapshot updates in CI**: if a snapshot test fails on CI, that's a
regression — CI doesn't auto-update. Developer updates locally with
`UPDATE_SNAPSHOTS=1`, verifies the diff is intentional, commits the
updated snapshot.

## 9. How tests evolve with the implementation plan

Each stage of the implementation plan adds tests:

| Stage | Tests added |
|-------|-------------|
| Stage 0 (skeleton) | CI runs `cargo build` and `cargo test` (both no-op) |
| Stage 1 (return) | First integration test (empty input); first snapshot |
| Stage 2 (constants) | Integration test per constant type; snapshot |
| Stage 3 (locals) | Integration test for locals; snapshot |
| Stages 4-6 | Per-stage integration tests; snapshots |
| Stage 7 (if/then) | First real CFG/SSA/structural-recovery unit tests; integration; snapshot |
| Stages 8-17 | Per-stage integration tests; snapshots; expansion of corpus v1 |

The first corpus regression run happens at the end of Stage 7 (first
stage with all pipeline pieces exercised). Before that, the corpus
regression is meaningless (the decompiler can't handle real code).

**Test coverage expectations**: every public function in `luadejit-
core` has at least one integration test exercising it. Internal
helpers get unit tests when their logic is non-trivial. We don't
try to hit a coverage percentage; we try to catch real regressions.

## 10. Calibration and open questions

**Confidence**:
- Test layers (unit/integration/snapshot/corpus): high confidence.
  Standard testing patterns adapted to our pipeline.
- Snapshot infrastructure (custom, not library): medium confidence.
  `insta` would be more featureful; we'd be reinventing a small
  wheel. Counter: keeping the implementation simple means we fully
  understand it.
- Corpus v1 selection methodology: medium confidence. The
  categorization approach is sound; the specific categories and
  sampling sizes will need tuning once we see what the corpus
  actually contains.

**Open questions** (resolve during implementation):

1. **Corpus analysis probe**: what specific features to categorize
   on, and what the actual distribution in the Darktide corpus is.
   Need to run a probe and see.

2. **FR2 fixture compilation**: we need FR2-mode bytecode fixtures
   for testing. System `luajit` produces FR2 by default; we may
   need to explicitly produce FR1 fixtures too (older LuaJIT or
   flag-based).

3. **Snapshot format**: plain Lua source? Annotated with comments
   showing the input? Lean toward plain source for simplicity.

4. **When to introduce CI**: Stage 0 sets up cargo workspace; CI
   could come then or be deferred until the decompiler does
   something. Design judgment; lean toward setting up CI in
   Stage 0 since it's cheaper to do early.

5. **`luajit -bl` as Lua parser for invariant checking**: this
   shells out to system `luajit`. Will need luajit as a build
   dependency. Acceptable for testing but worth noting.

## 11. Phase B complete

This is the final Phase B doc. Phase B has produced:

- `docs/architecture-v2.md` — pipeline, modules, IR choice, error
  handling, v1 scope (Step 1).
- `docs/architecture-v2-data-structures.md` — concrete Rust types,
  module interfaces, resolves Step 1's open questions (Step 2).
- `docs/implementation-plan.md` — 17 stages of incremental growth
  (Step 3).
- `docs/testing-strategy.md` — this doc (Step 4).

Phase B is complete. The next step is Phase B's actual output:
**start implementing Stage 0** of the plan, using the testing
infrastructure described here.

## 12. What's next

Implementation. Stage 0: stand up the cargo workspace, empty
modules, stub pipeline. First commit on the new architecture.

After Stage 0, follow the implementation plan stage by stage.

Phase C (comparison against current attempt) remains deferred until
we have enough of the clean-slate implementation to compare
meaningfully. Realistically that's after Stage 7 or 8 of the plan,
when the clean-slate decompiler can handle non-trivial input.
