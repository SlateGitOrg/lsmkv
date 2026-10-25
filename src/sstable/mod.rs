//! Sorted String Tables: immutable, sorted, checksummed files.
//!
//! ```text
//! [data block 0][trailer] ... [data block N][trailer]
//! [filter block][trailer]      bloom filter over all user keys
//! [metaindex block][trailer]   "filter.bloom" -> handle
//! [index block][trailer]       separator key >= last key of block i -> handle
//! [footer: metaindex handle, index handle, padding, magic]  (48 bytes)
//! ```
//!
//! Blocks use prefix compression with restart points (see [`block`]); each
//! block trailer is a type byte plus a masked CRC-32C.

pub mod block;
pub mod bloom;
pub mod builder;
pub mod format;
pub mod table;

pub use builder::TableBuilder;
pub use table::{Table, TableIter};

/// Tunables for building tables.
#[derive(Debug, Clone, Copy)]
pub struct TableOptions {
    /// Target uncompressed size of a data block.
    pub block_size: usize,
    /// Keys between restart points inside a block.
    pub restart_interval: usize,
    /// Bloom filter bits per key; 0 disables filters.
    pub bloom_bits_per_key: usize,
}

impl Default for TableOptions {
    fn default() -> Self {
        TableOptions {
            block_size: 4096,
            restart_interval: 16,
            bloom_bits_per_key: 10,
        }
    }
}

#[cfg(test)]
mod tests;
