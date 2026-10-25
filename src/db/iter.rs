//! User-facing iteration: collapses the merged internal stream into the
//! newest visible version of each user key, hiding tombstones.

use crate::error::Result;
use crate::iterator::{InternalIterator, MergingIterator};
use crate::key::{parse_internal_key, SeqNum, ValueKind};
use crate::version::{lookup_key, Version};
use std::sync::Arc;

/// A forward cursor over live keys as of a fixed sequence number, optionally
/// bounded to `[lower, upper)`.
///
/// The cursor pins the memtables and the version it was created from, so
/// it keeps seeing a consistent state while writes, flushes and compactions
/// continue.
pub struct DbIterator {
    inner: MergingIterator,
    seq: SeqNum,
    lower: Option<Vec<u8>>,
    upper: Option<Vec<u8>>,
    key: Vec<u8>,
    value: Vec<u8>,
    valid: bool,
    corrupt: Option<crate::Error>,
    _version: Arc<Version>,
}

impl DbIterator {
    pub(crate) fn new(
        inner: MergingIterator,
        seq: SeqNum,
        lower: Option<Vec<u8>>,
        upper: Option<Vec<u8>>,
        version: Arc<Version>,
    ) -> Self {
        DbIterator {
            inner,
            seq,
            lower,
            upper,
            key: Vec::new(),
            value: Vec::new(),
            valid: false,
            corrupt: None,
            _version: version,
        }
    }

    /// Advances `inner` to the next visible live entry. With `skipping`,
    /// entries whose user key is `<= self.key` are hidden (older versions of
    /// keys already returned or deleted).
    fn find_next_user_entry(&mut self, mut skipping: bool) {
        self.valid = false;
        while self.inner.valid() {
            let Some(p) = parse_internal_key(self.inner.key()) else {
                self.corrupt = Some(crate::Error::corruption("bad internal key during scan"));
                return;
            };
            if self.upper.as_deref().is_some_and(|u| p.user_key >= u) {
                return;
            }
            if p.seq <= self.seq {
                match p.kind {
                    ValueKind::Deletion => {
                        self.key.clear();
                        self.key.extend_from_slice(p.user_key);
                        skipping = true;
                    }
                    ValueKind::Value => {
                        if !(skipping && p.user_key <= self.key.as_slice()) {
                            self.key.clear();
                            self.key.extend_from_slice(p.user_key);
                            self.value.clear();
                            self.value.extend_from_slice(self.inner.value());
                            self.valid = true;
                            return;
                        }
                    }
                }
            }
            self.inner.next();
        }
    }

    /// Positions at the first live key (>= the lower bound).
    pub fn seek_to_first(&mut self) {
        match self.lower.clone() {
            Some(lo) => self.seek(&lo),
            None => {
                self.inner.seek_to_first();
                self.find_next_user_entry(false);
            }
        }
    }

    /// Positions at the first live key >= `target` (clamped to the bounds).
    pub fn seek(&mut self, target: &[u8]) {
        let target = match &self.lower {
            Some(lo) if target < lo.as_slice() => lo.clone(),
            _ => target.to_vec(),
        };
        self.inner.seek(&lookup_key(&target, self.seq));
        self.find_next_user_entry(false);
    }

    /// True if positioned at a key.
    pub fn valid(&self) -> bool {
        self.valid
    }

    /// Advances to the next live key. Requires `valid()`.
    pub fn next(&mut self) {
        assert!(self.valid, "next() on an invalid iterator");
        // `self.key` holds the current key, so skipping hides its older versions.
        self.inner.next();
        self.find_next_user_entry(true);
    }

    /// Current key. Requires `valid()`.
    pub fn key(&self) -> &[u8] {
        assert!(self.valid);
        &self.key
    }

    /// Current value. Requires `valid()`.
    pub fn value(&self) -> &[u8] {
        assert!(self.valid);
        &self.value
    }

    /// First error hit (I/O or corruption); check it when `valid()` turns false.
    pub fn status(&self) -> Result<()> {
        if let Some(e) = &self.corrupt {
            return Err(e.duplicate());
        }
        self.inner.status()
    }
}

/// A Rust [`Iterator`] over `(key, value)` pairs of a range, built on
/// [`DbIterator`]. Yields one `Err` and stops if the scan hits an error.
pub struct Scan {
    it: DbIterator,
    started: bool,
    done: bool,
}

impl Scan {
    pub(crate) fn new(it: DbIterator) -> Self {
        Scan {
            it,
            started: false,
            done: false,
        }
    }
}

impl Iterator for Scan {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if self.started {
            self.it.next();
        } else {
            self.it.seek_to_first();
            self.started = true;
        }
        if self.it.valid() {
            return Some(Ok((self.it.key().to_vec(), self.it.value().to_vec())));
        }
        self.done = true;
        match self.it.status() {
            Ok(()) => None,
            Err(e) => Some(Err(e)),
        }
    }
}
