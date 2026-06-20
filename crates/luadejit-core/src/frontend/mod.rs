//! Frontend: parse LuaJIT 2.x bytecode into a [`Module`] (defined in
//! [`crate::ir`]).
//!
//! The parser follows the format doc
//! (`docs/luajit-bytecode-format.md`) §1 grammar, §2 header, §3 proto
//! block, §4 instructions, §5 opcode table, §6 constants, §7
//! upvalues, §8 debug info, §9 ULEB128, and the §13 parsing
//! checklist. It is capable of decoding any well-formed LuaJIT 2.x
//! dump; what the rest of the pipeline *does* with a parsed module is
//! later stages' concern.
//!
//! The bytecode-format data types themselves ([`Module`], [`Proto`],
//! [`Instruction`], [`Opcode`], the constant enums, [`UpvalDesc`],
//! [`DebugInfo`], [`VarInfo`], and the format flag constants) live in
//! [`crate::ir`]. This module owns only the parsing logic: the
//! [`Reader`] cursor, the [`Module::from_bytes`] entry point, the
//! [`Instruction::read`] single-instruction decoder, and the `parse_*`
//! free functions.

pub mod reader;

pub use reader::Reader;

// IR types and format flag constants are defined in `crate::ir`; pull
// in everything the parser references so the bodies below can use the
// bare names. `FLAG_FFI` / `FLAG_FR2` are not consumed by the parser
// (they affect later stages), so they are intentionally not imported
// here — reach them via `crate::ir::` if ever needed.
use crate::ir::{
    DebugInfo, GcConst, Instruction, KtabK, Module, ModuleHeader, NumConst, Opcode, Proto,
    TableConst, UpvalDesc, VarInfo, VarKind,
};
use crate::ir::{
    FLAGS_KNOWN_MASK, FLAG_BE, FLAG_STRIP, PROTO_CHILD, PROTO_FFI, PROTO_VARARG, VAR_STR,
};
use crate::DecompilerError;

// ---- Parsing-only inherent impls for IR types ------------------------
//
// `Module` and `Instruction` are defined in `crate::ir` as pure data
// plus pure-data accessors. Their parsing entry points depend on the
// [`Reader`] and on the `parse_*` helpers in this module, so those
// impls live here alongside the rest of the parser. Rust permits
// inherent impls for a type to be split across modules within the
// same crate.

impl Module {
    /// Parse a complete bytecode module. See format doc §13 for the
    /// checklist this implementation follows.
    pub fn from_bytes(bytes: &[u8]) -> Result<Module, DecompilerError> {
        let mut reader = Reader::new(bytes);
        let header = parse_header(&mut reader)?;
        let protos = parse_protos(&mut reader, header.flags)?;
        Ok(Module { header, protos })
    }
}

impl Instruction {
    /// Read one 4-byte instruction from the reader. The opcode
    /// determines whether the remaining two bytes are interpreted as
    /// (C, B) for ABC or as a little-endian 16-bit D for AD.
    pub fn read(reader: &mut Reader<'_>) -> Result<Self, DecompilerError> {
        let raw = reader.read_bytes(4)?;
        let op_byte = raw[0];
        let a = raw[1];
        let op = Opcode::from_byte(op_byte, reader.pos() - 4)?;
        let (b_or_d, c) = if op.is_abc() {
            let c = raw[2];
            let b = raw[3];
            (u32::from(b), c)
        } else {
            let d_lo = u32::from(raw[2]);
            let d_hi = u32::from(raw[3]);
            let d = d_lo | (d_hi << 8);
            (d, 0)
        };
        Ok(Instruction { op, a, b_or_d, c })
    }
}

// ---- Header parsing --------------------------------------------------

fn parse_header(reader: &mut Reader<'_>) -> Result<ModuleHeader, DecompilerError> {
    // Magic: ESC 'L' 'J' (1B 4C 4A).
    let magic = reader.read_bytes(3)?;
    if magic != [0x1b, 0x4c, 0x4a] {
        return Err(DecompilerError::InvalidBytecode {
            offset: 0,
            reason: format!(
                "bad magic bytes {:#04x} {:#04x} {:#04x} (expected 1B 4C 4A)",
                magic[0], magic[1], magic[2]
            ),
        });
    }

    // Version 02 for LuaJIT 2.x. We support v2; reject anything else
    // rather than guessing at v1's opcode remapping.
    let version = reader.read_u8()?;
    if version != 0x02 {
        return Err(DecompilerError::InvalidBytecode {
            offset: 3,
            reason: format!(
                "unsupported bytecode version 0x{:02x} (only 0x02 supported)",
                version
            ),
        });
    }

    let flags = reader.read_uleb128()?;
    // Reject any flag bits outside the known mask. The spec carves out
    // one exception: the internal/writer-only deterministic bit
    // (0x80000000), which never appears in real dumps but is allowed
    // by the format. Any other bit makes the dump invalid.
    let allowed_flags_mask = FLAGS_KNOWN_MASK | 0x8000_0000;
    if flags & !allowed_flags_mask != 0 {
        return Err(DecompilerError::InvalidBytecode {
            offset: 4,
            reason: format!("unknown header flags 0x{:08x}", flags),
        });
    }
    // Reject big-endian dumps; the parser is little-endian only.
    if flags & FLAG_BE != 0 {
        return Err(DecompilerError::InvalidBytecode {
            offset: 4,
            reason: "big-endian bytecode is not supported".to_string(),
        });
    }

    let chunkname = if flags & FLAG_STRIP == 0 {
        let namelen = reader.read_uleb128()? as usize;
        let name = reader.read_bytes(namelen)?;
        Some(name.to_vec())
    } else {
        None
    };

    Ok(ModuleHeader { flags, chunkname })
}

// ---- Proto loop ------------------------------------------------------

fn parse_protos(reader: &mut Reader<'_>, header_flags: u32) -> Result<Vec<Proto>, DecompilerError> {
    let stripped = header_flags & FLAG_STRIP != 0;
    let mut protos = Vec::new();
    loop {
        let length = reader.read_uleb128()?;
        if length == 0 {
            break;
        }
        let length = length as usize;
        let start = reader.pos();
        let pdata = reader.read_bytes(length)?;
        // Parse the proto from a sub-reader so a runaway read inside
        // the proto cannot run past the proto's declared length and
        // desynchronize the parent loop. Errors use offsets relative
        // to the proto's pdata; `translate_offset` rewrites them to
        // absolute input offsets at the boundary.
        let mut sub = Reader::new(pdata);
        let proto = parse_proto(&mut sub, stripped).map_err(|e| translate_offset(e, start))?;
        // Sanity-check that the proto consumed exactly its declared
        // length; a mismatch means either the parser is buggy or the
        // input is corrupt.
        let consumed = sub.pos();
        if consumed != length {
            return Err(DecompilerError::InvalidBytecode {
                offset: start + consumed,
                reason: format!(
                    "proto length mismatch: header declared {} bytes but parser consumed {}",
                    length, consumed
                ),
            });
        }
        protos.push(proto);
    }
    if protos.is_empty() {
        return Err(DecompilerError::InvalidBytecode {
            offset: reader.pos(),
            reason: "no protos in dump (expected at least the main chunk)".to_string(),
        });
    }
    Ok(protos)
}

/// Lift a sub-reader-relative offset to an absolute input offset.
fn translate_offset(e: DecompilerError, base: usize) -> DecompilerError {
    match e {
        DecompilerError::InvalidBytecode { offset, reason } => DecompilerError::InvalidBytecode {
            offset: base + offset,
            reason,
        },
        other => other,
    }
}

// ---- Per-proto parsing ----------------------------------------------

fn parse_proto(reader: &mut Reader<'_>, stripped: bool) -> Result<Proto, DecompilerError> {
    // Note: offsets reported in errors here are relative to the
    // proto's pdata block. `parse_protos` translates them to absolute
    // input offsets before surfacing the error.
    // phead: 4 bytes then 3 ULEBs.
    let flags = reader.read_u8()?;
    if (flags & !(PROTO_CHILD | PROTO_VARARG | PROTO_FFI)) != 0 {
        return Err(DecompilerError::InvalidBytecode {
            offset: reader.pos() - 1,
            reason: format!("unknown proto flags byte 0x{:02x}", flags),
        });
    }
    let numparams = reader.read_u8()?;
    let framesize = reader.read_u8()?;
    let numuv = reader.read_u8()?;
    let numkgc = reader.read_uleb128()?;
    let numkn = reader.read_uleb128()?;
    let numbc = reader.read_uleb128()?;

    // Optional debug sizing.
    let mut sizedbg: usize = 0;
    let mut firstline: u32 = 0;
    let mut numline: u32 = 0;
    if !stripped {
        sizedbg = reader.read_uleb128()? as usize;
        if sizedbg > 0 {
            firstline = reader.read_uleb128()?;
            numline = reader.read_uleb128()?;
        }
    }

    // bcins. Real instructions start at in-memory index 1; slot 0 is
    // the synthesized FUNC* header (FUNCF for fixed-arg, FUNCV for
    // vararg) with A=framesize.
    let header_op = if flags & PROTO_VARARG != 0 {
        Opcode::Funcv
    } else {
        Opcode::Funcf
    };
    let mut insts = Vec::with_capacity(numbc as usize + 1);
    insts.push(Instruction::synthetic_header(header_op, framesize));
    for _ in 0..numbc {
        insts.push(Instruction::read(reader)?);
    }

    // uvdata: numuv descriptors, each H (16-bit LE).
    let mut upvalues = Vec::with_capacity(numuv as usize);
    for _ in 0..numuv {
        upvalues.push(UpvalDesc {
            raw: reader.read_u16()?,
        });
    }

    // kgc: numkgc GC constants. Stored in the file in a specific
    // order but referenced via reverse index; we keep them in file
    // order and let later passes do the reverse lookup.
    let mut gc_consts = Vec::with_capacity(numkgc as usize);
    for _ in 0..numkgc {
        gc_consts.push(read_gc_const(reader)?);
    }

    // knum: numkn number constants (33-bit ULEB128, format doc §6.3).
    let mut num_consts = Vec::with_capacity(numkn as usize);
    for _ in 0..numkn {
        num_consts.push(read_num_const(reader)?);
    }

    // Debug section. Present only when !STRIP and sizedbg > 0. We
    // record the start position so we can verify the section's
    // declared size matches what we consume.
    let debug = if !stripped && sizedbg > 0 {
        let dbg_start = reader.pos();
        let line_info = read_line_info(reader, numbc, firstline, numline)?;
        let upvalue_names = read_upvalue_names(reader, u32::from(numuv))?;
        let var_info = read_var_info(reader, numparams)?;
        let consumed = reader.pos() - dbg_start;
        if consumed != sizedbg {
            return Err(DecompilerError::InvalidBytecode {
                offset: reader.pos(),
                reason: format!(
                    "debug section length mismatch: declared {} bytes but consumed {}",
                    sizedbg, consumed
                ),
            });
        }
        Some(DebugInfo {
            firstline,
            numline,
            line_info,
            upvalue_names,
            var_info,
        })
    } else {
        None
    };

    Ok(Proto {
        flags,
        numparams,
        framesize,
        upvalues,
        gc_consts,
        num_consts,
        insts,
        debug,
    })
}

// ---- GC constant parsing (format doc §6.1, §6.2) --------------------

fn read_gc_const(reader: &mut Reader<'_>) -> Result<GcConst, DecompilerError> {
    let type_tag = reader.read_uleb128()?;
    let const_val = match type_tag {
        // KGC_CHILD: no payload.
        0 => GcConst::Child,
        // KGC_TAB: template table.
        1 => {
            let narray = reader.read_uleb128()?;
            let nhash = reader.read_uleb128()?;
            let mut array = Vec::with_capacity(narray as usize);
            for _ in 0..narray {
                array.push(read_ktabk(reader)?);
            }
            let mut hash = Vec::with_capacity(nhash as usize);
            for _ in 0..nhash {
                let key = read_ktabk(reader)?;
                let value = read_ktabk(reader)?;
                hash.push((key, value));
            }
            GcConst::Tab(TableConst { array, hash })
        }
        // KGC_I64 / KGC_U64: two ULEB128s forming a 64-bit value.
        2 => {
            let lo = u64::from(reader.read_uleb128()?);
            let hi = u64::from(reader.read_uleb128()?);
            GcConst::I64(lo | (hi << 32))
        }
        3 => {
            let lo = u64::from(reader.read_uleb128()?);
            let hi = u64::from(reader.read_uleb128()?);
            GcConst::U64(lo | (hi << 32))
        }
        // KGC_COMPLEX: real and imaginary doubles, each as 2x ULEB128.
        4 => {
            let rlo = u64::from(reader.read_uleb128()?);
            let rhi = u64::from(reader.read_uleb128()?);
            let ilo = u64::from(reader.read_uleb128()?);
            let ihi = u64::from(reader.read_uleb128()?);
            GcConst::Complex {
                real: f64::from_bits((rhi << 32) | rlo),
                imag: f64::from_bits((ihi << 32) | ilo),
            }
        }
        // KGC_STR: string of length (type - 5).
        n if n >= 5 => {
            let len = (n - 5) as usize;
            let bytes = reader.read_bytes(len)?;
            GcConst::Str(bytes.to_vec())
        }
        _ => {
            return Err(DecompilerError::InvalidBytecode {
                offset: reader.pos(),
                reason: format!("invalid KGC type tag {}", type_tag),
            });
        }
    };
    Ok(const_val)
}

/// Read a single table key/value (format doc §6.2 `BCDUMP_KTAB_*`).
fn read_ktabk(reader: &mut Reader<'_>) -> Result<KtabK, DecompilerError> {
    let type_tag = reader.read_uleb128()?;
    let val = match type_tag {
        0 => KtabK::Nil,
        1 => KtabK::False,
        2 => KtabK::True,
        3 => KtabK::Int(u64::from(reader.read_uleb128()?)),
        4 => {
            let lo = u64::from(reader.read_uleb128()?);
            let hi = u64::from(reader.read_uleb128()?);
            KtabK::Num(f64::from_bits((hi << 32) | lo))
        }
        n if n >= 5 => {
            let len = (n - 5) as usize;
            let bytes = reader.read_bytes(len)?;
            KtabK::Str(bytes.to_vec())
        }
        _ => {
            return Err(DecompilerError::InvalidBytecode {
                offset: reader.pos(),
                reason: format!("invalid KTAB type tag {}", type_tag),
            });
        }
    };
    Ok(val)
}

// ---- Number constant parsing (format doc §6.3) ----------------------

fn read_num_const(reader: &mut Reader<'_>) -> Result<NumConst, DecompilerError> {
    let (val, tag) = reader.read_uleb128_33()?;
    if tag == 0 {
        // Integer constant. Sign-extend from 32 bits.
        Ok(NumConst::Int(val as i32))
    } else {
        // Floating-point: val is low 32 bits; read the high 32 bits
        // of the IEEE-754 double as a separate ULEB128.
        let hi = u64::from(reader.read_uleb128()?);
        let lo = u64::from(val);
        Ok(NumConst::Num(f64::from_bits((hi << 32) | lo)))
    }
}

// ---- Debug section parsing (format doc §8) --------------------------

/// Read the per-instruction line-info array. Each entry is a delta
/// relative to `firstline`; the width depends on `numline`. The
/// returned values are absolute source lines for each real
/// instruction.
fn read_line_info(
    reader: &mut Reader<'_>,
    numbc: u32,
    firstline: u32,
    numline: u32,
) -> Result<Vec<u32>, DecompilerError> {
    let mut deltas = Vec::with_capacity(numbc as usize);
    if numline < 256 {
        for _ in 0..numbc {
            deltas.push(u32::from(reader.read_u8()?));
        }
    } else if numline < 65536 {
        for _ in 0..numbc {
            deltas.push(u32::from(reader.read_u16()?));
        }
    } else {
        for _ in 0..numbc {
            deltas.push(reader.read_u32()?);
        }
    }
    Ok(deltas
        .into_iter()
        .map(|d| firstline.wrapping_add(d))
        .collect())
}

/// Read `numuv` null-terminated upvalue name strings.
fn read_upvalue_names(reader: &mut Reader<'_>, numuv: u32) -> Result<Vec<String>, DecompilerError> {
    let mut names = Vec::with_capacity(numuv as usize);
    for _ in 0..numuv {
        names.push(read_cstring(reader)?);
    }
    Ok(names)
}

/// Read bytes until a `0x00` terminator, returning them as a UTF-8
/// string (lossy: upvalue names are conventionally ASCII).
fn read_cstring(reader: &mut Reader<'_>) -> Result<String, DecompilerError> {
    let mut bytes = Vec::new();
    loop {
        let b = reader.read_u8()?;
        if b == 0 {
            break;
        }
        bytes.push(b);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Read var-info records until the `0x00` terminator (format doc
/// §8.1). Each record is: a 1-byte type, then a ULEB128 `scope_delta`
/// applied to a running `scope_offset`, then a parameter-specific or
/// local-specific ULEB128 to fix up scope begin/end.
///
/// `numparams` is the proto's parameter count. It is needed for the
/// post-pass that computes each record's slot: parameters occupy
/// slots `0..numparams-1` and locals start at slot `numparams` and
/// grow via first-fit allocation.
fn read_var_info(reader: &mut Reader<'_>, numparams: u8) -> Result<Vec<VarInfo>, DecompilerError> {
    let mut records = Vec::new();
    let mut scope_offset: u32 = 0;
    loop {
        let type_byte = reader.read_u8()?;
        if type_byte == 0 {
            break;
        }

        let (kind, name) = if type_byte >= VAR_STR {
            // String name: the type byte is the first character.
            let mut name = String::new();
            name.push(type_byte as char);
            name.push_str(&read_cstring(reader)?);
            (VarKind::Name, Some(name))
        } else {
            let kind = match type_byte {
                1 => VarKind::ForIdx,
                2 => VarKind::ForStop,
                3 => VarKind::ForStep,
                4 => VarKind::ForGen,
                5 => VarKind::ForState,
                6 => VarKind::ForCtl,
                other => {
                    return Err(DecompilerError::InvalidBytecode {
                        offset: reader.pos() - 1,
                        reason: format!("invalid var-info type byte {}", other),
                    });
                }
            };
            (kind, None)
        };

        scope_offset = scope_offset.wrapping_add(reader.read_uleb128()?);
        // scope_offset of exactly 1 is forbidden (means the wrap to 0
        // happened with a non-zero delta; rejected by reference impl).
        if scope_offset == 1 {
            return Err(DecompilerError::InvalidBytecode {
                offset: reader.pos(),
                reason: "invalid var-info scope offset of 1".to_string(),
            });
        }

        let (is_parameter, scope_begin, scope_end) = if scope_offset == 0 {
            // Parameter: scope_begin implicit; read scopeEnd ULEB and
            // adjust by -2 to match the on-disk encoding.
            let scope_end_raw = reader.read_uleb128()?;
            (true, 0, scope_end_raw.wrapping_sub(2))
        } else {
            // Regular local: scope_begin = scope_offset - 2; read
            // length; scope_end = scope_begin + length.
            let scope_begin = scope_offset - 2;
            let len = reader.read_uleb128()?;
            (false, scope_begin, scope_begin.wrapping_add(len))
        };

        records.push(VarInfo {
            kind,
            name,
            is_parameter,
            slot: 0,
            scope_begin,
            scope_end,
        });
    }

    // Post-pass: compute slots via first-fit allocation, mirroring
    // LuaJIT's register allocator. Parameters are assigned sequential
    // slots 0, 1, 2, ... in declaration order. Locals start at slot
    // `numparams` and take the lowest slot not held by any prior
    // local whose scope overlaps this one — two scopes overlap when
    // `prev.scope_end >= current.scope_begin` (the previous variable
    // is still live at the point the new one starts). Slots occupied
    // by parameters are never reused by locals (parameters are live
    // for the whole proto).
    //
    // The borrow checker forbids reading `records[..i]` from inside
    // `records.iter_mut()`, so we compute each slot in a separate
    // pass over indices and assign via direct indexing.
    let mut next_param_slot = 0u8;
    for i in 0..records.len() {
        let slot = if records[i].is_parameter {
            let s = next_param_slot;
            next_param_slot += 1;
            s
        } else {
            let mut slot = numparams;
            loop {
                let occupied = records[..i].iter().any(|prev| {
                    !prev.is_parameter
                        && prev.slot == slot
                        && prev.scope_end >= records[i].scope_begin
                });
                if !occupied {
                    break;
                }
                slot += 1;
            }
            slot
        };
        records[i].slot = slot;
    }

    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic bytecode dump with the given pdata block.
    /// Useful for unit-testing the parser without reaching for the
    /// luajit-compiled fixture.
    fn make_dump(flags: u32, chunkname: Option<&[u8]>, pdata: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&[0x1b, 0x4c, 0x4a, 0x02]);
        out.push(flags as u8); // single-byte ULEB for the flags we use
        if let Some(name) = chunkname {
            out.push(name.len() as u8); // single-byte ULEB
            out.extend_from_slice(name);
        }
        // Proto block length (single-byte ULEB).
        out.push(pdata.len() as u8);
        out.extend_from_slice(pdata);
        // Dump terminator.
        out.push(0x00);
        out
    }

    // ---- Byte-encoding helpers for synthetic pdata -------------------
    //
    // These encode small values the way the parser expects, so the
    // tests below construct payloads by *intent* rather than by
    // hand-computed byte tables.

    /// Encode a `u32` as a standard ULEB128.
    fn uleb128_bytes(mut v: u32) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
                out.push(b);
            } else {
                out.push(b);
                break;
            }
        }
        out
    }

    /// Encode a value + tag bit as a 33-bit ULEB128 (format doc §6.3).
    /// `value` is the raw 32-bit value; `tag` is the low bit of byte0.
    fn uleb128_33_bytes(value: u32, tag: u8) -> Vec<u8> {
        let mut v = u64::from(value);
        let mut out = Vec::new();
        // First byte: bits [6:1] hold the low 6 value bits; bit 0 is
        // the tag; bit 7 is the continuation flag.
        let low6 = (v & 0x3f) as u8;
        v >>= 6;
        let mut first = (low6 << 1) | (tag & 1);
        if v == 0 {
            out.push(first);
            return out;
        }
        first |= 0x80;
        out.push(first);
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            } else {
                out.push(b | 0x80);
            }
        }
        out
    }

    /// Build a stripped proto's pdata block from its GC-constant and
    /// number-constant sections. The instruction count is derived from
    /// `bcins` (4 bytes per instruction). No upvalues, no debug.
    fn stripped_pdata(numkgc: u32, kgc: &[u8], numkn: u32, kn: &[u8], bcins: &[u8]) -> Vec<u8> {
        let mut out = vec![0x02, 0x00, 0x01, 0x00]; // flags=VARARG, params=0, framesize=1, numuv=0
        out.extend_from_slice(&uleb128_bytes(numkgc));
        out.extend_from_slice(&uleb128_bytes(numkn));
        out.extend_from_slice(&uleb128_bytes((bcins.len() / 4) as u32));
        out.extend_from_slice(bcins);
        out.extend_from_slice(kgc);
        out.extend_from_slice(kn);
        out
    }

    /// Parse a single stripped proto and return it.
    fn parse_one_stripped(pdata: &[u8]) -> Proto {
        let dump = make_dump(FLAG_STRIP, None, pdata);
        let module = Module::from_bytes(&dump).expect("stripped dump should parse");
        assert_eq!(module.protos.len(), 1);
        module.protos.into_iter().next().unwrap()
    }

    // ---- KtabK encoders (format doc §6.2) ----------------------------

    fn ktabk_nil() -> Vec<u8> {
        vec![0x00]
    }
    fn ktabk_false() -> Vec<u8> {
        vec![0x01]
    }
    fn ktabk_true() -> Vec<u8> {
        vec![0x02]
    }
    fn ktabk_int(v: u32) -> Vec<u8> {
        let mut o = vec![0x03];
        o.extend(uleb128_bytes(v));
        o
    }
    fn ktabk_num(f: f64) -> Vec<u8> {
        let bits = f.to_bits();
        let lo = bits as u32;
        let hi = (bits >> 32) as u32;
        let mut o = vec![0x04];
        o.extend(uleb128_bytes(lo));
        o.extend(uleb128_bytes(hi));
        o
    }
    fn ktabk_str(s: &[u8]) -> Vec<u8> {
        let mut o = vec![5u8 + s.len() as u8];
        o.extend_from_slice(s);
        o
    }

    #[test]
    fn rejects_bad_magic() {
        let bytes = [0x00, 0x00, 0x00, 0x02, 0x00, 0x00];
        let err = Module::from_bytes(&bytes).unwrap_err();
        match err {
            DecompilerError::InvalidBytecode { offset, reason } => {
                assert_eq!(offset, 0);
                assert!(reason.contains("magic"));
            }
            other => panic!("expected InvalidBytecode, got {:?}", other),
        }
    }

    #[test]
    fn rejects_unsupported_version() {
        let bytes = [0x1b, 0x4c, 0x4a, 0x03, 0x00, 0x00];
        let err = Module::from_bytes(&bytes).unwrap_err();
        match err {
            DecompilerError::InvalidBytecode { offset, reason } => {
                assert_eq!(offset, 3);
                assert!(reason.contains("version"));
            }
            other => panic!("expected InvalidBytecode, got {:?}", other),
        }
    }

    #[test]
    fn rejects_big_endian_flag() {
        // flags ULEB = 0x01 (BE).
        let bytes = [0x1b, 0x4c, 0x4a, 0x02, 0x01];
        let err = Module::from_bytes(&bytes).unwrap_err();
        assert!(matches!(err, DecompilerError::InvalidBytecode { .. }));
    }

    #[test]
    fn rejects_unknown_flag_bits() {
        // flags ULEB = 0x10 (unknown bit 4).
        let bytes = [0x1b, 0x4c, 0x4a, 0x02, 0x10];
        let err = Module::from_bytes(&bytes).unwrap_err();
        match err {
            DecompilerError::InvalidBytecode { reason, .. } => {
                assert!(reason.contains("unknown header flags"));
            }
            other => panic!("expected InvalidBytecode, got {:?}", other),
        }
    }

    #[test]
    fn rejects_dump_with_no_protos() {
        // STRIP flag set (so no chunkname section), then the dump
        // terminator. Header parses cleanly but the proto loop finds
        // no protos.
        let bytes = [0x1b, 0x4c, 0x4a, 0x02, FLAG_STRIP as u8, 0x00];
        let err = Module::from_bytes(&bytes).unwrap_err();
        match err {
            DecompilerError::InvalidBytecode { reason, .. } => {
                assert!(reason.contains("no protos"));
            }
            other => panic!("expected InvalidBytecode, got {:?}", other),
        }
    }

    #[test]
    fn parses_fixture_return_only_chunk() {
        // The fixture is compiled by luajit from `return\n`. We
        // expect one proto (the main chunk) whose in-memory
        // instruction stream is [FUNCV (synthetic), RET0].
        let bytes = include_bytes!("../../tests/fixtures/return/input.bc");
        let module = Module::from_bytes(bytes).expect("fixture should parse");
        assert_eq!(module.protos.len(), 1, "expected exactly one proto");
        let main = module.main_proto();
        assert!(main.is_vararg(), "main chunk should be VARARG");
        assert_eq!(
            main.insts.len(),
            2,
            "expected 2 instructions (FUNCV header + RET0), got {:?}",
            main.insts
        );
        assert_eq!(main.insts[0].op, Opcode::Funcv);
        assert_eq!(main.insts[0].a, main.framesize);
        assert_eq!(main.insts[1].op, Opcode::Ret0);
        // Debug info should be present (the fixture was compiled with -bg).
        assert!(
            main.debug.is_some(),
            "fixture should have debug info (compiled with -bg)"
        );
    }

    /// A smoke test of a stripped dump (no chunkname, no debug info).
    /// The proto's data still parses cleanly without the debug
    /// section.
    #[test]
    fn parses_stripped_proto() {
        // flags = STRIP. A single proto: VARARG, numparams=0,
        // framesize=1, numuv=0, numkgc=0, numkn=0, numbc=1, then one
        // RET0 instruction. No sizedbg, no debug section.
        let pdata: &[u8] = &[
            0x02, // flags = VARARG
            0x00, // numparams
            0x01, // framesize
            0x00, // numuv
            0x00, // numkgc
            0x00, // numkn
            0x01, // numbc
            // bcins: RET0 (op=0x4b) A=0 D=1 (AD format).
            0x4b, 0x00, 0x01, 0x00,
        ];
        let dump = make_dump(FLAG_STRIP, None, pdata);
        let module = Module::from_bytes(&dump).expect("stripped dump should parse");
        let main = module.main_proto();
        assert_eq!(main.insts.len(), 2);
        assert_eq!(main.insts[0].op, Opcode::Funcv);
        assert_eq!(main.insts[1].op, Opcode::Ret0);
        assert!(
            main.debug.is_none(),
            "stripped proto should have no debug info"
        );
    }

    #[test]
    fn opcode_from_byte_round_trips_all_variants() {
        for b in 0x00..=0x60u8 {
            let op = Opcode::from_byte(b, 0).expect("in-range byte should decode");
            assert_eq!(op as u8, b, "opcode {:?} should have value {}", op, b);
        }
    }

    #[test]
    fn proto_parse_errors_carry_absolute_offsets() {
        // Build a dump whose single proto contains an invalid opcode
        // byte. The error should land at the absolute offset of that
        // byte (header + chunkname length ULEB + chunkname bytes +
        // proto-length ULEB + phead bytes + instruction's op byte).
        //
        // Layout (stripped, so no chunkname section):
        //   [0..3] magic
        //   [3]    version
        //   [4]    flags = STRIP
        //   [5]    proto length ULEB = 11
        //   [6]    phead: flags
        //   [7]    numparams
        //   [8]    framesize
        //   [9]    numuv
        //   [10]   numkgc
        //   [11]   numkn
        //   [12]   numbc = 1
        //   [13..16] instruction: 0xff (bad opcode) 0x00 0x00 0x00
        let mut dump = vec![0x1b, 0x4c, 0x4a, 0x02, FLAG_STRIP as u8, 11];
        // phead: VARARG, 0 params, framesize 1, 0 uv, 0 kgc, 0 kn, 1 bc.
        dump.extend_from_slice(&[0x02, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01]);
        // instruction with invalid opcode byte 0xff.
        dump.extend_from_slice(&[0xff, 0x00, 0x00, 0x00]);
        let err = Module::from_bytes(&dump).unwrap_err();
        match err {
            DecompilerError::InvalidBytecode { offset, reason } => {
                // The instruction starts at absolute offset 13; the
                // op byte is the first byte of the instruction.
                assert_eq!(offset, 13);
                assert!(reason.contains("opcode"));
            }
            other => panic!("expected InvalidBytecode, got {:?}", other),
        }
    }

    #[test]
    fn opcode_from_byte_rejects_out_of_range() {
        assert!(Opcode::from_byte(0x61, 0).is_err());
        assert!(Opcode::from_byte(0xff, 0).is_err());
    }

    #[test]
    fn instruction_helpers_extract_b_and_d() {
        let abc = Instruction {
            op: Opcode::Addvv,
            a: 1,
            b_or_d: 200,
            c: 7,
        };
        assert_eq!(abc.b(), 200);
        let ad = Instruction {
            op: Opcode::Ret0,
            a: 0,
            b_or_d: 0x1234,
            c: 0,
        };
        assert_eq!(ad.d(), 0x1234);
    }

    // ---- GC-constant (KGC) variant coverage (format doc §6.1) --------

    #[test]
    fn parses_kgc_child() {
        // KGC_CHILD (tag 0): no payload.
        let kgc = vec![0x00];
        let pdata = stripped_pdata(1, &kgc, 0, &[], &[]);
        let proto = parse_one_stripped(&pdata);
        assert_eq!(proto.gc_consts.len(), 1);
        assert!(
            matches!(proto.gc_consts[0], GcConst::Child),
            "expected GcConst::Child"
        );
    }

    #[test]
    fn parses_kgc_tab_array_only() {
        // KGC_TAB: narray=2, nhash=0. Array = [True, Nil] (covers the
        // KTAB_TRUE and KTAB_NIL ktabk variants).
        let mut kgc = vec![0x01, 0x02, 0x00]; // tag, narray=2, nhash=0
        kgc.extend(ktabk_true());
        kgc.extend(ktabk_nil());
        let pdata = stripped_pdata(1, &kgc, 0, &[], &[]);
        let proto = parse_one_stripped(&pdata);
        let tab = match &proto.gc_consts[0] {
            GcConst::Tab(t) => t,
            other => panic!("expected GcConst::Tab, got {:?}", other),
        };
        assert_eq!(tab.array.len(), 2);
        assert!(matches!(tab.array[0], KtabK::True));
        assert!(matches!(tab.array[1], KtabK::Nil));
        assert!(tab.hash.is_empty(), "hash part should be empty");
    }

    #[test]
    fn parses_kgc_tab_hash_only() {
        // KGC_TAB: narray=0, nhash=1. Hash = { Str("k"): Num(1.5) }
        // (covers the KTAB_STR and KTAB_NUM ktabk variants).
        let mut kgc = vec![0x01, 0x00, 0x01]; // tag, narray=0, nhash=1
        kgc.extend(ktabk_str(b"k")); // key
        kgc.extend(ktabk_num(1.5)); // value
        let pdata = stripped_pdata(1, &kgc, 0, &[], &[]);
        let proto = parse_one_stripped(&pdata);
        let tab = match &proto.gc_consts[0] {
            GcConst::Tab(t) => t,
            other => panic!("expected GcConst::Tab, got {:?}", other),
        };
        assert!(tab.array.is_empty(), "array part should be empty");
        assert_eq!(tab.hash.len(), 1);
        match (&tab.hash[0].0, &tab.hash[0].1) {
            (KtabK::Str(k), KtabK::Num(v)) => {
                assert_eq!(k, b"k");
                assert_eq!(*v, 1.5);
            }
            other => panic!("hash entry shape mismatch: {:?}", other),
        }
    }

    #[test]
    fn parses_kgc_tab_array_and_hash() {
        // KGC_TAB: narray=1, nhash=1. Array = [False]; hash = { Int(1): Str("v") }
        // (covers the KTAB_FALSE, KTAB_INT, and KTAB_STR ktabk variants).
        let mut kgc = vec![0x01, 0x01, 0x01]; // tag, narray=1, nhash=1
        kgc.extend(ktabk_false()); // array[0]
        kgc.extend(ktabk_int(1)); // hash key
        kgc.extend(ktabk_str(b"v")); // hash value
        let pdata = stripped_pdata(1, &kgc, 0, &[], &[]);
        let proto = parse_one_stripped(&pdata);
        let tab = match &proto.gc_consts[0] {
            GcConst::Tab(t) => t,
            other => panic!("expected GcConst::Tab, got {:?}", other),
        };
        assert_eq!(tab.array.len(), 1);
        assert!(matches!(tab.array[0], KtabK::False));
        assert_eq!(tab.hash.len(), 1);
        match (&tab.hash[0].0, &tab.hash[0].1) {
            (KtabK::Int(k), KtabK::Str(v)) => {
                assert_eq!(*k, 1);
                assert_eq!(v, b"v");
            }
            other => panic!("hash entry shape mismatch: {:?}", other),
        }
    }

    #[test]
    fn parses_kgc_i64() {
        // KGC_I64 (tag 2): lo ULEB + hi ULEB. value = lo | (hi << 32).
        // Pick lo=1, hi=2 -> value = 0x2_0000_0001.
        let mut kgc = vec![0x02];
        kgc.extend(uleb128_bytes(1)); // lo
        kgc.extend(uleb128_bytes(2)); // hi
        let pdata = stripped_pdata(1, &kgc, 0, &[], &[]);
        let proto = parse_one_stripped(&pdata);
        match &proto.gc_consts[0] {
            GcConst::I64(v) => assert_eq!(*v, 0x2_0000_0001),
            other => panic!("expected GcConst::I64, got {:?}", other),
        }
    }

    #[test]
    fn parses_kgc_u64() {
        // KGC_U64 (tag 3): lo ULEB + hi ULEB. lo=0, hi=1 -> value = 0x1_0000_0000.
        let mut kgc = vec![0x03];
        kgc.extend(uleb128_bytes(0)); // lo
        kgc.extend(uleb128_bytes(1)); // hi
        let pdata = stripped_pdata(1, &kgc, 0, &[], &[]);
        let proto = parse_one_stripped(&pdata);
        match &proto.gc_consts[0] {
            GcConst::U64(v) => assert_eq!(*v, 0x1_0000_0000),
            other => panic!("expected GcConst::U64, got {:?}", other),
        }
    }

    #[test]
    fn parses_kgc_complex() {
        // KGC_COMPLEX (tag 4): rlo rhi ilo ihi, each ULEB. real/imag
        // are IEEE-754 doubles reassembled as (hi << 32) | lo.
        let real = 1.5_f64;
        let imag = -2.25_f64;
        let rbits = real.to_bits();
        let ibits = imag.to_bits();
        let mut kgc = vec![0x04];
        kgc.extend(uleb128_bytes(rbits as u32));
        kgc.extend(uleb128_bytes((rbits >> 32) as u32));
        kgc.extend(uleb128_bytes(ibits as u32));
        kgc.extend(uleb128_bytes((ibits >> 32) as u32));
        let pdata = stripped_pdata(1, &kgc, 0, &[], &[]);
        let proto = parse_one_stripped(&pdata);
        match &proto.gc_consts[0] {
            GcConst::Complex { real: r, imag: i } => {
                assert_eq!(*r, real);
                assert_eq!(*i, imag);
            }
            other => panic!("expected GcConst::Complex, got {:?}", other),
        }
    }

    #[test]
    fn parses_kgc_str() {
        // KGC_STR (tag >= 5): string of length (tag - 5).
        let kgc = ktabk_str(b"hello"); // tag = 5 + 5 = 10
        let pdata = stripped_pdata(1, &kgc, 0, &[], &[]);
        let proto = parse_one_stripped(&pdata);
        match &proto.gc_consts[0] {
            GcConst::Str(b) => assert_eq!(b, b"hello"),
            other => panic!("expected GcConst::Str, got {:?}", other),
        }
    }

    // ---- Number-constant coverage (format doc §6.3) -----------------

    #[test]
    fn parses_int_constants_positive_and_negative() {
        // Three integer constants via the 33-bit ULEB128 (tag=0):
        //   42 (small positive), -1 (all bits set), -1000.
        // Verified sign-extension: NumConst::Int(val as i32).
        let mut kn = Vec::new();
        kn.extend(uleb128_33_bytes(42, 0));
        kn.extend(uleb128_33_bytes(0xFFFF_FFFF, 0)); // -1 as u32
        kn.extend(uleb128_33_bytes((-1000i32) as u32, 0));
        let pdata = stripped_pdata(0, &[], 3, &kn, &[]);
        let proto = parse_one_stripped(&pdata);
        assert_eq!(proto.num_consts.len(), 3);
        match proto.num_consts[0] {
            NumConst::Int(v) => assert_eq!(v, 42),
            other => panic!("expected NumConst::Int(42), got {:?}", other),
        }
        match proto.num_consts[1] {
            NumConst::Int(v) => assert_eq!(v, -1),
            other => panic!("expected NumConst::Int(-1), got {:?}", other),
        }
        match proto.num_consts[2] {
            NumConst::Int(v) => assert_eq!(v, -1000),
            other => panic!("expected NumConst::Int(-1000), got {:?}", other),
        }
    }

    // ---- Multi-proto dump with named locals --------------------------

    #[test]
    fn parses_multi_proto_dump_with_named_locals() {
        // A non-stripped dump with two protos in children-first order:
        //   proto 1: a simple child (1 RET0, no debug, sizedbg=0).
        //   proto 2: the main chunk (PROTO_CHILD flag, 1 KGC_CHILD gc
        //            constant, debug section with a parameter "a" and a
        //            named local "x").
        //
        // The main chunk must be the LAST proto in the vector.
        //
        // Layout per format doc §3: phead (flags, numparams, framesize,
        // numuv) + 3 ULEBs (numkgc, numkn, numbc) + [if !strip:
        // sizedbg, [if >0: firstline, numline]] + bcins + uvdata + kgc
        // + kn + [if sizedbg>0: debug section].

        // --- child proto pdata ---
        let mut pdata_child = Vec::new();
        pdata_child.extend_from_slice(&[0x00, 0x00, 0x01, 0x00]); // flags=0, params=0, framesize=1, numuv=0
        pdata_child.extend_from_slice(&[0x00, 0x00, 0x01]); // numkgc=0, numkn=0, numbc=1
        pdata_child.push(0x00); // sizedbg=0 (no debug section)
        pdata_child.extend_from_slice(&[0x4b, 0x00, 0x01, 0x00]); // RET0 A=0 D=1

        // --- main proto pdata ---
        // Debug section first, so sizedbg can be computed exactly:
        //   line_info: numbc=1 entries, numline=1 (<256) -> 1 byte each.
        //     entry[0] = delta 0.
        //   upvalue names: numuv=0 -> nothing.
        //   var_info:
        //     parameter "a": type='a' 0x00(empty cstring) scope_delta=0 scopeEnd_raw=5
        //     local "x":     type='x' 0x00                   scope_delta=3 len=2
        //     terminator 0x00
        let mut debug = Vec::new();
        debug.push(0x00); // line_info[0] = 0
        debug.extend_from_slice(&[0x61, 0x00, 0x00, 0x05]); // parameter "a"
        debug.extend_from_slice(&[0x78, 0x00, 0x03, 0x02]); // local "x"
        debug.push(0x00); // var-info terminator
        let sizedbg = debug.len();

        let mut pdata_main = Vec::new();
        pdata_main.extend_from_slice(&[PROTO_CHILD, 0x01, 0x02, 0x00]); // flags, numparams=1, framesize=2, numuv=0
        pdata_main.extend_from_slice(&[0x01, 0x00, 0x01]); // numkgc=1, numkn=0, numbc=1
        pdata_main.extend(uleb128_bytes(sizedbg as u32)); // sizedbg
        pdata_main.extend_from_slice(&[0x01, 0x01]); // firstline=1, numline=1
        pdata_main.extend_from_slice(&[0x4b, 0x00, 0x01, 0x00]); // RET0 A=0 D=1
                                                                 // (no uvdata)
        pdata_main.push(0x00); // kgc: 1 x KGC_CHILD
                               // (no kn)
        pdata_main.extend_from_slice(&debug);

        // --- assemble the full non-stripped dump ---
        let mut dump = Vec::new();
        dump.extend_from_slice(&[0x1b, 0x4c, 0x4a, 0x02]); // magic + version
        dump.push(0x00); // flags ULEB = 0 (not stripped, LE)
        let chunkname = b"@test";
        dump.push(chunkname.len() as u8); // namelen ULEB
        dump.extend_from_slice(chunkname);
        // child proto (children-first).
        dump.extend(uleb128_bytes(pdata_child.len() as u32));
        dump.extend_from_slice(&pdata_child);
        // main proto (last).
        dump.extend(uleb128_bytes(pdata_main.len() as u32));
        dump.extend_from_slice(&pdata_main);
        dump.push(0x00); // dump terminator

        let module = Module::from_bytes(&dump).expect("multi-proto dump should parse");

        // Both protos parsed; main chunk is last.
        assert_eq!(module.protos.len(), 2, "expected exactly two protos");
        assert!(std::ptr::eq(module.main_proto(), &module.protos[1]));

        // Main chunk carries PROTO_CHILD and the KGC_CHILD constant.
        let main = module.main_proto();
        assert!(
            main.flags & PROTO_CHILD != 0,
            "main chunk should have PROTO_CHILD set"
        );
        assert_eq!(main.gc_consts.len(), 1);
        assert!(matches!(main.gc_consts[0], GcConst::Child));

        // Debug info present with a parameter record and a named local.
        let debug = main
            .debug
            .as_ref()
            .expect("main chunk should carry debug info");
        let has_param = debug
            .var_info
            .iter()
            .any(|v| v.is_parameter && v.name.as_deref() == Some("a"));
        assert!(
            has_param,
            "expected a parameter named \"a\", got {:?}",
            debug.var_info
        );
        let has_local_x = debug
            .var_info
            .iter()
            .any(|v| !v.is_parameter && v.name.as_deref() == Some("x"));
        assert!(
            has_local_x,
            "expected a named local \"x\", got {:?}",
            debug.var_info
        );
    }

    // ---- Var-info slot allocation (Stage 3) --------------------------
    //
    // These tests build non-stripped single-proto dumps with debug
    // sections and verify that the parser's first-fit slot-allocation
    // post-pass assigns each local the right register. The slot field
    // is derived from var_info ordering + scope ranges; it's not on
    // the wire.

    /// Build a non-stripped dump with one proto carrying the given
    /// instructions, framesize, and raw debug-section bytes. The
    /// debug bytes are emitted as-is after the bcins (no uv/kgc/kn),
    /// and `sizedbg` is computed from their length. `numline` is
    /// forced small (< 256) so the line-info width is 1 byte/entry —
    /// the tests construct the delta array explicitly.
    fn dump_with_debug(
        framesize: u8,
        numbc: u32,
        bcins: &[u8],
        firstline: u32,
        numline: u32,
        line_info: &[u8],
        var_info_bytes: &[u8],
    ) -> Vec<u8> {
        // Assemble the debug section: line_info bytes, then upvalue
        // names (none here, numuv=0), then var_info bytes.
        let mut debug = Vec::new();
        debug.extend_from_slice(line_info);
        debug.extend_from_slice(var_info_bytes);
        let sizedbg = debug.len() as u32;

        let mut pdata = Vec::new();
        // flags=VARARG, numparams=0, framesize, numuv=0.
        pdata.extend_from_slice(&[0x02, 0x00, framesize, 0x00]);
        // numkgc=0, numkn=0, numbc.
        pdata.extend_from_slice(&[0x00, 0x00]);
        pdata.extend(uleb128_bytes(numbc));
        // sizedbg.
        pdata.extend(uleb128_bytes(sizedbg));
        // firstline, numline.
        pdata.extend(uleb128_bytes(firstline));
        pdata.extend(uleb128_bytes(numline));
        // bcins.
        pdata.extend_from_slice(bcins);
        // (no uvdata / kgc / knum)
        // debug section.
        pdata.extend_from_slice(&debug);

        make_dump(0x00, Some(b"@slot-test"), &pdata)
    }

    /// Encode a var-info record for a named local with the given
    /// scope-delta and length. The record's wire form is:
    /// `<name_first_byte> <cstring_terminator> <scope_delta ULEB>
    /// <len ULEB>`. Caller passes the raw scope_delta (the parser
    /// adds it to a running offset internally).
    fn var_info_local(name_first: u8, scope_delta: u32, len: u32) -> Vec<u8> {
        let mut v = vec![name_first, 0x00]; // name char + empty cstring
        v.extend(uleb128_bytes(scope_delta));
        v.extend(uleb128_bytes(len));
        v
    }

    #[test]
    fn var_info_slots_assigned_sequentially() {
        // `local x = 5; local y = 10; return x`:
        //   bcins: KSHORT 0 5, KSHORT 1 10, RET1 0 2
        //   var_info: x scope [0,2], y scope [1,2]
        // Expected slot allocation: x -> 0, y -> 1 (their scopes
        // overlap, so y can't reuse x's slot).
        let bcins = [
            0x29, 0x00, 0x05, 0x00, // KSHORT 0 5
            0x29, 0x01, 0x0a, 0x00, // KSHORT 1 10
            0x4c, 0x00, 0x02, 0x00, // RET1 0 2
        ];
        let mut var_info = Vec::new();
        // x: scope_offset 0 -> 2, scope_begin = 0, len 2 -> scope_end 2.
        var_info.extend(var_info_local(b'x', 2, 2));
        // y: scope_offset 2 -> 3, scope_begin = 1, len 1 -> scope_end 2.
        var_info.extend(var_info_local(b'y', 1, 1));
        var_info.push(0x00); // terminator
        let dump = dump_with_debug(
            /*framesize*/ 2,
            /*numbc*/ 3,
            &bcins,
            /*firstline*/ 1,
            /*numline*/ 2,
            /*line_info*/ &[0, 0, 0],
            &var_info,
        );
        let module = Module::from_bytes(&dump).expect("dump should parse");
        let main = module.main_proto();
        let debug = main.debug.as_ref().expect("debug should be present");
        assert_eq!(debug.var_info.len(), 2);
        let x = &debug.var_info[0];
        assert_eq!(x.name.as_deref(), Some("x"));
        assert_eq!(x.slot, 0, "x should be at slot 0");
        let y = &debug.var_info[1];
        assert_eq!(y.name.as_deref(), Some("y"));
        assert_eq!(y.slot, 1, "y should be at slot 1 (x still live)");
    }

    #[test]
    fn var_info_slots_reused_after_scope_ends() {
        // `do local a = 1 end do local b = 2 end return`:
        //   bcins: KSHORT 0 1, KSHORT 0 2, RET0 0 1
        //   var_info: a scope [0,0], b scope [1,2]
        // Expected slot allocation: a -> 0, b -> 0 (a's scope ended
        // before b begins, so the slot is free for reuse).
        let bcins = [
            0x29, 0x00, 0x01, 0x00, // KSHORT 0 1
            0x29, 0x00, 0x02, 0x00, // KSHORT 0 2
            0x4b, 0x00, 0x01, 0x00, // RET0 0 1
        ];
        let mut var_info = Vec::new();
        // a: scope_offset 0 -> 2, scope_begin = 0, len 0 -> scope_end 0.
        var_info.extend(var_info_local(b'a', 2, 0));
        // b: scope_offset 2 -> 3, scope_begin = 1, len 1 -> scope_end 2.
        var_info.extend(var_info_local(b'b', 1, 1));
        var_info.push(0x00); // terminator
        let dump = dump_with_debug(
            /*framesize*/ 1,
            /*numbc*/ 3,
            &bcins,
            /*firstline*/ 1,
            /*numline*/ 1,
            /*line_info*/ &[0, 0, 0],
            &var_info,
        );
        let module = Module::from_bytes(&dump).expect("dump should parse");
        let main = module.main_proto();
        let debug = main.debug.as_ref().expect("debug should be present");
        assert_eq!(debug.var_info.len(), 2);
        let a = &debug.var_info[0];
        assert_eq!(a.name.as_deref(), Some("a"));
        assert_eq!(a.slot, 0, "a should be at slot 0");
        let b = &debug.var_info[1];
        assert_eq!(b.name.as_deref(), Some("b"));
        assert_eq!(
            b.slot, 0,
            "b should reuse slot 0 (a's scope ended before b begins)"
        );
    }
}
