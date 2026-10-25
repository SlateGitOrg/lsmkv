//! A sharded, charge-bounded LRU cache with shared (`Arc`) values.
//! Used for the block cache (keyed by file number + block offset) and the
//! table cache (keyed by file number).

use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, RandomState};
use std::sync::{Arc, Mutex, PoisonError};

const NIL: usize = usize::MAX;
const SHARDS: usize = 16;

struct Entry<K, V> {
    key: K,
    value: Arc<V>,
    charge: usize,
    prev: usize,
    next: usize,
}

struct Shard<K, V> {
    map: HashMap<K, usize>,
    slots: Vec<Option<Entry<K, V>>>,
    free: Vec<usize>,
    /// Most recently used.
    head: usize,
    /// Least recently used.
    tail: usize,
    usage: usize,
    capacity: usize,
}

impl<K: Hash + Eq + Clone, V> Shard<K, V> {
    fn new(capacity: usize) -> Self {
        Shard {
            map: HashMap::new(),
            slots: Vec::new(),
            free: Vec::new(),
            head: NIL,
            tail: NIL,
            usage: 0,
            capacity,
        }
    }

    fn entry(&mut self, i: usize) -> &mut Entry<K, V> {
        self.slots[i].as_mut().expect("live slot")
    }

    fn unlink(&mut self, i: usize) {
        let (prev, next) = {
            let e = self.entry(i);
            (e.prev, e.next)
        };
        if prev == NIL {
            self.head = next;
        } else {
            self.entry(prev).next = next;
        }
        if next == NIL {
            self.tail = prev;
        } else {
            self.entry(next).prev = prev;
        }
    }

    fn push_front(&mut self, i: usize) {
        let old = self.head;
        {
            let e = self.entry(i);
            e.prev = NIL;
            e.next = old;
        }
        if old == NIL {
            self.tail = i;
        } else {
            self.entry(old).prev = i;
        }
        self.head = i;
    }

    fn get(&mut self, key: &K) -> Option<Arc<V>> {
        let i = *self.map.get(key)?;
        self.unlink(i);
        self.push_front(i);
        Some(self.entry(i).value.clone())
    }

    fn remove_slot(&mut self, i: usize) {
        self.unlink(i);
        if let Some(e) = self.slots[i].take() {
            self.usage -= e.charge;
            self.map.remove(&e.key);
        }
        self.free.push(i);
    }

    fn insert(&mut self, key: K, value: Arc<V>, charge: usize) {
        if let Some(&i) = self.map.get(&key) {
            self.remove_slot(i);
        }
        let entry = Entry {
            key: key.clone(),
            value,
            charge,
            prev: NIL,
            next: NIL,
        };
        let i = match self.free.pop() {
            Some(i) => {
                self.slots[i] = Some(entry);
                i
            }
            None => {
                self.slots.push(Some(entry));
                self.slots.len() - 1
            }
        };
        self.map.insert(key, i);
        self.push_front(i);
        self.usage += charge;
        while self.usage > self.capacity && self.tail != NIL && self.tail != i {
            let t = self.tail;
            self.remove_slot(t);
        }
    }

    fn erase(&mut self, key: &K) {
        if let Some(&i) = self.map.get(key) {
            self.remove_slot(i);
        }
    }
}

/// Thread-safe LRU cache.
pub struct LruCache<K, V> {
    shards: Vec<Mutex<Shard<K, V>>>,
    hasher: RandomState,
}

impl<K: Hash + Eq + Clone, V> LruCache<K, V> {
    /// A cache holding at most `capacity` total charge (split across shards).
    pub fn new(capacity: usize) -> Self {
        let per = capacity.div_ceil(SHARDS).max(1);
        LruCache {
            shards: (0..SHARDS).map(|_| Mutex::new(Shard::new(per))).collect(),
            hasher: RandomState::new(),
        }
    }

    fn shard(&self, key: &K) -> std::sync::MutexGuard<'_, Shard<K, V>> {
        let i = (self.hasher.hash_one(key) as usize) % SHARDS;
        self.shards[i]
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Looks up `key`, marking it most recently used.
    pub fn get(&self, key: &K) -> Option<Arc<V>> {
        self.shard(key).get(key)
    }

    /// Inserts or replaces `key`, evicting least recently used entries.
    pub fn insert(&self, key: K, value: Arc<V>, charge: usize) {
        self.shard(&key).insert(key, value, charge);
    }

    /// Removes `key` if present.
    pub fn erase(&self, key: &K) {
        self.shard(key).erase(key);
    }

    /// Total charge currently held.
    pub fn usage(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.lock().unwrap_or_else(PoisonError::into_inner).usage)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_eviction_order_in_one_shard() {
        let mut s: Shard<u32, u32> = Shard::new(3);
        for k in 0..3 {
            s.insert(k, Arc::new(k * 10), 1);
        }
        assert_eq!(s.get(&0).as_deref(), Some(&0)); // 0 becomes MRU
        s.insert(3, Arc::new(30), 1); // evicts 1 (LRU)
        assert!(s.get(&1).is_none());
        assert!(s.get(&0).is_some() && s.get(&2).is_some() && s.get(&3).is_some());
        s.insert(2, Arc::new(99), 1); // replace keeps usage
        assert_eq!(s.usage, 3);
        assert_eq!(s.get(&2).as_deref(), Some(&99));
        s.erase(&2);
        s.erase(&42);
        assert_eq!(s.usage, 2);
        // An entry larger than capacity is kept alone (never evicts itself).
        s.insert(7, Arc::new(7), 10);
        assert_eq!(s.usage, 10);
        assert!(s.get(&7).is_some());
        assert!(s.get(&0).is_none());
    }

    #[test]
    fn sharded_cache_bounds_usage() {
        let c: LruCache<u64, Vec<u8>> = LruCache::new(16 * 100);
        for k in 0..10_000u64 {
            c.insert(k, Arc::new(vec![0; 10]), 10);
        }
        assert!(c.usage() <= 16 * 100 + 16 * 10);
        assert!(c.get(&9_999).is_some());
        c.erase(&9_999);
        assert!(c.get(&9_999).is_none());
    }
}
