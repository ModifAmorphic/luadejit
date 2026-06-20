//! Bytecode IR: the on-disk LuaJIT 2.x format as parsed.
//!
//! This module is the single source of truth for bytecode-format data
//! types ([`Module`], [`Proto`], [`Instruction`], [`Opcode`], the
//! constant enums, [`UpvalDesc`], [`DebugInfo`], etc.) and the
//! format-level flag constants that describe their on-disk layout. It
//! is intentionally pure data plus the small accessors needed to
//! interpret that data; the parsing logic that *produces* these types
//! lives in [`crate::frontend`], and later pipeline stages (CFG, SSA,
//! analysis, structure) consume them.
//!
//! Opcode values and the ABC/AD format split follow the format doc §5
//! and §4.1 verbatim.

use crate::DecompilerError;

// ---- Header-level flag constants (format doc §2.1) -------------------

/// `BCDUMP_F_BE`: big-endian byte order. We only support little-endian.
pub const FLAG_BE: u32 = 0x01;
/// `BCDUMP_F_STRIP`: debug info stripped (no chunkname, no line/var info).
pub const FLAG_STRIP: u32 = 0x02;
/// `BCDUMP_F_FFI`: dump contains FFI cdata constants.
pub const FLAG_FFI: u32 = 0x04;
/// `BCDUMP_F_FR2`: 2-slot frame layout. Affects CALL/CALLM operand
/// interpretation in later stages, not the parsing itself.
pub const FLAG_FR2: u32 = 0x08;
/// Mask of all known flag bits. Any bit outside this mask (other than
/// the internal deterministic bit) makes the dump invalid.
pub const FLAGS_KNOWN_MASK: u32 = 0x0F;

// ---- Prototype-level flag constants (format doc §3.2) ---------------

/// `PROTO_CHILD`: has nested child prototypes (referenced via FNEW).
pub const PROTO_CHILD: u8 = 0x01;
/// `PROTO_VARARG`: vararg function (`...`). Affects which synthetic
/// FUNC* opcode sits at in-memory slot 0.
pub const PROTO_VARARG: u8 = 0x02;
/// `PROTO_FFI`: uses FFI.
pub const PROTO_FFI: u8 = 0x04;

// ---- Debug section: var-info type tags (format doc §8.1) ------------

/// Threshold at and above which a var-info type byte is treated as the
/// first character of a string name rather than a special for-loop
/// tag. Matches the `BC_VAR_STR` constant in the reference parser.
pub const VAR_STR: u8 = 7;

// ---- Module-level types ----------------------------------------------

/// A parsed bytecode module: the file-level header plus every
/// prototype, in the on-disk (children-first post-order) order. The
/// main chunk is always the last element of `protos`.
#[derive(Clone, Debug)]
pub struct Module {
    pub header: ModuleHeader,
    pub protos: Vec<Proto>,
}

impl Module {
    /// The main (root) proto. Per format doc §1, the last proto read
    /// from disk is the main chunk.
    pub fn main_proto(&self) -> &Proto {
        self.protos
            .last()
            .expect("a well-formed module always has at least one proto")
    }
}

/// File-level header (format doc §2). The chunkname is preserved
/// verbatim, including its `@`/`=`/binary first-byte prefix.
#[derive(Clone, Debug)]
pub struct ModuleHeader {
    /// Raw header flags (format doc §2.1).
    pub flags: u32,
    /// The chunkname, including the `@`/`=` prefix when present.
    /// `None` when `BCDUMP_F_STRIP` was set.
    pub chunkname: Option<Vec<u8>>,
}

impl ModuleHeader {
    pub fn is_stripped(&self) -> bool {
        self.flags & FLAG_STRIP != 0
    }
    pub fn is_big_endian(&self) -> bool {
        self.flags & FLAG_BE != 0
    }
    pub fn is_fr2(&self) -> bool {
        self.flags & FLAG_FR2 != 0
    }
}

/// A single prototype (function body).
#[derive(Clone, Debug)]
pub struct Proto {
    pub flags: u8,
    pub numparams: u8,
    pub framesize: u8,
    pub upvalues: Vec<UpvalDesc>,
    pub gc_consts: Vec<GcConst>,
    pub num_consts: Vec<NumConst>,
    /// Bytecode instructions including the synthetic FUNC* header at
    /// index 0 (format doc §3.3: in-memory count is `numbc + 1`).
    /// Real instructions from disk start at index 1.
    pub insts: Vec<Instruction>,
    pub debug: Option<DebugInfo>,
}

impl Proto {
    pub fn is_vararg(&self) -> bool {
        self.flags & PROTO_VARARG != 0
    }

    /// Resolve a GC-constant operand (from KSTR, FNEW, TDUP, etc.) to
    /// the GC constant it references.
    ///
    /// GC constants are stored in file order but referenced via
    /// **reverse index**: operand `d` resolves to
    /// `gc_consts[len() - 1 - d]`. This helper centralizes the
    /// reverse-index arithmetic so call sites can't accidentally use
    /// forward indexing — the #1 parser bug per the format doc §6.1.
    ///
    /// Returns [`DecompilerError::InvalidBytecode`] if `d` is out of
    /// range for the proto's GC constant table.
    pub fn gc_const_for_operand(&self, d: u16) -> Result<&GcConst, DecompilerError> {
        let d = d as usize;
        let len = self.gc_consts.len();
        let idx = len
            .checked_sub(d + 1)
            .ok_or_else(|| DecompilerError::InvalidBytecode {
                // This is a post-parse resolution (we're past the reader),
                // so there is no meaningful byte offset to report. We use
                // offset 0 and carry the relevant indices in the reason for
                // debugging. A dedicated error variant would be cleaner but
                // is out of scope for this PR.
                offset: 0,
                reason: format!(
                    "GC constant operand {} out of range (table has {} entries)",
                    d, len
                ),
            })?;
        Ok(&self.gc_consts[idx])
    }
}

// ---- Instruction types -----------------------------------------------

/// Every opcode the LuaJIT 2.x interpreter recognizes, with the fixed
/// numeric values from format doc §5. Variants marked "internal" or
/// "JIT" never appear in dumped bytecode but are still valid opcode
/// bytes, so they round-trip through [`Opcode::try_from`].
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Opcode {
    // Comparison / test ops (AD format; D is a jump target)
    Islt = 0x00,
    Isge = 0x01,
    Isle = 0x02,
    Isgt = 0x03,
    Iseqv = 0x04,
    Isnev = 0x05,
    Iseqs = 0x06,
    Isnes = 0x07,
    Iseqn = 0x08,
    Isnen = 0x09,
    Iseqp = 0x0A,
    Isnep = 0x0B,
    Istc = 0x0C,
    Isfc = 0x0D,
    Ist = 0x0E,
    Isf = 0x0F,
    Istype = 0x10, // internal; not in dumped bytecode
    Isnum = 0x11,  // internal; not in dumped bytecode

    // Unary ops (AD)
    Mov = 0x12,
    Not = 0x13,
    Unm = 0x14,
    Len = 0x15,

    // Binary arithmetic (ABC)
    Addvn = 0x16,
    Subvn = 0x17,
    Mulvn = 0x18,
    Divvn = 0x19,
    Modvn = 0x1A,
    Addnv = 0x1B,
    Subnv = 0x1C,
    Mulnv = 0x1D,
    Divnv = 0x1E,
    Modnv = 0x1F,
    Addvv = 0x20,
    Subvv = 0x21,
    Mulvv = 0x22,
    Divvv = 0x23,
    Modvv = 0x24,
    Pow = 0x25,
    Cat = 0x26,

    // Constant load ops (AD)
    Kstr = 0x27,
    Kcdata = 0x28,
    Kshort = 0x29,
    Knum = 0x2A,
    Kpri = 0x2B,
    Knil = 0x2C,

    // Upvalue / function ops (AD)
    Uget = 0x2D,
    Usetv = 0x2E,
    Usets = 0x2F,
    Usetn = 0x30,
    Usetp = 0x31,
    Uclo = 0x32,
    Fnew = 0x33,

    // Table ops
    Tnew = 0x34,  // AD
    Tdup = 0x35,  // AD
    Gget = 0x36,  // AD
    Gset = 0x37,  // AD
    Tgetv = 0x38, // ABC
    Tgets = 0x39, // ABC
    Tgetb = 0x3A, // ABC
    Tgetr = 0x3B, // ABC (FFI; rare)
    Tsetv = 0x3C, // ABC
    Tsets = 0x3D, // ABC
    Tsetb = 0x3E, // ABC
    Tsetm = 0x3F, // AD
    Tsetr = 0x40, // ABC (FFI; rare)

    // Calls and vararg (ABC unless noted)
    Callm = 0x41,
    Call = 0x42,
    Callmt = 0x43, // AD
    Callt = 0x44,  // AD
    Iterc = 0x45,
    Itern = 0x46,
    Varg = 0x47,
    Isnext = 0x48, // AD

    // Returns (AD)
    Retm = 0x49,
    Ret = 0x4A,
    Ret0 = 0x4B,
    Ret1 = 0x4C,

    // Loops and branches (AD; D is jump unless noted)
    Fori = 0x4D,
    Jfori = 0x4E, // JIT; not in dumped bytecode
    Forl = 0x4F,
    Iforl = 0x50, // JIT; not in dumped bytecode
    Jforl = 0x51, // JIT; not in dumped bytecode
    Iterl = 0x52,
    Iiterl = 0x53, // JIT; not in dumped bytecode
    Jiterl = 0x54, // JIT; not in dumped bytecode
    Loop = 0x55,
    Iloop = 0x56, // JIT; not in dumped bytecode
    Jloop = 0x57, // JIT; not in dumped bytecode
    Jmp = 0x58,

    // Function-header opcodes (synthetic; never on disk, but valid
    // opcode bytes occupying bc[0] in the in-memory representation).
    Funcf = 0x59,
    Ifuncf = 0x5A,
    Jfuncf = 0x5B,
    Funcv = 0x5C,
    Ifuncv = 0x5D,
    Jfuncv = 0x5E,
    Funcc = 0x5F,
    Funccw = 0x60,
}

impl Opcode {
    /// Convert a raw opcode byte into an [`Opcode`]. Returns
    /// `InvalidBytecode` for any value outside `0x00..=0x60`.
    pub fn from_byte(byte: u8, offset: usize) -> Result<Self, DecompilerError> {
        // SAFETY: this variant is pure-data with `#[repr(u8)]` and
        // every value 0x00..=0x60 is listed, so a transmute of any
        // in-range byte is well-defined. We avoid `transmute` to stay
        // crate-portable; an explicit match would be huge, so use the
        // standard "validated transmute via ptr read" pattern.
        if byte > 0x60 {
            return Err(DecompilerError::InvalidBytecode {
                offset,
                reason: format!("unknown opcode byte 0x{:02x}", byte),
            });
        }
        // SAFETY: `Opcode` is `#[repr(u8)]` and we just verified the
        // value is within the range covered by an explicit variant.
        Ok(unsafe { core::mem::transmute::<u8, Opcode>(byte) })
    }

    /// Whether this opcode uses ABC format (vs the default AD). Per
    /// format doc §4.1: opcodes whose B-mode is non-`none` use ABC.
    /// The practical ABC list (also matches the C++ reference parser):
    /// the arithmetic *VN/*NV/*VV/POW/CAT, all TGET/TSET variants
    /// except TSETM, and CALLM/CALL/ITERC/ITERN/VARG.
    pub fn is_abc(self) -> bool {
        matches!(
            self,
            Opcode::Addvn
                | Opcode::Subvn
                | Opcode::Mulvn
                | Opcode::Divvn
                | Opcode::Modvn
                | Opcode::Addnv
                | Opcode::Subnv
                | Opcode::Mulnv
                | Opcode::Divnv
                | Opcode::Modnv
                | Opcode::Addvv
                | Opcode::Subvv
                | Opcode::Mulvv
                | Opcode::Divvv
                | Opcode::Modvv
                | Opcode::Pow
                | Opcode::Cat
                | Opcode::Tgetv
                | Opcode::Tgets
                | Opcode::Tgetb
                | Opcode::Tgetr
                | Opcode::Tsetv
                | Opcode::Tsets
                | Opcode::Tsetb
                | Opcode::Tsetr
                | Opcode::Callm
                | Opcode::Call
                | Opcode::Iterc
                | Opcode::Itern
                | Opcode::Varg
        )
    }
}

/// A decoded bytecode instruction.
///
/// The format is opcode-determined (ABC or AD per
/// [`Opcode::is_abc`]). This struct stores the union of both layouts:
/// `b_or_d` holds B (for ABC) or D (for AD); `c` is C for ABC and 0
/// for AD. The helper methods [`Instruction::b`] and
/// [`Instruction::d`] expose the value with the right width so call
/// sites don't need to remember which format applies.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Instruction {
    pub op: Opcode,
    pub a: u8,
    /// B (ABC format) or D (AD format). Stored wide so a single field
    /// serves both; only the low 8 bits are meaningful when read as B,
    /// and the low 16 bits when read as D.
    pub b_or_d: u32,
    /// C (ABC format). Always 0 for AD-format opcodes.
    pub c: u8,
}

impl Instruction {
    /// The B operand, as an 8-bit value (ABC semantics).
    pub fn b(&self) -> u8 {
        self.b_or_d as u8
    }

    /// The D operand, as a 16-bit value (AD semantics).
    pub fn d(&self) -> u16 {
        self.b_or_d as u16
    }

    /// Build a synthetic FUNC* header instruction for in-memory slot
    /// 0. Per format doc §3.3, this word isn't stored on disk: the
    /// reader inserts it with A=framesize. Opcode is FUNCF for normal
    /// fixed-arg protos, FUNCV when the proto is vararg.
    pub fn synthetic_header(op: Opcode, framesize: u8) -> Self {
        Instruction {
            op,
            a: framesize,
            b_or_d: 0,
            c: 0,
        }
    }
}

// ---- Constant types (format doc §6) ---------------------------------

/// A GC constant (format doc §6.1).
#[derive(Clone, Debug)]
pub enum GcConst {
    /// `KGC_CHILD`: a reference to a previously-read child prototype.
    /// The child itself lives earlier in the parent module's
    /// `protos` vector; only the index is recorded here so cross-proto
    /// passes can resolve it later.
    Child,
    /// `KGC_TAB`: a template table copied by `TDUP`.
    Tab(TableConst),
    /// `KGC_I64`: FFI signed 64-bit cdata.
    I64(u64),
    /// `KGC_U64`: FFI unsigned 64-bit cdata.
    U64(u64),
    /// `KGC_COMPLEX`: FFI complex number (real + imaginary doubles).
    Complex { real: f64, imag: f64 },
    /// `KGC_STR`: a string constant, stored as raw bytes (the format
    /// does not guarantee UTF-8).
    Str(Vec<u8>),
}

/// A template-table constant.
#[derive(Clone, Debug, Default)]
pub struct TableConst {
    /// Array part (positional keys 1, 2, ...).
    pub array: Vec<KtabK>,
    /// Hash part (explicit key/value pairs).
    pub hash: Vec<(KtabK, KtabK)>,
}

/// A single key or value inside a template table (format doc §6.2,
/// `BCDUMP_KTAB_*`).
#[derive(Clone, Debug)]
pub enum KtabK {
    Nil,
    False,
    True,
    Int(u64),
    Num(f64),
    Str(Vec<u8>),
}

/// A number constant (format doc §6.3). The 33-bit ULEB128 tag bit
/// distinguishes integer vs floating-point storage.
#[derive(Copy, Clone, Debug)]
pub enum NumConst {
    /// Tag bit 0: signed 32-bit integer constant (a boxed "int TValue").
    Int(i32),
    /// Tag bit 1: IEEE-754 double constant.
    Num(f64),
}

// ---- Upvalue descriptor (format doc §7) -----------------------------

/// A single upvalue descriptor (format doc §7). Each is a 16-bit
/// little-endian value; the top bits encode locality/immutability.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct UpvalDesc {
    /// Raw 16-bit descriptor. Bits per format doc §7:
    /// - bit 15 (`0x8000`): set = closed (parent's upvalue); clear = open (parent's register).
    /// - bit 14 (`0x4000`): immutable.
    /// - bits 0-13 (`0x3FFF`): index into parent's register or upvalue table.
    pub raw: u16,
}

impl UpvalDesc {
    pub const LOCAL: u16 = 0x8000;
    pub const IMMUTABLE: u16 = 0x4000;
    pub const INDEX_MASK: u16 = 0x3FFF;

    /// True if this upvalue is open (a register in the parent).
    pub fn is_open(&self) -> bool {
        self.raw & Self::LOCAL != 0
    }

    /// True if this upvalue is closed (an upvalue of the parent).
    pub fn is_closed(&self) -> bool {
        !self.is_open()
    }

    /// True if this upvalue is immutable.
    pub fn is_immutable(&self) -> bool {
        self.raw & Self::IMMUTABLE != 0
    }

    /// The parent-side register or upvalue index.
    pub fn index(&self) -> u16 {
        self.raw & Self::INDEX_MASK
    }
}

// ---- Debug info (format doc §8) -------------------------------------

/// Per-prototype debug info. Present only when `!STRIP` and
/// per-prototype `sizedbg > 0`.
#[derive(Clone, Debug, Default)]
pub struct DebugInfo {
    /// First source line of the proto (format doc §8).
    pub firstline: u32,
    /// Number of source lines spanned by the proto. Determines the
    /// line-info width when emitted.
    pub numline: u32,
    /// Source line per real instruction. Indexed by the on-disk
    /// instruction index (0 = first real instruction; does NOT
    /// include the synthetic FUNC* header at in-memory slot 0).
    /// Recovered from the delta array per format doc §8.
    pub line_info: Vec<u32>,
    /// Upvalue names, parallel to `Proto::upvalues`.
    pub upvalue_names: Vec<String>,
    /// Variable scope records.
    pub var_info: Vec<VarInfo>,
}

/// A variable scope record from the debug section (format doc §8.1).
#[derive(Clone, Debug)]
pub struct VarInfo {
    pub kind: VarKind,
    /// Source name when `kind == VarKind::Name`; `None` otherwise.
    pub name: Option<String>,
    /// Whether this record describes a parameter.
    pub is_parameter: bool,
    /// On-disk instruction index where the variable's scope begins.
    /// For parameters this is conceptually 0.
    pub scope_begin: u32,
    /// On-disk instruction index where the variable's scope ends
    /// (inclusive in LuaJIT's encoding).
    pub scope_end: u32,
}

/// The kind tag for a [`VarInfo`] record.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum VarKind {
    /// A named local variable (the type byte was the first character
    /// of the name; type byte >= `VAR_STR`).
    Name,
    /// For-loop index (`type byte == 1`).
    ForIdx,
    /// For-loop stop (`type byte == 2`).
    ForStop,
    /// For-loop step (`type byte == 3`).
    ForStep,
    /// Generic-for generator (`type byte == 4`).
    ForGen,
    /// Generic-for state (`type byte == 5`).
    ForState,
    /// Generic-for control (`type byte == 6`).
    ForCtl,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `Proto` whose only interesting field is `gc_consts`.
    /// Everything else is zeroed/empty — sufficient for testing the
    /// GC-constant operand resolver.
    fn proto_with_gc_consts(gc: Vec<GcConst>) -> Proto {
        Proto {
            flags: 0,
            numparams: 0,
            framesize: 0,
            upvalues: Vec::new(),
            gc_consts: gc,
            num_consts: Vec::new(),
            insts: Vec::new(),
            debug: None,
        }
    }

    #[test]
    fn gc_const_for_operand_reverse_indexes() {
        // gc_consts = [A, B, C] (len 3, file order).
        // Reverse index: operand 0 -> last (C), 1 -> B, 2 -> first (A).
        let proto = proto_with_gc_consts(vec![
            GcConst::Str(b"A".to_vec()),
            GcConst::Str(b"B".to_vec()),
            GcConst::Str(b"C".to_vec()),
        ]);
        // Match on each result so this test doesn't require GcConst to
        // derive PartialEq (keeps the change off the IR derives).
        match proto.gc_const_for_operand(0).unwrap() {
            GcConst::Str(b) => assert_eq!(b, &b"C".to_vec()),
            other => panic!("operand 0: expected Str(C), got {:?}", other),
        }
        match proto.gc_const_for_operand(1).unwrap() {
            GcConst::Str(b) => assert_eq!(b, &b"B".to_vec()),
            other => panic!("operand 1: expected Str(B), got {:?}", other),
        }
        match proto.gc_const_for_operand(2).unwrap() {
            GcConst::Str(b) => assert_eq!(b, &b"A".to_vec()),
            other => panic!("operand 2: expected Str(A), got {:?}", other),
        }
    }

    #[test]
    fn gc_const_for_operand_out_of_range() {
        let proto = proto_with_gc_consts(vec![
            GcConst::Str(b"A".to_vec()),
            GcConst::Str(b"B".to_vec()),
            GcConst::Str(b"C".to_vec()),
        ]);
        // operand == len is the first out-of-range value (valid range
        // is 0..=2).
        let err = proto.gc_const_for_operand(3).unwrap_err();
        match err {
            DecompilerError::InvalidBytecode { offset, reason } => {
                assert_eq!(offset, 0);
                assert!(reason.contains("out of range"), "reason was: {}", reason);
                assert!(reason.contains("3"), "reason was: {}", reason);
            }
            other => panic!("expected InvalidBytecode, got {:?}", other),
        }
    }

    #[test]
    fn gc_const_for_operand_empty_table() {
        let proto = proto_with_gc_consts(Vec::new());
        let err = proto.gc_const_for_operand(0).unwrap_err();
        match err {
            DecompilerError::InvalidBytecode { offset, reason } => {
                assert_eq!(offset, 0);
                assert!(reason.contains("out of range"), "reason was: {}", reason);
            }
            other => panic!("expected InvalidBytecode, got {:?}", other),
        }
    }
}
