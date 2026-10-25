//! Version edits: the records of the manifest log.
//!
//! Each edit is a list of tagged fields. Replaying every edit of a manifest in
//! order reconstructs the set of live tables per level plus the counters
//! needed to resume (next file number, last sequence, live WAL number).

use super::FileMetaData;
use crate::error::{Error, Result};
use crate::options::NUM_LEVELS;
use crate::util::coding::{put_length_prefixed, put_varint64, Decoder};
use std::collections::BTreeSet;

const TAG_LOG_NUMBER: u64 = 1;
const TAG_NEXT_FILE: u64 = 2;
const TAG_LAST_SEQUENCE: u64 = 3;
const TAG_COMPACT_POINTER: u64 = 4;
const TAG_DELETED_FILE: u64 = 5;
const TAG_NEW_FILE: u64 = 6;

/// A delta applied to a version.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionEdit {
    /// WALs numbered below this are no longer needed.
    pub log_number: Option<u64>,
    /// Next unused file number.
    pub next_file_number: Option<u64>,
    /// Largest sequence number used.
    pub last_sequence: Option<u64>,
    /// Round-robin compaction cursors, per level.
    pub compact_pointers: Vec<(usize, Vec<u8>)>,
    /// `(level, file number)` pairs removed.
    pub deleted_files: BTreeSet<(usize, u64)>,
    /// Files added, with their level.
    pub new_files: Vec<(usize, FileMetaData)>,
}

fn level_from(v: u64) -> Result<usize> {
    usize::try_from(v)
        .ok()
        .filter(|&l| l < NUM_LEVELS)
        .ok_or_else(|| Error::corruption(format!("version edit has invalid level {v}")))
}

impl VersionEdit {
    /// Records the removal of a file.
    pub fn delete_file(&mut self, level: usize, number: u64) {
        self.deleted_files.insert((level, number));
    }

    /// Records the addition of a file.
    pub fn add_file(&mut self, level: usize, f: FileMetaData) {
        self.new_files.push((level, f));
    }

    /// Serialises the edit.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(n) = self.log_number {
            put_varint64(&mut out, TAG_LOG_NUMBER);
            put_varint64(&mut out, n);
        }
        if let Some(n) = self.next_file_number {
            put_varint64(&mut out, TAG_NEXT_FILE);
            put_varint64(&mut out, n);
        }
        if let Some(n) = self.last_sequence {
            put_varint64(&mut out, TAG_LAST_SEQUENCE);
            put_varint64(&mut out, n);
        }
        for (level, key) in &self.compact_pointers {
            put_varint64(&mut out, TAG_COMPACT_POINTER);
            put_varint64(&mut out, *level as u64);
            put_length_prefixed(&mut out, key);
        }
        for &(level, number) in &self.deleted_files {
            put_varint64(&mut out, TAG_DELETED_FILE);
            put_varint64(&mut out, level as u64);
            put_varint64(&mut out, number);
        }
        for (level, f) in &self.new_files {
            put_varint64(&mut out, TAG_NEW_FILE);
            put_varint64(&mut out, *level as u64);
            put_varint64(&mut out, f.number);
            put_varint64(&mut out, f.size);
            put_length_prefixed(&mut out, &f.smallest);
            put_length_prefixed(&mut out, &f.largest);
        }
        out
    }

    /// Parses an encoded edit.
    pub fn decode(buf: &[u8]) -> Result<VersionEdit> {
        let bad = || Error::corruption("malformed version edit");
        let mut d = Decoder::new(buf);
        let mut e = VersionEdit::default();
        while !d.is_empty() {
            match d.varint64().ok_or_else(bad)? {
                TAG_LOG_NUMBER => e.log_number = Some(d.varint64().ok_or_else(bad)?),
                TAG_NEXT_FILE => e.next_file_number = Some(d.varint64().ok_or_else(bad)?),
                TAG_LAST_SEQUENCE => e.last_sequence = Some(d.varint64().ok_or_else(bad)?),
                TAG_COMPACT_POINTER => {
                    let level = level_from(d.varint64().ok_or_else(bad)?)?;
                    let key = d.length_prefixed().ok_or_else(bad)?.to_vec();
                    e.compact_pointers.push((level, key));
                }
                TAG_DELETED_FILE => {
                    let level = level_from(d.varint64().ok_or_else(bad)?)?;
                    let number = d.varint64().ok_or_else(bad)?;
                    e.deleted_files.insert((level, number));
                }
                TAG_NEW_FILE => {
                    let level = level_from(d.varint64().ok_or_else(bad)?)?;
                    let number = d.varint64().ok_or_else(bad)?;
                    let size = d.varint64().ok_or_else(bad)?;
                    let smallest = d.length_prefixed().ok_or_else(bad)?.to_vec();
                    let largest = d.length_prefixed().ok_or_else(bad)?.to_vec();
                    e.new_files.push((
                        level,
                        FileMetaData {
                            number,
                            size,
                            smallest,
                            largest,
                        },
                    ));
                }
                tag => return Err(Error::corruption(format!("unknown version edit tag {tag}"))),
            }
        }
        Ok(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{internal_key, ValueKind};

    #[test]
    fn roundtrip() {
        let mut e = VersionEdit {
            log_number: Some(4),
            next_file_number: Some(10),
            last_sequence: Some(1 << 40),
            ..VersionEdit::default()
        };
        e.compact_pointers
            .push((2, internal_key(b"m", 5, ValueKind::Value)));
        e.delete_file(1, 3);
        e.add_file(
            6,
            FileMetaData {
                number: 9,
                size: 4096,
                smallest: internal_key(b"a", 1, ValueKind::Value),
                largest: internal_key(b"z", 2, ValueKind::Deletion),
            },
        );
        let enc = e.encode();
        assert_eq!(VersionEdit::decode(&enc).unwrap(), e);
        assert_eq!(VersionEdit::decode(&[]).unwrap(), VersionEdit::default());
    }

    #[test]
    fn rejects_garbage() {
        assert!(VersionEdit::decode(&[99, 1]).is_err());
        assert!(VersionEdit::decode(&[TAG_LOG_NUMBER as u8]).is_err());
        assert!(VersionEdit::decode(&[TAG_DELETED_FILE as u8, 50, 1]).is_err());
        let mut e = VersionEdit::default();
        e.add_file(0, FileMetaData::default());
        let enc = e.encode();
        assert!(VersionEdit::decode(&enc[..enc.len() - 1]).is_err());
    }
}
