//! Record-oriented log format shared by the write-ahead log and the manifest.
//!
//! The file is a sequence of 32 KiB blocks. Each record is split into one or
//! more *fragments*, each with a 7-byte header:
//!
//! ```text
//! | masked crc32c (4) | length (2, LE) | type (1) | payload (length) |
//! ```
//!
//! `type` is FULL, FIRST, MIDDLE or LAST. The checksum covers the type byte and
//! the payload. A fragment never straddles a block boundary; a block tail
//! shorter than a header is zero-padded. Because blocks are fixed-size, a
//! reader can resynchronise at the next block after a corrupted fragment,
//! which lets recovery tell a torn tail (nothing valid after the damage) from
//! mid-log corruption (valid records after the damage).

mod reader;
mod writer;

pub use reader::{LogReader, ReadEvent};
pub use writer::LogWriter;

/// Size of a log block.
pub const BLOCK_SIZE: usize = 32 * 1024;
/// Size of a fragment header.
pub const HEADER_SIZE: usize = 7;

/// Fragment types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum RecordType {
    /// Reserved for preallocated / zeroed space.
    Zero = 0,
    Full = 1,
    First = 2,
    Middle = 3,
    Last = 4,
}

impl RecordType {
    fn from_u8(b: u8) -> Option<RecordType> {
        Some(match b {
            0 => RecordType::Zero,
            1 => RecordType::Full,
            2 => RecordType::First,
            3 => RecordType::Middle,
            4 => RecordType::Last,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{FileSystem, SimFs};
    use crate::util::rng::Rng;
    use std::path::Path;

    fn write_log(records: &[Vec<u8>]) -> Vec<u8> {
        let fs = SimFs::new();
        let path = Path::new("/log");
        let mut w = LogWriter::new(fs.create(path).unwrap());
        for r in records {
            w.add_record(r).unwrap();
        }
        w.sync().unwrap();
        assert_eq!(
            w.bytes_written(),
            fs.file_contents(path).unwrap().len() as u64
        );
        fs.file_contents(path).unwrap()
    }

    fn read_all(data: Vec<u8>) -> (Vec<Vec<u8>>, Vec<ReadEvent>) {
        let mut r = LogReader::new(data);
        let mut recs = Vec::new();
        let mut events = Vec::new();
        loop {
            match r.next_event() {
                ReadEvent::Record(rec) => recs.push(rec),
                ReadEvent::Eof => break,
                e => events.push(e),
            }
        }
        (recs, events)
    }

    fn sample_records() -> Vec<Vec<u8>> {
        let mut rng = Rng::new(1);
        let sizes = [
            0usize,
            1,
            100,
            BLOCK_SIZE - HEADER_SIZE,
            BLOCK_SIZE,
            3 * BLOCK_SIZE + 17,
            5,
        ];
        sizes
            .iter()
            .map(|&n| {
                let mut v = vec![0u8; n];
                rng.fill(&mut v);
                v
            })
            .collect()
    }

    #[test]
    fn roundtrip_across_blocks() {
        let recs = sample_records();
        let (got, events) = read_all(write_log(&recs));
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(got, recs);
    }

    #[test]
    fn block_trailer_padding() {
        // Leave exactly 6 bytes in the first block: too small for a header.
        let first = vec![1u8; BLOCK_SIZE - HEADER_SIZE - 6];
        let recs = vec![first, b"next".to_vec()];
        let data = write_log(&recs);
        assert_eq!(&data[BLOCK_SIZE - 6..BLOCK_SIZE], &[0u8; 6]);
        assert_eq!(read_all(data).0, recs);
    }

    #[test]
    fn every_truncation_yields_a_prefix() {
        let recs: Vec<Vec<u8>> = (0..20).map(|i| vec![i as u8; 1000 + i * 700]).collect();
        let data = write_log(&recs);
        for cut in (0..data.len()).step_by(97) {
            let (got, events) = read_all(data[..cut].to_vec());
            assert!(got.len() <= recs.len());
            assert_eq!(&recs[..got.len()], &got[..]);
            for e in events {
                assert!(matches!(e, ReadEvent::TruncatedTail { .. }), "{e:?}");
            }
        }
    }

    #[test]
    fn checksum_mismatch_is_reported_and_reader_resyncs() {
        let recs: Vec<Vec<u8>> = vec![vec![1; 100], vec![2; BLOCK_SIZE], vec![3; 10]];
        let mut data = write_log(&recs);
        data[HEADER_SIZE + 5] ^= 0xff; // corrupt record 0's payload
        let (got, events) = read_all(data);
        assert!(matches!(events[0], ReadEvent::Corruption { offset: 0, .. }));
        // Record 1 started in block 0 and is dropped with it; record 2 is in
        // block 1 after the tail of record 1 and survives via resync.
        assert_eq!(got.last().unwrap(), &recs[2]);
    }

    #[test]
    fn structural_corruptions() {
        // Bad type byte.
        let mut data = write_log(&[b"abc".to_vec()]);
        data[6] = 9;
        assert!(matches!(read_all(data).1[0], ReadEvent::Corruption { .. }));
        // Length pointing past the block end with more data following.
        let mut data = write_log(&[vec![0; 10], vec![0; BLOCK_SIZE]]);
        data[4] = 0xff;
        data[5] = 0xff;
        assert!(matches!(read_all(data).1[0], ReadEvent::Corruption { .. }));
        // A MIDDLE fragment with no FIRST.
        let mut w = Vec::new();
        writer::encode_fragment(&mut w, RecordType::Middle, b"orphan");
        writer::encode_fragment(&mut w, RecordType::Full, b"ok");
        let (got, events) = read_all(w);
        assert_eq!(got, vec![b"ok".to_vec()]);
        assert!(matches!(events[0], ReadEvent::Corruption { .. }));
        // FIRST followed by FULL: the partial record is dropped.
        let mut w = Vec::new();
        writer::encode_fragment(&mut w, RecordType::First, b"part");
        writer::encode_fragment(&mut w, RecordType::Full, b"ok");
        let (got, events) = read_all(w);
        assert_eq!(got, vec![b"ok".to_vec()]);
        assert_eq!(events.len(), 1);
        // Zero-filled tail (preallocated space) is skipped silently.
        let mut data = write_log(&[b"a".to_vec()]);
        data.extend_from_slice(&[0u8; 64]);
        let (got, events) = read_all(data);
        assert_eq!(got, vec![b"a".to_vec()]);
        assert!(events.is_empty());
        assert_eq!(RecordType::from_u8(0), Some(RecordType::Zero));
    }
}
