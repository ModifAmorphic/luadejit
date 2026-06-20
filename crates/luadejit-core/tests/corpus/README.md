# Regression corpus subset

This directory holds lists of source files selected from the Darktide
Lua corpus to serve as regression inputs for later pipeline stages
(CFG → SSA → structural recovery → emit).

## `v1.txt`

49 file paths (one per line), relative to the Darktide corpus root at
`~/repos/ModifAmorphic/Darktide-Magos/extracted/`. Selected per the
corpus analysis methodology documented in
`docs/corpus-analysis.md`: a stratified sample spanning handlers,
views, utilities, and generated dialogue tables, chosen to exercise
the control-flow and expression shapes the decompiler must handle.

The actual `.lua` / compiled bytecode files are **not** checked into
this repo (they are external to the project). Tests that consume the
corpus resolve each listed path under the corpus root and skip
gracefully when the files are absent, so CI on a clean checkout runs
without the corpus while local development can opt in by cloning the
corpus repo.
