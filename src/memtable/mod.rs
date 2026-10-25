//! The memtable: recent writes buffered in a skiplist ordered by internal key.

pub mod skiplist;

use crate::error::Result;
use crate::iterator::InternalIterator;
use crate::key::{
    append_internal_key, compare_internal, parse_internal_key, SeqNum, ValueKind, KIND_FOR_SEEK,
};
use skiplist::{SkipIter, SkipList};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Per-entry bookkeeping overhead counted towards memtable size.
const ENTRY_OVERHEAD: usize = 48;

/// Result of a point lookup that hit a memtable or table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// The newest visible version is a value.
    Found(Vec<u8>),
    /// The newest visible version is a tombstone.
    Deleted,
}

/// An in-memory sorted buffer of writes. Single writer, concurrent readers.
pub struct MemTable {
    list: Arc<SkipList>,
    bytes: AtomicUsize,
}

impl Default for MemTable {
    fn default() -> Self {
        MemTable::new()
    }
}

impl MemTable {
    /// An empty memtable.
    pub fn new() -> Self {
        MemTable {
            list: Arc::new(SkipList::new(compare_internal)),
            bytes: AtomicUsize::new(0),
        }
    }

    /// Records a write. Callers serialise writers (the DB's group-commit
    /// leader is the only writer at any time).
    pub fn add(&self, seq: SeqNum, kind: ValueKind, key: &[u8], value: &[u8]) {
        let mut ikey = Vec::with_capacity(key.len() + 8);
        append_internal_key(&mut ikey, key, seq, kind);
        self.list.insert(&ikey, value);
        self.bytes
            .fetch_add(ikey.len() + value.len() + ENTRY_OVERHEAD, Ordering::Relaxed);
    }

    /// Finds the newest version of `key` with sequence `<= seq`.
    pub fn get(&self, key: &[u8], seq: SeqNum) -> Option<Lookup> {
        let mut target = Vec::with_capacity(key.len() + 8);
        append_internal_key(&mut target, key, seq, KIND_FOR_SEEK);
        let mut it = SkipIter::new(self.list.clone());
        it.seek(&target);
        if !it.valid() {
            return None;
        }
        let parsed = parse_internal_key(it.key())?;
        if parsed.user_key != key {
            return None;
        }
        Some(match parsed.kind {
            ValueKind::Value => Lookup::Found(it.value().to_vec()),
            ValueKind::Deletion => Lookup::Deleted,
        })
    }

    /// Approximate memory used by entries, in bytes.
    pub fn approximate_bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Number of entries (all versions).
    pub fn len(&self) -> usize {
        self.list.len()
    }

    /// True if no writes were recorded.
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// An iterator over all entries in internal-key order.
    pub fn iter(&self) -> MemTableIter {
        MemTableIter {
            inner: SkipIter::new(self.list.clone()),
        }
    }
}

/// [`InternalIterator`] over a memtable; keeps the memtable's list alive.
pub struct MemTableIter {
    inner: SkipIter,
}

impl InternalIterator for MemTableIter {
    fn valid(&self) -> bool {
        self.inner.valid()
    }
    fn seek_to_first(&mut self) {
        self.inner.seek_to_first();
    }
    fn seek(&mut self, target: &[u8]) {
        self.inner.seek(target);
    }
    fn next(&mut self) {
        self.inner.next();
    }
    fn key(&self) -> &[u8] {
        self.inner.key()
    }
    fn value(&self) -> &[u8] {
        self.inner.value()
    }
    fn status(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::user_key;

    #[test]
    fn mvcc_point_lookups() {
        let m = MemTable::new();
        assert!(m.is_empty());
        m.add(1, ValueKind::Value, b"k", b"v1");
        m.add(2, ValueKind::Value, b"k", b"v2");
        m.add(3, ValueKind::Deletion, b"k", b"");
        m.add(4, ValueKind::Value, b"other", b"o");
        assert_eq!(m.get(b"k", 0), None);
        assert_eq!(m.get(b"k", 1), Some(Lookup::Found(b"v1".to_vec())));
        assert_eq!(m.get(b"k", 2), Some(Lookup::Found(b"v2".to_vec())));
        assert_eq!(m.get(b"k", 3), Some(Lookup::Deleted));
        assert_eq!(m.get(b"k", 100), Some(Lookup::Deleted));
        assert_eq!(m.get(b"j", 100), None);
        assert_eq!(m.get(b"zzz", 100), None);
        assert_eq!(m.len(), 4);
        assert!(m.approximate_bytes() > 4 * ENTRY_OVERHEAD);
    }

    #[test]
    fn iterates_newest_version_first() {
        let m = MemTable::default();
        m.add(1, ValueKind::Value, b"b", b"1");
        m.add(5, ValueKind::Value, b"a", b"5");
        m.add(2, ValueKind::Value, b"a", b"2");
        let mut it = m.iter();
        it.seek_to_first();
        let mut seen = Vec::new();
        while it.valid() {
            let p = parse_internal_key(it.key()).unwrap();
            seen.push((p.user_key.to_vec(), p.seq, it.value().to_vec()));
            it.next();
        }
        assert_eq!(
            seen,
            vec![
                (b"a".to_vec(), 5, b"5".to_vec()),
                (b"a".to_vec(), 2, b"2".to_vec()),
                (b"b".to_vec(), 1, b"1".to_vec())
            ]
        );
        let mut target = Vec::new();
        append_internal_key(&mut target, b"b", 9, KIND_FOR_SEEK);
        it.seek(&target);
        assert_eq!(user_key(it.key()), b"b");
        assert!(it.status().is_ok());
    }
}
