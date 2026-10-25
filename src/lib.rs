//! # lsmkv
//!
//! An LSM-tree key-value storage engine in the LevelDB lineage:
//!
//! * **write path** — writes are grouped (group commit), appended to a
//!   checksummed, block-framed write-ahead log, then inserted into a
//!   concurrent skiplist memtable;
//! * **SSTables** — full memtables are flushed to immutable sorted tables with
//!   prefix-compressed blocks, a block index and a bloom filter;
//! * **leveled compaction** — tables are merged level by level, dropping
//!   shadowed versions and dead tombstones while honouring live snapshots;
//! * **MVCC** — every write gets a sequence number; snapshots and iterators
//!   read a fixed point in that sequence;
//! * **crash safety** — a manifest log records every change to the set of
//!   tables; recovery replays the manifest and the WAL, tolerating torn tails.
//!
//! ```
//! use lsmkv::{Db, Options, ReadOptions};
//! use lsmkv::fs::SimFs;
//! use std::sync::Arc;
//!
//! let db = Db::open_with_fs(Arc::new(SimFs::new()), "/db", Options::for_testing())?;
//! db.put(b"hello", b"world")?;
//! let snap = db.snapshot();
//! db.delete(b"hello")?;
//! assert_eq!(db.get(b"hello")?, None);
//! let at_snap = ReadOptions { snapshot: Some(&snap), ..ReadOptions::default() };
//! assert_eq!(db.get_with(&at_snap, b"hello")?, Some(b"world".to_vec()));
//! # Ok::<(), lsmkv::Error>(())
//! ```
#![warn(missing_docs)]

pub mod batch;
pub mod cache;
pub mod compaction;
pub mod db;
pub mod error;
pub mod filename;
pub mod fs;
pub mod iterator;
pub mod key;
pub mod memtable;
pub mod options;
pub mod sstable;
pub mod stats;
pub mod util;
pub mod version;
pub mod wal;

pub use batch::WriteBatch;
pub use db::iter::{DbIterator, Scan};
pub use db::snapshot::Snapshot;
pub use db::{Db, LevelSummary};
pub use error::{Error, Result};
pub use options::{CompactionInfo, EventListener, Options, ReadOptions, WalRecovery, WriteOptions};
pub use stats::StatsSnapshot;
