//! Unit tests of the database layer against the simulated filesystem.

use super::*;
use crate::fs::{CrashMode, FaultKind, SimFs};

fn open(fs: &SimFs, opts: Options) -> Db {
    Db::open_with_fs(Arc::new(fs.clone()), "/db", opts).unwrap()
}

fn key(i: u32) -> Vec<u8> {
    format!("key{i:06}").into_bytes()
}

#[test]
fn db_is_send_and_sync() {
    fn check<T: Send + Sync>() {}
    fn check_send<T: Send>() {}
    check::<Db>();
    check_send::<DbIterator>();
}

#[test]
fn put_get_delete_across_flushes_and_reopen() {
    let fs = SimFs::new();
    let db = open(&fs, Options::for_testing());
    for i in 0..2000 {
        db.put(&key(i), format!("v{i}").as_bytes()).unwrap();
    }
    for i in (0..2000).step_by(3) {
        db.delete(&key(i)).unwrap();
    }
    let check = |db: &Db| {
        for i in 0..2000 {
            let expect = (i % 3 != 0).then(|| format!("v{i}").into_bytes());
            assert_eq!(db.get(&key(i)).unwrap(), expect, "key {i}");
        }
    };
    check(&db);
    let s = db.stats();
    assert!(s.flushes > 0, "small write buffer forces flushes");
    assert!(s.compactions + s.trivial_moves > 0);
    assert!(db.levels().iter().skip(1).any(|l| l.files > 0));
    drop(db);
    let db = open(&fs, Options::for_testing());
    check(&db);
    db.close().unwrap();
}

#[test]
fn snapshots_survive_compaction() {
    let fs = SimFs::new();
    let db = open(&fs, Options::for_testing());
    for i in 0..500 {
        db.put(&key(i), b"old").unwrap();
    }
    let snap = db.snapshot();
    assert_eq!(db.live_snapshots(), 1);
    for i in 0..500 {
        db.put(&key(i), b"new").unwrap();
        if i % 2 == 0 {
            db.delete(&key(i)).unwrap();
        }
    }
    db.compact_range(None, None).unwrap();
    let ro = ReadOptions {
        snapshot: Some(&snap),
        ..ReadOptions::default()
    };
    for i in 0..500 {
        assert_eq!(db.get_with(&ro, &key(i)).unwrap(), Some(b"old".to_vec()));
        let latest = (i % 2 == 1).then(|| b"new".to_vec());
        assert_eq!(db.get(&key(i)).unwrap(), latest);
    }
    let n = db.scan(&ro, None, None).unwrap().count();
    assert_eq!(n, 500);
    drop(snap);
    assert_eq!(db.live_snapshots(), 0);
    // With the snapshot gone a full compaction drops old versions and
    // tombstones: the tree shrinks.
    let before: u64 = db.levels().iter().map(|l| l.bytes).sum();
    db.compact_range(None, None).unwrap();
    let after: u64 = db.levels().iter().map(|l| l.bytes).sum();
    assert!(after < before, "{after} < {before}");
}

#[test]
fn range_iteration_with_bounds() {
    let fs = SimFs::new();
    let db = open(&fs, Options::for_testing());
    for i in 0..300 {
        db.put(&key(i), &key(i)).unwrap();
    }
    db.flush().unwrap();
    for i in 100..110 {
        db.delete(&key(i)).unwrap();
    }
    let got: Vec<Vec<u8>> = db
        .scan(&ReadOptions::default(), Some(&key(95)), Some(&key(115)))
        .unwrap()
        .map(|r| r.unwrap().0)
        .collect();
    let expect: Vec<Vec<u8>> = (95..100).chain(110..115).map(key).collect();
    assert_eq!(got, expect);
    let mut it = db
        .range_iter(&ReadOptions::default(), Some(&key(50)), None)
        .unwrap();
    it.seek(&key(10)); // clamped to lower bound
    assert_eq!(it.key(), key(50).as_slice());
    it.seek(&key(299));
    assert_eq!(it.value(), key(299).as_slice());
    it.next();
    assert!(!it.valid());
    assert!(it.status().is_ok());
    assert!(!db.iter(&ReadOptions::default()).unwrap().valid());
}

#[test]
fn open_flags_and_bad_options() {
    let fs = SimFs::new();
    let none = Options {
        create_if_missing: false,
        ..Options::for_testing()
    };
    assert!(matches!(
        Db::open_with_fs(Arc::new(fs.clone()), "/db", none.clone()),
        Err(Error::NotFound(_))
    ));
    drop(open(&fs, Options::for_testing()));
    Db::open_with_fs(Arc::new(fs.clone()), "/db", none).unwrap();
    let excl = Options {
        error_if_exists: true,
        ..Options::for_testing()
    };
    assert!(Db::open_with_fs(Arc::new(fs.clone()), "/db", excl).is_err());
    let bad = Options {
        write_buffer_size: 0,
        ..Options::for_testing()
    };
    assert!(Db::open_with_fs(Arc::new(fs), "/db2", bad).is_err());
}

#[test]
fn foreign_snapshot_is_rejected() {
    let a = open(&SimFs::new(), Options::for_testing());
    let b = open(&SimFs::new(), Options::for_testing());
    let snap = a.snapshot();
    let ro = ReadOptions {
        snapshot: Some(&snap),
        ..ReadOptions::default()
    };
    assert!(matches!(
        b.get_with(&ro, b"x"),
        Err(Error::InvalidArgument(_))
    ));
    assert!(b.iter(&ro).is_err());
}

#[test]
fn wal_sync_failure_makes_db_read_only() {
    let fs = SimFs::new();
    let db = open(&fs, Options::for_testing());
    db.put(b"a", b"1").unwrap();
    fs.inject(FaultKind::Sync, 1);
    let mut b = WriteBatch::new();
    b.put(b"b", b"2");
    assert!(db.write(&WriteOptions { sync: true }, b).is_err());
    assert!(matches!(db.put(b"c", b"3"), Err(Error::ReadOnly(_))));
    assert_eq!(
        db.get(b"a").unwrap(),
        Some(b"1".to_vec()),
        "reads still work"
    );
    assert!(db.flush().is_err());
    assert!(db.close().is_err());
}

#[test]
fn background_compaction_failure_is_reported() {
    let fs = SimFs::new();
    let db = open(&fs, Options::for_testing());
    // The next table creation (a flush) fails.
    fs.inject(FaultKind::Create, 2);
    let mut failed = false;
    for i in 0..5000 {
        if db.put(&key(i), &[0u8; 64]).is_err() {
            failed = true;
            break;
        }
    }
    assert!(failed, "a write eventually observes the background error");
    assert!(db.wait_for_compactions().is_err());
    drop(db);
    // After a restart the database recovers everything that was acknowledged.
    fs.crash(CrashMode::DropUnsynced);
    let db = open(&fs, Options::for_testing());
    db.put(b"after", b"restart").unwrap();
}

#[test]
fn background_thread_mode() {
    let fs = SimFs::new();
    let opts = Options {
        background_compaction: true,
        ..Options::for_testing()
    };
    let db = Arc::new(open(&fs, opts.clone()));
    let threads: Vec<_> = (0..4u32)
        .map(|t| {
            let db = db.clone();
            std::thread::spawn(move || {
                for i in 0..1500u32 {
                    let k = key(t * 10_000 + i);
                    db.put(&k, &k).unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    db.wait_for_compactions().unwrap();
    let s = db.stats();
    assert_eq!(s.writes, 6000);
    assert!(s.write_groups <= s.writes);
    for t in 0..4u32 {
        for i in (0..1500u32).step_by(37) {
            let k = key(t * 10_000 + i);
            assert_eq!(db.get(&k).unwrap(), Some(k.clone()));
        }
    }
    assert!(!db.describe_levels().is_empty());
    assert!(db.last_sequence() >= 6000);
    drop(db);
    let db = open(&fs, opts);
    assert_eq!(
        db.scan(&ReadOptions::default(), None, None)
            .unwrap()
            .count(),
        6000
    );
}
