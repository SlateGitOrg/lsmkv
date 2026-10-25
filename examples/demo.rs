//! End-to-end tour of lsmkv. Run with `make demo` (or
//! `cargo run --release --example demo`). Everything is generated
//! deterministically; parts 1–5 use the real filesystem under `./demo-data`,
//! parts 6–7 use the simulated filesystem to crash the engine on purpose.

use lsmkv::fs::{CrashMode, SimFs};
use lsmkv::util::rng::Rng;
use lsmkv::{CompactionInfo, Db, EventListener, Options, ReadOptions, WriteBatch, WriteOptions};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn key(i: u64) -> Vec<u8> {
    format!("key{i:07}").into_bytes()
}

fn value(i: u64, generation: u32) -> Vec<u8> {
    let mut v = format!("v{generation}-{i}-").into_bytes();
    v.resize(100, b'.');
    v
}

fn show(v: Option<Vec<u8>>) -> String {
    match v {
        Some(v) => String::from_utf8_lossy(&v[..v.len().min(14)]).into_owned() + "…",
        None => "(none)".to_string(),
    }
}

fn header(title: &str) {
    println!("\n== {title} {}", "=".repeat(70 - title.len().min(68)));
}

fn table_bytes(db: &Db) -> (usize, u64) {
    db.levels()
        .iter()
        .fold((0, 0), |(f, b), l| (f + l.files, b + l.bytes))
}

fn main() -> lsmkv::Result<()> {
    lsmkv::util::logger::init_from_env("LSMKV_LOG");
    let dir = std::path::Path::new("demo-data/db");
    let _ = std::fs::remove_dir_all("demo-data");
    let opts = Options {
        write_buffer_size: 512 << 10,
        max_file_size: 512 << 10,
        max_bytes_for_level_base: 2 << 20,
        ..Options::default()
    };
    let n = 100_000u64;

    header("1. Bulk load (real filesystem, background compaction)");
    let db = Db::open(dir, opts.clone())?;
    let t = Instant::now();
    let mut rng = Rng::new(7);
    let mut order: Vec<u64> = (0..n).collect();
    for i in (1..order.len()).rev() {
        order.swap(i, rng.below(i as u64 + 1) as usize);
    }
    for chunk in order.chunks(100) {
        let mut b = WriteBatch::new();
        for &i in chunk {
            b.put(&key(i), &value(i, 1));
        }
        db.write(&WriteOptions::default(), b)?;
    }
    db.wait_for_compactions()?;
    let secs = t.elapsed().as_secs_f64();
    println!(
        "loaded {n} keys in random order: {:.2}s ({:.0} keys/s)",
        secs,
        n as f64 / secs
    );
    print!("{}", db.describe_levels());
    let s = db.stats();
    println!(
        "flushes {}, compactions {}, trivial moves {}, write amplification {:.2} (WAL + tree), tree-only {:.2}",
        s.flushes,
        s.compactions,
        s.trivial_moves,
        s.write_amplification(),
        s.tree_write_amplification()
    );

    header("2. Point reads and bloom filters");
    for i in [0, 4242, 99_999] {
        println!(
            "get {} -> {}",
            String::from_utf8_lossy(&key(i)),
            show(db.get(&key(i))?)
        );
    }
    let before = db.stats();
    let mut misses = 0;
    for i in 0..10_000u64 {
        if db.get(format!("key{:07}x", i * 7).as_bytes())?.is_none() {
            misses += 1;
        }
    }
    let d = db.stats().since(&before);
    println!(
        "10000 lookups of absent keys: {misses} misses; {} table probes, bloom filters ruled out {} ({:.1}%)",
        d.get_table_probes,
        d.bloom_useful,
        100.0 * d.bloom_useful as f64 / d.get_table_probes.max(1) as f64
    );

    header("3. MVCC snapshot isolation");
    let snap = db.snapshot();
    println!("took snapshot at sequence {}", snap.sequence());
    for i in 0..1000 {
        db.put(&key(i), &value(i, 2))?;
    }
    for i in 1000..1500 {
        db.delete(&key(i))?;
    }
    db.flush()?;
    let at_snap = ReadOptions {
        snapshot: Some(&snap),
        ..ReadOptions::default()
    };
    for i in [5u64, 1200] {
        println!(
            "{}: latest = {:<18} snapshot = {}",
            String::from_utf8_lossy(&key(i)),
            show(db.get(&key(i))?),
            show(db.get_with(&at_snap, &key(i))?)
        );
    }
    let live_now = db.scan(&ReadOptions::default(), None, None)?.count();
    let live_snap = db.scan(&at_snap, None, None)?.count();
    println!("live keys: latest {live_now}, snapshot {live_snap}");
    drop(snap);

    header("4. Range scan [key0050000, key0050004)");
    for item in db.scan(
        &ReadOptions::default(),
        Some(&key(50_000)),
        Some(&key(50_004)),
    )? {
        let (k, v) = item?;
        println!("  {} = {}", String::from_utf8_lossy(&k), show(Some(v)));
    }

    header("5. Full compaction and space amplification");
    let (files, bytes) = table_bytes(&db);
    let logical: u64 = db
        .scan(&ReadOptions::default(), None, None)?
        .map(|r| r.map(|(k, v)| (k.len() + v.len()) as u64))
        .sum::<lsmkv::Result<u64>>()?;
    println!(
        "before: {files} tables, {bytes} bytes for {logical} live bytes -> space amp {:.2}",
        bytes as f64 / logical as f64
    );
    db.compact_range(None, None)?;
    db.wait_for_compactions()?;
    let (files, bytes) = table_bytes(&db);
    println!(
        "after:  {files} tables, {bytes} bytes for {logical} live bytes -> space amp {:.2}",
        bytes as f64 / logical as f64
    );
    print!("{}", db.describe_levels());
    db.close()?;
    let db = Db::open(dir, opts)?;
    println!(
        "reopened: {} = {}",
        String::from_utf8_lossy(&key(77)),
        show(db.get(&key(77))?)
    );
    drop(db);
    let _ = std::fs::remove_dir_all("demo-data");

    header("6. Power loss with torn writes (simulated filesystem)");
    let fs = SimFs::new();
    let small = Options {
        write_buffer_size: 64 << 10,
        ..Options::for_testing()
    };
    let db = Db::open_with_fs(Arc::new(fs.clone()), "/db", small.clone())?;
    for i in 0..1000 {
        let mut b = WriteBatch::new();
        b.put(&key(i), &value(i, 1));
        db.write(&WriteOptions { sync: true }, b)?;
    }
    for i in 1000..1300 {
        db.put(&key(i), &value(i, 1))?; // not fsynced
    }
    drop(db);
    fs.crash(CrashMode::TornWrites { seed: 2024 });
    let db = Db::open_with_fs(Arc::new(fs.clone()), "/db", small.clone())?;
    let synced_ok = (0..1000).all(|i| db.get(&key(i)).ok().flatten() == Some(value(i, 1)));
    let survived: Vec<u64> = (1000..1300)
        .filter(|&i| db.get(&key(i)).ok().flatten().is_some())
        .collect();
    let prefix = survived
        .iter()
        .enumerate()
        .all(|(j, &i)| i == 1000 + j as u64);
    println!("1000 fsynced writes + 300 unsynced writes, then power loss tearing the WAL tail");
    println!("all 1000 fsynced writes recovered: {synced_ok}");
    println!(
        "{} of 300 unsynced writes survived, forming a prefix of history: {prefix}",
        survived.len()
    );
    drop(db);

    header("7. Kill mid-compaction at every filesystem operation");
    struct Windows(SimFs, Mutex<Vec<u64>>, Mutex<Vec<(u64, u64)>>);
    impl EventListener for Windows {
        fn on_compaction_begin(&self, i: &CompactionInfo) {
            if i.inputs > 1 {
                self.1.lock().unwrap().push(self.0.ops());
            }
        }
        fn on_compaction_end(&self, i: &CompactionInfo) {
            if i.inputs > 1 {
                let b = self.1.lock().unwrap().pop().unwrap();
                self.2.lock().unwrap().push((b, self.0.ops()));
            }
        }
    }
    let writes: Vec<(Vec<u8>, Vec<u8>)> = {
        let mut rng = Rng::new(99);
        (0..1200)
            .map(|i| (key(rng.below(300)), value(i, 3)))
            .collect()
    };
    let mut models = vec![BTreeMap::new()];
    for (k, v) in &writes {
        let mut m = models.last().unwrap().clone();
        m.insert(k.clone(), v.clone());
        models.push(m);
    }
    let fs = SimFs::new();
    let rec = Arc::new(Windows(fs.clone(), Mutex::default(), Mutex::default()));
    let traced = Options {
        listener: Some(rec.clone()),
        ..Options::for_testing()
    };
    let run = |db: &Db| -> usize {
        for (i, (k, v)) in writes.iter().enumerate() {
            let mut b = WriteBatch::new();
            b.put(k, v);
            if db.write(&WriteOptions { sync: true }, b).is_err() {
                return i;
            }
        }
        writes.len()
    };
    run(&Db::open_with_fs(Arc::new(fs.clone()), "/db", traced)?);
    let windows = rec.2.lock().unwrap().clone();
    let points: Vec<u64> = windows.iter().flat_map(|&(b, e)| b + 1..=e + 1).collect();
    let mut consistent = 0;
    for &p in &points {
        let fs = SimFs::new();
        fs.crash_after(p);
        let acked = match Db::open_with_fs(Arc::new(fs.clone()), "/db", Options::for_testing()) {
            Ok(db) => run(&db),
            Err(_) => 0,
        };
        fs.crash(CrashMode::TornWrites { seed: p });
        let db = Db::open_with_fs(Arc::new(fs.clone()), "/db", Options::for_testing())?;
        let state: BTreeMap<Vec<u8>, Vec<u8>> = db
            .scan(&ReadOptions::default(), None, None)?
            .collect::<lsmkv::Result<_>>()?;
        if state == models[acked] || models.get(acked + 1) == Some(&state) {
            consistent += 1;
        }
    }
    println!(
        "{} multi-file compactions spanning {} filesystem operations",
        windows.len(),
        points.len()
    );
    println!(
        "crashed at each one, recovered, compared with a BTreeMap model: {consistent}/{} consistent",
        points.len()
    );
    Ok(())
}
