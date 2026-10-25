//! Engine-wide counters, used for amplification accounting and benchmarks.

use std::sync::atomic::{AtomicU64, Ordering};

/// Live counters, updated with relaxed atomics on the hot paths.
#[derive(Debug, Default)]
pub struct Stats {
    /// Bytes of keys + values submitted by the user.
    pub user_bytes: AtomicU64,
    /// Write batches committed.
    pub writes: AtomicU64,
    /// Group commits (WAL records); `writes / write_groups` is the batching factor.
    pub write_groups: AtomicU64,
    /// Bytes appended to write-ahead logs.
    pub wal_bytes: AtomicU64,
    /// WAL fsyncs.
    pub wal_syncs: AtomicU64,
    /// Memtable flushes.
    pub flushes: AtomicU64,
    /// Bytes of SSTables written by flushes.
    pub flush_bytes: AtomicU64,
    /// Compactions (excluding trivial moves).
    pub compactions: AtomicU64,
    /// Compactions satisfied by moving a file down one level.
    pub trivial_moves: AtomicU64,
    /// Bytes of SSTables read by compactions.
    pub compaction_bytes_read: AtomicU64,
    /// Bytes of SSTables written by compactions.
    pub compaction_bytes_written: AtomicU64,
    /// Microseconds writers spent stalled waiting for flush/compaction.
    pub stall_micros: AtomicU64,
    /// Bloom filter probes.
    pub bloom_checks: AtomicU64,
    /// Bloom probes that ruled a table out (avoided a block read).
    pub bloom_useful: AtomicU64,
    /// Block cache hits.
    pub block_cache_hits: AtomicU64,
    /// Block cache misses (block read from the file).
    pub block_cache_misses: AtomicU64,
    /// Point lookups served.
    pub gets: AtomicU64,
    /// Tables whose key range covered a point lookup's key (counted before
    /// the bloom filter; subtract `bloom_useful` for data-block reads).
    pub get_table_probes: AtomicU64,
}

/// A plain copy of [`Stats`] at one instant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct StatsSnapshot {
    pub user_bytes: u64,
    pub writes: u64,
    pub write_groups: u64,
    pub wal_bytes: u64,
    pub wal_syncs: u64,
    pub flushes: u64,
    pub flush_bytes: u64,
    pub compactions: u64,
    pub trivial_moves: u64,
    pub compaction_bytes_read: u64,
    pub compaction_bytes_written: u64,
    pub stall_micros: u64,
    pub bloom_checks: u64,
    pub bloom_useful: u64,
    pub block_cache_hits: u64,
    pub block_cache_misses: u64,
    pub gets: u64,
    pub get_table_probes: u64,
}

impl Stats {
    /// Adds `n` to a counter.
    pub fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    /// Copies every counter.
    pub fn snapshot(&self) -> StatsSnapshot {
        let l = |c: &AtomicU64| c.load(Ordering::Relaxed);
        StatsSnapshot {
            user_bytes: l(&self.user_bytes),
            writes: l(&self.writes),
            write_groups: l(&self.write_groups),
            wal_bytes: l(&self.wal_bytes),
            wal_syncs: l(&self.wal_syncs),
            flushes: l(&self.flushes),
            flush_bytes: l(&self.flush_bytes),
            compactions: l(&self.compactions),
            trivial_moves: l(&self.trivial_moves),
            compaction_bytes_read: l(&self.compaction_bytes_read),
            compaction_bytes_written: l(&self.compaction_bytes_written),
            stall_micros: l(&self.stall_micros),
            bloom_checks: l(&self.bloom_checks),
            bloom_useful: l(&self.bloom_useful),
            block_cache_hits: l(&self.block_cache_hits),
            block_cache_misses: l(&self.block_cache_misses),
            gets: l(&self.gets),
            get_table_probes: l(&self.get_table_probes),
        }
    }
}

impl StatsSnapshot {
    /// Device bytes written (WAL + flushes + compactions) per user byte.
    pub fn write_amplification(&self) -> f64 {
        if self.user_bytes == 0 {
            return 0.0;
        }
        (self.wal_bytes + self.flush_bytes + self.compaction_bytes_written) as f64
            / self.user_bytes as f64
    }

    /// Like [`StatsSnapshot::write_amplification`] but excluding the WAL,
    /// i.e. only the LSM tree's own rewriting.
    pub fn tree_write_amplification(&self) -> f64 {
        if self.user_bytes == 0 {
            return 0.0;
        }
        (self.flush_bytes + self.compaction_bytes_written) as f64 / self.user_bytes as f64
    }

    /// Mean tables whose key range covered the key, per point lookup.
    pub fn read_amplification(&self) -> f64 {
        if self.gets == 0 {
            return 0.0;
        }
        self.get_table_probes as f64 / self.gets as f64
    }

    /// Counter deltas between two snapshots (`self - earlier`).
    pub fn since(&self, earlier: &StatsSnapshot) -> StatsSnapshot {
        StatsSnapshot {
            user_bytes: self.user_bytes - earlier.user_bytes,
            writes: self.writes - earlier.writes,
            write_groups: self.write_groups - earlier.write_groups,
            wal_bytes: self.wal_bytes - earlier.wal_bytes,
            wal_syncs: self.wal_syncs - earlier.wal_syncs,
            flushes: self.flushes - earlier.flushes,
            flush_bytes: self.flush_bytes - earlier.flush_bytes,
            compactions: self.compactions - earlier.compactions,
            trivial_moves: self.trivial_moves - earlier.trivial_moves,
            compaction_bytes_read: self.compaction_bytes_read - earlier.compaction_bytes_read,
            compaction_bytes_written: self.compaction_bytes_written
                - earlier.compaction_bytes_written,
            stall_micros: self.stall_micros - earlier.stall_micros,
            bloom_checks: self.bloom_checks - earlier.bloom_checks,
            bloom_useful: self.bloom_useful - earlier.bloom_useful,
            block_cache_hits: self.block_cache_hits - earlier.block_cache_hits,
            block_cache_misses: self.block_cache_misses - earlier.block_cache_misses,
            gets: self.gets - earlier.gets,
            get_table_probes: self.get_table_probes - earlier.get_table_probes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amplification_math() {
        let s = Stats::default();
        assert_eq!(s.snapshot().write_amplification(), 0.0);
        assert_eq!(s.snapshot().read_amplification(), 0.0);
        assert_eq!(s.snapshot().tree_write_amplification(), 0.0);
        Stats::add(&s.user_bytes, 100);
        Stats::add(&s.wal_bytes, 110);
        Stats::add(&s.flush_bytes, 90);
        Stats::add(&s.compaction_bytes_written, 300);
        Stats::add(&s.gets, 4);
        Stats::add(&s.get_table_probes, 6);
        let snap = s.snapshot();
        assert!((snap.write_amplification() - 5.0).abs() < 1e-9);
        assert!((snap.tree_write_amplification() - 3.9).abs() < 1e-9);
        assert!((snap.read_amplification() - 1.5).abs() < 1e-9);
        let d = snap.since(&StatsSnapshot::default());
        assert_eq!(d, snap);
    }
}
