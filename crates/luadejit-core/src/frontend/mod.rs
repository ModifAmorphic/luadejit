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
use crate::ir::{FLAG_BE, FLAG_STRIP, FLAGS_KNOWN_MASK, PROTO_CHILD, PROTO_FFI, PROTO_VARARG, VAR_STR};
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
        let var_info = read_var_info(reader)?;
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
fn read_var_info(reader: &mut Reader<'_>) -> Result<Vec<VarInfo>, DecompilerError> {
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
            scope_begin,
            scope_end,
        });
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
}
