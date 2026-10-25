//! End-to-end tests on the real filesystem, including concurrent readers,
//! writers and background compaction.

use lsmkv::{Db, Options, ReadOptions, WriteBatch, WriteOptions};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

fn small_bg_opts() -> Options {
    Options {
        background_compaction: true,
        ..Options::for_testing()
    }
}

#[test]
fn persists_on_real_disk_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open(dir.path(), small_bg_opts()).unwrap();
        for i in 0..3000u32 {
            db.put(format!("k{i:05}").as_bytes(), &i.to_le_bytes())
                .unwrap();
        }
        let mut b = WriteBatch::new();
        b.delete(b"k00000").put(b"extra", b"1");
        db.write(&WriteOptions { sync: true }, b).unwrap();
        db.wait_for_compactions().unwrap();
        db.close().unwrap();
    }
    let db = Db::open(dir.path(), small_bg_opts()).unwrap();
    assert_eq!(db.get(b"k00000").unwrap(), None);
    assert_eq!(
        db.get(b"k02999").unwrap(),
        Some(2999u32.to_le_bytes().to_vec())
    );
    assert_eq!(db.get(b"extra").unwrap(), Some(b"1".to_vec()));
    let n = db
        .scan(&ReadOptions::default(), None, None)
        .unwrap()
        .count();
    assert_eq!(n, 3000);
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(names.iter().any(|n| n == "CURRENT"));
    assert!(names.iter().any(|n| n.ends_with(".sst")));
}

#[test]
fn batches_are_atomic_under_concurrent_snapshot_scans() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(dir.path(), small_bg_opts()).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..3u32)
        .map(|w| {
            let db = db.clone();
            thread::spawn(move || {
                for round in 0..400u32 {
                    // Each batch sets pair/a/N and pair/b/N to the same value.
                    let n = (round * 7 + w) % 50;
                    let v = format!("{w}-{round}").into_bytes();
                    let mut b = WriteBatch::new();
                    b.put(format!("pair/a/{n:02}").as_bytes(), &v);
                    b.put(format!("pair/b/{n:02}").as_bytes(), &v);
                    b.put(format!("bulk/{w}/{round:04}").as_bytes(), &[0u8; 64]);
                    db.write(&WriteOptions::default(), b).unwrap();
                }
            })
        })
        .collect();
    let readers: Vec<_> = (0..2)
        .map(|_| {
            let db = db.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                let mut checks = 0;
                while !stop.load(Ordering::Acquire) || checks == 0 {
                    let snap = db.snapshot();
                    let ro = ReadOptions {
                        snapshot: Some(&snap),
                        ..ReadOptions::default()
                    };
                    let pairs: Vec<(Vec<u8>, Vec<u8>)> = db
                        .scan(&ro, Some(b"pair/"), Some(b"pair0"))
                        .unwrap()
                        .map(|r| r.unwrap())
                        .collect();
                    let (a, b): (Vec<_>, Vec<_>) =
                        pairs.iter().partition(|(k, _)| k.starts_with(b"pair/a/"));
                    assert_eq!(a.len(), b.len(), "half a batch is visible");
                    for ((ka, va), (kb, vb)) in a.iter().zip(&b) {
                        assert_eq!(ka[7..], kb[7..]);
                        assert_eq!(va, vb, "torn batch at {}", String::from_utf8_lossy(ka));
                        // Point reads at the same snapshot agree with the scan.
                        assert_eq!(db.get_with(&ro, ka).unwrap().as_ref(), Some(va));
                    }
                    checks += 1;
                }
                checks
            })
        })
        .collect();
    for w in writers {
        w.join().unwrap();
    }
    stop.store(true, Ordering::Release);
    for r in readers {
        assert!(r.join().unwrap() > 0);
    }
    db.wait_for_compactions().unwrap();
    let s = db.stats();
    assert_eq!(s.writes, 1200);
    assert!(s.flushes > 0);
    let bulk = db
        .scan(&ReadOptions::default(), Some(b"bulk/"), Some(b"bulk0"))
        .unwrap()
        .count();
    assert_eq!(bulk, 1200);
}

#[test]
fn iterator_pins_its_version_across_compactions() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), Options::for_testing()).unwrap();
    for i in 0..1000u32 {
        db.put(format!("k{i:05}").as_bytes(), b"v1").unwrap();
    }
    let mut it = db.iter(&ReadOptions::default()).unwrap();
    it.seek_to_first();
    // Rewrite and fully compact everything while the iterator is open: the
    // tables it reads from become obsolete but must not be deleted yet.
    for i in 0..1000u32 {
        db.put(format!("k{i:05}").as_bytes(), b"v2").unwrap();
    }
    db.compact_range(None, None).unwrap();
    let mut n = 0;
    while it.valid() {
        assert_eq!(it.value(), b"v1");
        n += 1;
        it.next();
    }
    it.status().unwrap();
    assert_eq!(n, 1000);
    drop(it);
    db.wait_for_compactions().unwrap();
    assert_eq!(db.get(b"k00500").unwrap(), Some(b"v2".to_vec()));
}
