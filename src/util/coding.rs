//! Little-endian fixed-width and LEB128 varint encodings.
//!
//! All on-disk integers in lsmkv use one of these two encodings.

/// Appends `v` as 4 little-endian bytes.
pub fn put_fixed32(dst: &mut Vec<u8>, v: u32) {
    dst.extend_from_slice(&v.to_le_bytes());
}

/// Appends `v` as 8 little-endian bytes.
pub fn put_fixed64(dst: &mut Vec<u8>, v: u64) {
    dst.extend_from_slice(&v.to_le_bytes());
}

/// Reads 4 little-endian bytes. Panics if `src` is shorter than 4 bytes.
pub fn decode_fixed32(src: &[u8]) -> u32 {
    u32::from_le_bytes([src[0], src[1], src[2], src[3]])
}

/// Reads 8 little-endian bytes. Panics if `src` is shorter than 8 bytes.
pub fn decode_fixed64(src: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&src[..8]);
    u64::from_le_bytes(b)
}

/// Appends `v` as a LEB128 varint (1–10 bytes).
pub fn put_varint64(dst: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        dst.push((v as u8) | 0x80);
        v >>= 7;
    }
    dst.push(v as u8);
}

/// Appends `v` as a LEB128 varint (1–5 bytes).
pub fn put_varint32(dst: &mut Vec<u8>, v: u32) {
    put_varint64(dst, u64::from(v));
}

/// Decodes a varint from the front of `src`, returning the value and the
/// number of bytes consumed, or `None` if the input is truncated/overlong.
pub fn get_varint64(src: &[u8]) -> Option<(u64, usize)> {
    let mut result = 0u64;
    for (i, &byte) in src.iter().enumerate().take(10) {
        let shift = 7 * i as u32;
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }
    }
    None
}

/// Decodes a varint that must fit in 32 bits.
pub fn get_varint32(src: &[u8]) -> Option<(u32, usize)> {
    let (v, n) = get_varint64(src)?;
    u32::try_from(v).ok().map(|v| (v, n))
}

/// Appends a varint length prefix followed by the bytes.
pub fn put_length_prefixed(dst: &mut Vec<u8>, data: &[u8]) {
    put_varint64(dst, data.len() as u64);
    dst.extend_from_slice(data);
}

/// A cursor that decodes values from a byte slice, failing softly.
#[derive(Debug, Clone, Copy)]
pub struct Decoder<'a> {
    buf: &'a [u8],
}

impl<'a> Decoder<'a> {
    /// Wraps `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Decoder { buf }
    }

    /// Bytes not yet consumed.
    pub fn remaining(&self) -> &'a [u8] {
        self.buf
    }

    /// True if all bytes were consumed.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Reads a varint64.
    pub fn varint64(&mut self) -> Option<u64> {
        let (v, n) = get_varint64(self.buf)?;
        self.buf = &self.buf[n..];
        Some(v)
    }

    /// Reads a varint32.
    pub fn varint32(&mut self) -> Option<u32> {
        let (v, n) = get_varint32(self.buf)?;
        self.buf = &self.buf[n..];
        Some(v)
    }

    /// Reads a fixed 64-bit integer.
    pub fn fixed64(&mut self) -> Option<u64> {
        let b = self.bytes(8)?;
        Some(decode_fixed64(b))
    }

    /// Reads a fixed 32-bit integer.
    pub fn fixed32(&mut self) -> Option<u32> {
        let b = self.bytes(4)?;
        Some(decode_fixed32(b))
    }

    /// Reads one byte.
    pub fn u8(&mut self) -> Option<u8> {
        let b = self.bytes(1)?;
        Some(b[0])
    }

    /// Reads exactly `n` raw bytes.
    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.buf.len() < n {
            return None;
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Some(head)
    }

    /// Reads a varint length prefix and that many bytes.
    pub fn length_prefixed(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.varint64()?).ok()?;
        self.bytes(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_roundtrip() {
        let mut b = Vec::new();
        put_fixed32(&mut b, 0xdead_beef);
        put_fixed64(&mut b, 0x0123_4567_89ab_cdef);
        assert_eq!(decode_fixed32(&b), 0xdead_beef);
        assert_eq!(decode_fixed64(&b[4..]), 0x0123_4567_89ab_cdef);
    }

    #[test]
    fn varint_roundtrip_boundaries() {
        let values = [
            0u64,
            1,
            127,
            128,
            255,
            300,
            16_383,
            16_384,
            u64::from(u32::MAX),
            u64::MAX - 1,
            u64::MAX,
        ];
        let mut buf = Vec::new();
        for &v in &values {
            put_varint64(&mut buf, v);
        }
        let mut d = Decoder::new(&buf);
        for &v in &values {
            assert_eq!(d.varint64(), Some(v));
        }
        assert!(d.is_empty());
    }

    #[test]
    fn varint_truncated_and_overflow() {
        let mut buf = Vec::new();
        put_varint64(&mut buf, 1 << 40);
        assert!(get_varint64(&buf[..buf.len() - 1]).is_none());
        assert!(get_varint64(&[0xff; 11]).is_none());
        let mut big = Vec::new();
        put_varint64(&mut big, u64::from(u32::MAX) + 1);
        assert!(get_varint32(&big).is_none());
        put_varint32(&mut buf, 7);
    }

    #[test]
    fn decoder_soft_failures() {
        let mut buf = Vec::new();
        put_length_prefixed(&mut buf, b"hello");
        buf.push(9);
        let mut d = Decoder::new(&buf);
        assert_eq!(d.length_prefixed(), Some(&b"hello"[..]));
        assert_eq!(d.u8(), Some(9));
        assert_eq!(d.u8(), None);
        assert_eq!(d.fixed32(), None);
        assert_eq!(d.fixed64(), None);
        assert_eq!(d.remaining(), b"");
        let mut d = Decoder::new(&[5, 1, 2]);
        assert_eq!(d.length_prefixed(), None);
        let mut d = Decoder::new(&[1, 0, 0, 0]);
        assert_eq!(d.fixed32(), Some(1));
        let mut d = Decoder::new(&[3]);
        assert_eq!(d.varint32(), Some(3));
    }
}
