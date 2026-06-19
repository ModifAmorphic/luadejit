# luadejit

A LuaJIT 2.x bytecode decompiler written in Rust.

Decompiles LuaJIT bytecode files back into readable Lua source code. Validated against 9,648 real-world bytecode files without crashes or hangs.

## Features

- **Table literal reconstruction**: TDUP templates produce full `{ key = value, ... }` literals with TNEW + TSET* merging
- **Inline function definitions**: FNEW closures render as `function(args) body end` with correct child-proto mapping
- **Method call detection**: `obj:method(args)` syntax when first arg matches the object via structural equality
- **Dot notation**: `self.field` instead of `self["field"]` for valid Lua identifiers
- **Expression folding**: Multi-instruction sequences resolve inline via HashMap tracking (e.g., `Managers.event:trigger(...)`)
- **Condition resolution**: ISxx + JMP patterns recovered as boolean expressions with proper negation semantics
- **Local deduplication**: Duplicate `local x = ...` declarations collapsed to plain assignments; dead pure assignments removed
- **Variable naming from debug info**: Local variables resolved to actual names where available, including loop variable name resolution via __for* marker handling
- **Numeric-for loop reconstruction**: FORI loops with correct stack layout handling and setup instruction suppression
- **Generic-for loop reconstruction**: ISNEXT loops with iterator triplet collapse and correct ITERL matching by register base
- **Non-trivial boolean conditions**: ISxx instruction chains recovered as comparison expressions with short-circuit support (ISTC/ISFC)

## Architecture

The project uses a Cargo workspace with two crates:

- **luadejit-core** (library) — The decompilation engine:
  - **reader** — Parses raw LuaJIT bytecode into an in-memory module representation
  - **ir** — Analyzes bytecode: control-flow graphs, loop detection, condition recovery, scope analysis, upvalue resolution, expression building
  - **codegen** — Emits readable Lua source from analyzed instructions with register expression tracking

- **luadejit-cli** (root binary) — Command-line interface

## CLI Usage

```bash
# Decompile a single file to stdout
luadejit script.bc

# Decompile and write to output directory using chunkname paths
luadejit script.bc -o ./output

# Decompile with flat output (ignore chunkname, use input filename)
luadejit script.bc -o ./output --flat

# Disassemble instead of decompiling
luadejit script.bc --disasm

# Process an entire directory recursively
luadejit ./bytecode_dir -o ./output

# Strip a directory prefix from chunkname-based output paths
luadejit ./bytecode_dir -o ./output --strip-dir-prefix scripts/

# Quiet mode (only show final summary)
luadejit ./bytecode_dir -o ./output --quiet

# Verbose mode
luadejit script.bc -v
```

### Arguments

| Argument           | Description                                            |
|--------------------|--------------------------------------------------------|
| `INPUT`            | Input bytecode file or directory (positional, required)|

### Options

| Option                     | Description                                              |
|----------------------------|----------------------------------------------------------|
| `-o, --output <DIR>`       | Output directory (default: `./output`)                   |
| `--flat`                   | Use input filename for output, ignoring chunkname        |
| `--disasm`                 | Output disassembly instead of decompiled source          |
| `--strip-dir-prefix <P>`   | Strip a leading path prefix from chunkname-based paths   |
| `--quiet`                  | Suppress per-file logging, only show final summary       |
| `-v, --verbose`            | Enable verbose output                                    |
| `-V, --version`            | Print version                                            |

### Output Paths

By default, output file paths are derived from the bytecode chunkname embedded in each file:

- `@path/to/file.lua` chunkname produces `path/to/file.lua` under the output directory
- `=string` chunkname uses the remainder as the filename
- `=?` or empty chunkname falls back to the input filename stem with `.lua` extension
- Control characters are stripped, backslashes normalized to forward slashes

Use `--flat` to ignore chunknames entirely and use the input filename instead. Use `--strip-dir-prefix` to remove a common leading directory from chunkname paths.

### Exit Codes

- `0` — All files processed successfully (skipped non-bytecode files don't count as failures)
- `1` — One or more files failed to process

## Building

```bash
cargo build --workspace
```

## Limitations

- **While/repeat loops**: Not fully reconstructed (body emitted as flat statements)
- **Else branches**: `if then else end` not yet recovered (only `if then end`)
- **Variable naming**: Some temporaries show as `var_N` when debug info is unavailable or stripped
- **Complex nested scopes**: Some edge cases in nested function upvalue resolution
- **Break statements**: Often encoded as JMP past loop end, not recovered as explicit `break`

## Testing

The project has comprehensive test coverage:

```bash
# Run all tests
cargo test --workspace --release

# Run only unit tests
cargo test --workspace --release --lib

# Run generated fixture tests
cargo test --workspace --release --test generated

# Run regression tests (game script corpus)
cargo test --workspace --release --test regression
```

Test breakdown:
- 221 unit tests (inline `#[cfg(test)]` modules)
- 10 generated fixture tests (Lua sources compiled to bytecode fixtures)
- 26 regression tests (game script corpus, CI-safe skips when unavailable)

See `docs/testing.md` for detailed testing documentation including how to add new test scenarios.

## Documentation

- `docs/architecture.md` — Complete architecture and implementation details
- `docs/decompilation-patterns.md` — Bytecode-to-Lua recovery patterns and design decisions
- `docs/testing.md` — Test infrastructure and how to add tests
- `docs/luajit-bytecode-format.md` — LuaJIT bytecode format specification
- `docs/research/` — Phase A research artifacts (decompilation theory, diagnosis, design memos). Start with `decompilation-primer.md`.

## Platform Support

- Linux x64
- Windows x64
