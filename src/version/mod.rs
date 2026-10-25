//! Versions: immutable snapshots of which tables make up each level.
//!
//! A [`Version`] is created by applying a [`VersionEdit`] to its predecessor.
//! Readers pin a version (`Arc<Version>`) for the duration of a lookup or a
//! scan, so compactions can install new versions without disturbing them;
//! files are deleted only once no live version references them.

pub mod edit;
pub mod set;

pub use edit::VersionEdit;
pub use set::VersionSet;

use crate::cache::LruCache;
use crate::error::Result;
use crate::filename;
use crate::fs::FileSystem;
use crate::iterator::InternalIterator;
use crate::key::{
    compare_internal, internal_key, parse_internal_key, user_key, ValueKind, MAX_SEQUENCE,
};
use crate::memtable::Lookup;
use crate::options::{Options, NUM_LEVELS};
use crate::sstable::table::BlockCache;
use crate::sstable::{Table, TableIter};
use crate::stats::Stats;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Metadata of one table file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileMetaData {
    /// File number (`NNNNNN.sst`).
    pub number: u64,
    /// File size in bytes.
    pub size: u64,
    /// Smallest internal key in the file.
    pub smallest: Vec<u8>,
    /// Largest internal key in the file.
    pub largest: Vec<u8>,
}

impl FileMetaData {
    /// Smallest user key.
    pub fn smallest_user(&self) -> &[u8] {
        user_key(&self.smallest)
    }

    /// Largest user key.
    pub fn largest_user(&self) -> &[u8] {
        user_key(&self.largest)
    }
}

/// Opens tables on demand and keeps a bounded number of readers alive.
pub struct TableCache {
    fs: Arc<dyn FileSystem>,
    dir: PathBuf,
    tables: LruCache<u64, Table>,
    block_cache: Option<Arc<BlockCache>>,
    stats: Arc<Stats>,
}

impl TableCache {
    /// A cache over tables in `dir`.
    pub fn new(fs: Arc<dyn FileSystem>, dir: PathBuf, opts: &Options, stats: Arc<Stats>) -> Self {
        let block_cache =
            (opts.block_cache_bytes > 0).then(|| Arc::new(BlockCache::new(opts.block_cache_bytes)));
        TableCache {
            fs,
            dir,
            tables: LruCache::new(opts.max_open_tables),
            block_cache,
            stats,
        }
    }

    /// Returns an open reader for table `number` of `size` bytes.
    pub fn get(&self, number: u64, size: u64) -> Result<Arc<Table>> {
        if let Some(t) = self.tables.get(&number) {
            return Ok(t);
        }
        let file = self.fs.open(&filename::table_file(&self.dir, number))?;
        let table = Arc::new(Table::open(
            file,
            size,
            number,
            self.block_cache.clone(),
            self.stats.clone(),
        )?);
        self.tables.insert(number, table.clone(), 1);
        Ok(table)
    }

    /// Drops the cached reader of a deleted table.
    pub fn evict(&self, number: u64) {
        self.tables.erase(&number);
    }
}

/// Index of the first file whose largest key is >= `ikey` (files sorted and
/// non-overlapping).
fn find_file(files: &[Arc<FileMetaData>], ikey: &[u8]) -> usize {
    files.partition_point(|f| compare_internal(&f.largest, ikey).is_lt())
}

/// An immutable set of table files per level.
#[derive(Debug, Default)]
pub struct Version {
    /// `files[0]` is sorted by file number (oldest first) and may overlap;
    /// `files[1..]` are sorted by smallest key and non-overlapping.
    pub files: Vec<Vec<Arc<FileMetaData>>>,
    /// Highest compaction score (>= 1 means a compaction is due).
    pub compaction_score: f64,
    /// Level with the highest score.
    pub compaction_level: usize,
}

impl Version {
    /// An empty version.
    pub fn empty() -> Self {
        Version {
            files: vec![Vec::new(); NUM_LEVELS],
            compaction_score: 0.0,
            compaction_level: 0,
        }
    }

    /// Total bytes of tables at `level`.
    pub fn level_bytes(&self, level: usize) -> u64 {
        self.files[level].iter().map(|f| f.size).sum()
    }

    /// Number of files at `level`.
    pub fn num_files(&self, level: usize) -> usize {
        self.files[level].len()
    }

    /// Computes the compaction score: L0 by file count, other levels by size
    /// relative to their budget.
    pub fn finalize(&mut self, opts: &Options) {
        let mut best = (0.0f64, 0usize);
        for level in 0..NUM_LEVELS - 1 {
            let score = if level == 0 {
                self.files[0].len() as f64 / opts.l0_compaction_trigger as f64
            } else {
                self.level_bytes(level) as f64 / opts.max_bytes_for_level(level) as f64
            };
            if score > best.0 {
                best = (score, level);
            }
        }
        self.compaction_score = best.0;
        self.compaction_level = best.1;
    }

    /// Point lookup of the internal key `target` (`user_key@snapshot`).
    /// Searches L0 newest-first, then one file per deeper level.
    pub fn get(&self, tables: &TableCache, target: &[u8], stats: &Stats) -> Result<Option<Lookup>> {
        let uk = user_key(target);
        let mut candidates: Vec<&Arc<FileMetaData>> = self.files[0]
            .iter()
            .rev()
            .filter(|f| f.smallest_user() <= uk && uk <= f.largest_user())
            .collect();
        for level in 1..NUM_LEVELS {
            let files = &self.files[level];
            let i = find_file(files, target);
            if i < files.len() && files[i].smallest_user() <= uk {
                candidates.push(&files[i]);
            }
        }
        for f in candidates {
            Stats::add(&stats.get_table_probes, 1);
            let table = tables.get(f.number, f.size)?;
            if let Some((k, v)) = table.get(target)? {
                let parsed = parse_internal_key(&k)
                    .ok_or_else(|| crate::Error::corruption("bad internal key in table"))?;
                return Ok(Some(match parsed.kind {
                    ValueKind::Value => Lookup::Found(v),
                    ValueKind::Deletion => Lookup::Deleted,
                }));
            }
        }
        Ok(None)
    }

    /// Files at `level` whose user-key range intersects `[begin, end]`
    /// (`None` = unbounded). For L0 the range is widened transitively, since
    /// overlapping L0 files must be compacted together.
    pub fn overlapping_inputs(
        &self,
        level: usize,
        begin: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Vec<Arc<FileMetaData>> {
        let mut lo = begin.map(<[u8]>::to_vec);
        let mut hi = end.map(<[u8]>::to_vec);
        'restart: loop {
            let mut out = Vec::new();
            for f in &self.files[level] {
                let before = lo.as_deref().is_some_and(|b| f.largest_user() < b);
                let after = hi.as_deref().is_some_and(|e| f.smallest_user() > e);
                if before || after {
                    continue;
                }
                out.push(f.clone());
                if level == 0 {
                    if lo.as_deref().is_some_and(|b| f.smallest_user() < b) {
                        lo = Some(f.smallest_user().to_vec());
                        continue 'restart;
                    }
                    if hi.as_deref().is_some_and(|e| f.largest_user() > e) {
                        hi = Some(f.largest_user().to_vec());
                        continue 'restart;
                    }
                }
            }
            return out;
        }
    }

    /// True if any file at `level` intersects the user-key range.
    pub fn overlaps(&self, level: usize, smallest: &[u8], largest: &[u8]) -> bool {
        !self
            .overlapping_inputs(level, Some(smallest), Some(largest))
            .is_empty()
    }

    /// Appends iterators covering every table of this version: one per L0
    /// file (newest first) and one concatenating iterator per deeper level.
    pub fn add_iterators(
        &self,
        tables: &Arc<TableCache>,
        fill_cache: bool,
        out: &mut Vec<Box<dyn InternalIterator>>,
    ) -> Result<()> {
        for f in self.files[0].iter().rev() {
            out.push(Box::new(tables.get(f.number, f.size)?.iter(fill_cache)));
        }
        for level in 1..NUM_LEVELS {
            if !self.files[level].is_empty() {
                out.push(Box::new(LevelIter::new(
                    tables.clone(),
                    self.files[level].clone(),
                    fill_cache,
                )));
            }
        }
        Ok(())
    }
}

/// Iterates a sorted, non-overlapping list of files as one sorted run,
/// opening each table only when the cursor reaches it.
pub struct LevelIter {
    tables: Arc<TableCache>,
    files: Vec<Arc<FileMetaData>>,
    index: usize,
    current: Option<TableIter>,
    fill_cache: bool,
    err: Option<crate::Error>,
}

impl LevelIter {
    /// Iterates `files` (sorted by key, non-overlapping).
    pub fn new(tables: Arc<TableCache>, files: Vec<Arc<FileMetaData>>, fill_cache: bool) -> Self {
        let index = files.len();
        LevelIter {
            tables,
            files,
            index,
            current: None,
            fill_cache,
            err: None,
        }
    }

    fn open(&mut self, index: usize) {
        self.index = index;
        self.current = None;
        if let Some(f) = self.files.get(index) {
            match self.tables.get(f.number, f.size) {
                Ok(t) => self.current = Some(t.iter(self.fill_cache)),
                Err(e) => self.err = Some(e),
            }
        }
    }

    fn skip_exhausted(&mut self) {
        while self.err.is_none() {
            match &self.current {
                Some(it) if it.valid() => return,
                Some(it) => {
                    if let Err(e) = it.status() {
                        self.err = Some(e);
                        return;
                    }
                }
                None => return,
            }
            self.open(self.index + 1);
            if let Some(it) = self.current.as_mut() {
                it.seek_to_first();
            }
        }
    }
}

impl InternalIterator for LevelIter {
    fn valid(&self) -> bool {
        self.err.is_none() && self.current.as_ref().is_some_and(|c| c.valid())
    }

    fn seek_to_first(&mut self) {
        self.err = None;
        self.open(0);
        if let Some(it) = self.current.as_mut() {
            it.seek_to_first();
        }
        self.skip_exhausted();
    }

    fn seek(&mut self, target: &[u8]) {
        self.err = None;
        let i = find_file(&self.files, target);
        self.open(i);
        if let Some(it) = self.current.as_mut() {
            it.seek(target);
        }
        self.skip_exhausted();
    }

    fn next(&mut self) {
        if let Some(it) = self.current.as_mut() {
            it.next();
        }
        self.skip_exhausted();
    }

    fn key(&self) -> &[u8] {
        self.current.as_ref().expect("valid iterator").key()
    }

    fn value(&self) -> &[u8] {
        self.current.as_ref().expect("valid iterator").value()
    }

    fn status(&self) -> Result<()> {
        if let Some(e) = &self.err {
            return Err(e.duplicate());
        }
        match &self.current {
            Some(c) => c.status(),
            None => Ok(()),
        }
    }
}

/// Accumulates edits on top of a base version.
pub struct VersionBuilder {
    levels: Vec<BTreeMap<u64, Arc<FileMetaData>>>,
}

impl VersionBuilder {
    /// Starts from `base`.
    pub fn new(base: &Version) -> Self {
        VersionBuilder {
            levels: base
                .files
                .iter()
                .map(|fs| fs.iter().map(|f| (f.number, f.clone())).collect())
                .collect(),
        }
    }

    /// Applies one edit.
    pub fn apply(&mut self, edit: &VersionEdit) {
        for &(level, number) in &edit.deleted_files {
            self.levels[level].remove(&number);
        }
        for (level, f) in &edit.new_files {
            self.levels[*level].insert(f.number, Arc::new(f.clone()));
        }
    }

    /// Produces the new version with levels sorted.
    pub fn finish(self, opts: &Options) -> Version {
        let mut files: Vec<Vec<Arc<FileMetaData>>> = self
            .levels
            .into_iter()
            .map(|m| m.into_values().collect())
            .collect();
        for level in files.iter_mut().skip(1) {
            level.sort_by(|a, b| compare_internal(&a.smallest, &b.smallest));
        }
        let mut v = Version {
            files,
            compaction_score: 0.0,
            compaction_level: 0,
        };
        v.finalize(opts);
        v
    }
}

/// Checks the structural invariant of levels >= 1: files are sorted and
/// their **user-key** ranges are disjoint (no user key spans two files).
pub fn check_invariants(v: &Version) -> Result<()> {
    for (level, files) in v.files.iter().enumerate().skip(1) {
        for w in files.windows(2) {
            if w[0].largest_user() >= w[1].smallest_user() {
                return Err(crate::Error::corruption(format!(
                    "overlapping files {} and {} in level {level}",
                    w[0].number, w[1].number
                )));
            }
        }
    }
    Ok(())
}

/// Lookup key for "newest version of `key` visible at `seq`".
pub fn lookup_key(key: &[u8], seq: u64) -> Vec<u8> {
    internal_key(key, seq.min(MAX_SEQUENCE), crate::key::KIND_FOR_SEEK)
}
