//! Names of the files inside a database directory.

use std::path::{Path, PathBuf};

/// What a file in the database directory is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    /// `NNNNNN.log`: a write-ahead log.
    Log(u64),
    /// `NNNNNN.sst`: a table.
    Table(u64),
    /// `MANIFEST-NNNNNN`: a metadata log.
    Manifest(u64),
    /// `CURRENT`: names the live manifest.
    Current,
    /// `NNNNNN.tmp`: a temporary file (CURRENT replacement in progress).
    Temp(u64),
}

/// Path of WAL `n`.
pub fn log_file(dir: &Path, n: u64) -> PathBuf {
    dir.join(format!("{n:06}.log"))
}

/// Path of table `n`.
pub fn table_file(dir: &Path, n: u64) -> PathBuf {
    dir.join(format!("{n:06}.sst"))
}

/// Path of manifest `n`.
pub fn manifest_file(dir: &Path, n: u64) -> PathBuf {
    dir.join(manifest_name(n))
}

/// File name (no directory) of manifest `n`.
pub fn manifest_name(n: u64) -> String {
    format!("MANIFEST-{n:06}")
}

/// Path of the CURRENT pointer file.
pub fn current_file(dir: &Path) -> PathBuf {
    dir.join("CURRENT")
}

/// Path of temporary file `n`.
pub fn temp_file(dir: &Path, n: u64) -> PathBuf {
    dir.join(format!("{n:06}.tmp"))
}

/// Classifies a file name; `None` for files the engine does not own.
pub fn parse(name: &str) -> Option<FileType> {
    if name == "CURRENT" {
        return Some(FileType::Current);
    }
    if let Some(num) = name.strip_prefix("MANIFEST-") {
        return parse_number(num).map(FileType::Manifest);
    }
    let (num, ext) = name.split_once('.')?;
    let n = parse_number(num)?;
    match ext {
        "log" => Some(FileType::Log(n)),
        "sst" => Some(FileType::Table(n)),
        "tmp" => Some(FileType::Temp(n)),
        _ => None,
    }
}

fn parse_number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_roundtrip() {
        let d = Path::new("/db");
        let cases = [
            (log_file(d, 7), FileType::Log(7)),
            (table_file(d, 123_456_789), FileType::Table(123_456_789)),
            (manifest_file(d, 3), FileType::Manifest(3)),
            (current_file(d), FileType::Current),
            (temp_file(d, 9), FileType::Temp(9)),
        ];
        for (path, ty) in cases {
            let name = path.file_name().unwrap().to_str().unwrap();
            assert_eq!(parse(name), Some(ty), "{name}");
        }
        for bad in [
            "",
            "LOCK",
            "abc.log",
            "12.txt",
            "MANIFEST-",
            "MANIFEST-x1",
            ".sst",
            "1.2.sst",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }
}
