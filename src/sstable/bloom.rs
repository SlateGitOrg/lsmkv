//! Bloom filters over user keys, using double hashing (Kirsch–Mitzenmacher).
//!
//! With `b` bits per key and `k = round(b · ln 2)` probes, the false-positive
//! rate is ≈ `(1 − e^(−k/b))^k`: about 0.8% for the default `b = 10`.

use crate::util::hash::hash;

/// Seed shared by builder and reader.
const SEED: u32 = 0xbc9f_1d34;

/// Hashes a key the way filters expect.
pub fn key_hash(key: &[u8]) -> u32 {
    hash(key, SEED)
}

/// Builds a filter from precomputed key hashes. The last byte stores `k`.
pub fn build_filter(hashes: &[u32], bits_per_key: usize) -> Vec<u8> {
    let k = ((bits_per_key as f64 * std::f64::consts::LN_2) as usize).clamp(1, 30);
    let bits = (hashes.len() * bits_per_key).max(64);
    let bytes = bits.div_ceil(8);
    let bits = bytes * 8;
    let mut filter = vec![0u8; bytes + 1];
    for &h0 in hashes {
        let mut h = h0;
        let delta = h.rotate_right(17);
        for _ in 0..k {
            let pos = (h as usize) % bits;
            filter[pos / 8] |= 1 << (pos % 8);
            h = h.wrapping_add(delta);
        }
    }
    filter[bytes] = k as u8;
    filter
}

/// False means `key` is definitely absent; true means "maybe present".
pub fn may_contain(filter: &[u8], key: &[u8]) -> bool {
    if filter.len() < 2 {
        return true;
    }
    let k = filter[filter.len() - 1];
    if k > 30 {
        // Reserved for future encodings: err on the side of a lookup.
        return true;
    }
    let bits = (filter.len() - 1) * 8;
    let mut h = key_hash(key);
    let delta = h.rotate_right(17);
    for _ in 0..k {
        let pos = (h as usize) % bits;
        if filter[pos / 8] & (1 << (pos % 8)) == 0 {
            return false;
        }
        h = h.wrapping_add(delta);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(i: u32) -> [u8; 4] {
        i.to_le_bytes()
    }

    #[test]
    fn no_false_negatives_and_low_fp_rate() {
        for &n in &[1u32, 10, 100, 1000, 10_000] {
            let hashes: Vec<u32> = (0..n).map(|i| key_hash(&key(i))).collect();
            let f = build_filter(&hashes, 10);
            for i in 0..n {
                assert!(may_contain(&f, &key(i)), "false negative n={n} i={i}");
            }
            let fp = (0..10_000u32)
                .filter(|i| may_contain(&f, &key(i + 1_000_000_000)))
                .count();
            assert!(fp < 200, "n={n}: {fp} false positives out of 10000");
        }
    }

    #[test]
    fn degenerate_filters() {
        assert!(may_contain(&[], b"x"));
        assert!(may_contain(&[0, 31], b"x"));
        let empty = build_filter(&[], 10);
        assert!(!may_contain(&empty, b"x"));
    }
}
