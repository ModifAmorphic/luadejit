# LuaJIT Number Formatting — Research Note

**Purpose**: Document LuaJIT's number-to-string conversion rules so
the decompiler's emit step can match them. Needed before Stage 4
(arithmetic), where number emission becomes common.

**Methodology**: empirically tested LuaJIT 2.1.1774896198's
`tostring()` / `print()` for edge-case values. These use the same
formatting as `lua_tostring()`, which is what the LuaJIT compiler
itself uses when emitting number constants to bytecode dumps. The
format is C's `printf("%.14g", value)` with LuaJIT-specific handling
for special values.

## The rules

LuaJIT formats numbers using `%.14g` semantics:

1. **Precision**: at most 14 significant digits. Longer values are
   rounded (`1.234567890123456789` → `1.2345678901235`).

2. **Scientific notation** when the decimal exponent is < -4 or
   ≥ 14:
   - `100000000000000` (1e14) → `1e+14`
   - `0.0001` (1e-4) → `0.0001` (still decimal)
   - `0.00009999` (~1e-5) → `9.999e-05`
   - `1e-5` → `1e-05`

3. **Exponent format**: lowercase `e`, sign character (`+` or `-`),
   at least 2 digits (`1e+14`, `1e-05`, `1e+100`).

4. **Trailing zeros removed**: `1.10` → `1.1`. No unnecessary
   decimal places.

5. **Integer-valued floats**: formatted as integers (`3.0` → `3`,
   `100.0` → `100`). No trailing `.0`.

6. **Special values**: `inf`, `-inf`, `nan` (all lowercase, no `#`
   prefix).

7. **Negative zero**: `-0` (preserved as distinct from positive zero).

## Where Rust's default formatting diverges

Rust's `{}` formatter for `f64` uses a different algorithm (based on
Grisu/Ryū) that prioritizes round-trip correctness over brevity:

| Value | LuaJIT | Rust `{}` |
|-------|--------|-----------|
| `1e100` | `1e+100` | `1000...0` (101 digits) |
| `1.5e300` | `1.5e+300` | `1500...0` (300+ digits) |
| `0.00001` | `1e-05` | `0.00001` |
| `1.5e-300` | `1.5e-300` | `0.0000...015` (300 zeros) |
| `2^53` | `9.007199254741e+15` | `9007199254740992` |
| `f64::NAN` | `nan` | `NaN` |
| `1234567890123456789.0` | `1.2345678901235e+18` | `1234567890123456800` |

**Key divergences**:
1. Rust doesn't switch to scientific notation for large/small numbers.
2. Rust shows more than 14 significant digits.
3. Rust's `NaN` is capitalized; LuaJIT's is lowercase.
4. Rust's scientific exponent has no `+` sign; LuaJIT's does.
5. Rust's scientific exponent has no leading zero; LuaJIT's is
   zero-padded to 2 digits minimum.

## Implementation guidance

A pure-Rust formatter that matches `%.14g`. Suggested approach:

1. Handle special cases first: `inf`, `-inf`, `nan`, `-0.0`.
2. Use `format!("{:.13e}", val)` to get 14-significant-digit scientific
   notation. (Rust's `{:.Ne}` precision counts digits *after* the radix
   point, so `{:.13e}` = 1 before + 13 after = 14 sig figs, matching
   `%.14g`. Using `{:.14e}` would give 15 sig figs.)
3. Parse the exponent from the result.
4. If exponent is in range [-4, 14): reformat as decimal (strip
   trailing zeros, handle integer-valued floats).
5. If exponent is outside that range: keep scientific but adjust the
   exponent format to match LuaJIT (`e+15`, `e-05`).
6. Strip trailing zeros from both forms (the `%g` convention).

Estimated implementation: ~50-80 lines of Rust. No external
dependencies needed.

**Alternative**: use the `libc` crate's `snprintf("%.14g", val)` via
FFI for exact C-library match. Simpler code, but adds a C dependency
and may have platform-specific behavior (Windows vs Linux `printf`
implementations differ slightly). Not recommended for a project
that values portability.

## Verification methodology

When implementing the formatter, verify against these LuaJIT-produced
values (compile with `luajit -e 'print(tostring(value))'`):

| Input | Expected output |
|-------|----------------|
| `0.0` | `0` |
| `3.0` | `3` |
| `3.14` | `3.14` |
| `-7` | `-7` |
| `1e14` | `1e+14` |
| `1e-5` | `1e-05` |
| `0.0001` | `0.0001` |
| `1.234567890123456789` | `1.2345678901235` |
| `math.huge` | `inf` |
| `-math.huge` | `-inf` |
| `0/0` | `nan` |
| `-0.0` | `-0` |

## Sources

- Empirical: LuaJIT 2.1.1774896198 at `/usr/bin/luajit`.
- C `printf` `%g` specification: ISO C11 §7.21.6.1.
- LuaJIT source: `lj_strfmt.c` function `lj_strfmt_putfnum` uses
  `LUAI_NUMFMT` = `"%.14g"`.
- Rust formatting: `std::fmt` for `f64`, using Grisu/Ryū shortest-
  representation algorithm.
