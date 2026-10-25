//! Internal key encoding.
//!
//! Every entry stored by the engine is keyed by an *internal key*:
//! `user_key ++ fixed64_le(sequence << 8 | kind)`. Internal keys order by user
//! key ascending, then by sequence number **descending**, so the newest version
//! of a key is encountered first by a forward scan. This single ordering is
//! what makes MVCC snapshots, tombstones and merging of sorted runs work.

use crate::util::coding::{decode_fixed64, put_fixed64};
use std::cmp::Ordering;
use std::fmt;

/// Monotonic sequence number assigned to every write.
pub type SeqNum = u64;

/// Largest representable sequence number (56 bits).
pub const MAX_SEQUENCE: SeqNum = (1 << 56) - 1;

/// Size of the trailer appended to user keys.
pub const TRAILER_LEN: usize = 8;

/// Whether an entry carries a value or deletes the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum ValueKind {
    /// Tombstone.
    Deletion = 0,
    /// Regular value.
    Value = 1,
}

impl ValueKind {
    /// Decodes the kind byte.
    pub fn from_u8(b: u8) -> Option<ValueKind> {
        match b {
            0 => Some(ValueKind::Deletion),
            1 => Some(ValueKind::Value),
            _ => None,
        }
    }
}

/// The kind used when building seek targets: it is the largest kind, so a
/// seek for `(key, seq)` lands on the newest entry with sequence `<= seq`.
pub const KIND_FOR_SEEK: ValueKind = ValueKind::Value;

/// Packs a sequence number and kind into the 8-byte trailer value.
pub fn pack_trailer(seq: SeqNum, kind: ValueKind) -> u64 {
    debug_assert!(seq <= MAX_SEQUENCE);
    (seq << 8) | kind as u64
}

/// Appends the internal key for `(user_key, seq, kind)` to `dst`.
pub fn append_internal_key(dst: &mut Vec<u8>, user_key: &[u8], seq: SeqNum, kind: ValueKind) {
    dst.extend_from_slice(user_key);
    put_fixed64(dst, pack_trailer(seq, kind));
}

/// Builds an owned internal key.
pub fn internal_key(user_key: &[u8], seq: SeqNum, kind: ValueKind) -> Vec<u8> {
    let mut v = Vec::with_capacity(user_key.len() + TRAILER_LEN);
    append_internal_key(&mut v, user_key, seq, kind);
    v
}

/// A decoded internal key borrowing the user key.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ParsedKey<'a> {
    /// The application's key.
    pub user_key: &'a [u8],
    /// The write's sequence number.
    pub seq: SeqNum,
    /// Value or tombstone.
    pub kind: ValueKind,
}

impl fmt::Debug for ParsedKey<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}@{}:{:?}",
            String::from_utf8_lossy(self.user_key),
            self.seq,
            self.kind
        )
    }
}

/// Parses an internal key; `None` if it is too short or has a bad kind byte.
pub fn parse_internal_key(ikey: &[u8]) -> Option<ParsedKey<'_>> {
    if ikey.len() < TRAILER_LEN {
        return None;
    }
    let split = ikey.len() - TRAILER_LEN;
    let trailer = decode_fixed64(&ikey[split..]);
    let kind = ValueKind::from_u8((trailer & 0xff) as u8)?;
    Some(ParsedKey {
        user_key: &ikey[..split],
        seq: trailer >> 8,
        kind,
    })
}

/// The user-key part of an internal key. Panics on keys shorter than the trailer.
pub fn user_key(ikey: &[u8]) -> &[u8] {
    &ikey[..ikey.len() - TRAILER_LEN]
}

/// The packed trailer of an internal key.
pub fn trailer(ikey: &[u8]) -> u64 {
    decode_fixed64(&ikey[ikey.len() - TRAILER_LEN..])
}

/// Total order on internal keys: user key ascending, trailer descending.
pub fn compare_internal(a: &[u8], b: &[u8]) -> Ordering {
    match user_key(a).cmp(user_key(b)) {
        Ordering::Equal => trailer(b).cmp(&trailer(a)),
        o => o,
    }
}

/// Shortens `start` to a key `k` with `start <= k < limit` (by internal
/// order), used for index-block separators. Returns `start` unchanged when no
/// shorter separator exists.
pub fn shortest_separator(start: &[u8], limit: &[u8]) -> Vec<u8> {
    let us = user_key(start);
    let ul = user_key(limit);
    let common = us.iter().zip(ul).take_while(|(a, b)| a == b).count();
    if common < us.len().min(ul.len()) {
        let byte = us[common];
        if byte < 0xff && byte + 1 < ul[common] {
            let mut sep = us[..=common].to_vec();
            sep[common] += 1;
            debug_assert!(sep.as_slice() > us && sep.as_slice() < ul);
            let mut out = Vec::with_capacity(sep.len() + TRAILER_LEN);
            append_internal_key(&mut out, &sep, MAX_SEQUENCE, KIND_FOR_SEEK);
            return out;
        }
    }
    start.to_vec()
}

/// Shortens `key` to a short internal key `k >= key`, used for the last index
/// entry of a table.
pub fn short_successor(key: &[u8]) -> Vec<u8> {
    let uk = user_key(key);
    if let Some(i) = uk.iter().position(|&b| b != 0xff) {
        let mut succ = uk[..=i].to_vec();
        succ[i] += 1;
        let mut out = Vec::with_capacity(succ.len() + TRAILER_LEN);
        append_internal_key(&mut out, &succ, MAX_SEQUENCE, KIND_FOR_SEEK);
        return out;
    }
    key.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_parse() {
        let k = internal_key(b"apple", 42, ValueKind::Value);
        let p = parse_internal_key(&k).unwrap();
        assert_eq!(p.user_key, b"apple");
        assert_eq!(p.seq, 42);
        assert_eq!(p.kind, ValueKind::Value);
        assert_eq!(user_key(&k), b"apple");
        assert!(format!("{p:?}").contains("apple@42"));
        assert!(parse_internal_key(b"short").is_none());
        let mut bad = k.clone();
        let n = bad.len();
        bad[n - TRAILER_LEN] = 7;
        assert!(parse_internal_key(&bad).is_none());
        assert_eq!(ValueKind::from_u8(0), Some(ValueKind::Deletion));
    }

    #[test]
    fn ordering_newest_first() {
        let a1 = internal_key(b"a", 1, ValueKind::Value);
        let a2 = internal_key(b"a", 2, ValueKind::Value);
        let a2d = internal_key(b"a", 2, ValueKind::Deletion);
        let b0 = internal_key(b"b", 0, ValueKind::Value);
        assert_eq!(compare_internal(&a2, &a1), Ordering::Less);
        assert_eq!(compare_internal(&a2, &a2d), Ordering::Less);
        assert_eq!(compare_internal(&a1, &b0), Ordering::Less);
        assert_eq!(compare_internal(&a1, &a1), Ordering::Equal);
        let ab = internal_key(b"ab", 9, ValueKind::Value);
        assert_eq!(compare_internal(&a1, &ab), Ordering::Less);
    }

    #[test]
    fn separators() {
        let s = internal_key(b"abcd", 5, ValueKind::Value);
        let l = internal_key(b"abzz", 5, ValueKind::Value);
        let sep = shortest_separator(&s, &l);
        assert_eq!(user_key(&sep), b"abd");
        assert_ne!(compare_internal(&sep, &s), Ordering::Less);
        assert_eq!(compare_internal(&sep, &l), Ordering::Less);
        // Adjacent bytes: no shortening possible.
        let l2 = internal_key(b"abce", 5, ValueKind::Value);
        assert_eq!(shortest_separator(&s, &l2), s);
        // Prefix relation: no shortening.
        let l3 = internal_key(b"abcdef", 5, ValueKind::Value);
        assert_eq!(shortest_separator(&s, &l3), s);
        let succ = short_successor(&s);
        assert_eq!(user_key(&succ), b"b");
        let ff = internal_key(&[0xff, 0xff], 1, ValueKind::Value);
        assert_eq!(short_successor(&ff), ff);
    }
}
