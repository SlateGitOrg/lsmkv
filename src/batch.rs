//! Atomic groups of updates.
//!
//! Wire format (also the WAL record payload):
//! `seq: fixed64 | count: fixed32 | records...` where each record is
//! `kind: u8 | key: varint-len bytes | value: varint-len bytes (Value only)`.

use crate::error::{Error, Result};
use crate::key::{SeqNum, ValueKind};
use crate::util::coding::{decode_fixed32, decode_fixed64, put_length_prefixed, Decoder};

const HEADER: usize = 12;

/// A set of puts and deletes applied atomically, in order.
#[derive(Clone, PartialEq, Eq)]
pub struct WriteBatch {
    rep: Vec<u8>,
}

impl Default for WriteBatch {
    fn default() -> Self {
        WriteBatch::new()
    }
}

impl std::fmt::Debug for WriteBatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteBatch")
            .field("count", &self.count())
            .field("bytes", &self.rep.len())
            .finish()
    }
}

/// One decoded operation from a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchOp<'a> {
    /// Put or delete.
    pub kind: ValueKind,
    /// The user key.
    pub key: &'a [u8],
    /// The value (empty for deletes).
    pub value: &'a [u8],
}

impl WriteBatch {
    /// An empty batch.
    pub fn new() -> Self {
        WriteBatch {
            rep: vec![0; HEADER],
        }
    }

    /// Adds `key -> value`.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> &mut Self {
        self.set_count(self.count() + 1);
        self.rep.push(ValueKind::Value as u8);
        put_length_prefixed(&mut self.rep, key);
        put_length_prefixed(&mut self.rep, value);
        self
    }

    /// Deletes `key`.
    pub fn delete(&mut self, key: &[u8]) -> &mut Self {
        self.set_count(self.count() + 1);
        self.rep.push(ValueKind::Deletion as u8);
        put_length_prefixed(&mut self.rep, key);
        self
    }

    /// Removes all operations.
    pub fn clear(&mut self) {
        self.rep.clear();
        self.rep.resize(HEADER, 0);
    }

    /// Number of operations.
    pub fn count(&self) -> u32 {
        decode_fixed32(&self.rep[8..12])
    }

    fn set_count(&mut self, n: u32) {
        self.rep[8..12].copy_from_slice(&n.to_le_bytes());
    }

    /// True if the batch holds no operations.
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    /// Sequence number of the first operation (assigned at commit time).
    pub fn sequence(&self) -> SeqNum {
        decode_fixed64(&self.rep[..8])
    }

    /// Stamps the batch with the sequence number of its first operation.
    pub fn set_sequence(&mut self, seq: SeqNum) {
        self.rep[..8].copy_from_slice(&seq.to_le_bytes());
    }

    /// Encoded size in bytes.
    pub fn size_bytes(&self) -> usize {
        self.rep.len()
    }

    /// Bytes of user payload (keys + values), used for amplification stats.
    pub fn payload_bytes(&self) -> u64 {
        self.iter()
            .filter_map(|op| op.ok())
            .map(|op| (op.key.len() + op.value.len()) as u64)
            .sum()
    }

    /// Appends all operations of `other` (used by group commit).
    pub fn append(&mut self, other: &WriteBatch) {
        self.set_count(self.count() + other.count());
        self.rep.extend_from_slice(&other.rep[HEADER..]);
    }

    /// The encoded representation.
    pub fn data(&self) -> &[u8] {
        &self.rep
    }

    /// Reconstructs a batch from its encoding, validating it fully.
    pub fn from_data(data: &[u8]) -> Result<WriteBatch> {
        if data.len() < HEADER {
            return Err(Error::corruption("write batch shorter than header"));
        }
        let b = WriteBatch { rep: data.to_vec() };
        let mut n = 0u32;
        for op in b.iter() {
            op?;
            n += 1;
        }
        if n != b.count() {
            return Err(Error::corruption(format!(
                "write batch count {} but {n} records",
                b.count()
            )));
        }
        Ok(b)
    }

    /// Iterates the operations in insertion order.
    pub fn iter(&self) -> BatchIter<'_> {
        BatchIter {
            d: Decoder::new(&self.rep[HEADER..]),
        }
    }
}

/// Iterator over a batch's operations.
pub struct BatchIter<'a> {
    d: Decoder<'a>,
}

impl<'a> Iterator for BatchIter<'a> {
    type Item = Result<BatchOp<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.d.is_empty() {
            return None;
        }
        let parsed = (|| {
            let kind = ValueKind::from_u8(self.d.u8()?)?;
            let key = self.d.length_prefixed()?;
            let value = match kind {
                ValueKind::Value => self.d.length_prefixed()?,
                ValueKind::Deletion => &[][..],
            };
            Some(BatchOp { kind, key, value })
        })();
        match parsed {
            Some(op) => Some(Ok(op)),
            None => {
                self.d = Decoder::new(&[]);
                Some(Err(Error::corruption("malformed write batch record")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_iterate_roundtrip() {
        let mut b = WriteBatch::new();
        assert!(b.is_empty());
        b.put(b"k1", b"v1").delete(b"k2").put(b"k3", b"");
        b.set_sequence(100);
        assert_eq!(b.count(), 3);
        assert_eq!(b.sequence(), 100);
        assert_eq!(b.payload_bytes(), 2 + 2 + 2 + 2);
        let ops: Vec<_> = b.iter().map(|o| o.unwrap()).collect();
        assert_eq!(ops[0].key, b"k1");
        assert_eq!(ops[1].kind, ValueKind::Deletion);
        assert_eq!(ops[2].value, b"");
        let c = WriteBatch::from_data(b.data()).unwrap();
        assert_eq!(c, b);
        assert!(format!("{c:?}").contains("count: 3"));
        assert_eq!(b.size_bytes(), b.data().len());
    }

    #[test]
    fn append_and_clear() {
        let mut a = WriteBatch::new();
        a.put(b"a", b"1");
        let mut b = WriteBatch::default();
        b.put(b"b", b"2").delete(b"c");
        a.append(&b);
        assert_eq!(a.count(), 3);
        let keys: Vec<_> = a.iter().map(|o| o.unwrap().key.to_vec()).collect();
        assert_eq!(keys, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
        a.clear();
        assert!(a.is_empty());
        assert_eq!(a.iter().count(), 0);
    }

    #[test]
    fn rejects_corrupt_encodings() {
        assert!(WriteBatch::from_data(&[0; 5]).is_err());
        let mut b = WriteBatch::new();
        b.put(b"key", b"value");
        let data = b.data();
        assert!(WriteBatch::from_data(&data[..data.len() - 2]).is_err());
        let mut wrong_count = data.to_vec();
        wrong_count[8] = 2;
        assert!(WriteBatch::from_data(&wrong_count).is_err());
        let mut bad_kind = data.to_vec();
        bad_kind[12] = 9;
        assert!(WriteBatch::from_data(&bad_kind).is_err());
    }
}
