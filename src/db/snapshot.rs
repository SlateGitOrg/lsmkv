//! MVCC snapshots.
//!
//! A snapshot is just a sequence number: reads through it ignore every entry
//! with a larger sequence. The registry tracks live snapshots so compaction
//! keeps the versions they can still see (an older version of a key may only
//! be dropped if no snapshot falls between it and the next newer version).

use crate::key::SeqNum;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

/// Multiset of live snapshot sequence numbers.
#[derive(Debug, Default)]
pub struct SnapshotRegistry {
    live: Mutex<BTreeMap<SeqNum, usize>>,
}

impl SnapshotRegistry {
    fn acquire(&self, seq: SeqNum) {
        *self
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(seq)
            .or_insert(0) += 1;
    }

    fn release(&self, seq: SeqNum) {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(n) = live.get_mut(&seq) {
            *n -= 1;
            if *n == 0 {
                live.remove(&seq);
            }
        }
    }

    /// Oldest live snapshot, if any.
    pub fn oldest(&self) -> Option<SeqNum> {
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .next()
            .copied()
    }

    /// Number of live snapshots.
    pub fn count(&self) -> usize {
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .sum()
    }

    /// Registers a snapshot at `seq`.
    pub fn snapshot(self: &Arc<Self>, seq: SeqNum) -> Snapshot {
        self.acquire(seq);
        Snapshot {
            seq,
            registry: self.clone(),
        }
    }
}

/// A consistent read view of the database. Released on drop.
#[derive(Debug)]
pub struct Snapshot {
    seq: SeqNum,
    registry: Arc<SnapshotRegistry>,
}

impl Snapshot {
    /// The sequence number this snapshot reads at.
    pub fn sequence(&self) -> SeqNum {
        self.seq
    }

    /// True if this snapshot was taken from the database owning `registry`.
    pub(crate) fn belongs_to(&self, registry: &Arc<SnapshotRegistry>) -> bool {
        Arc::ptr_eq(&self.registry, registry)
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        self.registry.release(self.seq);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_tracks_oldest() {
        let r = Arc::new(SnapshotRegistry::default());
        assert_eq!(r.oldest(), None);
        let a = r.snapshot(10);
        let b = r.snapshot(5);
        let c = r.snapshot(5);
        assert_eq!(r.oldest(), Some(5));
        assert_eq!(r.count(), 3);
        drop(b);
        assert_eq!(r.oldest(), Some(5));
        drop(c);
        assert_eq!(r.oldest(), Some(10));
        assert_eq!(a.sequence(), 10);
        assert!(a.belongs_to(&r));
        assert!(!a.belongs_to(&Arc::new(SnapshotRegistry::default())));
        drop(a);
        assert_eq!(r.count(), 0);
        r.release(99); // unknown: ignored
    }
}
