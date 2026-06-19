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
    /// result is the decoded 32-bit value. Continuation bytes are
    /// consumed until the high bit is clear. A hard cap of 10 bytes
    /// guards against malicious inputs that would otherwise spin
    /// forever; well-formed 32-bit ULEB128s never exceed 5 bytes.
    pub fn read_uleb128(&mut self) -> Result<u32, DecompilerError> {
        let mut v: u32 = 0;
        let mut shift: u32 = 0;
        // 5 bytes is sufficient for any 32-bit value (5*7 = 35 >= 32).
        // Allow a few extra continuation bytes for tolerance, then
        // reject anything longer as malformed.
        const MAX_BYTES: usize = 10;
        for i in 0..MAX_BYTES {
            let b = self.read_u8()?;
            let contribution = u32::from(b & 0x7f);
            if shift < 32 {
                v |= contribution << shift;
            } else if contribution != 0 {
                // Set bits beyond bit 31 — invalid for a 32-bit value.
                return Err(DecompilerError::InvalidBytecode {
                    offset: self.pos,
                    reason: "ULEB128 value exceeds 32 bits".to_string(),
                });
            }
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift = shift.saturating_add(7);
            // After MAX_BYTES-1 continuation bytes there's no point
            // continuing; treat as malformed.
            if i + 1 == MAX_BYTES {
                return Err(DecompilerError::InvalidBytecode {
                    offset: self.pos,
                    reason: "ULEB128 sequence too long".to_string(),
                });
            }
        }
        // Unreachable: loop either returns or errors.
        Err(DecompilerError::InvalidBytecode {
            offset: self.pos,
            reason: "ULEB128 sequence too long".to_string(),
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
    pub fn read_uleb128_33(&mut self) -> Result<(u32, u8), DecompilerError> {
        let byte0 = self.read_u8()?;
        let tag = byte0 & 1;
        // Use u64 internally so later shifts (which can reach bit 27)
        // never overflow even when the masked-in byte is 0x7f.
        let mut v: u64 = u64::from(byte0 >> 1);
        if v >= 0x40 {
            // Continuation bit (top of the 7 value bits) was set.
            v &= 0x3f;
            let mut shift: u32 = 6;
            const MAX_BYTES: usize = 9;
            for i in 0..MAX_BYTES {
                let b = self.read_u8()?;
                let contribution = u64::from(b & 0x7f);
                v |= contribution << shift;
                if b & 0x80 == 0 {
                    return Ok((v as u32, tag));
                }
                shift = shift.saturating_add(7);
                if i + 1 == MAX_BYTES {
                    return Err(DecompilerError::InvalidBytecode {
                        offset: self.pos,
                        reason: "ULEB128_33 sequence too long".to_string(),
                    });
                }
            }
        }
        Ok((v as u32, tag))
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
    fn uleb128_too_long() {
        // 11 continuation bytes — clearly malformed.
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
