# Regression corpus subset

This directory holds lists of source files selected from the Darktide
Lua corpus to serve as regression inputs for later pipeline stages
(CFG → SSA → structural recovery → emit).

## `v1.txt`

49 file paths (one per line), relative to the Darktide corpus root at
`~/repos/ModifAmorphic/sandbox/extract-decompile/extracted/`. Selected
per the corpus analysis methodology documented in
`docs/corpus-analysis.md`: a stratified sample spanning handlers,
views, utilities, and generated dialogue tables, chosen to exercise
the control-flow and expression shapes the decompiler must handle.

The actual `.lua` / compiled bytecode files are **not** checked into
this repo (they are external to the project). Tests that consume the
corpus resolve each listed path under the corpus root and skip
gracefully when the files are absent, so CI on a clean checkout runs
without the corpus while local development can opt in by cloning the
corpus repo.

## Regression runner

`tests/corpus_regression.rs` walks `v1.txt`, decompiles each file
with `luadejit_core::decompile`, and prints a one-line outcome per
file plus a summary count (success / NotImplemented /
InvalidBytecode / panic / missing). Panics are caught with
`catch_unwind` so one bad file doesn't abort the run.

The test is `#[ignore]` and always passes — it's a reporting tool
for tracking Stage-by-stage progress on real bytecode, not a gate.
Run it explicitly:

```
cargo test --test corpus_regression -- --ignored
```

Skips automatically (with an `eprintln`) when the corpus root is
absent, so the same command is safe to run anywhere.
