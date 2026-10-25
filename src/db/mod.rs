//! The database: write path with group commit, read path over memtables and
//! levels, snapshots, iterators, recovery and lifecycle.
//!
//! Concurrency model:
//! * one `state` mutex guards metadata (memtable pointers, version set, the
//!   writer queue, background error);
//! * writers queue up; the queue head becomes the *leader*, merges the queued
//!   batches into one WAL record (group commit), writes and optionally fsyncs
//!   it **without** holding `state`, applies it to the memtable, then wakes the
//!   followers it committed for;
//! * readers take `state` only to clone `Arc`s of the memtables and the
//!   current version, then search lock-free;
//! * flushes and compactions (one at a time, serialised by `work_lock`) run on
//!   a background thread, or inline on the writing thread when
//!   [`Options::background_compaction`] is false.

mod background;
pub mod iter;
pub mod snapshot;

use crate::batch::WriteBatch;
use crate::error::{Error, Result};
use crate::filename::{self, FileType};
use crate::fs::{FileSystem, OsFs};
use crate::iterator::{InternalIterator, MergingIterator};
use crate::key::SeqNum;
use crate::memtable::{Lookup, MemTable};
use crate::options::{Options, ReadOptions, WalRecovery, WriteOptions, NUM_LEVELS};
use crate::stats::{Stats, StatsSnapshot};
use crate::version::{lookup_key, TableCache, VersionEdit, VersionSet};
use crate::wal::{LogReader, LogWriter, ReadEvent};
use iter::{DbIterator, Scan};
use snapshot::{Snapshot, SnapshotRegistry};
use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Group commits grow up to this many bytes beyond a small leader batch.
const SMALL_BATCH_SLACK: usize = 128 << 10;

/// One queued write. `batch == None` requests a memtable flush.
struct Writer {
    sync: bool,
    slot: Mutex<WriterSlot>,
}

struct WriterSlot {
    batch: Option<WriteBatch>,
    result: Option<Result<()>>,
}

impl Writer {
    fn slot(&self) -> MutexGuard<'_, WriterSlot> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub(crate) struct DbState {
    mem: Arc<MemTable>,
    imm: Option<Arc<MemTable>>,
    versions: VersionSet,
    writers: VecDeque<Arc<Writer>>,
    bg_error: Option<Error>,
    shutting_down: bool,
    /// Table numbers being written that no version references yet.
    pending_outputs: BTreeSet<u64>,
    /// Number of the WAL backing `mem`.
    log_number: u64,
}

pub(crate) struct DbInner {
    fs: Arc<dyn FileSystem>,
    dir: PathBuf,
    opts: Options,
    stats: Arc<Stats>,
    state: Mutex<DbState>,
    writers_cv: Condvar,
    /// Signalled whenever a unit of background work finishes or fails.
    bg_cv: Condvar,
    /// Wakes the background thread.
    work_cv: Condvar,
    /// Serialises flushes and compactions.
    work_lock: Mutex<()>,
    wal: Mutex<Option<LogWriter>>,
    snapshots: Arc<SnapshotRegistry>,
    table_cache: Arc<TableCache>,
    shutdown_flag: AtomicBool,
}

/// Per-level summary returned by [`Db::levels`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelSummary {
    /// Number of tables.
    pub files: usize,
    /// Total table bytes.
    pub bytes: u64,
}

/// An open database. `Db` is `Send + Sync`; share it with `Arc`.
pub struct Db {
    inner: Arc<DbInner>,
    bg_thread: Mutex<Option<JoinHandle<()>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn wait<'a, T>(cv: &Condvar, g: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    cv.wait(g).unwrap_or_else(PoisonError::into_inner)
}

/// Inserts every operation of a sequenced batch into `mem`.
fn insert_batch(mem: &MemTable, batch: &WriteBatch) -> Result<()> {
    for (seq, op) in (batch.sequence()..).zip(batch.iter()) {
        let op = op?;
        mem.add(seq, op.kind, op.key, op.value);
    }
    Ok(())
}

impl Db {
    /// Opens (or creates) a database in `path` on the real filesystem.
    pub fn open(path: impl AsRef<Path>, opts: Options) -> Result<Db> {
        Db::open_with_fs(Arc::new(OsFs), path, opts)
    }

    /// Opens a database through any [`FileSystem`] (e.g. [`crate::fs::SimFs`]).
    pub fn open_with_fs(
        fs: Arc<dyn FileSystem>,
        path: impl AsRef<Path>,
        opts: Options,
    ) -> Result<Db> {
        opts.validate()?;
        let dir = path.as_ref().to_path_buf();
        fs.create_dir_all(&dir)?;
        let stats = Arc::new(Stats::default());
        let table_cache = Arc::new(TableCache::new(
            fs.clone(),
            dir.clone(),
            &opts,
            stats.clone(),
        ));
        let mut versions =
            VersionSet::new(fs.clone(), dir.clone(), opts.clone(), table_cache.clone());
        if fs.exists(&filename::current_file(&dir)) {
            if opts.error_if_exists {
                return Err(Error::invalid(format!("{} already exists", dir.display())));
            }
            versions.recover()?;
        } else if !opts.create_if_missing {
            return Err(Error::NotFound(format!("no database in {}", dir.display())));
        }
        let inner = Arc::new(DbInner {
            fs,
            dir,
            opts,
            stats,
            state: Mutex::new(DbState {
                mem: Arc::new(MemTable::new()),
                imm: None,
                versions,
                writers: VecDeque::new(),
                bg_error: None,
                shutting_down: false,
                pending_outputs: BTreeSet::new(),
                log_number: 0,
            }),
            writers_cv: Condvar::new(),
            bg_cv: Condvar::new(),
            work_cv: Condvar::new(),
            work_lock: Mutex::new(()),
            wal: Mutex::new(None),
            snapshots: Arc::new(SnapshotRegistry::default()),
            table_cache,
            shutdown_flag: AtomicBool::new(false),
        });
        inner.recover_logs()?;
        let bg_thread = if inner.opts.background_compaction {
            let worker = inner.clone();
            Some(
                std::thread::Builder::new()
                    .name("lsmkv-bg".into())
                    .spawn(move || worker.background_loop())?,
            )
        } else {
            None
        };
        let db = Db {
            inner,
            bg_thread: Mutex::new(bg_thread),
        };
        db.inner.schedule_work();
        Ok(db)
    }

    /// Sets `key` to `value`.
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let mut b = WriteBatch::new();
        b.put(key, value);
        self.write(&WriteOptions::default(), b)
    }

    /// Removes `key` (writes a tombstone).
    pub fn delete(&self, key: &[u8]) -> Result<()> {
        let mut b = WriteBatch::new();
        b.delete(key);
        self.write(&WriteOptions::default(), b)
    }

    /// Applies `batch` atomically.
    pub fn write(&self, opts: &WriteOptions, batch: WriteBatch) -> Result<()> {
        for op in batch.iter() {
            let op = op?;
            if op.key.len() > u32::MAX as usize || op.value.len() > u32::MAX as usize {
                return Err(Error::invalid("keys and values must be < 4 GiB"));
            }
        }
        self.inner.write(opts.sync, Some(batch))
    }

    /// Reads the latest value of `key`.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.get_with(&ReadOptions::default(), key)
    }

    /// Reads `key` with explicit options (e.g. at a snapshot).
    pub fn get_with(&self, opts: &ReadOptions<'_>, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let (mem, imm, version, seq) = {
            let st = self.inner.lock_state();
            let seq = self.inner.read_sequence(&st, opts)?;
            (st.mem.clone(), st.imm.clone(), st.versions.current(), seq)
        };
        Stats::add(&self.inner.stats.gets, 1);
        let found = match mem.get(key, seq) {
            Some(l) => Some(l),
            None => match imm.as_ref().and_then(|m| m.get(key, seq)) {
                Some(l) => Some(l),
                None => version.get(
                    &self.inner.table_cache,
                    &lookup_key(key, seq),
                    &self.inner.stats,
                )?,
            },
        };
        Ok(match found {
            Some(Lookup::Found(v)) => Some(v),
            Some(Lookup::Deleted) | None => None,
        })
    }

    /// Captures the current state for repeatable reads.
    pub fn snapshot(&self) -> Snapshot {
        let st = self.inner.lock_state();
        self.inner.snapshots.snapshot(st.versions.last_sequence())
    }

    /// A cursor over all live keys.
    pub fn iter(&self, opts: &ReadOptions<'_>) -> Result<DbIterator> {
        self.range_iter(opts, None, None)
    }

    /// A cursor over live keys in `[lower, upper)` (`None` = unbounded).
    pub fn range_iter(
        &self,
        opts: &ReadOptions<'_>,
        lower: Option<&[u8]>,
        upper: Option<&[u8]>,
    ) -> Result<DbIterator> {
        let fill_cache = opts.fill_cache.unwrap_or(true);
        let st = self.inner.lock_state();
        let seq = self.inner.read_sequence(&st, opts)?;
        let mut children: Vec<Box<dyn InternalIterator>> = vec![Box::new(st.mem.iter())];
        if let Some(imm) = &st.imm {
            children.push(Box::new(imm.iter()));
        }
        let version = st.versions.current();
        drop(st);
        version.add_iterators(&self.inner.table_cache, fill_cache, &mut children)?;
        Ok(DbIterator::new(
            MergingIterator::new(children),
            seq,
            lower.map(<[u8]>::to_vec),
            upper.map(<[u8]>::to_vec),
            version,
        ))
    }

    /// An [`Iterator`] over `(key, value)` pairs in `[lower, upper)`.
    pub fn scan(
        &self,
        opts: &ReadOptions<'_>,
        lower: Option<&[u8]>,
        upper: Option<&[u8]>,
    ) -> Result<Scan> {
        Ok(Scan::new(self.range_iter(opts, lower, upper)?))
    }

    /// Writes the memtable to an L0 table and waits for it to commit.
    pub fn flush(&self) -> Result<()> {
        self.inner.write(false, None)?;
        self.inner.wait_until(|st| st.imm.is_none())
    }

    /// Compacts every file overlapping `[begin, end]` (user keys, `None` =
    /// unbounded): each level from L0 down to the deepest level holding data
    /// for the range is merged into the next one, so every overlapping entry
    /// is rewritten once and shadowed versions and dead tombstones are purged.
    pub fn compact_range(&self, begin: Option<&[u8]>, end: Option<&[u8]>) -> Result<()> {
        self.flush()?;
        let deepest = {
            let st = self.inner.lock_state();
            let v = st.versions.current();
            (0..NUM_LEVELS)
                .rev()
                .find(|&l| !v.overlapping_inputs(l, begin, end).is_empty())
                .unwrap_or(0)
        };
        for level in 0..=deepest.min(NUM_LEVELS - 2) {
            self.inner.manual_compaction(level, begin, end)?;
        }
        self.inner.schedule_work();
        Ok(())
    }

    /// Blocks until no flush or size-triggered compaction is pending.
    pub fn wait_for_compactions(&self) -> Result<()> {
        self.inner.schedule_work();
        self.inner
            .wait_until(|st| st.imm.is_none() && st.versions.current().compaction_score < 1.0)
    }

    /// A copy of the engine counters.
    pub fn stats(&self) -> StatsSnapshot {
        self.inner.stats.snapshot()
    }

    /// File count and bytes per level.
    pub fn levels(&self) -> Vec<LevelSummary> {
        let v = self.inner.lock_state().versions.current();
        (0..NUM_LEVELS)
            .map(|l| LevelSummary {
                files: v.num_files(l),
                bytes: v.level_bytes(l),
            })
            .collect()
    }

    /// Latest committed sequence number.
    pub fn last_sequence(&self) -> SeqNum {
        self.inner.lock_state().versions.last_sequence()
    }

    /// Number of live snapshots.
    pub fn live_snapshots(&self) -> usize {
        self.inner.snapshots.count()
    }

    /// Human-readable level summary, one line per non-empty level.
    pub fn describe_levels(&self) -> String {
        let mut out = String::new();
        for (l, s) in self.levels().iter().enumerate() {
            if s.files > 0 {
                out.push_str(&format!(
                    "L{l}: {:>4} files {:>12} bytes\n",
                    s.files, s.bytes
                ));
            }
        }
        if out.is_empty() {
            out.push_str("(no tables)\n");
        }
        out
    }

    /// Stops background work and closes the database. Dropping a `Db` does
    /// the same; `close` additionally reports a background error, if any.
    pub fn close(self) -> Result<()> {
        let err = self
            .inner
            .lock_state()
            .bg_error
            .as_ref()
            .map(Error::duplicate);
        drop(self);
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        {
            let mut st = self.inner.lock_state();
            st.shutting_down = true;
            self.inner.shutdown_flag.store(true, Ordering::Release);
        }
        self.inner.work_cv.notify_all();
        self.inner.bg_cv.notify_all();
        self.inner.writers_cv.notify_all();
        if let Some(h) = lock(&self.bg_thread).take() {
            let _ = h.join();
        }
    }
}

impl DbInner {
    fn lock_state(&self) -> MutexGuard<'_, DbState> {
        lock(&self.state)
    }

    fn read_sequence(&self, st: &DbState, opts: &ReadOptions<'_>) -> Result<SeqNum> {
        match opts.snapshot {
            Some(s) if !s.belongs_to(&self.snapshots) => {
                Err(Error::invalid("snapshot belongs to a different database"))
            }
            Some(s) => Ok(s.sequence()),
            None => Ok(st.versions.last_sequence()),
        }
    }

    fn read_only_error(e: &Error) -> Error {
        Error::ReadOnly(e.to_string())
    }

    /// Group-commit write path. `batch == None` forces a memtable switch.
    fn write(&self, sync: bool, batch: Option<WriteBatch>) -> Result<()> {
        let force = batch.is_none();
        let w = Arc::new(Writer {
            sync,
            slot: Mutex::new(WriterSlot {
                batch,
                result: None,
            }),
        });
        let mut st = self.lock_state();
        st.writers.push_back(w.clone());
        loop {
            if let Some(r) = w.slot().result.take() {
                return r;
            }
            if Arc::ptr_eq(&st.writers[0], &w) {
                break;
            }
            st = wait(&self.writers_cv, st);
        }

        // This thread is the leader.
        let (st2, mut result) = self.make_room_for_write(st, force);
        st = st2;
        let mut group_size = 1;
        if result.is_ok() && !force {
            let (group, n, group_sync) = Self::build_group(&st, self.opts.max_write_group_bytes);
            group_size = n;
            let first_seq = st.versions.last_sequence() + 1;
            let mut group = group;
            group.set_sequence(first_seq);
            let count = u64::from(group.count());
            let mem = st.mem.clone();
            drop(st);
            result = self.commit_group(&group, group_sync, &mem, n);
            st = self.lock_state();
            match &result {
                Ok(()) if count > 0 => st.versions.set_last_sequence(first_seq + count - 1),
                Ok(()) => {}
                Err(e) => {
                    log::error!("WAL write failed, database is now read-only: {e}");
                    st.bg_error.get_or_insert_with(|| e.duplicate());
                    self.bg_cv.notify_all();
                }
            }
        }
        for _ in 0..group_size {
            let x = st.writers.pop_front().expect("leader's group is queued");
            if !Arc::ptr_eq(&x, &w) {
                x.slot().result = Some(match &result {
                    Ok(()) => Ok(()),
                    Err(e) => Err(e.duplicate()),
                });
            }
        }
        self.writers_cv.notify_all();
        result
    }

    /// Merges the leader's batch with queued followers' batches.
    fn build_group(st: &DbState, max_bytes: usize) -> (WriteBatch, usize, bool) {
        let leader = &st.writers[0];
        let mut group = leader.slot().batch.take().unwrap_or_default();
        let mut sync = leader.sync;
        let limit = if group.size_bytes() <= SMALL_BATCH_SLACK {
            group.size_bytes() + SMALL_BATCH_SLACK
        } else {
            max_bytes
        };
        let mut n = 1;
        for w in st.writers.iter().skip(1) {
            let mut slot = w.slot();
            let Some(b) = slot.batch.as_ref() else { break }; // a flush request
            if group.size_bytes() + b.size_bytes() > limit {
                break;
            }
            group.append(b);
            slot.batch = None;
            sync |= w.sync;
            n += 1;
        }
        (group, n, sync)
    }

    /// Writes the group's WAL record and applies it to the memtable.
    fn commit_group(
        &self,
        group: &WriteBatch,
        sync: bool,
        mem: &MemTable,
        writers: usize,
    ) -> Result<()> {
        {
            let mut wal = lock(&self.wal);
            let wal = wal.as_mut().ok_or(Error::Closed)?;
            let before = wal.bytes_written();
            wal.add_record(group.data())?;
            Stats::add(&self.stats.wal_bytes, wal.bytes_written() - before);
            if sync {
                wal.sync()?;
                Stats::add(&self.stats.wal_syncs, 1);
            }
        }
        insert_batch(mem, group)?;
        Stats::add(&self.stats.user_bytes, group.payload_bytes());
        Stats::add(&self.stats.writes, writers as u64);
        Stats::add(&self.stats.write_groups, 1);
        Ok(())
    }

    /// Ensures the memtable has room, switching to a new memtable + WAL when
    /// full (or when `force`), and applies L0 back-pressure.
    fn make_room_for_write<'a>(
        &'a self,
        mut st: MutexGuard<'a, DbState>,
        mut force: bool,
    ) -> (MutexGuard<'a, DbState>, Result<()>) {
        let background = self.opts.background_compaction;
        let mut allow_delay = !force;
        loop {
            if let Some(e) = &st.bg_error {
                let e = Self::read_only_error(e);
                return (st, Err(e));
            }
            if st.shutting_down {
                return (st, Err(Error::Closed));
            }
            let l0 = st.versions.current().num_files(0);
            if background && allow_delay && l0 >= self.opts.l0_slowdown_trigger {
                // Soft back-pressure: delay this write by 1 ms once, spreading
                // the stall over many writes instead of one long stop.
                drop(st);
                std::thread::sleep(Duration::from_millis(1));
                Stats::add(&self.stats.stall_micros, 1000);
                st = self.lock_state();
                allow_delay = false;
                continue;
            }
            if !force && st.mem.approximate_bytes() <= self.opts.write_buffer_size {
                return (st, Ok(()));
            }
            if st.imm.is_some() || l0 >= self.opts.l0_stop_trigger {
                if background {
                    let t = Instant::now();
                    self.work_cv.notify_one();
                    st = wait(&self.bg_cv, st);
                    Stats::add(&self.stats.stall_micros, t.elapsed().as_micros() as u64);
                } else {
                    drop(st);
                    self.run_inline_work();
                    st = self.lock_state();
                }
                continue;
            }
            if force && st.mem.is_empty() {
                return (st, Ok(()));
            }
            if let Err(e) = self.switch_memtable(&mut st) {
                log::error!("memtable switch failed: {e}");
                st.bg_error.get_or_insert_with(|| e.duplicate());
                return (st, Err(e));
            }
            force = false;
            if background {
                self.work_cv.notify_one();
            } else {
                drop(st);
                self.run_inline_work();
                st = self.lock_state();
            }
        }
    }

    /// Starts a new WAL and memtable; the old memtable becomes immutable.
    fn switch_memtable(&self, st: &mut DbState) -> Result<()> {
        let n = st.versions.new_file_number();
        let file = self.fs.create(&filename::log_file(&self.dir, n))?;
        // The new WAL's directory entry must be durable before any write in
        // it is acknowledged as synced.
        self.fs.sync_dir(&self.dir)?;
        {
            let mut wal = lock(&self.wal);
            // Sync the old WAL so a crash can never keep later writes (in the
            // new WAL) while losing earlier ones (in the old WAL's tail).
            if let Some(old) = wal.as_mut() {
                old.sync()?;
            }
            *wal = Some(LogWriter::new(file));
        }
        let old = std::mem::replace(&mut st.mem, Arc::new(MemTable::new()));
        st.imm = Some(old);
        st.log_number = n;
        log::debug!("switched to WAL {n}");
        Ok(())
    }

    /// Waits until `done(state)` holds, running work inline when there is no
    /// background thread. Fails on a background error.
    fn wait_until(&self, done: impl Fn(&DbState) -> bool) -> Result<()> {
        loop {
            if !self.opts.background_compaction {
                self.run_inline_work();
            }
            let mut st = self.lock_state();
            if let Some(e) = &st.bg_error {
                return Err(Self::read_only_error(e));
            }
            if done(&st) {
                return Ok(());
            }
            if st.shutting_down {
                return Err(Error::Closed);
            }
            if self.opts.background_compaction {
                self.work_cv.notify_one();
                st = wait(&self.bg_cv, st);
                drop(st);
            }
        }
    }

    /// Replays WALs not yet covered by the manifest, writes their contents to
    /// L0, starts a fresh WAL and commits a new manifest.
    fn recover_logs(&self) -> Result<()> {
        let mut st = self.lock_state();
        let min_log = st.versions.log_number();
        let mut logs: Vec<u64> = self
            .fs
            .list(&self.dir)?
            .iter()
            .filter_map(|name| match filename::parse(name) {
                Some(FileType::Log(n)) if n >= min_log => Some(n),
                _ => None,
            })
            .collect();
        logs.sort_unstable();
        for &n in &logs {
            st.versions.mark_file_number_used(n);
        }
        let mut edit = VersionEdit::default();
        let mut max_seq = st.versions.last_sequence();
        let mut mem = MemTable::new();
        let mut damaged = false;
        let mut replayed = 0u64;
        for (i, &n) in logs.iter().enumerate() {
            let path = filename::log_file(&self.dir, n);
            if damaged {
                log::warn!(
                    "discarding WAL {n}: an earlier WAL was damaged (point-in-time recovery)"
                );
                continue;
            }
            let mut reader = LogReader::new(self.fs.read_all(&path)?);
            loop {
                let (offset, reason, truncated) = match reader.next_event() {
                    ReadEvent::Eof => break,
                    ReadEvent::Record(rec) => match WriteBatch::from_data(&rec) {
                        Ok(batch) => {
                            insert_batch(&mem, &batch)?;
                            if batch.count() > 0 {
                                max_seq =
                                    max_seq.max(batch.sequence() + u64::from(batch.count()) - 1);
                            }
                            replayed += u64::from(batch.count());
                            if mem.approximate_bytes() > self.opts.write_buffer_size {
                                let number = st.versions.new_file_number();
                                if let Some(meta) = self.write_table(number, mem.iter())? {
                                    edit.add_file(0, meta);
                                }
                                mem = MemTable::new();
                            }
                            continue;
                        }
                        Err(_) => (0, "undecodable write batch", false),
                    },
                    ReadEvent::TruncatedTail { offset } => (offset, "truncated tail", true),
                    ReadEvent::Corruption { offset, reason } => (offset, reason, false),
                };
                if self.opts.wal_recovery == WalRecovery::TolerateTailOnly {
                    let later_data = logs[i + 1..].iter().any(|&m| {
                        self.fs
                            .read_all(&filename::log_file(&self.dir, m))
                            .is_ok_and(|d| !d.is_empty())
                    });
                    if later_data || (!truncated && reader.any_record_follows()) {
                        return Err(Error::corruption(format!(
                            "WAL {n} corrupted at offset {offset} ({reason}) with valid data after it"
                        )));
                    }
                }
                log::warn!("WAL {n}: stopping replay at offset {offset}: {reason}");
                damaged = true;
                break;
            }
        }
        if !mem.is_empty() {
            let number = st.versions.new_file_number();
            if let Some(meta) = self.write_table(number, mem.iter())? {
                edit.add_file(0, meta);
            }
        }
        st.versions.set_last_sequence(max_seq);
        let new_log = st.versions.new_file_number();
        let file = self.fs.create(&filename::log_file(&self.dir, new_log))?;
        edit.log_number = Some(new_log);
        // Commits a fresh manifest; its CURRENT switch also syncs the
        // directory, making the new WAL's entry durable.
        st.versions.log_and_apply(edit)?;
        *lock(&self.wal) = Some(LogWriter::new(file));
        st.log_number = new_log;
        if !logs.is_empty() {
            log::info!(
                "recovered {replayed} writes from {} WAL(s); last_seq={max_seq}",
                logs.len()
            );
        }
        self.delete_obsolete_files(&mut st);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
