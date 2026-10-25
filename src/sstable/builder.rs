//! Streams sorted entries into a table file.

use super::block::BlockBuilder;
use super::bloom;
use super::format::{block_trailer, BlockHandle, Footer};
use super::TableOptions;
use crate::error::Result;
use crate::fs::WritableFile;
use crate::key::{compare_internal, short_successor, shortest_separator, user_key};

/// Metadata key under which the bloom filter's handle is stored.
pub const FILTER_META_KEY: &[u8] = b"filter.bloom";

/// Writes a table. Entries must be added in strictly increasing internal-key
/// order. The caller syncs and closes the file after [`TableBuilder::finish`].
pub struct TableBuilder {
    file: Box<dyn WritableFile>,
    opts: TableOptions,
    offset: u64,
    data_block: BlockBuilder,
    index_block: BlockBuilder,
    last_key: Vec<u8>,
    entries: u64,
    /// Handle of the last flushed data block, waiting for the next key so a
    /// short separator can be chosen.
    pending_index: Option<BlockHandle>,
    filter_hashes: Vec<u32>,
    last_user_key_hashed: Option<Vec<u8>>,
}

impl TableBuilder {
    /// Starts a table in an empty file.
    pub fn new(file: Box<dyn WritableFile>, opts: TableOptions) -> Self {
        TableBuilder {
            file,
            opts,
            offset: 0,
            data_block: BlockBuilder::new(opts.restart_interval),
            index_block: BlockBuilder::new(1),
            last_key: Vec::new(),
            entries: 0,
            pending_index: None,
            filter_hashes: Vec::new(),
            last_user_key_hashed: None,
        }
    }

    /// Adds one entry.
    pub fn add(&mut self, ikey: &[u8], value: &[u8]) -> Result<()> {
        debug_assert!(
            self.entries == 0 || compare_internal(&self.last_key, ikey).is_lt(),
            "table keys out of order"
        );
        if let Some(handle) = self.pending_index.take() {
            let sep = shortest_separator(&self.last_key, ikey);
            self.index_block.add(&sep, &handle.encode());
        }
        if self.opts.bloom_bits_per_key > 0 {
            let uk = user_key(ikey);
            if self.last_user_key_hashed.as_deref() != Some(uk) {
                self.filter_hashes.push(bloom::key_hash(uk));
                self.last_user_key_hashed = Some(uk.to_vec());
            }
        }
        self.last_key.clear();
        self.last_key.extend_from_slice(ikey);
        self.entries += 1;
        self.data_block.add(ikey, value);
        if self.data_block.estimated_size() >= self.opts.block_size {
            self.flush_data_block()?;
        }
        Ok(())
    }

    fn write_raw_block(&mut self, contents: &[u8]) -> Result<BlockHandle> {
        let handle = BlockHandle {
            offset: self.offset,
            size: contents.len() as u64,
        };
        self.file.append(contents)?;
        self.file.append(&block_trailer(contents))?;
        self.offset += (contents.len() + super::format::BLOCK_TRAILER_SIZE) as u64;
        Ok(handle)
    }

    fn flush_data_block(&mut self) -> Result<()> {
        if self.data_block.is_empty() {
            return Ok(());
        }
        let contents = self.data_block.finish();
        let handle = self.write_raw_block(&contents)?;
        self.pending_index = Some(handle);
        Ok(())
    }

    /// Number of entries added.
    pub fn num_entries(&self) -> u64 {
        self.entries
    }

    /// Bytes written so far plus the pending data block.
    pub fn file_size_estimate(&self) -> u64 {
        self.offset + self.data_block.estimated_size() as u64
    }

    /// Writes the filter, metaindex, index and footer; returns the file size
    /// and the file handle so the caller can sync it.
    pub fn finish(mut self) -> Result<(u64, Box<dyn WritableFile>)> {
        self.flush_data_block()?;
        if let Some(handle) = self.pending_index.take() {
            let succ = short_successor(&self.last_key);
            self.index_block.add(&succ, &handle.encode());
        }
        let mut meta = BlockBuilder::new(1);
        if self.opts.bloom_bits_per_key > 0 {
            let filter = bloom::build_filter(&self.filter_hashes, self.opts.bloom_bits_per_key);
            let fh = self.write_raw_block(&filter)?;
            meta.add(FILTER_META_KEY, &fh.encode());
        }
        let meta_contents = meta.finish();
        let metaindex = self.write_raw_block(&meta_contents)?;
        let index_contents = self.index_block.finish();
        let index = self.write_raw_block(&index_contents)?;
        let footer = Footer { metaindex, index }.encode();
        self.file.append(&footer)?;
        self.offset += footer.len() as u64;
        self.file.flush()?;
        Ok((self.offset, self.file))
    }
}
