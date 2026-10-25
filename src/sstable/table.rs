//! Reading tables: point lookups (bloom → index → block) and iteration.

use super::block::{Block, BlockIter};
use super::bloom;
use super::builder::FILTER_META_KEY;
use super::format::{read_block, BlockHandle, Footer, FOOTER_SIZE};
use crate::cache::LruCache;
use crate::error::{Error, Result};
use crate::fs::RandomAccessFile;
use crate::iterator::InternalIterator;
use crate::key::{compare_internal, user_key};
use crate::stats::Stats;
use std::sync::Arc;

/// Cache of decoded blocks keyed by `(file number, block offset)`.
pub type BlockCache = LruCache<(u64, u64), Block>;

/// An open, immutable table.
pub struct Table {
    file: Arc<dyn RandomAccessFile>,
    file_number: u64,
    index: Arc<Block>,
    filter: Option<Vec<u8>>,
    cache: Option<Arc<BlockCache>>,
    stats: Arc<Stats>,
}

impl std::fmt::Debug for Table {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Table")
            .field("file_number", &self.file_number)
            .finish()
    }
}

impl Table {
    /// Opens a table of `size` bytes: reads the footer, index and filter.
    pub fn open(
        file: Arc<dyn RandomAccessFile>,
        size: u64,
        file_number: u64,
        cache: Option<Arc<BlockCache>>,
        stats: Arc<Stats>,
    ) -> Result<Table> {
        if size < FOOTER_SIZE as u64 {
            return Err(Error::corruption(format!(
                "table {file_number} is too short to be an sstable"
            )));
        }
        let footer_bytes = file.read_at(size - FOOTER_SIZE as u64, FOOTER_SIZE)?;
        let footer = Footer::decode(&footer_bytes)?;
        let index = Arc::new(Block::new(read_block(file.as_ref(), footer.index)?)?);
        let meta = Arc::new(Block::new(read_block(file.as_ref(), footer.metaindex)?)?);
        let mut filter = None;
        let mut it = meta.iter(|a, b| a.cmp(b));
        it.seek(FILTER_META_KEY);
        if it.valid() && it.key() == FILTER_META_KEY {
            let handle = BlockHandle::decode(it.value())?;
            filter = Some(read_block(file.as_ref(), handle)?);
        }
        it.status()?;
        Ok(Table {
            file,
            file_number,
            index,
            filter,
            cache,
            stats,
        })
    }

    /// The file number this table was opened as.
    pub fn file_number(&self) -> u64 {
        self.file_number
    }

    /// Loads a data block, consulting the cache when `fill_cache` or a cached
    /// copy exists.
    fn block(&self, handle: BlockHandle, fill_cache: bool) -> Result<Arc<Block>> {
        let key = (self.file_number, handle.offset);
        if let Some(cache) = &self.cache {
            if let Some(b) = cache.get(&key) {
                Stats::add(&self.stats.block_cache_hits, 1);
                return Ok(b);
            }
        }
        Stats::add(&self.stats.block_cache_misses, 1);
        let block = Arc::new(Block::new(read_block(self.file.as_ref(), handle)?)?);
        if fill_cache {
            if let Some(cache) = &self.cache {
                cache.insert(key, block.clone(), block.size());
            }
        }
        Ok(block)
    }

    /// True unless the bloom filter proves `user_key` absent.
    pub fn may_contain(&self, user_key: &[u8]) -> bool {
        match &self.filter {
            Some(f) => {
                Stats::add(&self.stats.bloom_checks, 1);
                let maybe = bloom::may_contain(f, user_key);
                if !maybe {
                    Stats::add(&self.stats.bloom_useful, 1);
                }
                maybe
            }
            None => true,
        }
    }

    /// Returns the first entry with internal key >= `target`, if it has the
    /// same user key as `target`. The bloom filter is consulted first.
    pub fn get(&self, target: &[u8]) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let uk = user_key(target);
        if !self.may_contain(uk) {
            return Ok(None);
        }
        let mut index = self.index.iter(compare_internal);
        index.seek(target);
        index.status()?;
        if !index.valid() {
            return Ok(None);
        }
        let handle = BlockHandle::decode(index.value())?;
        let block = self.block(handle, true)?;
        let mut it = block.iter(compare_internal);
        it.seek(target);
        it.status()?;
        if it.valid() && user_key(it.key()) == uk {
            return Ok(Some((it.key().to_vec(), it.value().to_vec())));
        }
        Ok(None)
    }

    /// A two-level iterator over the whole table. `fill_cache = false` keeps
    /// bulk scans (compactions) from evicting hot blocks.
    pub fn iter(self: &Arc<Self>, fill_cache: bool) -> TableIter {
        TableIter {
            table: self.clone(),
            index: self.index.iter(compare_internal),
            data: None,
            fill_cache,
            err: None,
        }
    }
}

/// Iterates a table by walking the index block and loading data blocks lazily.
pub struct TableIter {
    table: Arc<Table>,
    index: BlockIter,
    data: Option<BlockIter>,
    fill_cache: bool,
    err: Option<Error>,
}

impl TableIter {
    fn load_data_block(&mut self) {
        self.data = None;
        if !self.index.valid() {
            return;
        }
        let loaded = BlockHandle::decode(self.index.value())
            .and_then(|h| self.table.block(h, self.fill_cache));
        match loaded {
            Ok(b) => self.data = Some(b.iter(compare_internal)),
            Err(e) => self.err = Some(e),
        }
    }

    /// Moves forward past exhausted data blocks.
    fn skip_empty_blocks(&mut self) {
        while self.err.is_none() && self.data.as_ref().is_none_or(|d| !d.valid()) {
            if let Some(Err(e)) = self.data.as_ref().map(|d| d.status()) {
                self.err = Some(e);
                return;
            }
            if !self.index.valid() {
                self.data = None;
                return;
            }
            self.index.next();
            self.load_data_block();
            if let Some(d) = self.data.as_mut() {
                d.seek_to_first();
            }
        }
    }
}

impl InternalIterator for TableIter {
    fn valid(&self) -> bool {
        self.err.is_none() && self.data.as_ref().is_some_and(|d| d.valid())
    }

    fn seek_to_first(&mut self) {
        self.err = None;
        self.index.seek_to_first();
        self.load_data_block();
        if let Some(d) = self.data.as_mut() {
            d.seek_to_first();
        }
        self.skip_empty_blocks();
    }

    fn seek(&mut self, target: &[u8]) {
        self.err = None;
        self.index.seek(target);
        self.load_data_block();
        if let Some(d) = self.data.as_mut() {
            d.seek(target);
        }
        self.skip_empty_blocks();
    }

    fn next(&mut self) {
        if let Some(d) = self.data.as_mut() {
            d.next();
        }
        self.skip_empty_blocks();
    }

    fn key(&self) -> &[u8] {
        self.data.as_ref().expect("valid iterator").key()
    }

    fn value(&self) -> &[u8] {
        self.data.as_ref().expect("valid iterator").value()
    }

    fn status(&self) -> Result<()> {
        if let Some(e) = &self.err {
            return Err(e.duplicate());
        }
        self.index.status()?;
        match &self.data {
            Some(d) => d.status(),
            None => Ok(()),
        }
    }
}
