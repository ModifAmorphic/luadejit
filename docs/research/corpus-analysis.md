# Corpus Analysis — Darktide Bytecode

**Purpose**: Categorize the 9,649-file Darktide bytecode corpus by
features, select a v1 test subset, and validate open design questions
(multres frequency, while/repeat prevalence).

**Methodology**: raw-byte parser in Python (`/tmp/opencode/
corpus_probe_v2.py`) that reads the LuaJIT bytecode format directly
(no `luajit -bl` dependency — important because the corpus is FR1
and the system `luajit` is FR2, so `luajit -bl` rejects the corpus
with "incompatible bytecode"). The probe counts opcodes and
categorizes features per file. All 9,649 files parsed successfully.

## Key findings

### All FR1, all non-stripped

Every file in the corpus is FR1 mode with debug info present (not
stripped). This matters because:
- Our locally-compiled fixtures are FR2 (system luajit is FR2 default).
- The production corpus is FR1. Our parser handles both modes (confirmed:
  all 9,649 FR1 files parsed without error).
- The v1 corpus subset provides FR1 test coverage that local fixtures
  can't.

### Size distribution

| Bucket | Files | % | Threshold |
|--------|-------|---|-----------|
| tiny | 561 | 5.8% | <10 instructions |
| small | 1,944 | 20.1% | 10-49 |
| medium | 3,838 | 39.8% | 50-199 |
| large | 2,456 | 25.5% | 200-999 |
| huge | 850 | 8.8% | ≥1000 |

Most files are medium or large. Data-table files (dialogues, content)
tend to be large/huge. Code files (scripts) tend to be small/medium.

### Feature presence

| Feature | Files with ≥1 | % | Total ops |
|---------|---------------|---|-----------|
| tables | 9,638 | 99.9% | 3,796,338 |
| constant_load | 8,683 | 90.0% | 904,749 |
| globals | 7,795 | 80.8% | 292,759 |
| calls | 7,673 | 79.5% | 179,368 |
| closures | 3,134 | 32.5% | 97,654 |
| conditionals | 2,705 | 28.0% | 92,304 |
| arithmetic | 2,330 | 24.1% | 58,768 |
| multres | 1,300 | 13.5% | 6,100 |
| numeric_for | 1,138 | 11.8% | 10,592 |
| generic_for | 991 | 10.3% | 11,410 |
| **while_repeat** | **255** | **2.6%** | **821** |

### Top 20 opcodes by frequency

| Opcode | Count | % of all |
|--------|-------|----------|
| TDUP | 1,319,261 | 22.6% |
| TSETV | 728,393 | 12.5% |
| TSETS | 665,304 | 11.4% |
| KSHORT | 642,639 | 11.0% |
| TGETS | 499,671 | 8.6% |
| TSETB | 389,117 | 6.7% |
| GGET | 292,482 | 5.0% |
| MOV | 207,963 | 3.6% |
| CALL | 171,957 | 2.9% |
| KSTR | 154,822 | 2.7% |
| TNEW | 149,693 | 2.6% |
| JMP | 113,718 | 1.9% |
| KNUM | 53,939 | 0.9% |
| KPRI | 51,756 | 0.9% |
| UGET | 49,892 | 0.9% |
| FNEW | 42,322 | 0.7% |
| ISF | 39,235 | 0.7% |
| TGETV | 36,133 | 0.6% |
| RET0 | 29,097 | 0.5% |
| RET1 | 24,803 | 0.4% |

Table operations (TDUP+TSET*+TGET*+TNEW) account for ~62% of all
opcodes. This corpus is heavily data-table-oriented.

## Q4 validation: multres frequency

| Opcode | Files | % | Instances |
|--------|-------|---|-----------|
| CALLM | 1,237 | 12.8% | 4,835 |
| VARG | 406 | 4.2% | 844 |
| TSETM | 95 | 1.0% | 188 |
| CALLMT | 80 | 0.8% | 137 |
| RETM | 38 | 0.4% | 96 |

**Assessment**: multres is present in 13.5% of files — not rare. The
Q4 design (dual representation: Known count + Dynamic) is justified.
CALLM at 12.8% means roughly 1 in 8 files has multres argument
passing. Ignoring or deferring multres handling would affect a
significant fraction of the corpus.

However, the truly "Dynamic" case (where the count is unknown at
compile time) is harder to measure from opcode presence alone — it
requires checking whether the consuming instruction's operand is 0
(multres marker). The raw CALLM count includes both the
"known-count-at-consumer" and "dynamic-count" cases. A follow-up
probe that checks operand values would give a more precise Dynamic
frequency. For now, the 13.5% presence rate is sufficient to
justify the design.

## While/repeat rarity

Only 2.6% of files (255 files, 821 total LOOP instructions) contain
while/repeat loops. This is consistent with the POC's README listing
while/repeat as "not fully reconstructed" — the POC may not have
prioritized them because they're rare.

**Implication for Stage 11**: the No More Gotos algorithm's
cyclic-region structuring (the hardest part to implement) is
exercised by only 2.6% of the corpus. But this doesn't mean
while/repeat should be deferred — the algorithm handles all loops
(including for-loops) uniformly, so while/repeat support is inherent
to implementing cyclic region structuring. There's no incremental
complexity cost to including it. Stage 11 stays in scope.

## FR1/FR2 testing asymmetry

The corpus is entirely FR1. Our local fixtures are entirely FR2
(system luajit is FR2). This means:
- Stages 1-6 test FR2 only (local fixtures).
- The corpus regression test (introduced at Stage 7 per testing-
  strategy.md) tests FR1 only (Darktide corpus).
- We have **no test that exercises both modes on the same code path**.

This is acceptable for early stages (the parsing is mode-agnostic;
only CALL/CALLM operand interpretation differs). But when Stage 12
(function calls) implements the FR1/FR2 arg-base offset, we need
both FR1 and FR2 test coverage for CALL instructions. The v1 corpus
subset provides FR1; local fixtures provide FR2.

## v1 corpus subset

49 files selected (target was 50-100 per testing-strategy.md §6).
Selection methodology: for each feature category, take the 3 smallest
files + 2 medium-sized files that exercise the feature. This ensures
coverage of all capabilities while keeping test runtime short.

Subset list: `/tmp/opencode/v1_corpus.txt`.

The subset should be committed to the repo (e.g.,
`tests/corpus/v1.txt`) when the corpus regression test is introduced
(Stage 7 per testing-strategy.md). The actual bytecode files stay
external (Darktide corpus is not in the repo; tests gracefully skip
when absent).

## Implications for the implementation plan

1. **Tables are the dominant feature.** Stage 14 (tables) handles the
   most common opcode class. Getting TDUP/TSET*/TGET* right is more
   important for corpus coverage than any other stage.

2. **Multres matters.** 13.5% of files have multres. The Q4 design is
   validated; the dual representation is the right call.

3. **While/repeat is rare but in scope.** Stage 11 handles it. If
   needed, it can be deferred past v1.0 without affecting most of the
   corpus, but it's included per the 17-stage plan.

4. **Corpus regression should be introduced at Stage 7** (per
   testing-strategy.md). The v1 subset is ready now.

5. **The FR1-only corpus means FR2-specific CALL handling** needs
   local fixture testing. Stage 12 should include FR2 CALL fixtures
   compiled with system luajit.

## Sources

- Probe: `/tmp/opencode/corpus_probe_v2.py` (raw-byte Python parser).
- v1 subset: `/tmp/opencode/v1_corpus.txt`.
- Format spec: `docs/luajit-bytecode-format.md` (opcode values for
  the Python parser's lookup table).
