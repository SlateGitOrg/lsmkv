//! Tunables for opening a database, reading and writing.

use crate::db::snapshot::Snapshot;
use crate::sstable::TableOptions;
use std::fmt;
use std::sync::Arc;

/// Number of levels in the LSM tree.
pub const NUM_LEVELS: usize = 7;

/// How to treat damage found in write-ahead logs during recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WalRecovery {
    /// Replay records up to the first damaged or missing one and discard the
    /// rest. The recovered state is always a consistent prefix of history.
    #[default]
    PointInTime,
    /// Like `PointInTime` for a damaged *tail* (the signature of a crash
    /// during an unsynced append), but fail the open if valid records follow
    /// the damage, since that indicates media corruption rather than a crash.
    TolerateTailOnly,
}

/// Information passed to [`EventListener`] callbacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionInfo {
    /// Source level.
    pub level: usize,
    /// Number of input files across both levels.
    pub inputs: usize,
    /// Number of files written (0 for a trivial move).
    pub outputs: usize,
}

/// Hooks for observing background work (used by tests and benchmarks).
pub trait EventListener: Send + Sync {
    /// A memtable flush is starting.
    fn on_flush_begin(&self) {}
    /// A memtable flush committed.
    fn on_flush_end(&self) {}
    /// A compaction is starting.
    fn on_compaction_begin(&self, _info: &CompactionInfo) {}
    /// A compaction committed.
    fn on_compaction_end(&self, _info: &CompactionInfo) {}
}

/// Options for [`crate::Db::open`].
#[derive(Clone)]
pub struct Options {
    /// Create the database if it does not exist.
    pub create_if_missing: bool,
    /// Fail if the database already exists.
    pub error_if_exists: bool,
    /// Memtable size that triggers a flush.
    pub write_buffer_size: usize,
    /// Target size of SSTables written by compactions.
    pub max_file_size: u64,
    /// Data block size, restart interval and bloom bits.
    pub table: TableOptions,
    /// Capacity of the shared block cache in bytes (0 disables it).
    pub block_cache_bytes: usize,
    /// Maximum number of open table readers kept in the table cache.
    pub max_open_tables: usize,
    /// Number of L0 files that triggers an L0→L1 compaction.
    pub l0_compaction_trigger: usize,
    /// Number of L0 files at which each write is delayed by 1 ms.
    pub l0_slowdown_trigger: usize,
    /// Number of L0 files at which writes stop until compaction catches up.
    pub l0_stop_trigger: usize,
    /// Size limit of level 1; level `n` gets `base * multiplier^(n-1)`.
    pub max_bytes_for_level_base: u64,
    /// Fan-out between adjacent levels.
    pub level_multiplier: u64,
    /// WAL damage policy at recovery.
    pub wal_recovery: WalRecovery,
    /// Run flushes and compactions on a background thread (true) or inline on
    /// the writing thread (false: deterministic, used by crash tests).
    pub background_compaction: bool,
    /// Upper bound on bytes merged into one group commit.
    pub max_write_group_bytes: usize,
    /// Roll over to a fresh manifest once the current one exceeds this size.
    pub max_manifest_bytes: u64,
    /// Observer for background events.
    pub listener: Option<Arc<dyn EventListener>>,
}

impl fmt::Debug for Options {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Options")
            .field("write_buffer_size", &self.write_buffer_size)
            .field("max_file_size", &self.max_file_size)
            .field("table", &self.table)
            .field("l0_compaction_trigger", &self.l0_compaction_trigger)
            .field("max_bytes_for_level_base", &self.max_bytes_for_level_base)
            .field("background_compaction", &self.background_compaction)
            .field("wal_recovery", &self.wal_recovery)
            .finish_non_exhaustive()
    }
}

impl Default for Options {
    fn default() -> Self {
        Options {
            create_if_missing: true,
            error_if_exists: false,
            write_buffer_size: 4 << 20,
            max_file_size: 2 << 20,
            table: TableOptions::default(),
            block_cache_bytes: 8 << 20,
            max_open_tables: 1000,
            l0_compaction_trigger: 4,
            l0_slowdown_trigger: 8,
            l0_stop_trigger: 12,
            max_bytes_for_level_base: 10 << 20,
            level_multiplier: 10,
            wal_recovery: WalRecovery::PointInTime,
            background_compaction: true,
            max_write_group_bytes: 1 << 20,
            max_manifest_bytes: 4 << 20,
            listener: None,
        }
    }
}

impl Options {
    /// Small buffers and files so tests exercise flushes and multi-level
    /// compaction with little data; work runs inline for determinism.
    pub fn for_testing() -> Self {
        Options {
            write_buffer_size: 8 << 10,
            max_file_size: 8 << 10,
            table: TableOptions {
                block_size: 512,
                restart_interval: 4,
                bloom_bits_per_key: 10,
            },
            block_cache_bytes: 64 << 10,
            max_open_tables: 64,
            max_bytes_for_level_base: 32 << 10,
            level_multiplier: 4,
            background_compaction: false,
            ..Options::default()
        }
    }

    /// Byte budget of `level` (>= 1).
    pub fn max_bytes_for_level(&self, level: usize) -> u64 {
        let mut bytes = self.max_bytes_for_level_base;
        for _ in 1..level {
            bytes = bytes.saturating_mul(self.level_multiplier);
        }
        bytes
    }

    pub(crate) fn validate(&self) -> crate::Result<()> {
        let bad = |m: &str| Err(crate::Error::invalid(m.to_string()));
        if self.write_buffer_size < 1024 {
            return bad("write_buffer_size must be at least 1 KiB");
        }
        if self.max_file_size < 1024 {
            return bad("max_file_size must be at least 1 KiB");
        }
        if self.table.block_size < 64 || self.table.restart_interval == 0 {
            return bad("block_size must be >= 64 and restart_interval >= 1");
        }
        if self.l0_compaction_trigger == 0
            || self.l0_slowdown_trigger < self.l0_compaction_trigger
            || self.l0_stop_trigger < self.l0_slowdown_trigger
        {
            return bad("need 0 < l0_compaction_trigger <= l0_slowdown_trigger <= l0_stop_trigger");
        }
        if self.level_multiplier < 2 {
            return bad("level_multiplier must be >= 2");
        }
        if self.max_open_tables == 0 {
            return bad("max_open_tables must be >= 1");
        }
        Ok(())
    }
}

/// Options for a single write.
#[derive(Debug, Clone, Copy, Default)]
pub struct WriteOptions {
    /// fsync the WAL before acknowledging. Without it a crash may lose the
    /// most recent writes (but never reorders or half-applies them).
    pub sync: bool,
}

/// Options for reads.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReadOptions<'a> {
    /// Read as of this snapshot instead of the latest state.
    pub snapshot: Option<&'a Snapshot>,
    /// Insert blocks read by this operation into the block cache
    /// (`None` = yes for point reads and scans).
    pub fill_cache: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate_and_level_budgets_grow() {
        let o = Options::default();
        o.validate().unwrap();
        Options::for_testing().validate().unwrap();
        assert_eq!(o.max_bytes_for_level(1), 10 << 20);
        assert_eq!(o.max_bytes_for_level(3), 1000 << 20);
        assert!(format!("{o:?}").contains("write_buffer_size"));
        let bad = [
            Options {
                write_buffer_size: 1,
                ..Options::default()
            },
            Options {
                max_file_size: 1,
                ..Options::default()
            },
            Options {
                l0_stop_trigger: 1,
                ..Options::default()
            },
            Options {
                level_multiplier: 1,
                ..Options::default()
            },
            Options {
                max_open_tables: 0,
                ..Options::default()
            },
            Options {
                table: TableOptions {
                    block_size: 1,
                    ..TableOptions::default()
                },
                ..Options::default()
            },
        ];
        for b in bad {
            assert!(b.validate().is_err());
        }
    }
}
