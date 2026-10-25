//! [`FileSystem`] backed by the real operating system.

use super::{FileSystem, RandomAccessFile, WritableFile};
use crate::error::Result;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

/// The operating-system filesystem.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsFs;

struct OsWritable {
    inner: BufWriter<File>,
}

impl WritableFile for OsWritable {
    fn append(&mut self, data: &[u8]) -> Result<()> {
        self.inner.write_all(data)?;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.inner.flush()?;
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        self.inner.flush()?;
        self.inner.get_ref().sync_data()?;
        Ok(())
    }
}

struct OsRandom {
    file: File,
}

impl RandomAccessFile for OsRandom {
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        let mut buf = vec![0u8; len];
        let mut filled = 0;
        while filled < len {
            let n = self.file.read_at(&mut buf[filled..], offset + filled as u64)?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        buf.truncate(filled);
        Ok(buf)
    }

    fn len(&self) -> Result<u64> {
        Ok(self.file.metadata()?.len())
    }
}

impl FileSystem for OsFs {
    fn create(&self, path: &Path) -> Result<Box<dyn WritableFile>> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(Box::new(OsWritable {
            inner: BufWriter::with_capacity(64 * 1024, file),
        }))
    }

    fn open(&self, path: &Path) -> Result<Arc<dyn RandomAccessFile>> {
        Ok(Arc::new(OsRandom {
            file: File::open(path)?,
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        fs::rename(from, to)?;
        Ok(())
    }

    fn remove(&self, path: &Path) -> Result<()> {
        fs::remove_file(path)?;
        Ok(())
    }

    fn list(&self, dir: &Path) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str() {
                out.push(name.to_string());
            }
        }
        Ok(out)
    }

    fn create_dir_all(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir)?;
        Ok(())
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn sync_dir(&self, dir: &Path) -> Result<()> {
        File::open(dir)?.sync_all()?;
        Ok(())
    }
}
