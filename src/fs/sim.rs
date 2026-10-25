//! An in-memory filesystem with a power-loss model and fault injection.
//!
//! Durability model (deliberately as pessimistic as POSIX allows):
//! * bytes appended to a file survive a crash only once `sync` was called on
//!   that file;
//! * a creation, rename or removal survives a crash only once `sync_dir` was
//!   called on the containing directory.
//!
//! Faults:
//! * [`SimFs::crash_after`] makes the N-th mutating operation (and everything
//!   after it) fail, emulating the process being killed at that instant;
//! * [`SimFs::crash`] then applies power-loss semantics, optionally tearing
//!   the unsynced tail of each file ([`CrashMode::TornWrites`]);
//! * [`SimFs::inject`] makes one future operation of a given kind fail with an
//!   I/O error while the "process" keeps running.

use super::{FileSystem, RandomAccessFile, WritableFile};
use crate::error::{Error, Result};
use crate::util::rng::Rng;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// The operation kinds that can be targeted by [`SimFs::inject`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FaultKind {
    /// File creation.
    Create,
    /// `WritableFile::append`.
    Append,
    /// `WritableFile::sync`.
    Sync,
    /// Renames.
    Rename,
    /// Removals.
    Remove,
    /// Directory syncs.
    SyncDir,
    /// Positional reads.
    Read,
}

/// What happens to unsynced data when [`SimFs::crash`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashMode {
    /// Every unsynced byte is lost; synced bytes survive intact.
    DropUnsynced,
    /// A random prefix of each file's unsynced tail survives, and the last
    /// few surviving bytes may be overwritten with garbage (a torn sector).
    TornWrites {
        /// Seed for the tearing decisions, so failures are reproducible.
        seed: u64,
    },
}

#[derive(Debug, Default)]
struct InodeData {
    data: Vec<u8>,
    synced_len: usize,
}

type Inode = Arc<Mutex<InodeData>>;

#[derive(Debug, Default)]
struct State {
    /// What a running process sees.
    live: BTreeMap<PathBuf, Inode>,
    /// What survives a power loss.
    durable: BTreeMap<PathBuf, Inode>,
    dirs: BTreeSet<PathBuf>,
    /// Count of mutating operations so far in this epoch.
    ops: u64,
    crash_at: Option<u64>,
    dead: bool,
    epoch: u64,
    injected: Vec<(FaultKind, u64)>,
    kind_counts: BTreeMap<FaultKind, u64>,
    bytes_appended: u64,
    syncs: u64,
}

/// Simulated filesystem; cheap to clone (clones share state).
#[derive(Debug, Clone, Default)]
pub struct SimFs {
    state: Arc<Mutex<State>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn crash_error() -> Error {
    Error::Io(io::Error::other("simulated crash: process is dead"))
}

impl State {
    /// Accounts for one operation of `kind`, failing it when a fault is due.
    fn check(&mut self, kind: FaultKind, epoch: u64) -> Result<()> {
        if self.dead || epoch != self.epoch {
            return Err(crash_error());
        }
        let count = self.kind_counts.entry(kind).or_insert(0);
        *count += 1;
        let nth = *count;
        if let Some(pos) = self
            .injected
            .iter()
            .position(|&(k, n)| k == kind && n == nth)
        {
            self.injected.swap_remove(pos);
            return Err(Error::Io(io::Error::other(format!(
                "injected {kind:?} failure"
            ))));
        }
        if kind == FaultKind::Read {
            return Ok(());
        }
        self.ops += 1;
        if let Some(at) = self.crash_at {
            if self.ops >= at {
                self.dead = true;
                return Err(crash_error());
            }
        }
        Ok(())
    }
}

impl SimFs {
    /// An empty filesystem.
    pub fn new() -> Self {
        SimFs::default()
    }

    /// Kill the "process" at the `n`-th mutating operation from now (1-based):
    /// that operation and all later ones fail until [`SimFs::crash`] is called.
    pub fn crash_after(&self, n: u64) {
        let mut st = lock(&self.state);
        st.crash_at = Some(st.ops + n.max(1));
    }

    /// Number of mutating operations performed in the current epoch.
    pub fn ops(&self) -> u64 {
        lock(&self.state).ops
    }

    /// True once a scheduled crash point has been reached.
    pub fn is_dead(&self) -> bool {
        lock(&self.state).dead
    }

    /// Total bytes appended to any file and total file syncs, since creation.
    pub fn io_totals(&self) -> (u64, u64) {
        let st = lock(&self.state);
        (st.bytes_appended, st.syncs)
    }

    /// Makes the `nth` future operation of `kind` (1-based, counted from now)
    /// fail once with an I/O error, without killing the process.
    pub fn inject(&self, kind: FaultKind, nth: u64) {
        let mut st = lock(&self.state);
        let base = st.kind_counts.get(&kind).copied().unwrap_or(0);
        st.injected.push((kind, base + nth.max(1)));
    }

    /// Simulates power loss + reboot: the visible state becomes the durable
    /// state, unsynced bytes are dropped or torn, all open handles are
    /// invalidated and pending faults are cleared.
    pub fn crash(&self, mode: CrashMode) {
        let mut st = lock(&self.state);
        let mut rng = match mode {
            CrashMode::TornWrites { seed } => Some(Rng::new(seed)),
            CrashMode::DropUnsynced => None,
        };
        // Files that were created/renamed but whose directory entry was never
        // synced vanish; their inodes are simply dropped with `live`.
        let mut seen: Vec<*const Mutex<InodeData>> = Vec::new();
        for inode in st.durable.values() {
            let ptr = Arc::as_ptr(inode);
            if seen.contains(&ptr) {
                continue;
            }
            seen.push(ptr);
            let mut d = lock(inode);
            let synced = d.synced_len.min(d.data.len());
            let unsynced = d.data.len() - synced;
            let keep = match rng.as_mut() {
                Some(r) if unsynced > 0 => r.below(unsynced as u64 + 1) as usize,
                _ => 0,
            };
            d.data.truncate(synced + keep);
            if keep > 0 {
                if let Some(r) = rng.as_mut() {
                    if r.one_in(2) {
                        let garbage = (r.below(16) as usize + 1).min(keep);
                        let start = d.data.len() - garbage;
                        r.fill(&mut d.data[start..]);
                    }
                }
            }
            d.synced_len = d.data.len();
        }
        st.live = st.durable.clone();
        st.dead = false;
        st.crash_at = None;
        st.injected.clear();
        st.ops = 0;
        st.kind_counts.clear();
        st.epoch += 1;
    }

    /// Returns a copy of a file's live contents (for corruption tests).
    pub fn file_contents(&self, path: &Path) -> Option<Vec<u8>> {
        let st = lock(&self.state);
        st.live.get(path).map(|i| lock(i).data.clone())
    }

    /// Overwrites the live contents of an existing file in place, marking them
    /// durable. Used by tests to plant corruption.
    pub fn overwrite_file(&self, path: &Path, data: Vec<u8>) -> bool {
        let st = lock(&self.state);
        match st.live.get(path) {
            Some(i) => {
                let mut d = lock(i);
                d.synced_len = data.len();
                d.data = data;
                true
            }
            None => false,
        }
    }
}

fn not_found(path: &Path) -> Error {
    Error::Io(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{} not found", path.display()),
    ))
}

struct SimWritable {
    fs: SimFs,
    inode: Inode,
    epoch: u64,
}

impl WritableFile for SimWritable {
    fn append(&mut self, data: &[u8]) -> Result<()> {
        let mut st = lock(&self.fs.state);
        st.check(FaultKind::Append, self.epoch)?;
        st.bytes_appended += data.len() as u64;
        lock(&self.inode).data.extend_from_slice(data);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        let st = lock(&self.fs.state);
        if st.dead || st.epoch != self.epoch {
            return Err(crash_error());
        }
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        let mut st = lock(&self.fs.state);
        st.check(FaultKind::Sync, self.epoch)?;
        st.syncs += 1;
        let mut d = lock(&self.inode);
        d.synced_len = d.data.len();
        Ok(())
    }
}

struct SimRandom {
    fs: SimFs,
    inode: Inode,
    epoch: u64,
}

impl RandomAccessFile for SimRandom {
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        lock(&self.fs.state).check(FaultKind::Read, self.epoch)?;
        let d = lock(&self.inode);
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(d.data.len());
        let end = start.saturating_add(len).min(d.data.len());
        Ok(d.data[start..end].to_vec())
    }

    fn len(&self) -> Result<u64> {
        Ok(lock(&self.inode).data.len() as u64)
    }
}

impl FileSystem for SimFs {
    fn create(&self, path: &Path) -> Result<Box<dyn WritableFile>> {
        let mut st = lock(&self.state);
        let epoch = st.epoch;
        st.check(FaultKind::Create, epoch)?;
        let inode: Inode = Arc::default();
        st.live.insert(path.to_path_buf(), inode.clone());
        Ok(Box::new(SimWritable {
            fs: self.clone(),
            inode,
            epoch,
        }))
    }

    fn open(&self, path: &Path) -> Result<Arc<dyn RandomAccessFile>> {
        let st = lock(&self.state);
        if st.dead {
            return Err(crash_error());
        }
        let inode = st.live.get(path).cloned().ok_or_else(|| not_found(path))?;
        Ok(Arc::new(SimRandom {
            fs: self.clone(),
            inode,
            epoch: st.epoch,
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let mut st = lock(&self.state);
        let epoch = st.epoch;
        st.check(FaultKind::Rename, epoch)?;
        let inode = st.live.remove(from).ok_or_else(|| not_found(from))?;
        st.live.insert(to.to_path_buf(), inode);
        Ok(())
    }

    fn remove(&self, path: &Path) -> Result<()> {
        let mut st = lock(&self.state);
        let epoch = st.epoch;
        st.check(FaultKind::Remove, epoch)?;
        st.live
            .remove(path)
            .map(|_| ())
            .ok_or_else(|| not_found(path))
    }

    fn list(&self, dir: &Path) -> Result<Vec<String>> {
        let st = lock(&self.state);
        if st.dead {
            return Err(crash_error());
        }
        Ok(st
            .live
            .keys()
            .filter(|p| p.parent() == Some(dir))
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
            .collect())
    }

    fn create_dir_all(&self, dir: &Path) -> Result<()> {
        let mut st = lock(&self.state);
        if st.dead {
            return Err(crash_error());
        }
        // Directories are modelled as always durable: the engine creates its
        // directory once, before any data exists.
        st.dirs.insert(dir.to_path_buf());
        Ok(())
    }

    fn exists(&self, path: &Path) -> bool {
        let st = lock(&self.state);
        st.live.contains_key(path) || st.dirs.contains(path)
    }

    fn sync_dir(&self, dir: &Path) -> Result<()> {
        let mut st = lock(&self.state);
        let epoch = st.epoch;
        st.check(FaultKind::SyncDir, epoch)?;
        let st = &mut *st;
        st.durable.retain(|p, _| p.parent() != Some(dir));
        for (p, inode) in &st.live {
            if p.parent() == Some(dir) {
                st.durable.insert(p.clone(), inode.clone());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from("/d").join(s)
    }

    fn write(fs: &SimFs, name: &str, data: &[u8], sync: bool) -> Box<dyn WritableFile> {
        let mut w = fs.create(&p(name)).unwrap();
        w.append(data).unwrap();
        if sync {
            w.sync().unwrap();
        }
        w
    }

    #[test]
    fn unsynced_data_and_entries_are_lost() {
        let fs = SimFs::new();
        fs.create_dir_all(Path::new("/d")).unwrap();
        let _a = write(&fs, "a", b"synced", true);
        let mut b = write(&fs, "b", b"x", true);
        fs.sync_dir(Path::new("/d")).unwrap();
        b.append(b"lost").unwrap();
        let _c = write(&fs, "c", b"never in dir", true);
        fs.crash(CrashMode::DropUnsynced);
        assert_eq!(fs.read_all(&p("a")).unwrap(), b"synced");
        assert_eq!(fs.read_all(&p("b")).unwrap(), b"x");
        assert!(!fs.exists(&p("c")));
        // Old handles are dead after the crash.
        assert!(b.append(b"zombie").is_err());
        assert!(b.flush().is_err());
    }

    #[test]
    fn rename_needs_dir_sync() {
        let fs = SimFs::new();
        let _a = write(&fs, "a", b"1", true);
        fs.sync_dir(Path::new("/d")).unwrap();
        fs.rename(&p("a"), &p("b")).unwrap();
        fs.crash(CrashMode::DropUnsynced);
        assert!(fs.exists(&p("a")));
        assert!(!fs.exists(&p("b")));
        fs.rename(&p("a"), &p("b")).unwrap();
        fs.sync_dir(Path::new("/d")).unwrap();
        fs.remove(&p("b")).unwrap();
        fs.crash(CrashMode::DropUnsynced);
        assert!(fs.exists(&p("b")), "unsynced removal is undone");
    }

    #[test]
    fn crash_after_kills_process() {
        let fs = SimFs::new();
        fs.crash_after(3);
        let mut w = fs.create(&p("a")).unwrap(); // op 1
        w.append(b"1").unwrap(); // op 2
        assert!(w.append(b"2").is_err()); // op 3 -> dead
        assert!(fs.is_dead());
        assert!(fs.create(&p("b")).is_err());
        assert!(fs.list(Path::new("/d")).is_err());
        assert!(fs.open(&p("a")).is_err());
        fs.crash(CrashMode::DropUnsynced);
        assert!(!fs.is_dead());
        assert_eq!(fs.ops(), 0);
    }

    #[test]
    fn torn_writes_keep_a_prefix() {
        let mut saw_partial = false;
        for seed in 0..50 {
            let fs = SimFs::new();
            let mut w = write(&fs, "a", b"base", true);
            fs.sync_dir(Path::new("/d")).unwrap();
            w.append(&[7u8; 100]).unwrap();
            fs.crash(CrashMode::TornWrites { seed });
            let data = fs.read_all(&p("a")).unwrap();
            assert!(data.len() >= 4 && data.len() <= 104);
            assert_eq!(&data[..4], b"base");
            if data.len() > 4 && data.len() < 104 {
                saw_partial = true;
            }
        }
        assert!(saw_partial);
    }

    #[test]
    fn injected_faults_fire_once() {
        let fs = SimFs::new();
        let mut w = fs.create(&p("a")).unwrap();
        fs.inject(FaultKind::Sync, 2);
        w.sync().unwrap();
        assert!(w.sync().is_err());
        w.sync().unwrap();
        fs.inject(FaultKind::Read, 1);
        let r = fs.open(&p("a")).unwrap();
        assert!(r.read_at(0, 1).is_err());
        assert!(r.read_at(0, 1).is_ok());
        let (bytes, syncs) = fs.io_totals();
        assert_eq!(bytes, 0);
        assert_eq!(syncs, 2);
    }

    #[test]
    fn plant_corruption() {
        let fs = SimFs::new();
        let _w = write(&fs, "a", b"abc", true);
        assert_eq!(fs.file_contents(&p("a")).unwrap(), b"abc");
        assert!(fs.overwrite_file(&p("a"), b"xyz".to_vec()));
        assert!(!fs.overwrite_file(&p("zz"), vec![]));
        assert_eq!(fs.read_all(&p("a")).unwrap(), b"xyz");
    }
}
