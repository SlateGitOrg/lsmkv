//! Shared helpers for integration tests: deterministic workloads, a
//! `BTreeMap` reference model, and database dumps.
#![allow(dead_code)]

use lsmkv::fs::SimFs;
use lsmkv::util::rng::Rng;
use lsmkv::{Db, Options, ReadOptions, WriteBatch, WriteOptions};
use std::collections::BTreeMap;
use std::sync::Arc;

pub type Model = BTreeMap<Vec<u8>, Vec<u8>>;

/// One logical write of a workload.
#[derive(Debug, Clone)]
pub enum Write {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
    Batch(Vec<(Vec<u8>, Option<Vec<u8>>)>),
}

impl Write {
    pub fn apply(&self, m: &mut Model) {
        match self {
            Write::Put(k, v) => {
                m.insert(k.clone(), v.clone());
            }
            Write::Delete(k) => {
                m.remove(k);
            }
            Write::Batch(ops) => {
                for (k, v) in ops {
                    match v {
                        Some(v) => m.insert(k.clone(), v.clone()),
                        None => m.remove(k),
                    };
                }
            }
        }
    }

    pub fn to_batch(&self) -> WriteBatch {
        let mut b = WriteBatch::new();
        match self {
            Write::Put(k, v) => {
                b.put(k, v);
            }
            Write::Delete(k) => {
                b.delete(k);
            }
            Write::Batch(ops) => {
                for (k, v) in ops {
                    match v {
                        Some(v) => b.put(k, v),
                        None => b.delete(k),
                    };
                }
            }
        }
        b
    }
}

pub fn key(i: u64) -> Vec<u8> {
    format!("user/{i:05}").into_bytes()
}

/// A deterministic mix of puts (70%), deletes (15%) and multi-op batches (15%)
/// over `keys` distinct keys with values of 20–220 bytes.
pub fn workload(seed: u64, n: usize, keys: u64) -> Vec<Write> {
    let mut rng = Rng::new(seed);
    let value = |rng: &mut Rng, i: usize| {
        let len = 20 + rng.below(200) as usize;
        let mut v = format!("v{i}:").into_bytes();
        v.resize(len, b'a' + (i % 26) as u8);
        v
    };
    (0..n)
        .map(|i| match rng.below(100) {
            0..=69 => Write::Put(key(rng.below(keys)), value(&mut rng, i)),
            70..=84 => Write::Delete(key(rng.below(keys))),
            _ => Write::Batch(
                (0..2 + rng.below(4))
                    .map(|_| {
                        let k = key(rng.below(keys));
                        let v = (!rng.one_in(4)).then(|| value(&mut rng, i));
                        (k, v)
                    })
                    .collect(),
            ),
        })
        .collect()
}

/// `models[k]` is the state after the first `k` writes.
pub fn prefix_models(writes: &[Write]) -> Vec<Model> {
    let mut out = Vec::with_capacity(writes.len() + 1);
    let mut m = Model::new();
    out.push(m.clone());
    for w in writes {
        w.apply(&mut m);
        out.push(m.clone());
    }
    out
}

/// Full scan of the latest state, checked against point lookups.
pub fn dump(db: &Db) -> Model {
    let m: Model = db
        .scan(&ReadOptions::default(), None, None)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    for (k, v) in m.iter().take(20) {
        assert_eq!(db.get(k).unwrap().as_ref(), Some(v), "get/scan disagree");
    }
    m
}

pub fn open(fs: &SimFs, opts: Options) -> lsmkv::Result<Db> {
    Db::open_with_fs(Arc::new(fs.clone()), "/db", opts)
}

/// Applies writes until one fails; returns how many were acknowledged.
pub fn run_until_failure(db: &Db, writes: &[Write], sync: impl Fn(usize) -> bool) -> usize {
    for (i, w) in writes.iter().enumerate() {
        if db
            .write(&WriteOptions { sync: sync(i) }, w.to_batch())
            .is_err()
        {
            return i;
        }
    }
    writes.len()
}

/// Asserts every table on disk is referenced by the current version (no
/// leaked files after recovery and garbage collection).
pub fn assert_no_leaked_tables(fs: &SimFs, db: &Db) {
    use lsmkv::fs::FileSystem;
    db.wait_for_compactions().unwrap();
    let mut names = fs.list(std::path::Path::new("/db")).unwrap();
    names.sort();
    let on_disk = names.iter().filter(|n| n.ends_with(".sst")).count();
    let referenced: usize = db.levels().iter().map(|l| l.files).sum();
    assert_eq!(
        on_disk,
        referenced,
        "orphaned table files after recovery: {names:?}\n{}",
        db.describe_levels()
    );
}
