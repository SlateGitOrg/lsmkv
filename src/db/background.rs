//! Flushes, compactions and obsolete-file collection.

use super::{lock, wait, DbInner, DbState};
use crate::compaction::{self, Compaction};
use crate::error::{Error, Result};
use crate::filename::{self, FileType};
use crate::iterator::{InternalIterator, MergingIterator};
use crate::key::{parse_internal_key, ValueKind, MAX_SEQUENCE};
use crate::options::CompactionInfo;
use crate::sstable::TableBuilder;
use crate::stats::Stats;
use crate::version::{FileMetaData, LevelIter, VersionEdit};
use std::sync::atomic::Ordering;
use std::sync::MutexGuard;

type Guarded<'a> = (MutexGuard<'a, DbState>, Result<()>);

impl DbInner {
    fn needs_work(st: &DbState) -> bool {
        st.bg_error.is_none()
            && !st.shutting_down
            && (st.imm.is_some() || st.versions.current().compaction_score >= 1.0)
    }

    /// Wakes the background thread, or runs pending work inline.
    pub(super) fn schedule_work(&self) {
        if self.opts.background_compaction {
            self.work_cv.notify_one();
        } else {
            self.run_inline_work();
        }
    }

    /// Body of the background thread.
    pub(super) fn background_loop(&self) {
        loop {
            {
                let mut st = self.lock_state();
                while !st.shutting_down && !Self::needs_work(&st) {
                    st = wait(&self.work_cv, st);
                }
                if st.shutting_down {
                    return;
                }
            }
            self.background_unit();
        }
    }

    /// Runs units of work until none is pending (inline mode).
    pub(super) fn run_inline_work(&self) {
        while self.background_unit() {}
    }

    /// Performs one flush or one compaction. Returns false if there was
    /// nothing to do (or a background error stops all work).
    fn background_unit(&self) -> bool {
        let _work = lock(&self.work_lock);
        let st = self.lock_state();
        if st.bg_error.is_some() || st.shutting_down {
            return false;
        }
        let (mut st, result) = if st.imm.is_some() {
            self.flush_imm(st)
        } else if let Some(c) = compaction::pick_compaction(&st.versions, &self.opts) {
            self.run_compaction(st, c)
        } else {
            return false;
        };
        let did_work = match result {
            Ok(()) => {
                self.delete_obsolete_files(&mut st);
                true
            }
            Err(e) => {
                self.record_bg_error(&mut st, e);
                false
            }
        };
        drop(st);
        self.bg_cv.notify_all();
        did_work
    }

    fn record_bg_error(&self, st: &mut DbState, e: Error) {
        if matches!(e, Error::Closed) {
            return; // shutdown interrupted the work; not a failure
        }
        log::error!("background error, database is now read-only: {e}");
        st.bg_error.get_or_insert(e);
    }

    /// Runs a manual compaction of `level` over a key range.
    pub(super) fn manual_compaction(
        &self,
        level: usize,
        begin: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Result<()> {
        let _work = lock(&self.work_lock);
        let st = self.lock_state();
        if let Some(e) = &st.bg_error {
            return Err(Self::read_only_error(e));
        }
        let Some(c) = compaction::compact_range(&st.versions, &self.opts, level, begin, end) else {
            return Ok(());
        };
        let (mut st, result) = self.run_compaction(st, c);
        let out = match result {
            Ok(()) => {
                self.delete_obsolete_files(&mut st);
                Ok(())
            }
            Err(e) => {
                let dup = e.duplicate();
                self.record_bg_error(&mut st, e);
                Err(dup)
            }
        };
        drop(st);
        self.bg_cv.notify_all();
        out
    }

    /// Writes the immutable memtable to a new L0 table and commits it.
    fn flush_imm<'a>(&'a self, mut st: MutexGuard<'a, DbState>) -> Guarded<'a> {
        let imm = st
            .imm
            .clone()
            .expect("flush requires an immutable memtable");
        let number = st.versions.new_file_number();
        st.pending_outputs.insert(number);
        let log_number = st.log_number;
        drop(st);
        if let Some(l) = &self.opts.listener {
            l.on_flush_begin();
        }
        let written = self.write_table(number, imm.iter());
        let mut st = self.lock_state();
        st.pending_outputs.remove(&number);
        let meta = match written {
            Ok(m) => m,
            Err(e) => return (st, Err(e)),
        };
        let mut edit = VersionEdit {
            log_number: Some(log_number),
            ..VersionEdit::default()
        };
        let size = meta.as_ref().map_or(0, |m| m.size);
        if let Some(m) = meta {
            edit.add_file(0, m);
        }
        if let Err(e) = st.versions.log_and_apply(edit) {
            return (st, Err(e));
        }
        st.imm = None;
        Stats::add(&self.stats.flushes, 1);
        Stats::add(&self.stats.flush_bytes, size);
        log::info!("flushed memtable to table {number} ({size} bytes)");
        if let Some(l) = &self.opts.listener {
            l.on_flush_end();
        }
        (st, Ok(()))
    }

    /// Writes every entry of `iter` into table `number`, fsyncs it and its
    /// directory entry, and verifies it opens. `None` if `iter` is empty.
    pub(super) fn write_table(
        &self,
        number: u64,
        mut iter: impl InternalIterator,
    ) -> Result<Option<FileMetaData>> {
        iter.seek_to_first();
        if !iter.valid() {
            return iter.status().map(|()| None);
        }
        let path = filename::table_file(&self.dir, number);
        let mut builder = TableBuilder::new(self.fs.create(&path)?, self.opts.table);
        let smallest = iter.key().to_vec();
        let mut largest = Vec::new();
        while iter.valid() {
            builder.add(iter.key(), iter.value())?;
            largest.clear();
            largest.extend_from_slice(iter.key());
            iter.next();
        }
        iter.status()?;
        let (size, mut file) = builder.finish()?;
        file.sync()?;
        drop(file);
        self.fs.sync_dir(&self.dir)?;
        self.table_cache.get(number, size)?;
        Ok(Some(FileMetaData {
            number,
            size,
            smallest,
            largest,
        }))
    }

    /// Executes a compaction (or trivial move) and commits its edit.
    pub(super) fn run_compaction<'a>(
        &'a self,
        mut st: MutexGuard<'a, DbState>,
        mut c: Compaction,
    ) -> Guarded<'a> {
        let mut info = CompactionInfo {
            level: c.level,
            inputs: c.num_input_files(),
            outputs: 0,
        };
        if let Some(l) = &self.opts.listener {
            l.on_compaction_begin(&info);
        }
        if c.is_trivial_move() {
            let f = c.inputs[0][0].clone();
            c.edit.delete_file(c.level, f.number);
            c.edit.add_file(c.level + 1, (*f).clone());
            if let Err(e) = st.versions.log_and_apply(c.edit) {
                return (st, Err(e));
            }
            Stats::add(&self.stats.trivial_moves, 1);
            log::info!(
                "moved table {} from L{} to L{}",
                f.number,
                c.level,
                c.level + 1
            );
            if let Some(l) = &self.opts.listener {
                l.on_compaction_end(&info);
            }
            return (st, Ok(()));
        }
        let smallest_snapshot = self
            .snapshots
            .oldest()
            .unwrap_or_else(|| st.versions.last_sequence());
        drop(st);
        let mut allocated = Vec::new();
        let outputs = self.compaction_io(&mut c, smallest_snapshot, &mut allocated);
        let mut st = self.lock_state();
        for n in &allocated {
            st.pending_outputs.remove(n);
        }
        let outputs = match outputs {
            Ok(o) => o,
            Err(e) => return (st, Err(e)),
        };
        let written: u64 = outputs.iter().map(|f| f.size).sum();
        info.outputs = outputs.len();
        c.add_input_deletions();
        for f in outputs {
            c.edit.add_file(c.level + 1, f);
        }
        let read = c.input_bytes();
        if let Err(e) = st.versions.log_and_apply(std::mem::take(&mut c.edit)) {
            return (st, Err(e));
        }
        Stats::add(&self.stats.compactions, 1);
        Stats::add(&self.stats.compaction_bytes_read, read);
        Stats::add(&self.stats.compaction_bytes_written, written);
        log::info!(
            "compacted L{}+L{}: {} files ({read} bytes) -> {} files ({written} bytes)",
            c.level,
            c.level + 1,
            info.inputs,
            info.outputs
        );
        if let Some(l) = &self.opts.listener {
            l.on_compaction_end(&info);
        }
        (st, Ok(()))
    }

    /// Merges the inputs, dropping shadowed versions and obsolete tombstones,
    /// into size-bounded output tables. Runs without the state lock.
    fn compaction_io(
        &self,
        c: &mut Compaction,
        smallest_snapshot: u64,
        allocated: &mut Vec<u64>,
    ) -> Result<Vec<FileMetaData>> {
        let mut children: Vec<Box<dyn InternalIterator>> = Vec::new();
        if c.level == 0 {
            let mut l0 = c.inputs[0].clone();
            l0.sort_by_key(|f| std::cmp::Reverse(f.number));
            for f in l0 {
                children.push(Box::new(
                    self.table_cache.get(f.number, f.size)?.iter(false),
                ));
            }
        } else {
            children.push(Box::new(LevelIter::new(
                self.table_cache.clone(),
                c.inputs[0].clone(),
                false,
            )));
        }
        if !c.inputs[1].is_empty() {
            children.push(Box::new(LevelIter::new(
                self.table_cache.clone(),
                c.inputs[1].clone(),
                false,
            )));
        }
        let mut input = MergingIterator::new(children);
        input.seek_to_first();

        let mut outputs = Vec::new();
        let mut builder: Option<OutputTable> = None;
        let mut current_user_key: Option<Vec<u8>> = None;
        let mut last_seq_for_key = MAX_SEQUENCE;
        while input.valid() {
            if self.shutdown_flag.load(Ordering::Acquire) {
                return Err(Error::Closed);
            }
            let key = input.key();
            if c.should_stop_before(key) {
                if let Some(b) = builder.take() {
                    outputs.push(b.finish()?);
                }
            }
            let p = parse_internal_key(key)
                .ok_or_else(|| Error::corruption("bad internal key in compaction input"))?;
            if current_user_key.as_deref() != Some(p.user_key) {
                current_user_key = Some(p.user_key.to_vec());
                last_seq_for_key = MAX_SEQUENCE;
            }
            // A newer version of this key is visible to every snapshot, so
            // this one can never be read again.
            let shadowed = last_seq_for_key <= smallest_snapshot;
            // A tombstone visible to every snapshot with nothing beneath it
            // has nothing left to hide.
            let dead_tombstone = p.kind == ValueKind::Deletion
                && p.seq <= smallest_snapshot
                && c.is_base_level_for_key(p.user_key);
            last_seq_for_key = p.seq;
            if !(shadowed || dead_tombstone) {
                if builder.is_none() {
                    let number = {
                        let mut st = self.lock_state();
                        let n = st.versions.new_file_number();
                        st.pending_outputs.insert(n);
                        n
                    };
                    allocated.push(number);
                    builder = Some(OutputTable::new(self, number)?);
                }
                let b = builder.as_mut().expect("builder exists");
                b.add(key, input.value())?;
                if b.builder.file_size_estimate() >= c.max_output_file_size {
                    if let Some(b) = builder.take() {
                        outputs.push(b.finish()?);
                    }
                }
            }
            input.next();
        }
        input.status()?;
        if let Some(b) = builder.take() {
            outputs.push(b.finish()?);
        }
        if !outputs.is_empty() {
            self.fs.sync_dir(&self.dir)?;
        }
        for f in &outputs {
            self.table_cache.get(f.number, f.size)?;
        }
        Ok(outputs)
    }

    /// Deletes WALs, manifests, tables and temp files no longer referenced.
    pub(super) fn delete_obsolete_files(&self, st: &mut DbState) {
        if st.bg_error.is_some() {
            // The on-disk state is uncertain; keep everything for recovery.
            return;
        }
        let mut live = st.versions.live_files();
        live.extend(st.pending_outputs.iter().copied());
        let names = match self.fs.list(&self.dir) {
            Ok(n) => n,
            Err(e) => {
                log::warn!("cannot list {} for cleanup: {e}", self.dir.display());
                return;
            }
        };
        for name in names {
            let keep = match filename::parse(&name) {
                Some(FileType::Log(n)) => n >= st.versions.log_number(),
                Some(FileType::Manifest(n)) => n >= st.versions.manifest_number(),
                Some(FileType::Table(n)) => live.contains(&n),
                Some(FileType::Temp(_)) => false,
                Some(FileType::Current) | None => true,
            };
            if keep {
                continue;
            }
            if let Some(FileType::Table(n)) = filename::parse(&name) {
                self.table_cache.evict(n);
            }
            match self.fs.remove(&self.dir.join(&name)) {
                Ok(()) => log::debug!("deleted obsolete file {name}"),
                Err(e) => log::warn!("failed to delete {name}: {e}"),
            }
        }
    }
}

/// A compaction output under construction.
struct OutputTable {
    number: u64,
    builder: TableBuilder,
    smallest: Vec<u8>,
    largest: Vec<u8>,
}

impl OutputTable {
    fn new(db: &DbInner, number: u64) -> Result<Self> {
        let file = db.fs.create(&filename::table_file(&db.dir, number))?;
        Ok(OutputTable {
            number,
            builder: TableBuilder::new(file, db.opts.table),
            smallest: Vec::new(),
            largest: Vec::new(),
        })
    }

    fn add(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        if self.smallest.is_empty() {
            self.smallest = key.to_vec();
        }
        self.largest.clear();
        self.largest.extend_from_slice(key);
        self.builder.add(key, value)
    }

    fn finish(self) -> Result<FileMetaData> {
        let (size, mut file) = self.builder.finish()?;
        file.sync()?;
        Ok(FileMetaData {
            number: self.number,
            size,
            smallest: self.smallest,
            largest: self.largest,
        })
    }
}
