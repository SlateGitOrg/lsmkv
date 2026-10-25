//! The version set: the current version, the manifest log that makes it
//! durable, and the counters recovered from it.
//!
//! Crash safety of metadata rests on two rules:
//! 1. an edit is durable once its manifest record is fsynced; a torn final
//!    record is an edit that never committed, so recovery ignores it;
//! 2. switching to a new manifest writes and fsyncs the whole new manifest
//!    first, then atomically replaces `CURRENT` (write temp, fsync, rename,
//!    fsync directory). Until that rename is durable the old manifest, which
//!    is never deleted before the switch, remains authoritative.

use super::{check_invariants, TableCache, Version, VersionBuilder, VersionEdit};
use crate::error::{Error, Result};
use crate::filename;
use crate::fs::FileSystem;
use crate::options::{Options, NUM_LEVELS};
use crate::wal::{LogReader, LogWriter, ReadEvent};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Weak};

/// Owner of the version history; protected by the database mutex.
pub struct VersionSet {
    fs: Arc<dyn FileSystem>,
    dir: PathBuf,
    opts: Options,
    table_cache: Arc<TableCache>,
    current: Arc<Version>,
    live: Vec<Weak<Version>>,
    next_file_number: u64,
    manifest_number: u64,
    last_sequence: u64,
    log_number: u64,
    compact_pointers: Vec<Vec<u8>>,
    manifest: Option<LogWriter>,
}

impl VersionSet {
    /// An empty version set for a new database.
    pub fn new(
        fs: Arc<dyn FileSystem>,
        dir: PathBuf,
        opts: Options,
        table_cache: Arc<TableCache>,
    ) -> Self {
        VersionSet {
            fs,
            dir,
            opts,
            table_cache,
            current: Arc::new(Version::empty()),
            live: Vec::new(),
            next_file_number: 1,
            manifest_number: 0,
            last_sequence: 0,
            log_number: 0,
            compact_pointers: vec![Vec::new(); NUM_LEVELS],
            manifest: None,
        }
    }

    /// Rebuilds state from the manifest named by `CURRENT`.
    pub fn recover(&mut self) -> Result<()> {
        let current = self.fs.read_all(&filename::current_file(&self.dir))?;
        let name = std::str::from_utf8(&current)
            .ok()
            .and_then(|s| s.strip_suffix('\n'))
            .ok_or_else(|| Error::corruption("CURRENT file is malformed"))?;
        let Some(filename::FileType::Manifest(manifest_number)) = filename::parse(name) else {
            return Err(Error::corruption(format!(
                "CURRENT names a non-manifest {name:?}"
            )));
        };
        let data = self.fs.read_all(&self.dir.join(name))?;
        let mut reader = LogReader::new(data);
        let mut builder = VersionBuilder::new(&Version::empty());
        let (mut next_file, mut last_seq, mut log_number) = (None, None, None);
        let mut edits = 0usize;
        loop {
            match reader.next_event() {
                ReadEvent::Record(rec) => {
                    let edit = VersionEdit::decode(&rec)?;
                    builder.apply(&edit);
                    next_file = edit.next_file_number.or(next_file);
                    last_seq = edit.last_sequence.or(last_seq);
                    log_number = edit.log_number.or(log_number);
                    for (level, key) in edit.compact_pointers {
                        self.compact_pointers[level] = key;
                    }
                    edits += 1;
                }
                ReadEvent::Eof => break,
                ReadEvent::TruncatedTail { offset } => {
                    log::warn!("manifest {name}: ignoring torn final record at offset {offset}");
                    break;
                }
                ReadEvent::Corruption { offset, reason } => {
                    if reader.any_record_follows() {
                        return Err(Error::corruption(format!(
                            "manifest {name} corrupted at offset {offset}: {reason}"
                        )));
                    }
                    log::warn!(
                        "manifest {name}: ignoring damaged tail at offset {offset} ({reason})"
                    );
                    break;
                }
            }
        }
        let (Some(next_file), Some(last_seq)) = (next_file, last_seq) else {
            return Err(Error::corruption(format!(
                "manifest {name} lacks next-file or last-sequence entries"
            )));
        };
        let version = builder.finish(&self.opts);
        check_invariants(&version)?;
        self.next_file_number = next_file.max(manifest_number + 1);
        self.last_sequence = last_seq;
        self.log_number = log_number.unwrap_or(0);
        self.manifest_number = manifest_number;
        self.install(version);
        log::info!(
            "recovered manifest {name}: {edits} edits, next_file={}, last_seq={}, log={}",
            self.next_file_number,
            self.last_sequence,
            self.log_number
        );
        Ok(())
    }

    fn install(&mut self, v: Version) {
        let v = Arc::new(v);
        self.live.push(Arc::downgrade(&v));
        self.current = v;
    }

    /// The current version.
    pub fn current(&self) -> Arc<Version> {
        self.current.clone()
    }

    /// The shared table cache.
    pub fn table_cache(&self) -> &Arc<TableCache> {
        &self.table_cache
    }

    /// Allocates a fresh file number.
    pub fn new_file_number(&mut self) -> u64 {
        let n = self.next_file_number;
        self.next_file_number += 1;
        n
    }

    /// Ensures `n` is never handed out again (used for WALs found on disk).
    pub fn mark_file_number_used(&mut self, n: u64) {
        if self.next_file_number <= n {
            self.next_file_number = n + 1;
        }
    }

    /// Largest sequence number assigned.
    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    /// Records that sequence numbers up to `seq` are in use.
    pub fn set_last_sequence(&mut self, seq: u64) {
        debug_assert!(seq >= self.last_sequence);
        self.last_sequence = seq;
    }

    /// WALs numbered below this are obsolete.
    pub fn log_number(&self) -> u64 {
        self.log_number
    }

    /// Number of the manifest currently being appended to.
    pub fn manifest_number(&self) -> u64 {
        self.manifest_number
    }

    /// Round-robin compaction cursor of `level` (empty = start of level).
    pub fn compact_pointer(&self, level: usize) -> &[u8] {
        &self.compact_pointers[level]
    }

    fn snapshot_edit(&self) -> VersionEdit {
        let mut e = VersionEdit::default();
        for (level, key) in self.compact_pointers.iter().enumerate() {
            if !key.is_empty() {
                e.compact_pointers.push((level, key.clone()));
            }
        }
        for (level, files) in self.current.files.iter().enumerate() {
            for f in files {
                e.add_file(level, (**f).clone());
            }
        }
        e
    }

    fn set_current_file(&self, manifest_number: u64) -> Result<()> {
        let tmp = filename::temp_file(&self.dir, manifest_number);
        let mut f = self.fs.create(&tmp)?;
        f.append(format!("{}\n", filename::manifest_name(manifest_number)).as_bytes())?;
        f.sync()?;
        drop(f);
        self.fs.rename(&tmp, &filename::current_file(&self.dir))?;
        self.fs.sync_dir(&self.dir)
    }

    /// Durably records `edit` in the manifest and installs the resulting
    /// version. On error nothing is installed and the next call starts a
    /// fresh manifest.
    pub fn log_and_apply(&mut self, mut edit: VersionEdit) -> Result<()> {
        let log_number = edit.log_number.unwrap_or(self.log_number);
        debug_assert!(log_number >= self.log_number);
        edit.log_number = Some(log_number);
        for (level, key) in &edit.compact_pointers {
            self.compact_pointers[*level] = key.clone();
        }
        let mut builder = VersionBuilder::new(&self.current);
        builder.apply(&edit);
        let version = builder.finish(&self.opts);
        check_invariants(&version)?;

        let mut new_manifest = None;
        if self.manifest.is_none() {
            new_manifest = Some(self.new_file_number());
        }
        edit.next_file_number = Some(self.next_file_number);
        edit.last_sequence = Some(self.last_sequence);

        let result = (|| -> Result<()> {
            if let Some(n) = new_manifest {
                let file = self.fs.create(&filename::manifest_file(&self.dir, n))?;
                let mut w = LogWriter::new(file);
                w.add_record(&self.snapshot_edit().encode())?;
                self.manifest = Some(w);
            }
            let w = self.manifest.as_mut().expect("manifest writer exists");
            w.add_record(&edit.encode())?;
            w.sync()?;
            if let Some(n) = new_manifest {
                self.set_current_file(n)?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            // The manifest's state is unknown; never append to it again.
            self.manifest = None;
            log::error!("manifest write failed: {e}");
            return Err(e);
        }
        if let Some(n) = new_manifest {
            self.manifest_number = n;
        }
        self.log_number = log_number;
        self.install(version);
        if self
            .manifest
            .as_ref()
            .is_some_and(|w| w.bytes_written() > self.opts.max_manifest_bytes)
        {
            self.manifest = None; // roll over on the next edit
        }
        Ok(())
    }

    /// File numbers of tables referenced by any version still in use.
    pub fn live_files(&mut self) -> BTreeSet<u64> {
        self.live.retain(|w| w.strong_count() > 0);
        let mut out = BTreeSet::new();
        for v in self.live.iter().filter_map(Weak::upgrade) {
            for files in &v.files {
                out.extend(files.iter().map(|f| f.number));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::SimFs;
    use crate::key::{internal_key, ValueKind};
    use crate::stats::Stats;
    use crate::version::FileMetaData;
    use std::path::Path;

    fn new_set(fs: &SimFs) -> VersionSet {
        let fs: Arc<dyn FileSystem> = Arc::new(fs.clone());
        let opts = Options::for_testing();
        let dir = PathBuf::from("/db");
        fs.create_dir_all(&dir).unwrap();
        let tc = Arc::new(TableCache::new(
            fs.clone(),
            dir.clone(),
            &opts,
            Arc::new(Stats::default()),
        ));
        VersionSet::new(fs, dir, opts, tc)
    }

    fn meta(n: u64, lo: &str, hi: &str) -> FileMetaData {
        FileMetaData {
            number: n,
            size: 100,
            smallest: internal_key(lo.as_bytes(), 1, ValueKind::Value),
            largest: internal_key(hi.as_bytes(), 1, ValueKind::Value),
        }
    }

    #[test]
    fn log_and_apply_then_recover() {
        let fs = SimFs::new();
        let mut vs = new_set(&fs);
        let n = vs.new_file_number();
        let mut e = VersionEdit::default();
        e.add_file(1, meta(n, "a", "c"));
        e.compact_pointers
            .push((1, internal_key(b"b", 1, ValueKind::Value)));
        vs.set_last_sequence(42);
        vs.log_and_apply(e).unwrap();
        let mut e = VersionEdit::default();
        let m = vs.new_file_number();
        e.add_file(1, meta(m, "d", "f"));
        e.log_number = Some(7);
        vs.log_and_apply(e).unwrap();
        assert_eq!(vs.current().num_files(1), 2);
        let held = vs.current();
        let mut e = VersionEdit::default();
        e.delete_file(1, n);
        vs.log_and_apply(e).unwrap();
        // `held` still pins file n.
        assert!(vs.live_files().contains(&n));
        drop(held);
        assert!(!vs.live_files().contains(&n));

        fs.crash(crate::fs::CrashMode::DropUnsynced);
        let mut vs2 = new_set(&fs);
        vs2.recover().unwrap();
        assert_eq!(vs2.last_sequence(), 42);
        assert_eq!(vs2.log_number(), 7);
        assert_eq!(vs2.current().num_files(1), 1);
        assert_eq!(vs2.current().files[1][0].number, m);
        assert!(vs2.new_file_number() > m);
        assert_eq!(
            vs2.compact_pointer(1),
            internal_key(b"b", 1, ValueKind::Value)
        );
        assert_eq!(vs2.manifest_number(), vs.manifest_number());
    }

    #[test]
    fn overlapping_levels_are_rejected() {
        let fs = SimFs::new();
        let mut vs = new_set(&fs);
        let mut e = VersionEdit::default();
        e.add_file(2, meta(1, "a", "m"));
        e.add_file(2, meta(2, "k", "z"));
        assert!(vs.log_and_apply(e).unwrap_err().is_corruption());
    }

    #[test]
    fn bad_current_and_manifest() {
        let fs = SimFs::new();
        let mut vs = new_set(&fs);
        assert!(vs.recover().is_err(), "no CURRENT");
        let cur = Path::new("/db/CURRENT");
        let mut w = fs.create(cur).unwrap();
        w.append(b"garbage").unwrap();
        drop(w);
        assert!(vs.recover().unwrap_err().is_corruption());
        let mut w = fs.create(cur).unwrap();
        w.append(b"000001.log\n").unwrap();
        drop(w);
        assert!(vs.recover().unwrap_err().is_corruption());
        // A manifest with an edit missing required fields.
        let mut w = fs.create(cur).unwrap();
        w.append(b"MANIFEST-000009\n").unwrap();
        drop(w);
        let mut lw = LogWriter::new(fs.create(Path::new("/db/MANIFEST-000009")).unwrap());
        lw.add_record(&VersionEdit::default().encode()).unwrap();
        assert!(vs.recover().unwrap_err().is_corruption());
    }

    #[test]
    fn manifest_rollover_and_mid_corruption() {
        let fs = SimFs::new();
        let mut vs = new_set(&fs);
        vs.opts.max_manifest_bytes = 64;
        for i in 0..5 {
            let mut e = VersionEdit::default();
            let n = vs.new_file_number();
            e.add_file(0, meta(n, &format!("k{i}"), &format!("k{i}")));
            vs.log_and_apply(e).unwrap();
        }
        let first = vs.manifest_number();
        let mut e = VersionEdit::default();
        let n = vs.new_file_number();
        e.add_file(0, meta(n, "x", "y"));
        vs.log_and_apply(e).unwrap();
        assert!(vs.manifest_number() > first, "rolled over");
        let mut vs2 = new_set(&fs);
        vs2.recover().unwrap();
        assert_eq!(vs2.current().num_files(0), 6);

        // Corrupt the first record of the live manifest while later records
        // are intact: recovery must refuse rather than silently drop edits.
        let mut e = VersionEdit::default();
        e.delete_file(0, n);
        let mut vs3 = new_set(&fs);
        vs3.recover().unwrap();
        vs3.opts.max_manifest_bytes = 1 << 20;
        vs3.log_and_apply(e).unwrap();
        for i in 0..3 {
            let mut e = VersionEdit::default();
            let n = vs3.new_file_number();
            e.add_file(0, meta(n, &format!("p{i}"), &format!("p{i}")));
            // Pad records so they land in a later log block.
            e.compact_pointers.push((3, vec![b'z'; 20_000]));
            vs3.log_and_apply(e).unwrap();
        }
        let path = filename::manifest_file(Path::new("/db"), vs3.manifest_number());
        let mut data = fs.file_contents(&path).unwrap();
        data[10] ^= 0xff;
        fs.overwrite_file(&path, data);
        let mut vs4 = new_set(&fs);
        assert!(vs4.recover().unwrap_err().is_corruption());
    }

    #[test]
    fn failed_manifest_write_installs_nothing() {
        let fs = SimFs::new();
        let mut vs = new_set(&fs);
        vs.log_and_apply(VersionEdit::default()).unwrap();
        fs.inject(crate::fs::FaultKind::Sync, 1);
        let mut e = VersionEdit::default();
        e.add_file(0, meta(50, "a", "b"));
        assert!(vs.log_and_apply(e).is_err());
        assert_eq!(vs.current().num_files(0), 0);
        // Next edit goes to a brand new manifest.
        let before = vs.manifest_number();
        vs.log_and_apply(VersionEdit::default()).unwrap();
        assert!(vs.manifest_number() > before);
    }
}
