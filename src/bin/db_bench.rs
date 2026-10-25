//! `db_bench` — LevelDB-style micro-benchmarks for lsmkv.
//!
//! ```text
//! db_bench [--num=N] [--value_size=B] [--db=DIR] [--threads=T]
//!          [--benchmarks=fillseq,fillrandom,overwrite,readrandom,readseq,fillsync,fillsync_mt]
//! ```
//! Keys are 16 bytes (`%016d`), values are pseudo-random bytes (the engine
//! does not compress). Each benchmark prints micros/op and MB/s; write
//! benchmarks also print write amplification.

use lsmkv::util::rng::Rng;
use lsmkv::{Db, Options, ReadOptions, StatsSnapshot, WriteBatch, WriteOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

struct Config {
    num: u64,
    value_size: usize,
    threads: u64,
    db: PathBuf,
    benchmarks: Vec<String>,
}

fn parse_args() -> Result<Config, String> {
    let mut c = Config {
        num: 1_000_000,
        value_size: 100,
        threads: 4,
        db: PathBuf::from("bench/tmp/db_bench"),
        benchmarks: "fillseq,fillrandom,overwrite,readrandom,readseq,fillsync,fillsync_mt"
            .split(',')
            .map(str::to_string)
            .collect(),
    };
    for arg in std::env::args().skip(1) {
        let (k, v) = arg
            .strip_prefix("--")
            .and_then(|a| a.split_once('='))
            .ok_or_else(|| format!("bad argument {arg:?} (expected --name=value)"))?;
        let num = |v: &str| {
            v.parse::<u64>()
                .map_err(|_| format!("--{k} needs a number"))
        };
        match k {
            "num" => c.num = num(v)?,
            "value_size" => c.value_size = num(v)? as usize,
            "threads" => c.threads = num(v)?.max(1),
            "db" => c.db = PathBuf::from(v),
            "benchmarks" => c.benchmarks = v.split(',').map(str::to_string).collect(),
            _ => return Err(format!("unknown flag --{k}")),
        }
    }
    Ok(c)
}

fn key(i: u64) -> [u8; 16] {
    let mut k = [0u8; 16];
    k.copy_from_slice(format!("{i:016}").as_bytes());
    k
}

struct Bench<'a> {
    cfg: &'a Config,
    value_pool: Vec<u8>,
}

struct Outcome {
    ops: u64,
    bytes: u64,
    secs: f64,
    note: String,
}

impl Bench<'_> {
    fn value(&self, i: u64) -> &[u8] {
        let off = (i as usize * 7919) % (self.value_pool.len() - self.cfg.value_size);
        &self.value_pool[off..off + self.cfg.value_size]
    }

    fn fresh_db(&self) -> lsmkv::Result<Db> {
        let _ = std::fs::remove_dir_all(&self.cfg.db);
        Db::open(&self.cfg.db, Options::default())
    }

    fn reopen(&self) -> lsmkv::Result<Db> {
        Db::open(&self.cfg.db, Options::default())
    }

    fn fill(&self, db: &Db, n: u64, random: bool, sync: bool, seed: u64) -> lsmkv::Result<u64> {
        let mut rng = Rng::new(seed);
        let opts = WriteOptions { sync };
        let mut bytes = 0;
        for i in 0..n {
            let k = if random { rng.below(self.cfg.num) } else { i };
            let mut b = WriteBatch::new();
            b.put(&key(k), self.value(i));
            db.write(&opts, b)?;
            bytes += (16 + self.cfg.value_size) as u64;
        }
        Ok(bytes)
    }

    fn write_note(before: &StatsSnapshot, db: &Db) -> lsmkv::Result<String> {
        db.wait_for_compactions()?;
        let s = db.stats().since(before);
        Ok(format!(
            "WA {:.2} (tree {:.2}), {} flushes, {} compactions, stall {:.1}s",
            s.write_amplification(),
            s.tree_write_amplification(),
            s.flushes,
            s.compactions,
            s.stall_micros as f64 / 1e6
        ))
    }

    fn run(&self, name: &str) -> lsmkv::Result<Option<Outcome>> {
        let n = self.cfg.num;
        let start;
        let out = match name {
            "fillseq" | "fillrandom" => {
                let db = self.fresh_db()?;
                let before = db.stats();
                start = Instant::now();
                let bytes = self.fill(&db, n, name == "fillrandom", false, 1)?;
                let secs = start.elapsed().as_secs_f64();
                Outcome {
                    ops: n,
                    bytes,
                    secs,
                    note: Self::write_note(&before, &db)?,
                }
            }
            "overwrite" => {
                let db = self.reopen()?;
                let before = db.stats();
                start = Instant::now();
                let bytes = self.fill(&db, n, true, false, 2)?;
                let secs = start.elapsed().as_secs_f64();
                Outcome {
                    ops: n,
                    bytes,
                    secs,
                    note: Self::write_note(&before, &db)?,
                }
            }
            "readrandom" => {
                let db = self.reopen()?;
                db.wait_for_compactions()?;
                let before = db.stats();
                let mut rng = Rng::new(3);
                let mut found = 0u64;
                let mut bytes = 0u64;
                start = Instant::now();
                for _ in 0..n {
                    if let Some(v) = db.get(&key(rng.below(n)))? {
                        found += 1;
                        bytes += 16 + v.len() as u64;
                    }
                }
                let secs = start.elapsed().as_secs_f64();
                let s = db.stats().since(&before);
                let block_reads = s.get_table_probes - s.bloom_useful;
                Outcome {
                    ops: n,
                    bytes,
                    secs,
                    note: format!(
                        "{found} of {n} found; {:.2} tables/get, {:.2} block reads/get, bloom saved {}, cache hit {:.0}%",
                        s.get_table_probes as f64 / n as f64,
                        block_reads as f64 / n as f64,
                        s.bloom_useful,
                        100.0 * s.block_cache_hits as f64
                            / (s.block_cache_hits + s.block_cache_misses).max(1) as f64
                    ),
                }
            }
            "readseq" => {
                let db = self.reopen()?;
                let mut it = db.iter(&ReadOptions::default())?;
                let mut count = 0u64;
                let mut bytes = 0u64;
                start = Instant::now();
                it.seek_to_first();
                while it.valid() {
                    bytes += (it.key().len() + it.value().len()) as u64;
                    count += 1;
                    it.next();
                }
                it.status()?;
                let secs = start.elapsed().as_secs_f64();
                let logical = bytes;
                let physical: u64 = db.levels().iter().map(|l| l.bytes).sum();
                Outcome {
                    ops: count,
                    bytes,
                    secs,
                    note: format!(
                        "{count} live keys; space amplification {:.2} ({physical} table bytes / {logical} live bytes)",
                        physical as f64 / logical.max(1) as f64
                    ),
                }
            }
            "fillsync" => {
                let db = self.fresh_db()?;
                let ops = (n / 100).max(1);
                start = Instant::now();
                let bytes = self.fill(&db, ops, true, true, 4)?;
                let secs = start.elapsed().as_secs_f64();
                let s = db.stats();
                Outcome {
                    ops,
                    bytes,
                    secs,
                    note: format!("{} fsyncs for {} writes", s.wal_syncs, s.writes),
                }
            }
            "fillsync_mt" => {
                let db = Arc::new(self.fresh_db()?);
                let per = ((n / 100) / self.cfg.threads).max(1);
                start = Instant::now();
                let handles: Vec<_> = (0..self.cfg.threads)
                    .map(|t| {
                        let db = db.clone();
                        let value = self.value(t).to_vec();
                        std::thread::spawn(move || -> lsmkv::Result<()> {
                            let mut rng = Rng::new(100 + t);
                            for _ in 0..per {
                                let mut b = WriteBatch::new();
                                b.put(&key(rng.below(10_u64.pow(16))), &value);
                                db.write(&WriteOptions { sync: true }, b)?;
                            }
                            Ok(())
                        })
                    })
                    .collect();
                for h in handles {
                    h.join().expect("bench thread panicked")?;
                }
                let secs = start.elapsed().as_secs_f64();
                let s = db.stats();
                let ops = per * self.cfg.threads;
                Outcome {
                    ops,
                    bytes: ops * (16 + self.cfg.value_size as u64),
                    secs,
                    note: format!(
                        "{} threads; group commit: {} writes in {} WAL records, {} fsyncs ({:.2} writes/fsync)",
                        self.cfg.threads,
                        s.writes,
                        s.write_groups,
                        s.wal_syncs,
                        s.writes as f64 / s.wal_syncs.max(1) as f64
                    ),
                }
            }
            _ => return Ok(None),
        };
        Ok(Some(out))
    }
}

fn main() {
    lsmkv::util::logger::init_from_env("LSMKV_LOG");
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("db_bench: {e}");
            std::process::exit(2);
        }
    };
    let mut pool = vec![0u8; 1 << 20];
    Rng::new(42).fill(&mut pool);
    if cfg.value_size == 0 || cfg.value_size >= pool.len() {
        eprintln!("db_bench: --value_size must be in 1..{}", pool.len());
        std::process::exit(2);
    }
    let bench = Bench {
        cfg: &cfg,
        value_pool: pool,
    };
    println!(
        "lsmkv db_bench: keys 16 bytes, values {} bytes, entries {}, db {}",
        cfg.value_size,
        cfg.num,
        cfg.db.display()
    );
    println!("{}", "-".repeat(100));
    for name in &cfg.benchmarks {
        match bench.run(name) {
            Ok(Some(o)) => {
                let micros = o.secs * 1e6 / o.ops.max(1) as f64;
                let mbs = o.bytes as f64 / 1_048_576.0 / o.secs.max(1e-9);
                println!(
                    "{name:<12}: {micros:>10.3} micros/op; {mbs:>7.1} MB/s  {}",
                    o.note
                );
            }
            Ok(None) => eprintln!("db_bench: unknown benchmark {name:?}"),
            Err(e) => {
                eprintln!("db_bench: {name} failed: {e}");
                std::process::exit(1);
            }
        }
    }
    if cfg.db.starts_with(Path::new("bench/tmp")) {
        let _ = std::fs::remove_dir_all(&cfg.db);
    }
}
