//! Checksums catch on-disk damage, and the WAL recovery policy decides
//! whether damage is tolerated (point-in-time) or fatal.

mod common;

use common::*;
use lsmkv::fs::{FileSystem, SimFs};
use lsmkv::{Error, Options, ReadOptions, WalRecovery};
use std::path::{Path, PathBuf};

fn files_with_suffix(fs: &SimFs, suffix: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs
        .list(Path::new("/db"))
        .unwrap()
        .into_iter()
        .filter(|n| n.ends_with(suffix))
        .map(|n| Path::new("/db").join(n))
        .collect();
    v.sort();
    v
}

fn flip(fs: &SimFs, path: &Path, offset: usize) {
    let mut data = fs.file_contents(path).unwrap();
    data[offset] ^= 0x5a;
    fs.overwrite_file(path, data);
}

fn wal_only_opts() -> Options {
    Options {
        write_buffer_size: 4 << 20,
        ..Options::for_testing()
    }
}

/// Writes 400 records (~2.5 blocks of WAL) and closes without flushing.
fn fill_wal(fs: &SimFs, opts: &Options) -> Vec<Write> {
    let writes: Vec<Write> = (0..400u64)
        .map(|i| Write::Put(key(i), vec![b'v'; 200]))
        .collect();
    let db = open(fs, opts.clone()).unwrap();
    run_until_failure(&db, &writes, |_| false);
    drop(db);
    writes
}

#[test]
fn sstable_bit_flip_is_reported_not_returned() {
    let fs = SimFs::new();
    let opts = Options {
        block_cache_bytes: 0,
        ..Options::for_testing()
    };
    let db = open(&fs, opts.clone()).unwrap();
    for i in 0..300 {
        db.put(&key(i), &[b'x'; 100]).unwrap();
    }
    db.compact_range(None, None).unwrap();
    drop(db);
    let tables = files_with_suffix(&fs, ".sst");
    assert!(!tables.is_empty());
    flip(&fs, &tables[0], 20); // inside the first data block
    let db = open(&fs, opts).unwrap();
    let err = (0..300).find_map(|i| db.get(&key(i)).err());
    assert!(matches!(err, Some(Error::Corruption(_))), "{err:?}");
    let scan: Vec<_> = db
        .scan(&ReadOptions::default(), None, None)
        .unwrap()
        .collect();
    assert!(scan.last().unwrap().as_ref().unwrap_err().is_corruption());
}

#[test]
fn mid_wal_corruption_point_in_time_vs_strict() {
    // Point-in-time (default): replay stops at the damage; later records,
    // even though intact, are discarded so the result is a prefix.
    let fs = SimFs::new();
    let writes = fill_wal(&fs, &wal_only_opts());
    let models = prefix_models(&writes);
    let wal = files_with_suffix(&fs, ".log").pop().unwrap();
    flip(&fs, &wal, 40_000); // inside the second 32 KiB block
    let db = open(&fs, wal_only_opts()).unwrap();
    let state = dump(&db);
    let k = models.iter().position(|m| *m == state).expect("a prefix");
    assert!(k > 100 && k < 400, "recovered {k} writes");
    drop(db);

    // Strict: the same damage with valid records after it refuses to open.
    let fs = SimFs::new();
    fill_wal(&fs, &wal_only_opts());
    let wal = files_with_suffix(&fs, ".log").pop().unwrap();
    flip(&fs, &wal, 40_000);
    let strict = Options {
        wal_recovery: WalRecovery::TolerateTailOnly,
        ..wal_only_opts()
    };
    assert!(matches!(open(&fs, strict), Err(Error::Corruption(_))));
}

#[test]
fn strict_mode_still_tolerates_a_torn_tail() {
    let fs = SimFs::new();
    let writes = fill_wal(&fs, &wal_only_opts());
    let wal = files_with_suffix(&fs, ".log").pop().unwrap();
    let data = fs.file_contents(&wal).unwrap();
    // Damage only the final record: a torn write.
    flip(&fs, &wal, data.len() - 3);
    let strict = Options {
        wal_recovery: WalRecovery::TolerateTailOnly,
        ..wal_only_opts()
    };
    let db = open(&fs, strict).unwrap();
    assert_eq!(dump(&db), prefix_models(&writes)[399]);
}

#[test]
fn damaged_metadata_refuses_to_open() {
    let fs = SimFs::new();
    let db = open(&fs, Options::for_testing()).unwrap();
    for i in 0..500 {
        db.put(&key(i), &[b'm'; 80]).unwrap();
    }
    drop(db);
    let manifest = files_with_suffix(&fs, "").into_iter().find(|p| {
        p.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("MANIFEST-")
    });
    let manifest = manifest.unwrap();
    // Damage the first record while later records exist.
    let size = fs.file_contents(&manifest).unwrap().len();
    assert!(size > 100);
    flip(&fs, &manifest, 10);
    assert!(open(&fs, Options::for_testing()).is_err());

    // CURRENT naming a manifest that does not exist.
    let fs = SimFs::new();
    drop(open(&fs, Options::for_testing()).unwrap());
    let current = Path::new("/db/CURRENT");
    fs.overwrite_file(current, b"MANIFEST-999999\n".to_vec());
    assert!(open(&fs, Options::for_testing()).is_err());
    fs.overwrite_file(current, b"not a manifest\n".to_vec());
    assert!(matches!(
        open(&fs, Options::for_testing()),
        Err(Error::Corruption(_))
    ));
}

#[test]
fn missing_table_file_is_an_error_not_silent_data_loss() {
    let fs = SimFs::new();
    let db = open(&fs, Options::for_testing()).unwrap();
    for i in 0..300 {
        db.put(&key(i), &[b'y'; 100]).unwrap();
    }
    db.compact_range(None, None).unwrap();
    drop(db);
    let tables = files_with_suffix(&fs, ".sst");
    fs.remove(&tables[0]).unwrap();
    let db = open(&fs, Options::for_testing()).unwrap();
    let errs = (0..300).filter(|&i| db.get(&key(i)).is_err()).count();
    assert!(errs > 0);
}
