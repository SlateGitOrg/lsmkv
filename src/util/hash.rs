//! A fast non-cryptographic 32-bit hash used by bloom filters and the cache.
//! This is the Murmur-like hash from LevelDB (`util/hash.cc`).

use super::coding::decode_fixed32;

/// Hashes `data` with `seed`.
pub fn hash(data: &[u8], seed: u32) -> u32 {
    const M: u32 = 0xc6a4_a793;
    const R: u32 = 24;
    let mut h = seed ^ (data.len() as u32).wrapping_mul(M);
    let mut chunks = data.chunks_exact(4);
    for ch in &mut chunks {
        let w = decode_fixed32(ch);
        h = h.wrapping_add(w);
        h = h.wrapping_mul(M);
        h ^= h >> 16;
    }
    let rest = chunks.remainder();
    if rest.len() == 3 {
        h = h.wrapping_add(u32::from(rest[2]) << 16);
    }
    if rest.len() >= 2 {
        h = h.wrapping_add(u32::from(rest[1]) << 8);
    }
    if !rest.is_empty() {
        h = h.wrapping_add(u32::from(rest[0]));
        h = h.wrapping_mul(M);
        h ^= h >> R;
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_leveldb_vectors() {
        // Vectors from LevelDB's hash_test.cc.
        assert_eq!(hash(&[], 0xbc9f_1d34), 0xbc9f_1d34);
        assert_eq!(hash(&[0x62], 0xbc9f_1d34), 0xef13_45c4);
        assert_eq!(hash(&[0xc3, 0x97], 0xbc9f_1d34), 0x5b66_3814);
        assert_eq!(hash(&[0xe2, 0x99, 0xa5], 0xbc9f_1d34), 0x323c_078f);
        assert_eq!(hash(&[0xe1, 0x80, 0xb9, 0x32], 0xbc9f_1d34), 0xed21_633a);
    }
}
