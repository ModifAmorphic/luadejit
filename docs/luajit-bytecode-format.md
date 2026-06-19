# LuaJIT 2.x Bytecode Format Specification

Authoritative reference for writing a LuaJIT bytecode parser/decompiler. Derived
from the canonical LuaJIT v2.1 source (`lj_bcdump.h`, `lj_bc.h`, `lj_bcread.c`,
`lj_bcwrite.c`) and verified against ~9,600 real LuaJIT v2 bytecode dumps.

**LuaJIT 2.x specifics:** files begin with magic `1B 4C 4A` ("ESC LJ"), version
byte `02`, little-endian. Both stripped and non-stripped dumps appear in the wild.

---

## 1. Top-Level Grammar

From `lj_bcdump.h` (the canonical BNF):

```
dump    = header proto+ 0U
header  = ESC 'L' 'J' versionB flagsU [namelenU nameB*]
proto   = lengthU pdata
pdata   = phead bcinsW* uvdataH* kgc* knum* [debugB*]
phead   = flagsB numparamsB framesizeB numuvB numkgcU numknU numbcU
          [debuglenU [firstlineU numlineU]]
```

Type legend:
- `B` = 8-bit byte
- `H` = 16-bit (little-endian)
- `W` = 32-bit (little-endian)
- `U` = ULEB128 of a 32-bit value
- `U0` / `U1` = ULEB128 of a 32-bit value, with bit 0 being the type tag (see Number Constants)
- `0U` = a single ULEB128 byte `0x00` terminator

**Parsing order:**
1. Header (5 bytes fixed + chunkname if present)
2. Repeat: read `lengthU` prototype block; if length is 0, done
3. Prototypes are emitted **depth-first, children before parents**. The *last*
   prototype read is the *main* (root) chunk.

---

## 2. File Header

| Offset | Bytes | Field | Value |
|--------|-------|-------|-------|
| 0 | 3 | Magic | `1B 4C 4A` (`ESC 'L' 'J'`, `BCDUMP_HEAD1/2/3`) |
| 3 | 1 | Version | `02` for LuaJIT 2.x (`BCDUMP_VERSION`). v1 = `01` |
| 4 | ULEB128 | Flags | combination of `BCDUMP_F_*` bits (see below) |

If `BCDUMP_F_STRIP` is **not** set, immediately follows:
| | ULEB128 | namelen | length of chunkname |
| | namelen bytes | chunkname | raw chunkname bytes (NOT null-terminated) |

### 2.1 Header Flags (`flags`, ULEB128)

| Bit | Mask | Name | Meaning |
|-----|------|------|---------|
| 0 | `0x01` | `BCDUMP_F_BE` | Big-endian byte order. **Most dumps are LE; reject or handle specially.** |
| 1 | `0x02` | `BCDUMP_F_STRIP` | Debug info stripped (no chunkname, no line info, no var names) |
| 2 | `0x04` | `BCDUMP_F_FFI` | Contains FFI cdata constants |
| 3 | `0x08` | `BCDUMP_F_FR2` | 2-slot frame layout (LJ_FR2 / 64-bit GC mode). Affects CALL/CALLM operand offsets. |
| 31 | `0x80000000` | `BCDUMP_F_DETERMINISTIC` | Internal/writer-only; never seen in dumped files |

`BCDUMP_F_KNOWN = (BCDUMP_F_FR2*2 - 1) = 0x0F` — any bits outside `0x0F` (other
than the internal deterministic bit) make the dump invalid.

> **FR2 significance:** When set, calls place a function+slot pair, so the
> argument base is `A+2` instead of `A+1`. Most LuaJIT desktop builds are
> single-slot (`FR2` unset).

### 2.2 Chunkname

The chunkname is the source name. The first character encodes its type:

| Prefix | Meaning |
|--------|---------|
| `@` | Source came from a file; the rest is the file path (e.g. `@scripts/foo.lua`) |
| `=`` | Source came from a literal string; the rest is a chosen name |
| (none, i.e. `BCDUMP_HEAD1` = `0x1b`) | Binary source; LuaJIT synthesizes `=?` |

**LuaJIT 2.x chunknames** almost always start with `@` followed by a script path
like `@scripts/managers/multiplayer/connection_boot_manager.lua`.

When STRIP is set and no name was supplied, the loader uses `=?` or the original
loader argument.

---

## 3. Prototype Block

Each prototype is a length-prefixed block:

```
lengthU   <- ULEB128; 0 terminates the dump
... pdata ...
```

The loader reads exactly `length` bytes; if the inner parser doesn't land on
`start + length`, the dump is corrupt.

### 3.1 Prototype Header (`phead`)

| Field | Encoding | Meaning |
|-------|----------|---------|
| `flags` | byte | `PROTO_*` bits (see below) |
| `numparams` | byte | Number of fixed parameters |
| `framesize` | byte | Number of stack slots (register file size) |
| `numuv` | byte | Number of upvalues |
| `numkgc` | ULEB128 | Number of GC constants |
| `numkn` | ULEB128 | Number of number constants |
| `numbc` | ULEB128 | Number of bytecode instructions **stored** (the in-memory count is `numbc + 1`; the extra +1 is the synthetic `FUNC*` header word not present on disk) |

If STRIP is **not** set, then:
| `debuglen` (`sizedbg`) | ULEB128 | Total size of debug info section |
If `debuglen > 0`:
| `firstline` | ULEB128 | First source line number |
| `numline` | ULEB128 | Number of lines (determines line-info width) |

### 3.2 Prototype Flags (`PROTO_*`, in `flags` byte)

| Bit | Mask | Name | Meaning |
|-----|------|------|---------|
| 0 | `0x01` | `PROTO_CHILD` | Has nested child prototypes (FNEW references) |
| 1 | `0x02` | `PROTO_VARARG` | Is vararg (`...`) |
| 2 | `0x04` | `PROTO_FFI` | Uses FFI |

**Main chunk invariant:** the root prototype is always `VARARG`, has
`numparams == 0`, and `numuv == 0`.

### 3.3 Layout order within `pdata` (on-disk)

1. Prototype header (`phead`)
2. `bcins`: `numbc` instructions (the FUNC header word is NOT on disk; it's synthesized)
3. `uvdata`: `numuv` upvalue descriptors, each 16-bit (`H`)
4. `kgc`: `numkgc` GC constants
5. `knum`: `numkn` number constants
6. `debug`: `debuglen` bytes of debug info (only if present)

> Note: the in-memory GC-constant array is stored *backwards* relative to
> constant indices. Instruction operands reference GC constants via a *negative*
> index from the end of the kgc array. See Section 6.

---

## 4. Instructions (`bcins`)

Each instruction is **4 bytes / 32 bits**, little-endian. Two packings:

```
Format ABC:  +----+----+----+----+
             | B  | C  | A  | OP |   (fields: op @0, a @8, c @16, b @24)
             +----+----+----+----+

Format AD:   +----+----+--------+
             |       D    | A  | OP |   (fields: op @0, a @8, d @16)
             +-------------+----+
             MSB                          LSB
```

Field extraction (host byte order, here for LE):
- `op = ins[0] & 0xff`
- `a  = (ins >> 8) & 0xff`
- `b  = (ins >> 24) & 0xff`      (ABC only)
- `c  = (ins >> 16) & 0xff`      (ABC only)
- `d  = (ins >> 16) & 0xffff`    (AD only)
- `j  = d - 0x8000`              (signed jump offset; `BCBIAS_J = 0x8000`)

Operand ranges: `A,B,C ∈ [0,255]`, `D ∈ [0,65535]`, `NO_REG = 0xff`.

### 4.1 Which format? (ABC vs AD)

The format is **not** encoded per-instruction; it's determined by the **opcode's
modes** (defined in `lj_bc.h` BCDEF). Concretely, an opcode uses ABC format iff
its B-mode is non-`none`. Equivalently, if the C-mode (a.k.a. D-mode when B is
none) is paired with a real B operand, it's ABC.

Practical ABC-format opcodes (from `luajit-decompiler-v2/bytecode/instructions.h`):
```
ADDVN SUBVN MULVN DIVVN MODVN
ADDNV SUBNV MULNV DIVNV MODNV
ADDVV SUBVV MULVV DIVVV MODVV POW CAT
TGETV TGETS TGETB TGETR TSETV TSETS TSETB TSETR
CALLM CALL ITERC ITERN VARG
```
All other opcodes use AD format (the D field occupies bytes 2-3).

When reading: read `op`, read `a`. If ABC, read `c` then `b`. If AD, read `d`
as little-endian 16-bit.

### 4.2 Jump Encoding

Jump targets use `j = d - 0x8000`. The target is a **relative offset** from the
current instruction: a jump in instruction at index `i` goes to `i + 1 + j`
(i.e. relative to the instruction *after* the jump). `j` is a signed 16-bit
value (range -32768..+32767).

---

## 5. Full Instruction Set

Opcode order is **fixed** (matters for v1 remapping). Below: opcode value,
mnemonic, format, operand semantics. Operand type suffixes:
- `V` = variable (register slot)
- `S` = GC string constant
- `N` = number constant
- `P` = primitive (`nil`/`false`/`true`)
- `B` = unsigned byte literal (in C field)
- `M` = multiple args/results (B=0 means "fill to top")
- `UV` = upvalue index
- `FUNC`/`TAB` = GC child-prototype / template-table constant

### Comparison / test ops (AD format; D is a jump)

| Op | Hex | Name | Semantics |
|----|-----|------|-----------|
| 00 | ISLT | A<VAR> < D<VAR> ? jmp | if R[A] < R[D] then jump |
| 01 | ISGE | A<VAR> < D<VAR> ? | if NOT(R[A] < R[D]) then jump (>=) |
| 02 | ISLE | A <= D | if R[A] <= R[D] then jump |
| 03 | ISGT | A <= D ? | if NOT(R[A] <= R[D]) then jump (>) |
| 04 | ISEQV | A == D (var) | if R[A] == R[D] then jump |
| 05 | ISNEV | A ~= D (var) | if R[A] ~= R[D] then jump |
| 06 | ISEQS | A == D<STR> | if R[A] == KSTR[D] then jump |
| 07 | ISNES | A ~= D<STR> | if R[A] ~= KSTR[D] then jump |
| 08 | ISEQN | A == D<NUM> | if R[A] == KNUM[D] then jump |
| 09 | ISNEN | A ~= D<NUM> | if R[A] ~= KNUM[D] then jump |
| 0A | ISEQP | A == D<PRI> | if R[A] == KPRI[D] then jump |
| 0B | ISNEP | A ~= D<PRI> | if R[A] ~= KPRI[D] then jump |
| 0C | ISTC | A<DST> = D<VAR> if D | if truthy(R[D]): R[A]=R[D]; jump |
| 0D | ISFC | A<DST> = D<VAR> if !D | if falsy(R[D]): R[A]=R[D]; jump |
| 0E | IST | if D<VAR> | if truthy(R[D]) then jump |
| 0F | ISF | if !D<VAR> | if falsy(R[D]) then jump |
| 10 | ISTYPE | *(internal)* type check; not in dumped bytecode |
| 11 | ISNUM | *(internal)* numeric type check; not in dumped bytecode |

### Unary ops (AD)

| Op | Hex | Name | Semantics |
|----|-----|------|-----------|
| 12 | MOV | A<DST> = D<VAR> | R[A] = R[D] |
| 13 | NOT | A<DST> = not D<VAR> | R[A] = not R[D] |
| 14 | UNM | A<DST> = -D<VAR> | R[A] = -R[D] (may call __unm) |
| 15 | LEN | A<DST> = #D<VAR> | R[A] = #R[D] (may call __len) |

### Binary arithmetic (ABC: A, B, C)

The middle letter of the suffix tells C's type: `VN` = C is number const,
`NV` = C is number const but operands swapped, `VV` = C is a register.

| Op | Hex | Name | Semantics |
|----|-----|------|-----------|
| 16 | ADDVN | A = B + C<NUM> | R[A] = R[B] + KNUM[C] |
| 17 | SUBVN | A = B - C<NUM> | |
| 18 | MULVN | A = B * C<NUM> | |
| 19 | DIVVN | A = B / C<NUM> | |
| 1A | MODVN | A = B % C<NUM> | |
| 1B | ADDNV | A = C<NUM> + B | R[A] = KNUM[C] + R[B] |
| 1C | SUBNV | A = C<NUM> - B | R[A] = KNUM[C] - R[B] |
| 1D | MULNV | A = C<NUM> * B | |
| 1E | DIVNV | A = C<NUM> / B | |
| 1F | MODNV | A = C<NUM> % B | |
| 20 | ADDVV | A = B + C | R[A] = R[B] + R[C] |
| 21 | SUBVV | A = B - C | |
| 22 | MULVV | A = B * C | |
| 23 | DIVVV | A = B / C | |
| 24 | MODVV | A = B % C | |
| 25 | POW | A = B ^ C | R[A] = R[B] ^ R[C] (ABC) |
| 26 | CAT | A = B..C | R[A] = R[B] .. ... .. R[C] (range concat) |

### Constant load ops (AD)

| Op | Hex | Name | Semantics |
|----|-----|------|-----------|
| 27 | KSTR | A<DST> = D<STR> | R[A] = KSTR[D] |
| 28 | KCDATA | A<DST> = D<CDATA> | R[A] = cdata constant (FFI) |
| 29 | KSHORT | A<DST> = D<LITS> | R[A] = (int16)(D) — signed 16-bit literal! |
| 2A | KNUM | A<DST> = D<NUM> | R[A] = KNUM[D] |
| 2B | KPRI | A<DST> = D<PRI> | R[A] = primitive: D=0 nil, D=1 false, D=2 true |
| 2C | KNIL | A<BASE>..D<BASE> = nil | R[A] through R[D] set to nil |

**KSHORT** treats `D` as a **signed** 16-bit integer: value = `(int16_t)D`.
**KPRI** primitive map: `D=0 → nil`, `D=1 → false`, `D=2 → true`.

### Upvalue / function ops

| Op | Hex | Fmt | Name | Semantics |
|----|-----|-----|------|-----------|
| 2D | UGET | AD | A<DST> = D<UV> | R[A] = upvalue[D] |
| 2E | USETV | AD | A<UV> = D<VAR> | upvalue[A] = R[D] |
| 2F | USETS | AD | A<UV> = D<STR> | upvalue[A] = KSTR[D] |
| 30 | USETN | AD | A<UV> = D<NUM> | upvalue[A] = KNUM[D] |
| 31 | USETP | AD | A<UV> = D<PRI> | upvalue[A] = KPRI[D] |
| 32 | UCLO | AD | close upvals A..; goto D<JUMP> | close upvalues >= R[A]; jump |
| 33 | FNEW | AD | A<DST> = D<FUNC> | R[A] = closure over child prototype KGC[D] |

### Table ops

| Op | Hex | Fmt | Name | Semantics |
|----|-----|-----|------|-----------|
| 34 | TNEW | AD | A<DST> = {} | new table; D low 11 bits = array size, high 5 bits = hash size hint |
| 35 | TDUP | AD | A<DST> = D<TAB> | R[A] = copy of template table KGC[D] |
| 36 | GGET | AD | A<DST> = _G[D<STR>] | R[A] = _G[KSTR[D]] (global read) |
| 37 | GSET | AD | _G[D<STR>] = A<VAR> | _G[KSTR[D]] = R[A] (global write) |
| 38 | TGETV | ABC | A = B[C] | R[A] = R[B][R[C]] |
| 39 | TGETS | ABC | A = B[C<STR>] | R[A] = R[B][KSTR[C]] |
| 3A | TGETB | ABC | A = B[C<LIT>] | R[A] = R[B][C] (integer key) |
| 3B | TGETR | ABC | *(rawtable, FFI)* | rarely seen |
| 3C | TSETV | ABC | B[C] = A | R[B][R[C]] = R[A] |
| 3D | TSETS | ABC | B[C<STR>] = A | R[B][KSTR[C]] = R[A] |
| 3E | TSETB | ABC | B[C<LIT>] = A | R[B][C] = R[A] |
| 3F | TSETM | AD | A-1[KNUM(D)]=..., multres | set multires into table |
| 40 | TSETR | ABC | *(rawtable, FFI)* | rarely seen |

**TNEW D encoding:** `D = hashsize<<11 | arraysize`. Array size = `D & 0x7ff`
(0..2047). Hash size hint = `(D >> 11) & 0x1f`, interpreted as a bit-size
(`0`=0, else `1<<(hsize-1)` entries).

### Calls and vararg (ABC unless noted)

The `B` operand = number of args: `B>0` means `B-1` fixed args; `B=0` means
"fill from R[A] to top" (multres, from a previous call/VARG). The `C` operand
= number of results: `C>0` means `C-1` fixed results; `C=0` means "pass all
results up" (multres). When `FR2` is set, the real call base is `A+2`, else
`A+1`.

| Op | Hex | Fmt | Name | Semantics |
|----|-----|-----|------|-----------|
| 41 | CALLM | ABC | A(args) = f(args), +multres | call with multres args; C results |
| 42 | CALL | ABC | A(args) = f(args) | normal call; B args, C results |
| 43 | CALLMT | AD | return f(...) + multres | tail call with multres args |
| 44 | CALLT | AD | return f(...) | tail call; D-1 args |
| 45 | ITERC | ABC | A(A..) = iterator(A-3..A-1) | generic-for step (call) |
| 46 | ITERN | ABC | *(numeric fast path for ITERC)* | like ITERC, numeric |
| 47 | VARG | ABC | A.. = ... | vararg; B=0→multres, C results |
| 48 | ISNEXT | AD | goto ITERN at D<JUMP> | validates/initializes next() loop |

**CALL semantics detail:** the function is at R[A]; arguments are R[A+gap ..
A+gap+B'-1] where gap = FR2?2:1, B' = (B==0? top-A-gap : B-1). Results land at
R[A..]; result count = (C==0? multres : C-1).

### Returns (AD)

| Op | Hex | Name | Semantics |
|----|-----|------|-----------|
| 49 | RETM | A<BASE>, D<LIT> | return R[A], R[A+1..A+D-1] + multres |
| 4A | RET | A<RBASE>, D<LIT> | return R[A..A+D-2] (D-1 values) |
| 4B | RET0 | | return (no values) |
| 4C | RET1 | A<RBASE> | return R[A] (one value) |

**RET/RET1/RET0/RETM** D operand: for RET it's `nresults+1`; RET0/RET1 ignore D
(conceptually 1 and 2).

### Loops and branches (AD, D is a jump unless noted)

| Op | Hex | Name | Semantics |
|----|-----|------|-----------|
| 4D | FORI | numeric-for init | sets up loop; exit-target = D<JUMP> |
| 4E | JFORI | *(JIT)* | not in dumped bytecode |
| 4F | FORL | numeric-for body end | loop-back = D<JUMP> |
| 50 | IFORL | *(JIT/interp)* | not in dumped bytecode |
| 51 | JFORL | *(JIT)* D=LIT trace# | not in dumped bytecode |
| 52 | ITERL | generic-for body end | loop-back = D<JUMP> |
| 53 | IITERL | *(JIT/interp)* | not in dumped bytecode |
| 54 | JITERL | *(JIT)* D=LIT | not in dumped bytecode |
| 55 | LOOP | loop marker | exit-target = D<JUMP>; used by while/repeat/for |
| 56 | ILOOP | *(JIT/interp)* | not in dumped bytecode |
| 57 | JLOOP | *(JIT)* D=LIT trace# | not in dumped bytecode |
| 58 | JMP | unconditional/conditional goto | target = D<JUMP> |

> Opcodes 0x10-0x11 (ISTYPE/ISNUM) and all `I*`/`J*` (JIT) variants do **not**
> appear in dumped bytecode — they're synthesized by the JIT compiler. A pure
> dump reader can ignore them (or assert). The 5 trailing FUNC* entries below
> also only appear as the synthetic in-memory header word, never on disk.

### Function headers (synthetic, in-memory index 0 only)

| Op | Hex | Name | Notes |
|----|-----|------|-------|
| 59 | FUNCF | fixed-arg interp | appended as bc[0] when !VARARG |
| 5A | IFUNCF | fixed-arg JIT | |
| 5B | JFUNCF | fixed-arg JIT trace | |
| 5C | FUNCV | vararg interp | appended as bc[0] when VARARG |
| 5D | IFUNCV | vararg JIT | |
| 5E | JFUNCV | vararg JIT trace | |
| 5F | FUNCC | C function | |
| 60 | FUNCCW | C function (wrap) | |

These are **never stored on disk**. The reader synthesizes bc[0] = `FUNCF` (or
`FUNCV` if VARARG) with A=framesize. A dump parser should reserve slot 0 for the
synthesized header and start real instructions at index 1.

### Numeric-for register layout (relative to operand A)

FORI/FORL use these slots starting at R[A]:
- `R[A+0]` = FORL_IDX (index)
- `R[A+1]` = FORL_STOP (limit)
- `R[A+2]` = FORL_STEP (step)
- `R[A+3]` = FORL_EXT (external loop variable, visible to body)

---

## 6. Constants

### 6.1 GC Constants (`kgc`) — `BCDUMP_KGC_*`

Stored in `numkgc` entries. Types (`bcread_kgc`):

| ULEB128 value | Name | Payload |
|---------------|------|---------|
| 0 | KGC_CHILD | (none) — references a previously-read child prototype |
| 1 | KGC_TAB | template table (see below) |
| 2 | KGC_I64 | FFI int64: `loU hiU` |
| 3 | KGC_U64 | FFI uint64: `loU hiU` |
| 4 | KGC_COMPLEX | FFI complex: `rloU rhiU iloU ihiU` |
| >= 5 | KGC_STR | string of length `value - 5`; raw bytes follow |

**Indexing convention (important!):** The in-memory GC array is stored in
*reverse* order relative to instruction references. KSTR/TDUP/FNEW operands use
a *negative index from the end*: a KGC operand value `d` refers to GC constant
`kgc[numkgc - 1 - d]`. Equivalently, when you read kgc in file order into an
array, the first-read constant is referenced by the *highest* index.

Template table (`KGC_TAB`):
```
narrayU nhashU  karray[narray]  khash[nhash]
karray = ktabk
khash  = ktabk ktabk     (key then value)
```

### 6.2 Table Key/Value (`ktabk`) — `BCDUMP_KTAB_*`

| ULEB128 value | Name | Payload |
|---------------|------|---------|
| 0 | KTAB_NIL | nil |
| 1 | KTAB_FALSE | false |
| 2 | KTAB_TRUE | true |
| 3 | KTAB_INT | integer: `U` |
| 4 | KTAB_NUM | number: `loU hiU` (IEEE-754 double, lo/hi 32-bit halves) |
| >= 5 | KTAB_STR | string of length `value - 5`; raw bytes follow |

### 6.3 Number Constants (`knum`)

Each of the `numkn` entries uses a **33-bit ULEB128** encoding where the least
significant bit of the first byte is a type tag:

1. Read first byte. The **low bit** (`byte & 1`) is the type tag.
2. The remaining **33-bit ULEB128** is decoded from the **upper 7 bits** of the
   first byte (shifted right by 1) plus continuation bytes.

Algorithm (`bcread_uleb128_33`):
```
v = byte0 >> 1          # top 6 bits of value (in first byte's upper 7 bits)
if v >= 0x40:           # continuation bit set (byte0 & 0x80)
    v &= 0x3f
    shift = -1
    repeat:
        shift += 7
        b = next_byte
        v |= (b & 0x7f) << shift
    until b < 0x80
```

Then:
- **If tag bit (byte0 & 1) == 0:** integer constant. Value = the 33-bit ULEB128
  result interpreted as a **signed 32-bit** integer (the 33rd bit handles sign
  extension). This is a boxed "int TValue".
- **If tag bit == 1:** floating-point (double) constant. The 33-bit result is the
  **low 32 bits** of the IEEE-754 double; read one more ULEB128 for the **high
  32 bits**. Reassemble as `f64::from_bits(hi << 32 | lo)`.

**Practical read order:** read the 33-bit ULEB128 → that's either the integer
(tag 0) or the low word (tag 1). If tag 1, read a second normal ULEB128 for the
high word.

> Negative integers: the writer emits `2*k | (k & 0x80000000)` and patches the
> last byte's top bits, so the 33-bit field carries the sign. A parser should
> treat the 33-bit value, and when it's an int (tag 0), sign-extend from 32 bits.

### 6.4 Primitive Constants (`PRI`)

KPRI/USETP/ISEQP operands encode primitives directly in the D field:
- `0` → `nil`
- `1` → `false`
- `2` → `true`

(These are the `BCDUMP_KTAB_*` values inverted: `~tp` maps nil→0x7fffffff etc.,
but in instructions the literal small integers are used.)

---

## 7. Upvalues

Each prototype has `numuv` upvalue descriptors, each a **16-bit** (`H`) value:

| Bits | Mask | Name | Meaning |
|------|------|------|---------|
| 15 | `0x8000` | `BC_UV_LOCAL` (PROTO_UV_LOCAL) | 0 = open (from parent's stack), 1 = closed (references parent's upvalue) |
| 14 | `0x4000` | `BC_UV_IMMUTABLE` (PROTO_UV_IMMUTABLE) | upvalue is immutable |
| 0-13 | `0x3FFF` | index | if LOCAL: register in parent; else: parent's upvalue index |

- **Open upvalue** (LOCAL bit set): `index` is a register number in the
  *enclosing* (parent) function.
- **Closed upvalue** (LOCAL bit clear): `index` is an upvalue index in the parent.

When STRIP is off, debug info includes `numuv` upvalue **name** strings (see Debug).

---

## 8. Debug Information

Present only when `!(flags & BCDUMP_F_STRIP)` and per-prototype `debuglen > 0`.

Layout (in order):
1. **Line info array** — `numbc` entries (note: `numbc` = instructions-on-disk;
   the in-memory sizebc is numbc+1, and line info covers `sizebc-1 = numbc`
   entries — one per *real* instruction, none for the synthetic FUNC header).
   Width depends on `numline`:
   - `numline < 256`: 1 byte each
   - `numline < 65536`: 2 bytes each (LE)
   - else: 4 bytes each (LE)

   **IMPORTANT — line values are DELTAS relative to `firstline`, not absolute
   line numbers.** To recover the source line of instruction at in-memory index
   `i` (i = 1..numbc), use `lj_debug_line` semantics:
   - `i == 0` (the synthetic FUNC header) → `firstline`.
   - Otherwise → `firstline + lineinfo[i - 1]`.
   The stored array is `lineinfo[0..numbc-1]`. So real instruction #1's line is
   `firstline + lineinfo[0]`, instruction #2's is `firstline + lineinfo[1]`, etc.
   `lastlinedefined = firstline + numline`.

   Verified on a real bytecode file (`expedition_loot_player_drop.lua`): `firstline=9`,
   `numline=2`, stored array `[1,1,1,1,1,1,1,1,2]` → instruction lines 10,10,...,11.
2. **Upvalue names** — `numuv` null-terminated strings
3. **Variable info** — compressed local-variable / parameter scope records

### 8.1 Variable Info Records

Read repeatedly until a `0x00` terminator byte. Each record:

```
typeB    <- 1 byte; 0 = end
   if type >= BC_VAR_STR (a printable char / >= 0x80 range for STR):
       it's a string name: first char = type byte, rest = get_string() (bytes until 0x00)
   else it's a special VAR type (for-loop internal vars):
       1 = FOR_IDX, 2 = FOR_STOP, 3 = FOR_STEP, 4 = FOR_GEN, 5 = FOR_STATE, 6 = FOR_CTL
scope_deltaU   <- ULEB128 added to running scopeOffset
   if scopeOffset becomes 0: this is a PARAMETER; read scopeEndU; scopeEnd = U - 2
   else: scopeBegin = scopeOffset - 2; then read lenU; scopeEnd = scopeBegin + U
```

Concretely (from the C++ parser):
- Maintain `scopeOffset` (starts 0).
- For each variable: `scopeOffset += read_uleb128()`.
- If `scopeOffset == 0`: it's a parameter (the uleb delta wrapped to 0); read a
  second uleb `u`; `scopeEnd = u - 2`.
- Else: `scopeBegin = scopeOffset - 2`; read uleb `len`; `scopeEnd = scopeBegin + len`.

Scope begin/end are **instruction indices** (zero-based, into the on-disk
instruction array). `BC_VAR` enum:

| Value | Meaning |
|-------|---------|
| 0 | END (terminator) |
| 1 | for-loop index |
| 2 | for-loop stop |
| 3 | for-loop step |
| 4 | generic-for generator |
| 5 | generic-for state |
| 6 | generic-for control |
| >= 7 (printable) | local variable name |

String-name variables: the type byte itself is the **first character** of the
name (names are >= 1 char), followed by the remaining bytes until a `0x00`.

### 8.2 Stripped vs Non-Stripped

| Feature | Non-stripped | Stripped (`BCDUMP_F_STRIP`) |
|---------|--------------|------------------------------|
| Chunkname | present | absent (synthesized as `=?`) |
| Per-proto debuglen | present | absent |
| Line info | present | absent |
| Upvalue names | present | absent |
| Variable names/scopes | present | absent |

The prototype header still carries `flags/numparams/framesize/numuv/numkgc/numkn/numbc`.
Stripping only removes the optional debug section after `numbc`.

---

## 9. ULEB128 Reference

Standard LEB128 (unsigned). Used for nearly all multi-byte lengths/counts:

```
result = 0; shift = 0
repeat:
    byte = next()
    result |= (byte & 0x7f) << shift
    shift += 7
until (byte & 0x80) == 0
```

The 33-bit variant (`uleb128_33`) is used **only** for number constants; see
Section 6.3.

---

## 10. Endianness

- Instructions, upvalue descriptors, and line info are written in **host byte
  order**. LuaJIT 2.x dumps are **little-endian** (`BCDUMP_F_BE` unset).
- If `BCDUMP_F_BE` is set, byte-swap every 32-bit instruction, every 16-bit
  upvalue, and every multi-byte line entry when loading.
- A pure LE parser can reject `BCDUMP_F_BE` dumps.

---

## 11. Version 1 vs Version 2

LuaJIT 2.x uses **version 2** (`02`). Key differences for completeness:

- Version 1 (`01`) is the older LuaJIT 1.x/early-2.x format. The opcode numbering
  differs: opcodes `>= ISTYPE` are shifted because v1 lacks ISTYPE/ISNUM (2
  opcodes) and TGETR/TSETR (2 more at the end region). The C++ reference parser
  remaps v1 opcode bytes:
  - For `byte >= ISTYPE`: add 2 (skip the two internal test ops).
  - Near the end (TGETR-2, TSETR-3 region): add 3 or 4.
- Version 2 introduced `BCDUMP_F_FR2` (the `0x08` flag) — only valid in v2.
- For most use cases, **treat v2 as canonical**; v1 support is optional.

The `FR2` flag (when set) changes CALL/CALLM/CALLT argument base from `A+1` to
`A+2` (two-slot frame). Verify the flag in the header and apply the gap.

---

## 12. Worked Example (real bytecode file)

File `91f5a29faccd6975.a14e8dfa2cd117e2` (81 bytes). Verified decode:

```
Header:
  1b 4c 4a       magic "ESC LJ"
  02             version 2
  00             flags ULEB = 0  -> not stripped, LE, no FR2, no FFI
  39             chunkname length ULEB = 57
  40 73 63 ...   chunkname "@scripts/managers/multiplayer/connection_boot_manager.lua"
                 (0x40 = '@', then 56 more chars)

Prototype block (last = main chunk):
  10             block length ULEB = 16 bytes
  02             proto flags = VARARG (0x02)
  00             numparams = 0
  01             framesize = 1
  00             numuv = 0
  00 00 00       numkgc=0, numkn=0, numbc=0  (0 real instrs; sizebc=1 in memory)
  02             sizedbg = 2
  00             firstline = 0
  01             numline = 1
  (line info: 0 entries since numbc=0)
  (no upvalue names, no var info)
  01 00          2 debug bytes (here: a degenerate empty lineinfo/varinfo region)
Dump terminator:
  00             end of dump
```

This is a near-empty main chunk (the real content was stripped to a single
synthesized `FUNCF`/`RET0`). Confirms: header parsing, chunkname `@`-prefix,
ULEB lengths, prototype header field order, debug section presence, and the
trailing `0x00` dump terminator.

A richer example (`expedition_loot_player_drop.lua`, 693 bytes, firstline=9,
numline=2, 3 params) decoded with full instruction + GC-constant + debug output
matched byte-for-byte (`proto_match=True`), confirming the GC-constant
reverse-indexing, ABC/AD instruction formats, 2-byte line deltas, and parameter
scope records.

---

## 13. Parsing Checklist (for a Rust implementation)

1. Read 3 magic bytes; assert `1B 4C 4A`.
2. Read version byte; branch on v1/v2 opcode remapping if v1 support is wanted.
3. Read flags ULEB128. Assert `(flags & !KNOWN) == 0`. Handle/reject BE.
4. If `!STRIP`: read chunkname (uleb len + bytes). Note `@`/`=`/binary prefix.
5. Loop: read prototype length uleb. If 0 (or a lone `0x00`), stop.
   - Otherwise read exactly `length` bytes into the prototype sub-parser.
6. Prototype sub-parser:
   a. flags, numparams, framesize, numuv (4 bytes).
   b. numkgc, numkn, numbc (3 ulebs).
   c. If `!STRIP`: sizedbg uleb; if >0: firstline, numline ulebs.
   d. `numbc` instructions: op byte + A byte; then (ABC: C,B) or (AD: D lo,hi).
   e. `numuv` upvalue descriptors (2 bytes each, LE).
   f. `numkgc` GC constants (CHILD/TAB/I64/U64/COMPLEX/STR).
   g. `numkn` number constants (33-bit uleb; int or f64).
   h. If sizedbg>0: line array (width by numline), upvalue names, var info.
7. The **last** prototype is the main chunk (root). Children precede parents.
8. Resolve GC-constant references with the reverse-index convention
   (`kgc[numkgc-1-d]`). Resolve KSTR/D from the GC array; KNUM/D from the number
   array (forward index). FNEW links to a child prototype via KGC_CHILD.
9. Link upvalues: open (LOCAL bit) → parent register; closed → parent upvalue.

### Constant-index summary

| Operand type | Array | Index direction |
|--------------|-------|-----------------|
| KSTR (D or C) | GC constants (kgc) | **reverse**: `kgc[numkgc-1-d]` |
| FUNC (FNEW D) | GC constants (kgc) | **reverse** (a KGC_CHILD) |
| TAB (TDUP D) | GC constants (kgc) | **reverse** (a KGC_TAB) |
| NUM (KNUM/C, ISxxN) | number constants (kn) | **forward**: `kn[d]` |
| CDATA | GC constants | **reverse** |
| PRI | inline literal (0/1/2) | — |
| LIT (TGETB/TSETB C) | inline byte | — |
| LITS (KSHORT D) | inline signed int16 | — |
| UV (UGET/USETx) | upvalue descriptors | forward `uv[d]` |

> **Common pitfall:** string/table/func/cdata constants are reverse-indexed from
> the kgc array; number constants are forward-indexed. Mixing these up is the
> #1 parser bug.

---

## Sources

- `lj_bcdump.h` — formal grammar, magic/flag constants, KGC/KTAB enums.
- `lj_bc.h` — instruction bit layout, full BCDEF opcode table + operand modes,
  jump bias, for-loop register enum.
- `lj_bcread.c` — authoritative reader: `bcread_header`, `lj_bcread_proto`,
  `bcread_bytecode`, `bcread_uv`, `bcread_kgc`, `bcread_knum`, `bcread_ktab`,
  `bcread_dbg`, `bcread_uleb128`, `bcread_uleb128_33`.
- `lj_bcwrite.c` — writer (encoding truth for numbers, template tables, debug).
- Reference C++ parser: `luajit-decompiler-v2/bytecode/{bytecode,prototype,constants,instructions}.{cpp,h}`.
- Verified against ~9,600 real LuaJIT v2 bytecode dumps (magic `1B 4C 4A 02`).
