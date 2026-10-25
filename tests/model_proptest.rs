//! Model-based property tests: random operation sequences are applied both to
//! the engine and to a `BTreeMap` model (plus a frozen copy of the model per
//! live snapshot), and every read must agree.

mod common;

use common::Model;
use lsmkv::fs::SimFs;
use lsmkv::{Db, Options, ReadOptions, Snapshot, WriteBatch, WriteOptions};
use proptest::prelude::*;
use std::sync::Arc;

#[derive(Debug, Clone)]
enum Op {
    Put(u8, u16),
    Delete(u8),
    Batch(Vec<(u8, Option<u16>)>),
    Get(u8),
    Scan(u8, u8),
    TakeSnapshot,
    ReadSnapshot(usize, u8),
    ScanSnapshot(usize),
    DropSnapshot(usize),
    Flush,
    CompactAll,
    CompactRange(u8, u8),
    Reopen,
}

/// Keys share long prefixes (exercising prefix compression) and collide often
/// (exercising overwrites, tombstones and version dropping).
fn key(k: u8) -> Vec<u8> {
    format!("tenant-{}/object-{:03}", k % 4, k).into_bytes()
}

fn value(k: u8, v: u16) -> Vec<u8> {
    let mut out = format!("{k}:{v}:").into_bytes();
    out.resize(8 + (v as usize % 300), b'x');
    out
}

fn op() -> impl Strategy<Value = Op> {
    let k = 0u8..80;
    prop_oneof![
        30 => (k.clone(), any::<u16>()).prop_map(|(k, v)| Op::Put(k, v)),
        8 => k.clone().prop_map(Op::Delete),
        6 => prop::collection::vec((k.clone(), prop::option::weighted(0.8, any::<u16>())), 1..6)
            .prop_map(Op::Batch),
        12 => k.clone().prop_map(Op::Get),
        4 => (k.clone(), k.clone()).prop_map(|(a, b)| Op::Scan(a, b)),
        3 => Just(Op::TakeSnapshot),
        5 => (any::<usize>(), k.clone()).prop_map(|(i, k)| Op::ReadSnapshot(i, k)),
        2 => any::<usize>().prop_map(Op::ScanSnapshot),
        2 => any::<usize>().prop_map(Op::DropSnapshot),
        2 => Just(Op::Flush),
        1 => Just(Op::CompactAll),
        1 => (k.clone(), k).prop_map(|(a, b)| Op::CompactRange(a, b)),
        1 => Just(Op::Reopen),
    ]
}

/// The two keys in byte order.
fn bounds(a: u8, b: u8) -> (Vec<u8>, Vec<u8>) {
    let (x, y) = (key(a), key(b));
    if x <= y {
        (x, y)
    } else {
        (y, x)
    }
}

fn scan_db(
    db: &Db,
    ro: &ReadOptions<'_>,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    db.scan(ro, lo, hi).unwrap().map(|r| r.unwrap()).collect()
}

fn scan_model(m: &Model, lo: &[u8], hi: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    m.range(lo.to_vec()..hi.to_vec())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn run(ops: Vec<Op>, opts: Options) -> Result<(), TestCaseError> {
    let fs = SimFs::new();
    let open = || Db::open_with_fs(Arc::new(fs.clone()), "/db", opts.clone()).unwrap();
    let mut db = open();
    let mut model = Model::new();
    let mut snaps: Vec<(Snapshot, Model)> = Vec::new();
    for op in ops {
        match op {
            Op::Put(k, v) => {
                db.put(&key(k), &value(k, v)).unwrap();
                model.insert(key(k), value(k, v));
            }
            Op::Delete(k) => {
                db.delete(&key(k)).unwrap();
                model.remove(&key(k));
            }
            Op::Batch(entries) => {
                let mut b = WriteBatch::new();
                for (k, v) in entries {
                    match v {
                        Some(v) => {
                            b.put(&key(k), &value(k, v));
                            model.insert(key(k), value(k, v));
                        }
                        None => {
                            b.delete(&key(k));
                            model.remove(&key(k));
                        }
                    }
                }
                db.write(&WriteOptions::default(), b).unwrap();
            }
            Op::Get(k) => {
                prop_assert_eq!(db.get(&key(k)).unwrap(), model.get(&key(k)).cloned());
            }
            Op::Scan(a, b) => {
                let (lo, hi) = bounds(a, b);
                let got = scan_db(&db, &ReadOptions::default(), Some(&lo), Some(&hi));
                prop_assert_eq!(got, scan_model(&model, &lo, &hi));
            }
            Op::TakeSnapshot => snaps.push((db.snapshot(), model.clone())),
            Op::ReadSnapshot(i, k) if !snaps.is_empty() => {
                let (s, m) = &snaps[i % snaps.len()];
                let ro = ReadOptions {
                    snapshot: Some(s),
                    ..ReadOptions::default()
                };
                prop_assert_eq!(db.get_with(&ro, &key(k)).unwrap(), m.get(&key(k)).cloned());
            }
            Op::ScanSnapshot(i) if !snaps.is_empty() => {
                let (s, m) = &snaps[i % snaps.len()];
                let ro = ReadOptions {
                    snapshot: Some(s),
                    ..ReadOptions::default()
                };
                let got = scan_db(&db, &ro, None, None);
                let want: Vec<_> = m.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                prop_assert_eq!(got, want);
            }
            Op::DropSnapshot(i) if !snaps.is_empty() => {
                let idx = i % snaps.len();
                snaps.remove(idx);
            }
            Op::ReadSnapshot(..) | Op::ScanSnapshot(_) | Op::DropSnapshot(_) => {}
            Op::Flush => db.flush().unwrap(),
            Op::CompactAll => db.compact_range(None, None).unwrap(),
            Op::CompactRange(a, b) => {
                let (lo, hi) = bounds(a, b);
                db.compact_range(Some(&lo), Some(&hi)).unwrap();
            }
            Op::Reopen => {
                snaps.clear();
                drop(db);
                db = open();
            }
        }
        prop_assert_eq!(db.live_snapshots(), snaps.len());
    }
    // Final full comparison, then once more after a reopen.
    let all: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    prop_assert_eq!(
        scan_db(&db, &ReadOptions::default(), None, None),
        all.clone()
    );
    snaps.clear();
    drop(db);
    let db = open();
    prop_assert_eq!(scan_db(&db, &ReadOptions::default(), None, None), all);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn engine_matches_btreemap_inline(ops in prop::collection::vec(op(), 1..600)) {
        run(ops, Options::for_testing())?;
    }

    #[test]
    fn engine_matches_btreemap_background(ops in prop::collection::vec(op(), 1..300)) {
        run(ops, Options { background_compaction: true, ..Options::for_testing() })?;
    }

    #[test]
    fn engine_matches_btreemap_tiny_blocks_no_bloom(ops in prop::collection::vec(op(), 1..300)) {
        let mut opts = Options::for_testing();
        opts.table.block_size = 64;
        opts.table.restart_interval = 1;
        opts.table.bloom_bits_per_key = 0;
        opts.block_cache_bytes = 0;
        opts.max_open_tables = 2;
        run(ops, opts)?;
    }
}
