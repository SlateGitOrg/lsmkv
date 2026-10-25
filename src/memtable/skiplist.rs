//! A concurrent skiplist: one writer at a time, any number of lock-free readers.
//!
//! This follows LevelDB's design. Nodes are heap-allocated, published with a
//! `Release` store into their predecessors' `next` pointers and read with
//! `Acquire` loads, so a reader that observes a node pointer also observes the
//! node's fully initialised contents. Nodes are never unlinked or freed while
//! the list is alive (memtables are immutable once full and are dropped
//! wholesale), which is what makes lock-free reads sound without epochs or
//! hazard pointers. Writers serialise on an internal mutex.

use crate::util::rng::Rng;
use std::cmp::Ordering;
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, PoisonError};

/// Maximum tower height; with branching factor 4 this comfortably indexes
/// 4^12 ≈ 16M entries per memtable.
pub const MAX_HEIGHT: usize = 12;
const BRANCHING: u64 = 4;

/// Key comparison function used to order the list.
pub type Comparator = fn(&[u8], &[u8]) -> Ordering;

struct Node {
    key: Box<[u8]>,
    value: Box<[u8]>,
    next: Box<[AtomicPtr<Node>]>,
}

impl Node {
    fn alloc(key: &[u8], value: &[u8], height: usize) -> *mut Node {
        let next = (0..height)
            .map(|_| AtomicPtr::new(ptr::null_mut()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Box::into_raw(Box::new(Node {
            key: key.into(),
            value: value.into(),
            next,
        }))
    }

    fn next(&self, level: usize) -> *mut Node {
        self.next[level].load(AtomicOrdering::Acquire)
    }
}

/// An ordered, insert-only map from byte keys to byte values.
pub struct SkipList {
    head: *mut Node,
    max_height: AtomicUsize,
    cmp: Comparator,
    writer: Mutex<Rng>,
    len: AtomicUsize,
}

// SAFETY: all shared mutable state is either atomic (`next` links, counters)
// or behind the writer mutex. Node contents other than links are immutable
// after publication, and nodes are only freed in `Drop`, which has exclusive
// access.
unsafe impl Send for SkipList {}
// SAFETY: see the `Send` impl above; `&SkipList` only permits atomic reads and
// mutex-guarded inserts.
unsafe impl Sync for SkipList {}

impl SkipList {
    /// An empty list ordered by `cmp`.
    pub fn new(cmp: Comparator) -> Self {
        SkipList {
            head: Node::alloc(&[], &[], MAX_HEIGHT),
            max_height: AtomicUsize::new(1),
            cmp,
            writer: Mutex::new(Rng::new(0xdead_beef)),
            len: AtomicUsize::new(0),
        }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.len.load(AtomicOrdering::Acquire)
    }

    /// True if the list has no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn head(&self) -> &Node {
        // SAFETY: `head` is allocated in `new` and freed only in `Drop`.
        unsafe { &*self.head }
    }

    /// Returns the first node with key >= `key` and, when `prev` is given,
    /// records the last node < `key` at each level.
    fn find_greater_or_equal(
        &self,
        key: &[u8],
        mut prev: Option<&mut [*mut Node; MAX_HEIGHT]>,
    ) -> *mut Node {
        let mut x: *mut Node = self.head;
        let mut level = self.max_height.load(AtomicOrdering::Acquire) - 1;
        loop {
            // SAFETY: `x` is the head or a node reached through Acquire loads
            // of published links; nodes live as long as the list.
            let next = unsafe { (*x).next(level) };
            // SAFETY: `next` is null or a published node (see above).
            let go_right = !next.is_null() && (self.cmp)(unsafe { &(*next).key }, key).is_lt();
            if go_right {
                x = next;
            } else {
                if let Some(p) = prev.as_deref_mut() {
                    p[level] = x;
                }
                if level == 0 {
                    return next;
                }
                level -= 1;
            }
        }
    }

    /// Inserts `key -> value`. Keys must be unique; inserting a key that is
    /// already present is a logic error (internal keys embed a unique sequence
    /// number, so the engine never does it).
    pub fn insert(&self, key: &[u8], value: &[u8]) {
        let mut rng = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let mut prev = [ptr::null_mut(); MAX_HEIGHT];
        let found = self.find_greater_or_equal(key, Some(&mut prev));
        debug_assert!(
            // SAFETY: `found` is null or a published node.
            found.is_null() || (self.cmp)(unsafe { &(*found).key }, key).is_ne(),
            "duplicate key inserted into skiplist"
        );
        let mut height = 1;
        while height < MAX_HEIGHT && rng.one_in(BRANCHING) {
            height += 1;
        }
        let cur_max = self.max_height.load(AtomicOrdering::Relaxed);
        if height > cur_max {
            for p in prev.iter_mut().take(height).skip(cur_max) {
                *p = self.head;
            }
            // Readers that see the new height before the node is linked just
            // find null links at the new levels and drop down: harmless.
            self.max_height.store(height, AtomicOrdering::Release);
        }
        let node = Node::alloc(key, value, height);
        for (level, &p) in prev.iter().enumerate().take(height) {
            // SAFETY: `node` is freshly allocated and unpublished; `p` is the
            // head or a live node. The Relaxed store into the new node is
            // published by the Release store into `p` that follows.
            unsafe {
                let succ = (*p).next[level].load(AtomicOrdering::Relaxed);
                (*node).next[level].store(succ, AtomicOrdering::Relaxed);
                (*p).next[level].store(node, AtomicOrdering::Release);
            }
        }
        self.len.fetch_add(1, AtomicOrdering::Release);
    }

    /// True if an entry equal to `key` exists.
    pub fn contains(&self, key: &[u8]) -> bool {
        let n = self.find_greater_or_equal(key, None);
        // SAFETY: `n` is null or a published node.
        !n.is_null() && (self.cmp)(unsafe { &(*n).key }, key).is_eq()
    }
}

impl Drop for SkipList {
    fn drop(&mut self) {
        let mut x = self.head;
        while !x.is_null() {
            // SAFETY: we have exclusive access; every node was created by
            // `Box::into_raw` and is reachable exactly once via level 0.
            let node = unsafe { Box::from_raw(x) };
            x = node.next[0].load(AtomicOrdering::Relaxed);
        }
    }
}

/// A cursor over a shared skiplist. Holding the `Arc` keeps every node alive,
/// so the cursor's raw position stays valid for its whole lifetime.
pub struct SkipIter {
    list: Arc<SkipList>,
    node: *const Node,
}

// SAFETY: the cursor only reads immutable node data and atomic links of a list
// it keeps alive through the `Arc`.
unsafe impl Send for SkipIter {}
// SAFETY: as above; `&SkipIter` exposes read-only access.
unsafe impl Sync for SkipIter {}

impl SkipIter {
    /// A cursor that is initially not positioned.
    pub fn new(list: Arc<SkipList>) -> Self {
        SkipIter {
            list,
            node: ptr::null(),
        }
    }

    fn node(&self) -> &Node {
        assert!(self.valid(), "skiplist cursor is not positioned");
        // SAFETY: non-null positions always point at live nodes of `list`.
        unsafe { &*self.node }
    }

    /// True if positioned at an entry.
    pub fn valid(&self) -> bool {
        !self.node.is_null()
    }

    /// Current key. Panics if not valid.
    pub fn key(&self) -> &[u8] {
        &self.node().key
    }

    /// Current value. Panics if not valid.
    pub fn value(&self) -> &[u8] {
        &self.node().value
    }

    /// Advances to the next entry.
    pub fn next(&mut self) {
        self.node = self.node().next(0);
    }

    /// Positions at the first entry with key >= `target`.
    pub fn seek(&mut self, target: &[u8]) {
        self.node = self.list.find_greater_or_equal(target, None);
    }

    /// Positions at the first entry.
    pub fn seek_to_first(&mut self) {
        self.node = self.list.head().next(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::thread;

    fn bytewise(a: &[u8], b: &[u8]) -> Ordering {
        a.cmp(b)
    }

    #[test]
    fn empty_list() {
        let list = Arc::new(SkipList::new(bytewise));
        assert!(list.is_empty());
        assert!(!list.contains(b"x"));
        let mut it = SkipIter::new(list);
        assert!(!it.valid());
        it.seek_to_first();
        assert!(!it.valid());
        it.seek(b"a");
        assert!(!it.valid());
    }

    #[test]
    fn insert_and_iterate_sorted() {
        let list = Arc::new(SkipList::new(bytewise));
        let mut model = BTreeSet::new();
        let mut rng = Rng::new(7);
        for _ in 0..2000 {
            let k = format!("{:06}", rng.below(5000));
            if model.insert(k.clone()) {
                list.insert(k.as_bytes(), b"v");
            }
        }
        assert_eq!(list.len(), model.len());
        let mut it = SkipIter::new(list.clone());
        it.seek_to_first();
        for k in &model {
            assert!(it.valid());
            assert_eq!(it.key(), k.as_bytes());
            assert_eq!(it.value(), b"v");
            it.next();
        }
        assert!(!it.valid());
        for probe in ["000000", "002500", "004999", "999999"] {
            it.seek(probe.as_bytes());
            let expect = model.range(probe.to_string()..).next();
            assert_eq!(
                it.valid().then(|| it.key().to_vec()),
                expect.map(|s| s.as_bytes().to_vec())
            );
            assert_eq!(list.contains(probe.as_bytes()), model.contains(probe));
        }
    }

    #[test]
    fn concurrent_readers_see_consistent_prefixes() {
        let list = Arc::new(SkipList::new(bytewise));
        let writer = {
            let list = list.clone();
            thread::spawn(move || {
                for i in 0..5000u32 {
                    list.insert(&i.to_be_bytes(), &i.to_le_bytes());
                }
            })
        };
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let list = list.clone();
                thread::spawn(move || {
                    for _ in 0..50 {
                        let mut it = SkipIter::new(list.clone());
                        it.seek_to_first();
                        let mut expect = 0u32;
                        while it.valid() {
                            // Keys are inserted in order, so every snapshot a
                            // reader sees is a contiguous prefix.
                            assert_eq!(it.key(), expect.to_be_bytes());
                            assert_eq!(it.value(), expect.to_le_bytes());
                            expect += 1;
                            it.next();
                        }
                    }
                })
            })
            .collect();
        writer.join().unwrap();
        for r in readers {
            r.join().unwrap();
        }
        assert_eq!(list.len(), 5000);
    }
}
