//! Crash-recovery tests with fault injection on the simulated filesystem.
//!
//! Each test first runs a workload without faults to count the filesystem
//! operations it performs, then re-runs it once per crash point: the N-th
//! mutating operation "kills the process", the filesystem applies power-loss
//! semantics (unsynced bytes dropped or torn, unsynced directory entries
//! lost), and the database is reopened and checked against a reference model.

mod common;

use common::*;
use lsmkv::fs::{CrashMode, FaultKind, SimFs};
use lsmkv::{CompactionInfo, EventListener, Options};
use std::sync::{Arc, Mutex};

fn crash_mode(trial: u64) -> CrashMode {
    if trial % 2 == 0 {
        CrashMode::DropUnsynced
    } else {
        CrashMode::TornWrites { seed: trial }
    }
}

/// Counts the mutating fs operations of a fault-free run.
fn dry_run_ops(writes: &[Write], opts: &Options, sync: impl Fn(usize) -> bool) -> u64 {
    let fs = SimFs::new();
    let db = open(&fs, opts.clone()).unwrap();
    assert_eq!(run_until_failure(&db, writes, sync), writes.len());
    drop(db);
    fs.ops()
}

/// Crash at op `point`, recover, and return (acknowledged writes, recovered state).
fn crash_trial(
    writes: &[Write],
    opts: &Options,
    point: u64,
    mode: CrashMode,
    sync: impl Fn(usize) -> bool,
) -> (usize, Model) {
    let fs = SimFs::new();
    fs.crash_after(point);
    let acked = match open(&fs, opts.clone()) {
        Ok(db) => run_until_failure(&db, writes, sync),
        Err(_) => 0,
    };
    fs.crash(mode);
    let db =
        open(&fs, opts.clone()).unwrap_or_else(|e| panic!("recovery failed at point {point}: {e}"));
    let state = dump(&db);
    assert_no_leaked_tables(&fs, &db);
    // The recovered database must accept writes and survive another reopen.
    db.put(b"zz-after-recovery", b"1").unwrap();
    drop(db);
    let db = open(&fs, opts.clone()).unwrap();
    assert_eq!(db.get(b"zz-after-recovery").unwrap(), Some(b"1".to_vec()));
    (acked, state)
}

#[test]
fn sync_writes_survive_a_crash_at_every_operation() {
    let writes = workload(1, 120, 40);
    let models = prefix_models(&writes);
    let opts = Options::for_testing();
    let total = dry_run_ops(&writes, &opts, |_| true);
    assert!(total > 200, "workload too small: {total} ops");
    eprintln!("sync workload: {total} crash points");
    for point in 1..=total {
        let (acked, state) = crash_trial(&writes, &opts, point, crash_mode(point), |_| true);
        // Every acknowledged write is durable; the in-flight one may or may
        // not have reached the disk; nothing else may appear.
        let ok = state == models[acked] || (acked < writes.len() && state == models[acked + 1]);
        assert!(
            ok,
            "crash point {point}/{total}: acked={acked}, recovered {} keys",
            state.len()
        );
    }
}

#[test]
fn unsynced_writes_recover_to_a_consistent_prefix() {
    let writes = workload(2, 200, 50);
    let models = prefix_models(&writes);
    let opts = Options::for_testing();
    let sync = |i: usize| i % 10 == 9;
    let total = dry_run_ops(&writes, &opts, sync);
    let mut lossy = 0;
    for point in (1..=total).step_by(3) {
        let (acked, state) = crash_trial(&writes, &opts, point, crash_mode(point), sync);
        // Writes up to the last acknowledged *synced* write must survive.
        let durable = (0..acked).rev().find(|&i| sync(i)).map_or(0, |i| i + 1);
        let hi = (acked + 1).min(writes.len());
        let k = (durable..=hi).find(|&k| models[k] == state);
        assert!(
            k.is_some(),
            "point {point}: state is not a prefix in [{durable}, {hi}] (acked={acked})"
        );
        if k.unwrap() < acked {
            lossy += 1;
        }
    }
    assert!(
        lossy > 0,
        "some crashes should lose unsynced (acknowledged) writes"
    );
}

/// Records the filesystem op counter at compaction boundaries.
struct CompactionWindows {
    fs: SimFs,
    begins: Mutex<Vec<u64>>,
    windows: Mutex<Vec<(u64, u64)>>,
}

impl EventListener for CompactionWindows {
    fn on_compaction_begin(&self, info: &CompactionInfo) {
        if info.inputs > 1 {
            self.begins.lock().unwrap().push(self.fs.ops());
        }
    }
    fn on_compaction_end(&self, info: &CompactionInfo) {
        if info.inputs > 1 {
            let b = self.begins.lock().unwrap().pop().unwrap();
            self.windows.lock().unwrap().push((b, self.fs.ops()));
        }
    }
}

#[test]
fn kill_mid_compaction_at_every_operation() {
    let writes = workload(3, 1500, 150);
    let models = prefix_models(&writes);
    let base = Options::for_testing();

    let fs = SimFs::new();
    let rec = Arc::new(CompactionWindows {
        fs: fs.clone(),
        begins: Mutex::new(Vec::new()),
        windows: Mutex::new(Vec::new()),
    });
    let opts = Options {
        listener: Some(rec.clone()),
        ..base.clone()
    };
    let db = open(&fs, opts).unwrap();
    assert_eq!(run_until_failure(&db, &writes, |_| true), writes.len());
    drop(db);
    let windows = rec.windows.lock().unwrap().clone();
    assert!(
        windows.len() >= 3,
        "workload must run several real compactions: {windows:?}"
    );

    let mut points: Vec<u64> = windows.iter().flat_map(|&(b, e)| b + 1..=e + 1).collect();
    points.dedup();
    let mut tested = 0;
    for &point in &points {
        let (acked, state) = crash_trial(&writes, &base, point, crash_mode(point), |_| true);
        let ok = state == models[acked] || (acked < writes.len() && state == models[acked + 1]);
        assert!(ok, "mid-compaction crash at op {point}: acked={acked}");
        tested += 1;
    }
    assert!(tested >= 100, "only {tested} mid-compaction crash points");
    eprintln!(
        "{} compactions, {tested} mid-compaction crash points",
        windows.len()
    );
}

#[test]
fn injected_io_errors_never_lose_acknowledged_writes() {
    let writes = workload(4, 150, 40);
    let models = prefix_models(&writes);
    let opts = Options::for_testing();
    let kinds = [
        FaultKind::Append,
        FaultKind::Sync,
        FaultKind::Create,
        FaultKind::Rename,
        FaultKind::SyncDir,
        FaultKind::Remove,
    ];
    let mut failures_seen = 0;
    for kind in kinds {
        for nth in [1u64, 2, 3, 5, 8, 13, 21, 34, 55, 89] {
            let fs = SimFs::new();
            let db = open(&fs, opts.clone()).unwrap();
            fs.inject(kind, nth);
            let acked = run_until_failure(&db, &writes, |_| true);
            if acked < writes.len() {
                failures_seen += 1;
            }
            drop(db);
            fs.crash(CrashMode::DropUnsynced);
            let db = open(&fs, opts.clone()).unwrap_or_else(|e| panic!("{kind:?}#{nth}: {e}"));
            let state = dump(&db);
            let ok = state == models[acked] || (acked < writes.len() && state == models[acked + 1]);
            assert!(ok, "{kind:?}#{nth}: acked={acked}");
        }
    }
    assert!(
        failures_seen > 10,
        "faults should interrupt writes ({failures_seen})"
    );
}

#[test]
fn torn_wal_tails_across_many_seeds() {
    let writes = workload(5, 60, 30);
    let models = prefix_models(&writes);
    let opts = Options {
        write_buffer_size: 1 << 20, // keep everything in the WAL
        ..Options::for_testing()
    };
    let mut distinct_outcomes = std::collections::BTreeSet::new();
    for seed in 0..64 {
        let fs = SimFs::new();
        let db = open(&fs, opts.clone()).unwrap();
        // First half synced, second half not.
        run_until_failure(&db, &writes, |i| i < 30);
        drop(db);
        fs.crash(CrashMode::TornWrites { seed });
        let db = open(&fs, opts.clone()).unwrap();
        let state = dump(&db);
        let k = (30..=60).find(|&k| models[k] == state);
        assert!(k.is_some(), "seed {seed}: not a prefix");
        distinct_outcomes.insert(k.unwrap());
    }
    assert!(
        distinct_outcomes.len() > 3,
        "tearing should cut at many points"
    );
}

#[test]
fn background_mode_crashes_recover_to_a_consistent_prefix() {
    // With a background thread the interleaving of writes, flushes and
    // compactions varies run to run; the recovery guarantee must hold for
    // every interleaving, so each sampled crash point is checked the same way.
    let writes = workload(6, 600, 80);
    let models = prefix_models(&writes);
    let opts = Options {
        background_compaction: true,
        ..Options::for_testing()
    };
    let sync = |i: usize| i % 7 == 6;
    let total = dry_run_ops(&writes, &opts, sync);
    let step = (total / 80).max(1);
    for point in (1..=total).step_by(step as usize) {
        let (acked, state) = crash_trial(&writes, &opts, point, crash_mode(point), sync);
        let durable = (0..acked).rev().find(|&i| sync(i)).map_or(0, |i| i + 1);
        let hi = (acked + 1).min(writes.len());
        assert!(
            (durable..=hi).any(|k| models[k] == state),
            "background crash at op {point}: not a prefix in [{durable}, {hi}]"
        );
    }
}
