use super::*;
use crate::fs::{FileSystem, SimFs};
use crate::iterator::InternalIterator;
use crate::key::{internal_key, user_key, ValueKind, MAX_SEQUENCE};
use crate::stats::Stats;
use std::path::Path;
use std::sync::Arc;
use table::BlockCache;

fn build_table(fs: &SimFs, name: &str, n: u32, opts: TableOptions) -> (Arc<Table>, Arc<Stats>) {
    let path = Path::new("/").join(name);
    let mut b = TableBuilder::new(fs.create(&path).unwrap(), opts);
    for i in 0..n {
        let k = internal_key(
            format!("key{:06}", i * 2).as_bytes(),
            u64::from(i) + 1,
            ValueKind::Value,
        );
        b.add(&k, format!("value-{i}").as_bytes()).unwrap();
    }
    assert_eq!(b.num_entries(), u64::from(n));
    assert!(b.file_size_estimate() > 0);
    let (size, mut f) = b.finish().unwrap();
    f.sync().unwrap();
    let stats = Arc::new(Stats::default());
    let cache = Arc::new(BlockCache::new(1 << 20));
    let file = fs.open(&path).unwrap();
    assert_eq!(file.size().unwrap(), size);
    let t = Table::open(file, size, 7, Some(cache), stats.clone()).unwrap();
    assert_eq!(t.file_number(), 7);
    (Arc::new(t), stats)
}

fn small_blocks() -> TableOptions {
    TableOptions {
        block_size: 256,
        restart_interval: 4,
        bloom_bits_per_key: 10,
    }
}

#[test]
fn point_lookups_hit_and_miss() {
    let fs = SimFs::new();
    let (t, stats) = build_table(&fs, "t.sst", 1000, small_blocks());
    for i in 0..1000u32 {
        let target = internal_key(
            format!("key{:06}", i * 2).as_bytes(),
            MAX_SEQUENCE,
            ValueKind::Value,
        );
        let (k, v) = t.get(&target).unwrap().expect("present");
        assert_eq!(user_key(&k), format!("key{:06}", i * 2).as_bytes());
        assert_eq!(v, format!("value-{i}").as_bytes());
        // Snapshot below the entry's sequence: the entry is invisible.
        let old = internal_key(
            format!("key{:06}", i * 2).as_bytes(),
            u64::from(i),
            ValueKind::Value,
        );
        assert!(t.get(&old).unwrap().is_none());
    }
    let mut false_positive_reads = 0;
    for i in 0..1000u32 {
        let target = internal_key(
            format!("key{:06}", i * 2 + 1).as_bytes(),
            MAX_SEQUENCE,
            ValueKind::Value,
        );
        let before = stats.snapshot().bloom_useful;
        assert!(t.get(&target).unwrap().is_none());
        if stats.snapshot().bloom_useful == before {
            false_positive_reads += 1;
        }
    }
    assert!(false_positive_reads < 40, "{false_positive_reads}");
    let s = stats.snapshot();
    assert!(s.block_cache_hits > 0 && s.block_cache_misses > 0);
}

#[test]
fn iteration_and_seek() {
    let fs = SimFs::new();
    for opts in [
        small_blocks(),
        TableOptions::default(),
        TableOptions {
            bloom_bits_per_key: 0,
            ..small_blocks()
        },
    ] {
        let (t, _) = build_table(&fs, "t.sst", 500, opts);
        let mut it = t.iter(false);
        it.seek_to_first();
        let mut n = 0u32;
        while it.valid() {
            assert_eq!(user_key(it.key()), format!("key{:06}", n * 2).as_bytes());
            n += 1;
            it.next();
        }
        assert_eq!(n, 500);
        assert!(it.status().is_ok());
        for probe in [0u32, 1, 2, 499, 500, 997, 998, 999, 5000] {
            let target = internal_key(
                format!("key{probe:06}").as_bytes(),
                MAX_SEQUENCE,
                ValueKind::Value,
            );
            it.seek(&target);
            let expect = (0..500u32).map(|i| i * 2).find(|&k| k >= probe);
            match expect {
                Some(k) => assert_eq!(user_key(it.key()), format!("key{k:06}").as_bytes()),
                None => assert!(!it.valid()),
            }
        }
    }
}

#[test]
fn empty_table() {
    let fs = SimFs::new();
    let (t, _) = build_table(&fs, "e.sst", 0, TableOptions::default());
    let mut it = t.iter(true);
    it.seek_to_first();
    assert!(!it.valid());
    let target = internal_key(b"x", MAX_SEQUENCE, ValueKind::Value);
    assert!(t.get(&target).unwrap().is_none());
}

#[test]
fn corruption_is_detected() {
    let fs = SimFs::new();
    let (t, _) = build_table(&fs, "c.sst", 300, small_blocks());
    drop(t);
    let path = Path::new("/c.sst");
    let good = fs.file_contents(path).unwrap();
    let stats = Arc::new(Stats::default());

    // Flip a byte in the first data block: reads of that block fail.
    let mut bad = good.clone();
    bad[10] ^= 0x40;
    fs.overwrite_file(path, bad);
    let t = Arc::new(
        Table::open(
            fs.open(path).unwrap(),
            good.len() as u64,
            1,
            None,
            stats.clone(),
        )
        .unwrap(),
    );
    let target = internal_key(b"key000000", MAX_SEQUENCE, ValueKind::Value);
    assert!(t.get(&target).unwrap_err().is_corruption());
    let mut it = t.iter(false);
    it.seek_to_first();
    assert!(!it.valid());
    assert!(it.status().unwrap_err().is_corruption());

    // Bad magic.
    let mut bad = good.clone();
    let n = bad.len();
    bad[n - 1] ^= 1;
    fs.overwrite_file(path, bad);
    assert!(Table::open(fs.open(path).unwrap(), n as u64, 1, None, stats.clone()).is_err());

    // Too short.
    fs.overwrite_file(path, vec![0; 10]);
    assert!(Table::open(fs.open(path).unwrap(), 10, 1, None, stats.clone()).is_err());

    // Truncated: footer is read from the wrong place.
    fs.overwrite_file(path, good[..good.len() - 7].to_vec());
    assert!(Table::open(
        fs.open(path).unwrap(),
        (good.len() - 7) as u64,
        1,
        None,
        stats
    )
    .is_err());
}

#[test]
fn format_helpers() {
    use format::{BlockHandle, Footer, FOOTER_SIZE};
    let h = BlockHandle {
        offset: 1 << 40,
        size: 77,
    };
    assert_eq!(BlockHandle::decode(&h.encode()).unwrap(), h);
    assert!(BlockHandle::decode(&[0x80]).is_err());
    let f = Footer {
        metaindex: h,
        index: BlockHandle { offset: 5, size: 6 },
    };
    let enc = f.encode();
    assert_eq!(enc.len(), FOOTER_SIZE);
    assert_eq!(Footer::decode(&enc).unwrap(), f);
    assert!(Footer::decode(&enc[1..]).is_err());
}
