//! Filesystem abstraction.
//!
//! The engine never touches `std::fs` directly; every byte goes through a
//! [`FileSystem`]. Production uses [`OsFs`]; tests use [`SimFs`], an in-memory
//! filesystem that models what survives a power loss (only fsynced bytes, only
//! directory entries made durable by a directory fsync) and can inject crashes,
//! torn writes and I/O errors.

mod os;
mod sim;

pub use os::OsFs;
pub use sim::{CrashMode, FaultKind, SimFs};

use crate::error::Result;
use std::fmt::Debug;
use std::path::Path;
use std::sync::Arc;

/// A file opened for appending.
pub trait WritableFile: Send {
    /// Appends `data` at the end of the file (possibly buffered in memory).
    fn append(&mut self, data: &[u8]) -> Result<()>;
    /// Pushes buffered bytes to the operating system (not durable yet).
    fn flush(&mut self) -> Result<()>;
    /// Makes every appended byte durable (`fdatasync`).
    fn sync(&mut self) -> Result<()>;
}

/// A file opened for positional reads. Shared between threads.
pub trait RandomAccessFile: Send + Sync {
    /// Reads up to `len` bytes starting at `offset`. Returns fewer bytes only
    /// at end of file.
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>>;
    /// Current file length in bytes.
    fn size(&self) -> Result<u64>;
}

/// The set of operations the engine needs from a filesystem.
pub trait FileSystem: Send + Sync + Debug {
    /// Creates (or truncates) `path` for writing.
    fn create(&self, path: &Path) -> Result<Box<dyn WritableFile>>;
    /// Opens `path` for reading.
    fn open(&self, path: &Path) -> Result<Arc<dyn RandomAccessFile>>;
    /// Atomically renames `from` to `to`, replacing `to` if present.
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;
    /// Removes a file.
    fn remove(&self, path: &Path) -> Result<()>;
    /// Lists the file names (not paths) directly inside `dir`.
    fn list(&self, dir: &Path) -> Result<Vec<String>>;
    /// Creates `dir` and its parents.
    fn create_dir_all(&self, dir: &Path) -> Result<()>;
    /// True if `path` exists.
    fn exists(&self, path: &Path) -> bool;
    /// Makes creations, renames and removals inside `dir` durable.
    fn sync_dir(&self, dir: &Path) -> Result<()>;

    /// Reads a whole file into memory.
    fn read_all(&self, path: &Path) -> Result<Vec<u8>> {
        let f = self.open(path)?;
        let len = usize::try_from(f.size()?)
            .map_err(|_| crate::Error::invalid("file too large to read into memory"))?;
        f.read_at(0, len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn exercise(fs: &dyn FileSystem, dir: &Path) {
        fs.create_dir_all(dir).unwrap();
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        let mut w = fs.create(&a).unwrap();
        w.append(b"hello ").unwrap();
        w.append(b"world").unwrap();
        w.flush().unwrap();
        w.sync().unwrap();
        drop(w);
        assert!(fs.exists(&a));
        assert_eq!(fs.read_all(&a).unwrap(), b"hello world");
        let r = fs.open(&a).unwrap();
        assert_eq!(r.size().unwrap(), 11);
        assert_eq!(r.read_at(6, 100).unwrap(), b"world");
        assert_eq!(r.read_at(50, 4).unwrap(), b"");
        fs.rename(&a, &b).unwrap();
        fs.sync_dir(dir).unwrap();
        assert!(!fs.exists(&a));
        let mut names = fs.list(dir).unwrap();
        names.sort();
        assert_eq!(names, vec!["b.txt".to_string()]);
        fs.remove(&b).unwrap();
        assert!(!fs.exists(&b));
        assert!(fs.open(&b).is_err());
        assert!(fs.remove(&b).is_err());
    }

    #[test]
    fn os_fs_contract() {
        let tmp = tempfile::tempdir().unwrap();
        exercise(&OsFs, &tmp.path().join("sub"));
    }

    #[test]
    fn sim_fs_contract() {
        exercise(&SimFs::new(), &PathBuf::from("/db"));
    }
}
