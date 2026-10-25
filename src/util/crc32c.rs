//! CRC-32C (Castagnoli), table-driven, plus the LevelDB-style mask that keeps
//! checksums of checksummed data from looking like valid checksums.

const POLY: u32 = 0x82f6_3b78;

const fn make_table() -> [[u32; 256]; 8] {
    let mut table = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ POLY } else { crc >> 1 };
            j += 1;
        }
        table[0][i] = crc;
        i += 1;
    }
    let mut t = 1;
    while t < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = table[t - 1][i];
            table[t][i] = (prev >> 8) ^ table[0][(prev & 0xff) as usize];
            i += 1;
        }
        t += 1;
    }
    table
}

static TABLE: [[u32; 256]; 8] = make_table();

/// Extends a running CRC-32C `crc` (as returned by [`value`]) with `data`.
pub fn extend(crc: u32, data: &[u8]) -> u32 {
    let mut c = !crc;
    let mut chunks = data.chunks_exact(8);
    for ch in &mut chunks {
        let lo = u32::from_le_bytes([ch[0], ch[1], ch[2], ch[3]]) ^ c;
        let hi = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]);
        c = TABLE[7][(lo & 0xff) as usize]
            ^ TABLE[6][((lo >> 8) & 0xff) as usize]
            ^ TABLE[5][((lo >> 16) & 0xff) as usize]
            ^ TABLE[4][(lo >> 24) as usize]
            ^ TABLE[3][(hi & 0xff) as usize]
            ^ TABLE[2][((hi >> 8) & 0xff) as usize]
            ^ TABLE[1][((hi >> 16) & 0xff) as usize]
            ^ TABLE[0][(hi >> 24) as usize];
    }
    for &b in chunks.remainder() {
        c = TABLE[0][((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

/// CRC-32C of `data`.
pub fn value(data: &[u8]) -> u32 {
    extend(0, data)
}

const MASK_DELTA: u32 = 0xa282_ead8;

/// Masks a CRC for storage next to the data it covers.
pub fn mask(crc: u32) -> u32 {
    crc.rotate_right(15).wrapping_add(MASK_DELTA)
}

/// Inverse of [`mask`].
pub fn unmask(masked: u32) -> u32 {
    masked.wrapping_sub(MASK_DELTA).rotate_left(15)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // RFC 3720 B.4 test vectors.
        assert_eq!(value(&[0u8; 32]), 0x8a91_36aa);
        assert_eq!(value(&[0xffu8; 32]), 0x62a8_ab43);
        let inc: Vec<u8> = (0u8..32).collect();
        assert_eq!(value(&inc), 0x46dd_794e);
        let dec: Vec<u8> = (0u8..32).rev().collect();
        assert_eq!(value(&dec), 0x113f_db5c);
        assert_eq!(value(b"123456789"), 0xe306_9283);
    }

    #[test]
    fn extend_is_concatenation() {
        let data = b"hello world, this is a longer buffer for crc";
        for split in 0..data.len() {
            let (a, b) = data.split_at(split);
            assert_eq!(extend(value(a), b), value(data));
        }
    }

    #[test]
    fn mask_roundtrip() {
        let c = value(b"foo");
        assert_ne!(mask(c), c);
        assert_ne!(mask(mask(c)), c);
        assert_eq!(unmask(mask(c)), c);
        assert_eq!(unmask(unmask(mask(mask(c)))), c);
    }
}
