//! Low-level cursor over the bytecode input.
//!
//! The [`Reader`] is a thin abstraction over a `&[u8]` slice that tracks
//! the current position and provides the primitive reads the format
//! requires: fixed-width little-endian integers, raw byte slices, and
//! the two LEB128 variants used by the LuaJIT bytecode format
//! (`uleb128` and the 33-bit `uleb128_33` described in format doc §9
//! and §6.3).
//!
//! All methods return [`DecompilerError::InvalidBytecode`] on
//! out-of-bounds reads, recording the absolute byte offset where the
//! failure occurred so callers get a useful diagnostic.

use crate::DecompilerError;

/// A position-tracking cursor over a byte slice.
pub struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Construct a new reader over the given byte slice, starting at
    /// offset 0.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// The current absolute byte offset.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// The total number of bytes available.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Number of bytes remaining unread.
    pub fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    /// Read a single byte.
    pub fn read_u8(&mut self) -> Result<u8, DecompilerError> {
        if self.pos >= self.bytes.len() {
            return Err(self.eof("u8"));
        }
        let v = self.bytes[self.pos];
        self.pos += 1;
        Ok(v)
    }

    /// Read a 16-bit little-endian value.
    pub fn read_u16(&mut self) -> Result<u16, DecompilerError> {
        let bytes = self.read_bytes(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    /// Read a 32-bit little-endian value.
    pub fn read_u32(&mut self) -> Result<u32, DecompilerError> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Borrow the next `n` bytes as a slice without copying. Advances
    /// the cursor past them.
    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], DecompilerError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| self.eof_with("byte slice", n))?;
        if end > self.bytes.len() {
            return Err(self.eof_with("byte slice", n));
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    /// Read a standard unsigned LEB128 value (format doc §9). The
    /// result is the decoded 32-bit value.
    ///
    /// Well-formed 32-bit ULEB128s never exceed 5 bytes (5*7 = 35 >= 32).
    /// This reader enforces that canonical limit strictly: a sequence
    /// longer than 5 bytes — or one whose decoded value exceeds 32 bits
    /// — is rejected as `InvalidBytecode`. Over-long encodings are a
    /// sign of a buggy or malicious writer and provide no legitimate
    /// expressiveness for a 32-bit field, so they are not tolerated.
    ///
    /// Accumulation happens in a `u64` so the final 5th-byte shift (at
    /// bit 28) can set bits above bit 31 without silently truncating;
    /// the value is then validated to fit `u32` before being returned.
    pub fn read_uleb128(&mut self) -> Result<u32, DecompilerError> {
        let mut v: u64 = 0;
        let mut shift: u32 = 0;
        const MAX_BYTES: usize = 5; // canonical max for a 32-bit ULEB128
        for _ in 0..MAX_BYTES {
            let b = self.read_u8()?;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return u32::try_from(v).map_err(|_| DecompilerError::InvalidBytecode {
                    offset: self.pos,
                    reason: format!("ULEB128 value 0x{:x} exceeds 32 bits", v),
                });
            }
            shift += 7;
        }
        // Still in continuation after 5 bytes — non-canonical for a
        // u32 ULEB128; treat as malformed.
        Err(DecompilerError::InvalidBytecode {
            offset: self.pos,
            reason: "ULEB128 sequence exceeds 5 bytes (canonical limit for u32)".to_string(),
        })
    }

    /// Read the 33-bit ULEB128 variant used for number constants
    /// (format doc §6.3). The low bit of the first byte is a type tag
    /// and is returned alongside the decoded value so callers can
    /// distinguish integer vs floating-point constants.
    ///
    /// Layout: bits [7:1] of the first byte hold the high 6-7 bits of
    /// the value (with the top bit acting as the standard LEB128
    /// continuation flag). Subsequent bytes contribute 7 more bits
    /// each, in standard LEB128 fashion.
    ///
    /// Well-formed 33-bit ULEB128s never exceed 6 bytes total (1 first
    /// byte with 6 value bits + up to 5 continuation bytes). This
    /// reader caps the continuation at 5 bytes and rejects anything
    /// longer as malformed. The decoded value is validated to fit
    /// `u32` before being returned (the 33rd bit is only meaningful
    /// for sign-extension of integer constants, handled by the caller;
    /// values exceeding `u32::MAX` are out of range for this reader).
    pub fn read_uleb128_33(&mut self) -> Result<(u32, u8), DecompilerError> {
        let byte0 = self.read_u8()?;
        let tag = byte0 & 1;
        // Accumulate in u64 so the later continuation shifts (which can
        // reach bit 34) never silently truncate.
        let mut v: u64 = u64::from(byte0 >> 1);
        if v >= 0x40 {
            // Continuation bit (top of the first byte's 7 value bits)
            // was set: mask it off and read continuation bytes.
            v &= 0x3f;
            let mut shift: u32 = 6;
            const MAX_CONT: usize = 5; // canonical max continuation bytes for 33-bit
            for _ in 0..MAX_CONT {
                let b = self.read_u8()?;
                v |= u64::from(b & 0x7f) << shift;
                if b & 0x80 == 0 {
                    return pack_uleb128_33(v, tag, self.pos);
                }
                shift += 7;
            }
            return Err(DecompilerError::InvalidBytecode {
                offset: self.pos,
                reason: "ULEB128_33 sequence exceeds 6 bytes (canonical limit for 33-bit)"
                    .to_string(),
            });
        }
        pack_uleb128_33(v, tag, self.pos)
    }

    fn eof(&self, what: &str) -> DecompilerError {
        DecompilerError::InvalidBytecode {
            offset: self.pos,
            reason: format!("unexpected end of input while reading {}", what),
        }
    }

    fn eof_with(&self, what: &str, wanted: usize) -> DecompilerError {
        DecompilerError::InvalidBytecode {
            offset: self.pos,
            reason: format!(
                "unexpected end of input while reading {} ({} bytes wanted, {} remaining)",
                what,
                wanted,
                self.remaining()
            ),
        }
    }
}

/// Pack a decoded 33-bit ULEB128 value into the `(u32, tag)` result,
/// rejecting values that do not fit in 32 bits. Shared by the single-
/// and multi-byte return paths of [`Reader::read_uleb128_33`].
fn pack_uleb128_33(v: u64, tag: u8, offset: usize) -> Result<(u32, u8), DecompilerError> {
    match u32::try_from(v) {
        Ok(val) => Ok((val, tag)),
        Err(_) => Err(DecompilerError::InvalidBytecode {
            offset,
            reason: format!("ULEB128_33 value 0x{:x} exceeds 32 bits", v),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_u8_advances() {
        let mut r = Reader::new(&[0x01, 0x02, 0x03]);
        assert_eq!(r.read_u8().unwrap(), 1);
        assert_eq!(r.read_u8().unwrap(), 2);
        assert_eq!(r.read_u8().unwrap(), 3);
        assert!(r.read_u8().is_err());
    }

    #[test]
    fn read_u16_le() {
        let mut r = Reader::new(&[0x34, 0x12]);
        assert_eq!(r.read_u16().unwrap(), 0x1234);
    }

    #[test]
    fn read_u32_le() {
        let mut r = Reader::new(&[0x78, 0x56, 0x34, 0x12]);
        assert_eq!(r.read_u32().unwrap(), 0x12345678);
    }

    #[test]
    fn uleb128_single_byte() {
        let mut r = Reader::new(&[0x00, 0x01, 0x7f]);
        assert_eq!(r.read_uleb128().unwrap(), 0);
        assert_eq!(r.read_uleb128().unwrap(), 1);
        assert_eq!(r.read_uleb128().unwrap(), 0x7f);
    }

    #[test]
    fn uleb128_multi_byte() {
        // Canonical example from the format doc / Wikipedia:
        // 624485 = 0xE5 0x8E 0x26
        let mut r = Reader::new(&[0xe5, 0x8e, 0x26]);
        assert_eq!(r.read_uleb128().unwrap(), 624485);
    }

    #[test]
    fn uleb128_max_32bit() {
        // 0xFFFFFFFF = 5 bytes: 0xFF 0xFF 0xFF 0xFF 0x0F
        let mut r = Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x0f]);
        assert_eq!(r.read_uleb128().unwrap(), 0xffff_ffff);
    }

    #[test]
    fn uleb128_overflow_5th_byte() {
        // 5 bytes, but the 5th contributes bits above bit 31: the
        // canonical u32::MAX encoding uses 0x0F as the final byte, not
        // 0x1F. With 0x1F the decoded value is 0x1_FFFF_FFFF (bit 32
        // set), which exceeds 32 bits and must be rejected rather than
        // silently truncated to 0xFFFFFFFF.
        let mut r = Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x1f]);
        let err = r.read_uleb128().unwrap_err();
        match err {
            DecompilerError::InvalidBytecode { reason, .. } => {
                assert!(reason.contains("exceeds 32 bits"), "reason was: {}", reason);
            }
            other => panic!("expected InvalidBytecode, got {:?}", other),
        }
    }

    #[test]
    fn uleb128_just_over_u32_max() {
        // 0x1_0000_0000 = one bit over u32::MAX. ULEB128 encoding:
        //   byte0 = 0x80 (cont, 0 value bits)
        //   bytes 1-3 = 0x80 (cont, 0 value bits)
        //   byte4 = 0x10 (final; 0x10 << 28 = bit 32)
        let mut r = Reader::new(&[0x80, 0x80, 0x80, 0x80, 0x10]);
        let err = r.read_uleb128().unwrap_err();
        match err {
            DecompilerError::InvalidBytecode { reason, .. } => {
                assert!(reason.contains("exceeds 32 bits"), "reason was: {}", reason);
            }
            other => panic!("expected InvalidBytecode, got {:?}", other),
        }
    }

    #[test]
    fn uleb128_six_bytes_rejected() {
        // 6 continuation bytes: non-canonical for a u32 ULEB128 (max 5).
        let mut r = Reader::new(&[0xff; 6]);
        let err = r.read_uleb128().unwrap_err();
        match err {
            DecompilerError::InvalidBytecode { reason, .. } => {
                assert!(reason.contains("exceeds 5 bytes"), "reason was: {}", reason);
            }
            other => panic!("expected InvalidBytecode, got {:?}", other),
        }
    }

    #[test]
    fn uleb128_too_long() {
        // 11 continuation bytes — clearly malformed. Still rejected
        // (now via the "exceeds 5 bytes" canonical-limit path).
        let mut r = Reader::new(&[0xff; 11]);
        let err = r.read_uleb128().unwrap_err();
        assert!(matches!(err, DecompilerError::InvalidBytecode { .. }));
    }

    #[test]
    fn uleb128_33_short() {
        // First byte 0x06: tag bit = 0, value = 0x06 >> 1 = 3. No
        // continuation. So (value=3, tag=0).
        let mut r = Reader::new(&[0x06]);
        assert_eq!(r.read_uleb128_33().unwrap(), (3, 0));
    }

    #[test]
    fn uleb128_33_continuation() {
        // byte0 = 0xC0: tag = 0; bits 1-6 of byte0 (== 0b100000 == 32)
        // form the low 6 bits of the value; bit 7 is set so a
        // continuation byte follows. Continuation byte 0x01
        // contributes (1 << 6) = 64. Final value = 32 + 64 = 96.
        let mut r = Reader::new(&[0xc0, 0x01]);
        assert_eq!(r.read_uleb128_33().unwrap(), (96, 0));
    }

    #[test]
    fn uleb128_33_tag_bit_preserved() {
        // byte0 = 0x07: value = 0x07>>1 = 3, tag = 1.
        let mut r = Reader::new(&[0x07]);
        assert_eq!(r.read_uleb128_33().unwrap(), (3, 1));
    }

    #[test]
    fn uleb128_33_overflow() {
        // Encode 0x1_0000_0000 (one bit over u32::MAX) in the 33-bit
        // format with tag = 0:
        //   byte0 = 0x80 (tag 0, cont, 0 value bits)
        //   bytes 1-3 = 0x80 (cont, 0 value bits)
        //   byte4 = 0x20 (final; 0x20 << 27 = bit 32 in the 33-bit field)
        // The value overflows u32 and must be rejected.
        let mut r = Reader::new(&[0x80, 0x80, 0x80, 0x80, 0x20]);
        let err = r.read_uleb128_33().unwrap_err();
        match err {
            DecompilerError::InvalidBytecode { reason, .. } => {
                assert!(reason.contains("exceeds 32 bits"), "reason was: {}", reason);
            }
            other => panic!("expected InvalidBytecode, got {:?}", other),
        }
    }

    #[test]
    fn uleb128_33_too_long() {
        // byte0 with continuation + 5 continuation bytes all with
        // continuation set = 6 bytes total, still continuing. Exceeds
        // the 6-byte canonical limit for a 33-bit value.
        let mut r = Reader::new(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80]);
        let err = r.read_uleb128_33().unwrap_err();
        match err {
            DecompilerError::InvalidBytecode { reason, .. } => {
                assert!(reason.contains("exceeds 6 bytes"), "reason was: {}", reason);
            }
            other => panic!("expected InvalidBytecode, got {:?}", other),
        }
    }

    #[test]
    fn read_bytes_returns_slice_and_advances() {
        let mut r = Reader::new(&[0xaa, 0xbb, 0xcc, 0xdd]);
        let s = r.read_bytes(2).unwrap();
        assert_eq!(s, &[0xaa, 0xbb]);
        assert_eq!(r.pos(), 2);
    }

    #[test]
    fn read_bytes_past_end_errors_with_offset() {
        let r_bytes: &[u8] = &[0xaa, 0xbb];
        let mut r = Reader::new(r_bytes);
        // Consume one byte successfully so the position advances;
        // then attempt a read that runs past the end.
        let first = r.read_bytes(1).unwrap();
        assert_eq!(first, &[0xaa]);
        let err = r.read_bytes(5).unwrap_err();
        match err {
            DecompilerError::InvalidBytecode { offset, reason } => {
                assert_eq!(offset, 1);
                assert!(reason.contains("byte slice"));
            }
            other => panic!("unexpected variant: {:?}", other),
        }
    }
}
