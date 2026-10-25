//! Internal iterators over sorted runs of internal keys, and the k-way merge
//! that combines memtables and SSTables into one ordered stream.

use crate::error::Result;
use crate::key::compare_internal;
use std::cmp::Ordering;

/// A forward cursor over `(internal key, value)` pairs in internal-key order.
///
/// Errors are sticky: an iterator that hits an I/O or corruption error becomes
/// invalid and reports the error from [`InternalIterator::status`].
pub trait InternalIterator: Send {
    /// True if positioned at an entry.
    fn valid(&self) -> bool;
    /// Positions at the first entry.
    fn seek_to_first(&mut self);
    /// Positions at the first entry with internal key >= `target`.
    fn seek(&mut self, target: &[u8]);
    /// Advances. Requires `valid()`.
    fn next(&mut self);
    /// Current internal key. Requires `valid()`.
    fn key(&self) -> &[u8];
    /// Current value. Requires `valid()`.
    fn value(&self) -> &[u8];
    /// The first error encountered, if any.
    fn status(&self) -> Result<()>;
}

/// An iterator over an in-memory vector of entries; used for tests and for
/// small fixed sets.
#[derive(Debug, Default)]
pub struct VecIter {
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    pos: usize,
}

impl VecIter {
    /// Wraps entries that must already be sorted by internal key.
    pub fn new(entries: Vec<(Vec<u8>, Vec<u8>)>) -> Self {
        debug_assert!(entries
            .windows(2)
            .all(|w| compare_internal(&w[0].0, &w[1].0).is_lt()));
        let pos = entries.len();
        VecIter { entries, pos }
    }
}

impl InternalIterator for VecIter {
    fn valid(&self) -> bool {
        self.pos < self.entries.len()
    }
    fn seek_to_first(&mut self) {
        self.pos = 0;
    }
    fn seek(&mut self, target: &[u8]) {
        self.pos = self
            .entries
            .partition_point(|(k, _)| compare_internal(k, target).is_lt());
    }
    fn next(&mut self) {
        self.pos += 1;
    }
    fn key(&self) -> &[u8] {
        &self.entries[self.pos].0
    }
    fn value(&self) -> &[u8] {
        &self.entries[self.pos].1
    }
    fn status(&self) -> Result<()> {
        Ok(())
    }
}

/// Merges `k` sorted children with a binary min-heap: `O(log k)` per step.
///
/// When two children hold the same internal key (which the engine never
/// produces, but tests may), the child with the lower index wins; callers pass
/// children newest-first.
pub struct MergingIterator {
    children: Vec<Box<dyn InternalIterator>>,
    /// Indices of valid children, heap-ordered by current key.
    heap: Vec<usize>,
}

impl MergingIterator {
    /// Merges `children` (newest source first).
    pub fn new(children: Vec<Box<dyn InternalIterator>>) -> Self {
        MergingIterator {
            children,
            heap: Vec::new(),
        }
    }

    fn less(&self, a: usize, b: usize) -> bool {
        match compare_internal(self.children[a].key(), self.children[b].key()) {
            Ordering::Less => true,
            Ordering::Equal => a < b,
            Ordering::Greater => false,
        }
    }

    fn sift_down(&mut self, mut i: usize) {
        let n = self.heap.len();
        loop {
            let l = 2 * i + 1;
            let r = l + 1;
            let mut m = i;
            if l < n && self.less(self.heap[l], self.heap[m]) {
                m = l;
            }
            if r < n && self.less(self.heap[r], self.heap[m]) {
                m = r;
            }
            if m == i {
                return;
            }
            self.heap.swap(i, m);
            i = m;
        }
    }

    fn rebuild(&mut self) {
        self.heap = (0..self.children.len())
            .filter(|&i| self.children[i].valid())
            .collect();
        for i in (0..self.heap.len() / 2).rev() {
            self.sift_down(i);
        }
    }
}

impl InternalIterator for MergingIterator {
    fn valid(&self) -> bool {
        !self.heap.is_empty() && self.status().is_ok()
    }

    fn seek_to_first(&mut self) {
        for c in &mut self.children {
            c.seek_to_first();
        }
        self.rebuild();
    }

    fn seek(&mut self, target: &[u8]) {
        for c in &mut self.children {
            c.seek(target);
        }
        self.rebuild();
    }

    fn next(&mut self) {
        let top = self.heap[0];
        self.children[top].next();
        if !self.children[top].valid() {
            let last = self.heap.len() - 1;
            self.heap.swap(0, last);
            self.heap.pop();
        }
        if !self.heap.is_empty() {
            self.sift_down(0);
        }
    }

    fn key(&self) -> &[u8] {
        self.children[self.heap[0]].key()
    }

    fn value(&self) -> &[u8] {
        self.children[self.heap[0]].value()
    }

    fn status(&self) -> Result<()> {
        for c in &self.children {
            c.status()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{internal_key, ValueKind};

    fn ik(k: &str, seq: u64) -> Vec<u8> {
        internal_key(k.as_bytes(), seq, ValueKind::Value)
    }

    fn run(entries: &[(&str, u64)]) -> VecIter {
        VecIter::new(
            entries
                .iter()
                .map(|&(k, s)| (ik(k, s), format!("{k}{s}").into_bytes()))
                .collect(),
        )
    }

    fn collect(it: &mut dyn InternalIterator) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while it.valid() {
            out.push(it.value().to_vec());
            it.next();
        }
        out
    }

    #[test]
    fn merges_in_internal_order() {
        let a = run(&[("a", 5), ("c", 1), ("e", 3)]);
        let b = run(&[("a", 2), ("b", 9), ("e", 4)]);
        let c = run(&[]);
        let mut m = MergingIterator::new(vec![Box::new(a), Box::new(b), Box::new(c)]);
        m.seek_to_first();
        let got: Vec<String> = collect(&mut m)
            .into_iter()
            .map(|v| String::from_utf8(v).unwrap())
            .collect();
        assert_eq!(got, ["a5", "a2", "b9", "c1", "e4", "e3"]);
        m.seek(&ik("c", 100));
        assert_eq!(m.key(), ik("c", 1).as_slice());
        m.seek(&ik("z", 0));
        assert!(!m.valid());
        assert!(m.status().is_ok());
    }

    #[test]
    fn ties_prefer_lower_index() {
        let a = VecIter::new(vec![(ik("k", 1), b"new".to_vec())]);
        let b = VecIter::new(vec![(ik("k", 1), b"old".to_vec())]);
        let mut m = MergingIterator::new(vec![Box::new(a), Box::new(b)]);
        m.seek_to_first();
        assert_eq!(m.value(), b"new");
        m.next();
        assert_eq!(m.value(), b"old");
        m.next();
        assert!(!m.valid());
    }

    #[test]
    fn many_children() {
        let children: Vec<Box<dyn InternalIterator>> = (0..17u64)
            .map(|i| {
                let entries = (0..20u64)
                    .map(|j| (ik(&format!("{:04}", j * 17 + i), 1), vec![]))
                    .collect();
                Box::new(VecIter::new(entries)) as Box<dyn InternalIterator>
            })
            .collect();
        let mut m = MergingIterator::new(children);
        m.seek_to_first();
        let mut n = 0u64;
        while m.valid() {
            assert_eq!(crate::key::user_key(m.key()), format!("{n:04}").as_bytes());
            n += 1;
            m.next();
        }
        assert_eq!(n, 340);
    }
}
