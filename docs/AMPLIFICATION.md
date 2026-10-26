# Write, read and space amplification

An LSM tree trades three costs against each other. This page defines how
lsmkv measures each one, what the leveled design predicts, and what was
measured on the build machine. All measured numbers come from
`bench/RESULTS.md` (`make bench`, 1,000,000 entries, 16-byte keys, 100-byte
values, default options) unless noted.

## Definitions (as implemented in `src/stats.rs`)

| Metric | Formula | Counter source |
|---|---|---|
| Write amplification (WA) | (WAL bytes + flushed table bytes + compaction output bytes) / user key+value bytes | `wal_bytes`, `flush_bytes`, `compaction_bytes_written`, `user_bytes` |
| Tree WA | same, without WAL bytes | |
| Read amplification (point) | tables whose key range covered the key, per lookup; *block reads* = that minus bloom-filter rejections | `get_table_probes`, `bloom_useful`, `gets` |
| Space amplification (SA) | total table bytes / live key+value bytes (from a full scan) | `Db::levels()`, `db_bench readseq` |

Internal-key trailers (8 bytes per entry), block restart arrays, index
blocks, bloom filters and checksums all count as amplification, so a 116-byte
logical entry starts at ~1.07× before any duplication.

## What the leveled design predicts

Let *F* be the fan-out (`level_multiplier`, 10) and *L* the number of
non-empty levels below L0.

**Write amplification.** Every byte is written once to the WAL and once by
the flush. Each time data moves from level *n* to *n+1* it is merged with the
overlapping part of *n+1*, which is up to *F* times larger; on average a byte
is rewritten roughly *F/2 … F* times per level it descends. So
`WA ≈ 2 + c·F·L` with `c ∈ [0.5, 1]` once levels are full; a tree that is
still filling (as in a benchmark that starts empty) sits below that bound.
Sequential inserts are the best case: new tables never overlap existing ones,
so compactions become **trivial moves** and WA collapses to ≈ 2.

**Read amplification.** A point lookup consults the memtables, up to
`l0_compaction_trigger`–`l0_stop_trigger` L0 files, and at most one file per
deeper level, i.e. ≤ *k + L* tables. With 10-bit bloom filters (≈ 1%
false-positive rate, `src/sstable/bloom.rs`) only ≈ 1 data block is read for
a present key and ≈ 0.01·(k+L) for an absent key. Range scans cannot use
bloom filters and touch every level.

**Space amplification.** In a leveled tree whose last level holds almost
all data, at most ~1/F of the bytes in upper levels can be obsolete versions
of last-level keys, so `SA ≈ 1 + 1/F ≈ 1.1` plus the fixed per-entry overhead.
Tombstones and versions pinned by snapshots add to it until they are
compacted away; `Db::compact_range(None, None)` rewrites everything and
purges them.

## Measured

From `bench/RESULTS.md` (`make bench`; the machine was running other builds,
load average ≈ 7 on 8 cores, so throughput numbers are pessimistic; the
amplification ratios depend far less on load, though background compaction
timing can shift them slightly between runs):

| Benchmark | WA (incl. WAL) | Tree WA | Notes |
|---|---|---|---|
| fillseq (1M sequential keys, empty DB) | 2.19 | 1.00 | 41 flushes, 0 compactions — every table is trivially moved down |
| fillrandom (1M random keys, empty DB) | 5.60 | 4.41 | 44 compactions |
| overwrite (1M random keys onto that DB) | 7.27 | 6.08 | 58 compactions |

| Read metric (readrandom, 1M lookups, ~87% present) | Value |
|---|---|
| tables consulted per lookup | 1.53 |
| data-block reads per lookup | 0.88 |
| table probes skipped by bloom filters | 658,673 |

| Space metric (after overwrite + `wait_for_compactions`) | Value |
|---|---|
| table bytes / live key+value bytes | 1.08 (108.7 MB / 100.3 MB) |

The end-to-end demo (`make demo`, 100k random inserts with
`write_buffer_size` 512 KiB and `max_bytes_for_level_base` 2 MiB, so the tree
is one level deeper relative to its size) shows the same shape: WA 5.46
(tree 4.44), bloom filters filtering out 99% of probes for absent keys, and SA
falling from 1.07 to 1.05 after a full manual compaction.

## Knobs

| Option | Effect |
|---|---|
| `write_buffer_size` | larger memtables → fewer, larger L0 files → less WA, more memory and longer recovery |
| `level_multiplier` (*F*) | larger *F* → fewer levels (less read amp) but more rewriting per level (more WA) |
| `max_bytes_for_level_base` | size of L1; with *F* sets how many levels a dataset needs |
| `l0_compaction_trigger` | more L0 files tolerated → less WA, more read amp |
| `table.bloom_bits_per_key` | 10 bits ≈ 1% false positives; 0 disables filters |
| `max_file_size` | smaller files → finer-grained compactions and less wasted overlap, more files |
| `WriteOptions::sync` | per-write fsync; concurrent sync writers share fsyncs through group commit |

## How to reproduce

```sh
make bench                                   # 1M entries, writes bench/RESULTS.md
cargo run --release --bin db_bench -- --num=200000 --benchmarks=fillrandom,readrandom,readseq
cargo run --release --bin db_bench -- --num=100000 --benchmarks=fillsync_mt --threads=16
```
