//! Unsigned LEB128 varints, strict on decode.
//!
//! Same encoding as `cavs-manifest` uses, restated here so the object model
//! does not depend on the manifest crate. Decoding rejects truncation,
//! overlong forms and values past u64, so every number has exactly one
//! canonical byte form.

use crate::error::{ObjectError, Result};

/// A u64 never needs more than 10 LEB128 bytes.
pub const MAX_VARINT_BYTES: usize = 10;

pub fn write_varuint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Decode one varuint from the front of `input`, advancing it.
pub fn read_varuint(input: &mut &[u8]) -> Result<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    for i in 0..MAX_VARINT_BYTES {
        let Some(&byte) = input.get(i) else {
            return Err(ObjectError::Truncated("varint"));
        };
        // Overlong: a continuation led here but this byte adds nothing.
        if byte == 0 && shift != 0 {
            return Err(ObjectError::VarintNotCanonical);
        }
        // The 10th byte may only carry the single remaining bit of a u64.
        if i == MAX_VARINT_BYTES - 1 && byte > 1 {
            return Err(ObjectError::VarintNotCanonical);
        }
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            *input = &input[i + 1..];
            return Ok(value);
        }
        shift += 7;
    }
    Err(ObjectError::VarintNotCanonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for value in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            write_varuint(value, &mut buf);
            let mut slice = buf.as_slice();
            assert_eq!(read_varuint(&mut slice).unwrap(), value);
            assert!(slice.is_empty());
        }
    }

    #[test]
    fn rejects_non_canonical() {
        let mut overlong: &[u8] = &[0x80, 0x00];
        assert!(read_varuint(&mut overlong).is_err());
        let mut truncated: &[u8] = &[0x80];
        assert!(read_varuint(&mut truncated).is_err());
        let mut overflow: &[u8] = &[0xff; 11];
        assert!(read_varuint(&mut overflow).is_err());
    }
}
