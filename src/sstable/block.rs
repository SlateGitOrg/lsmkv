//! Prefix-compressed blocks with restart points.
//!
//! ```text
//! entry   := shared: varint32 | unshared: varint32 | value_len: varint32
//!            | key_delta[unshared] | value[value_len]
//! block   := entry* | restart_offset: fixed32 * R | R: fixed32
//! ```
//!
//! Every `restart_interval` entries the full key is stored (`shared = 0`) and
//! its offset recorded, so a seek binary-searches the restart array and then
//! scans at most `restart_interval` entries: `O(log(n/r) + r)` per seek.

use crate::error::{Error, Result};
use crate::iterator::InternalIterator;
use crate::util::coding::{decode_fixed32, put_fixed32, put_varint32, Decoder};
use std::cmp::Ordering;
use std::sync::Arc;

/// Key ordering used inside a block.
pub type KeyCmp = fn(&[u8], &[u8]) -> Ordering;

/// Accumulates sorted entries into a block.
#[derive(Debug)]
pub struct BlockBuilder {
    buf: Vec<u8>,
    restarts: Vec<u32>,
    counter: usize,
    restart_interval: usize,
    last_key: Vec<u8>,
    entries: usize,
}

impl BlockBuilder {
    /// A builder with the given restart interval (>= 1).
    pub fn new(restart_interval: usize) -> Self {
        BlockBuilder {
            buf: Vec::new(),
            restarts: vec![0],
            counter: 0,
            restart_interval: restart_interval.max(1),
            last_key: Vec::new(),
            entries: 0,
        }
    }

    /// Appends an entry; keys must arrive in strictly increasing order.
    pub fn add(&mut self, key: &[u8], value: &[u8]) {
        let shared = if self.counter < self.restart_interval {
            self.last_key
                .iter()
                .zip(key)
                .take_while(|(a, b)| a == b)
                .count()
        } else {
            self.restarts.push(self.buf.len() as u32);
            self.counter = 0;
            0
        };
        put_varint32(&mut self.buf, shared as u32);
        put_varint32(&mut self.buf, (key.len() - shared) as u32);
        put_varint32(&mut self.buf, value.len() as u32);
        self.buf.extend_from_slice(&key[shared..]);
        self.buf.extend_from_slice(value);
        self.last_key.truncate(shared);
        self.last_key.extend_from_slice(&key[shared..]);
        self.counter += 1;
        self.entries += 1;
    }

    /// Size the block would have if finished now.
    pub fn estimated_size(&self) -> usize {
        self.buf.len() + 4 * self.restarts.len() + 4
    }

    /// True if no entries were added since the last reset.
    pub fn is_empty(&self) -> bool {
        self.entries == 0
    }

    /// Writes the restart trailer and returns the finished block, resetting
    /// the builder for reuse.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = std::mem::take(&mut self.buf);
        for &r in &self.restarts {
            put_fixed32(&mut out, r);
        }
        put_fixed32(&mut out, self.restarts.len() as u32);
        self.restarts = vec![0];
        self.counter = 0;
        self.last_key.clear();
        self.entries = 0;
        out
    }
}

/// An immutable, validated block.
#[derive(Debug)]
pub struct Block {
    data: Vec<u8>,
    restart_offset: usize,
    num_restarts: usize,
}

impl Block {
    /// Validates the restart trailer of `data`.
    pub fn new(data: Vec<u8>) -> Result<Block> {
        if data.len() < 4 {
            return Err(Error::corruption("block too short"));
        }
        let num_restarts = decode_fixed32(&data[data.len() - 4..]) as usize;
        let max = (data.len() - 4) / 4;
        if num_restarts == 0 || num_restarts > max {
            return Err(Error::corruption("bad restart count in block"));
        }
        let restart_offset = data.len() - 4 - 4 * num_restarts;
        Ok(Block {
            data,
            restart_offset,
            num_restarts,
        })
    }

    /// Bytes held by the block (for cache charging).
    pub fn size(&self) -> usize {
        self.data.len()
    }

    fn restart_point(&self, i: usize) -> usize {
        decode_fixed32(&self.data[self.restart_offset + 4 * i..]) as usize
    }

    /// A cursor over this block ordered by `cmp`.
    pub fn iter(self: &Arc<Self>, cmp: KeyCmp) -> BlockIter {
        BlockIter {
            block: self.clone(),
            cmp,
            current: self.restart_offset,
            next: self.restart_offset,
            key: Vec::new(),
            value: (0, 0),
            err: None,
        }
    }
}

/// Cursor over a shared [`Block`].
pub struct BlockIter {
    block: Arc<Block>,
    cmp: KeyCmp,
    /// Offset of the current entry; `restart_offset` means invalid.
    current: usize,
    next: usize,
    key: Vec<u8>,
    value: (usize, usize),
    err: Option<Error>,
}

impl BlockIter {
    fn corrupt(&mut self, msg: &str) {
        self.err = Some(Error::corruption(msg.to_string()));
        self.current = self.block.restart_offset;
        self.next = self.block.restart_offset;
        self.key.clear();
    }

    /// Decodes the entry at `self.next`. Returns false at end or on corruption.
    fn parse_next(&mut self) -> bool {
        let end = self.block.restart_offset;
        self.current = self.next;
        if self.current >= end {
            self.current = end;
            return false;
        }
        let mut d = Decoder::new(&self.block.data[self.current..end]);
        let header = (d.varint32(), d.varint32(), d.varint32());
        let (Some(shared), Some(unshared), Some(vlen)) = header else {
            self.corrupt("bad entry header in block");
            return false;
        };
        let (shared, unshared, vlen) = (shared as usize, unshared as usize, vlen as usize);
        let rest = d.remaining();
        if shared > self.key.len() || rest.len() < unshared + vlen {
            self.corrupt("bad entry in block");
            return false;
        }
        let key_start = end - rest.len();
        self.key.truncate(shared);
        self.key
            .extend_from_slice(&self.block.data[key_start..key_start + unshared]);
        self.value = (key_start + unshared, key_start + unshared + vlen);
        self.next = self.value.1;
        true
    }

    fn seek_to_restart(&mut self, i: usize) {
        self.key.clear();
        self.next = self.block.restart_point(i);
    }

    /// Decodes the full key stored at restart point `i`.
    fn restart_key(&self, i: usize) -> Option<&[u8]> {
        let off = self.block.restart_point(i);
        let end = self.block.restart_offset;
        if off >= end {
            return None;
        }
        let mut d = Decoder::new(&self.block.data[off..end]);
        let shared = d.varint32()?;
        let unshared = d.varint32()? as usize;
        d.varint32()?;
        if shared != 0 {
            return None;
        }
        d.bytes(unshared)
    }
}

impl InternalIterator for BlockIter {
    fn valid(&self) -> bool {
        self.err.is_none() && self.current < self.block.restart_offset
    }

    fn seek_to_first(&mut self) {
        self.err = None;
        self.seek_to_restart(0);
        self.parse_next();
    }

    fn seek(&mut self, target: &[u8]) {
        self.err = None;
        // Find the last restart point whose key is < target.
        let (mut lo, mut hi) = (0usize, self.block.num_restarts - 1);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            match self.restart_key(mid) {
                Some(k) if (self.cmp)(k, target).is_lt() => lo = mid,
                Some(_) => hi = mid - 1,
                None => {
                    self.corrupt("bad restart point in block");
                    return;
                }
            }
        }
        self.seek_to_restart(lo);
        while self.parse_next() {
            if (self.cmp)(&self.key, target).is_ge() {
                return;
            }
        }
    }

    fn next(&mut self) {
        self.parse_next();
    }

    fn key(&self) -> &[u8] {
        &self.key
    }

    fn value(&self) -> &[u8] {
        &self.block.data[self.value.0..self.value.1]
    }

    fn status(&self) -> Result<()> {
        match &self.err {
            Some(e) => Err(e.duplicate()),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytewise(a: &[u8], b: &[u8]) -> Ordering {
        a.cmp(b)
    }

    fn build(keys: &[String], interval: usize) -> Arc<Block> {
        let mut b = BlockBuilder::new(interval);
        assert!(b.is_empty());
        for k in keys {
            b.add(k.as_bytes(), format!("v-{k}").as_bytes());
        }
        let est = b.estimated_size();
        let data = b.finish();
        assert_eq!(data.len(), est);
        assert!(b.is_empty());
        Arc::new(Block::new(data).unwrap())
    }

    #[test]
    fn roundtrip_and_seek() {
        let keys: Vec<String> = (0..500).map(|i| format!("key{:05}", i * 2)).collect();
        for interval in [1, 2, 16, 1000] {
            let block = build(&keys, interval);
            let mut it = block.iter(bytewise);
            it.seek_to_first();
            for k in &keys {
                assert!(it.valid());
                assert_eq!(it.key(), k.as_bytes());
                assert_eq!(it.value(), format!("v-{k}").as_bytes());
                it.next();
            }
            assert!(!it.valid());
            for probe in 0..1001 {
                let target = format!("key{probe:05}");
                it.seek(target.as_bytes());
                let expect = keys.iter().find(|k| **k >= target);
                assert_eq!(
                    it.valid().then(|| it.key().to_vec()),
                    expect.map(|k| k.as_bytes().to_vec())
                );
            }
            it.seek(b"");
            assert_eq!(it.key(), keys[0].as_bytes());
            assert!(it.status().is_ok());
        }
    }

    #[test]
    fn prefix_compression_shrinks_blocks() {
        let keys: Vec<String> = (0..200)
            .map(|i| format!("a-long-common-prefix/{i:06}"))
            .collect();
        let compressed = build(&keys, 16).size();
        let raw: usize = keys.iter().map(|k| k.len() * 2 + 5).sum();
        assert!(compressed < raw * 7 / 10, "{compressed} vs {raw}");
    }

    #[test]
    fn empty_block_and_corruption() {
        let mut b = BlockBuilder::new(16);
        let block = Arc::new(Block::new(b.finish()).unwrap());
        let mut it = block.iter(bytewise);
        it.seek_to_first();
        assert!(!it.valid());
        it.seek(b"x");
        assert!(!it.valid());
        assert!(Block::new(vec![1, 2]).is_err());
        assert!(Block::new(vec![0, 0, 0, 0]).is_err());
        assert!(Block::new(vec![9, 0, 0, 0, 200, 0, 0, 0]).is_err());

        // An entry whose unshared length overruns the block.
        let mut data = Vec::new();
        put_varint32(&mut data, 0);
        put_varint32(&mut data, 100);
        put_varint32(&mut data, 0);
        data.extend_from_slice(b"ab");
        put_fixed32(&mut data, 0);
        put_fixed32(&mut data, 1);
        let block = Arc::new(Block::new(data).unwrap());
        let mut it = block.iter(bytewise);
        it.seek_to_first();
        assert!(!it.valid());
        assert!(it.status().unwrap_err().is_corruption());
        // Restart point with a non-zero shared length is detected on seek.
        let mut data = Vec::new();
        for _ in 0..2 {
            put_varint32(&mut data, 1);
            put_varint32(&mut data, 1);
            put_varint32(&mut data, 0);
            data.push(b'z');
        }
        put_fixed32(&mut data, 0);
        put_fixed32(&mut data, 4);
        put_fixed32(&mut data, 2);
        let block = Arc::new(Block::new(data).unwrap());
        let mut it = block.iter(bytewise);
        it.seek(b"m");
        assert!(it.status().is_err());
        // Truncated header.
        let block = Arc::new(Block::new(vec![0x80, 0, 0, 0, 0, 1, 0, 0, 0]).unwrap());
        let mut it = block.iter(bytewise);
        it.seek_to_first();
        assert!(it.status().is_err());
    }
}
