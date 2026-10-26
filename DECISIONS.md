# Design decisions

Key design decisions in lsmkv, each with the reasoning and what it would cost if it turned out wrong.

## Scope

- follow the LevelDB design lineage (WAL + skiplist memtable + leveled SSTables + manifest) rather than inventing a new layout — it is the best-documented LSM design, so every choice can be checked against prior art and the code reads as a study of it — a reader expecting RocksDB features (column families, universal compaction, merge operators) will not find them.
- no block compression (the compression byte is always 0) — keeps the crate dependency-free and benchmark numbers attributable to the engine itself — tables and space amplification are larger than LevelDB-with-Snappy for compressible data; the format reserves the byte so compression can be added without a format break.
- forward iteration only (no `prev`) — reverse iteration roughly doubles the iterator code (direction switches in the merging and user iterators) and is out of scope — reverse range scans are unsupported.
- one built-in bytewise key order, no pluggable comparator — every on-disk structure depends on it, and a comparator registry adds a compatibility surface — applications needing another order must encode keys to sort bytewise.
- single process per database directory, no `LOCK` file — the crash model and tests cover one process; advisory locks need platform-specific code — two processes opening the same directory will corrupt it.
- no seek-triggered compactions (LevelDB `allowed_seeks`) — size/count triggers alone already bound read amplification for the tested workloads — read-heavy workloads over rarely-compacted ranges may probe more tables than LevelDB would.

## Durability and recovery

- the old WAL is fsynced when the memtable switches, before the new WAL takes writes (LevelDB does not do this) — otherwise a crash could keep synced writes in WAL N+1 while losing unsynced writes from WAL N's tail, a hole in history; the background-mode crash test fails without it — one extra fsync per memtable flush.
- the directory is fsynced right after a new WAL is created, before any write to it is acknowledged — an fsynced WAL whose directory entry is not durable vanishes on power loss; the background-mode crash test fails without it — one directory fsync per memtable.
- tables and compaction outputs are fsynced, then the directory is fsynced, before the manifest edit that references them — the manifest must never point at a file that may not survive — one directory fsync per flush/compaction.
- `CURRENT` is replaced by write-temp, fsync, rename, fsync-directory, and the previous manifest is deleted only after the switch commits — makes the manifest switch atomic under the pessimistic POSIX model the simulator implements — none known.
- default WAL policy is point-in-time recovery (stop at the first damaged record and discard everything after, including later WAL files); `WalRecovery::TolerateTailOnly` instead refuses to open when valid records follow the damage — point-in-time always yields a consistent prefix, which is what the crash tests assert; strict mode is for users who prefer failing loudly on media corruption — default mode can silently drop intact records that follow a corrupted one (logged at warn).
- recovery always writes replayed WAL contents to a new L0 table and starts a fresh WAL and a fresh manifest (no log reuse) — one simple recovery path that the crash tests enumerate exhaustively — slower reopen for large unflushed WALs.
- a torn final manifest record is ignored, but a damaged manifest record followed by valid ones is a hard error — the former is an edit that never committed; the latter is media corruption, and skipping an edit could resurrect deleted files — a damaged manifest needs manual repair (there is no repair tool).
- any WAL write/sync failure or background failure makes the database read-only until reopened, and obsolete-file deletion stops — after a failed fsync the on-disk state is unknown; keeping every file preserves recovery options — a transient I/O error requires a reopen.
- on macOS, `sync` is `File::sync_data` (fsync), not `F_FULLFSYNC` — portable std-only code — on macOS, data acknowledged as synced can still be lost on power failure if the drive cache is volatile; the guarantees are proven against the simulator's model, not real hardware.

## Engine behaviour

- compaction outputs are cut only between user keys, and the level invariant checks that user-key ranges of files in L1+ are disjoint — the model-based proptest (background mode) found a resurrection bug when one user key's versions straddled two output files and a later compaction moved only the newer file down a level; LevelDB solved the same bug with "boundary inputs" — a single key with very many live versions (pinned by snapshots) can produce an oversized table.
- group commit fsyncs the whole group when any member asked for `sync` (LevelDB instead keeps sync writers out of a non-sync leader's group) — simpler and never weaker — non-sync writers occasionally pay for an fsync they did not request.
- the manifest append + fsync happens while holding the state mutex (LevelDB releases it) — simpler reasoning about version installation — readers and writers briefly block during a manifest fsync (once per flush/compaction).
- one whole-table bloom filter over user keys (like RocksDB's full filter) instead of LevelDB's per-2 KiB filter blocks — one probe per table, simpler format — the filter for a huge table is loaded entirely into memory when the table opens.
- manual `compact_range` never uses trivial moves and also rewrites the deepest level into the next one — a manual compaction is how users purge tombstones and shadowed versions, so it must rewrite data — repeated full compactions push data one level deeper each time (bounded by L6).
- L0 back-pressure (1 ms delay per write at `l0_slowdown_trigger`, full stop at `l0_stop_trigger`) applies only with a background thread; inline mode simply compacts on the writing thread — inline mode exists for deterministic tests and has no concurrency to throttle — none.
- crash-point tests run with inline (same-thread) compaction so every filesystem operation index is reproducible; a separate background-mode test samples crash points and checks the same invariant — exhaustive enumeration needs determinism, while concurrency bugs need the real thread — the background test is randomized by scheduling and could miss a rare interleaving.

## Measurement

- write amplification = (WAL bytes + flushed table bytes + compaction output bytes) / user key+value bytes; "tree" WA excludes the WAL; read amplification = tables whose key range covered a point lookup, and data-block reads = that minus bloom-filter rejections; space amplification = table bytes / live key+value bytes from a full scan — these are the standard definitions and are all computed from counters in `Stats` — internal-key trailers (8 bytes/entry) and block overhead count as amplification, which makes small values look worse.
- benchmarks run on the development machine, possibly while other builds are running, and the load average is recorded next to the numbers — every reported number is measured on this machine, none are estimated — absolute numbers are noisy and lower than an idle machine would give.
