//! Reads records back, classifying damage as torn tail vs corruption.

use super::{RecordType, BLOCK_SIZE, HEADER_SIZE};
use crate::util::crc32c;

/// What the reader found next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadEvent {
    /// A complete, checksum-verified record.
    Record(Vec<u8>),
    /// The file ends in the middle of a fragment or record: the signature of
    /// a crash during an unsynced append. Reading stops.
    TruncatedTail {
        /// File offset where the incomplete data starts.
        offset: u64,
    },
    /// Damaged bytes (bad checksum, bad type, impossible length or broken
    /// fragment sequence). The reader skips ahead and keeps going.
    Corruption {
        /// File offset of the damaged fragment.
        offset: u64,
        /// Human-readable reason.
        reason: &'static str,
    },
    /// Clean end of the log.
    Eof,
}

enum Fragment {
    Data(RecordType, usize, usize),
    Bad(u64, &'static str),
    Truncated(u64),
    Eof,
}

/// Sequentially decodes a log held in memory.
pub struct LogReader {
    data: Vec<u8>,
    pos: usize,
    pending: Option<(u64, Vec<u8>)>,
    done: bool,
}

impl LogReader {
    /// Reads from the complete contents of a log file.
    pub fn new(data: Vec<u8>) -> Self {
        LogReader {
            data,
            pos: 0,
            pending: None,
            done: false,
        }
    }

    fn skip_to_next_block(&mut self) {
        let next = (self.pos / BLOCK_SIZE + 1) * BLOCK_SIZE;
        self.pos = next.min(self.data.len());
    }

    fn next_fragment(&mut self) -> Fragment {
        loop {
            let in_block = BLOCK_SIZE - self.pos % BLOCK_SIZE;
            if in_block < HEADER_SIZE {
                self.pos += in_block.min(self.data.len() - self.pos);
                continue;
            }
            let start = self.pos;
            let remaining = self.data.len() - start;
            if remaining == 0 {
                return Fragment::Eof;
            }
            if remaining < HEADER_SIZE {
                // A torn header; zero padding at the very end is not damage.
                self.pos = self.data.len();
                if self.data[start..].iter().all(|&b| b == 0) {
                    return Fragment::Eof;
                }
                return Fragment::Truncated(start as u64);
            }
            let h = &self.data[start..start + HEADER_SIZE];
            let stored_crc = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
            let len = usize::from(u16::from_le_bytes([h[4], h[5]]));
            let ty_byte = h[6];
            if ty_byte == RecordType::Zero as u8 && len == 0 && stored_crc == 0 {
                // Zeroed / preallocated region: skip the rest of the block.
                self.skip_to_next_block();
                continue;
            }
            if HEADER_SIZE + len > in_block {
                self.skip_to_next_block();
                return Fragment::Bad(start as u64, "fragment length crosses block boundary");
            }
            if HEADER_SIZE + len > remaining {
                self.pos = self.data.len();
                return Fragment::Truncated(start as u64);
            }
            let payload = start + HEADER_SIZE..start + HEADER_SIZE + len;
            let actual = crc32c::extend(crc32c::value(&[ty_byte]), &self.data[payload.clone()]);
            if crc32c::unmask(stored_crc) != actual {
                self.skip_to_next_block();
                return Fragment::Bad(start as u64, "checksum mismatch");
            }
            let Some(ty) = RecordType::from_u8(ty_byte).filter(|t| *t != RecordType::Zero) else {
                self.skip_to_next_block();
                return Fragment::Bad(start as u64, "unknown fragment type");
            };
            self.pos = payload.end;
            return Fragment::Data(ty, payload.start, payload.end);
        }
    }

    /// Returns the next event. After [`ReadEvent::Eof`] or
    /// [`ReadEvent::TruncatedTail`] every call returns `Eof`.
    pub fn next_event(&mut self) -> ReadEvent {
        if self.done {
            return ReadEvent::Eof;
        }
        loop {
            match self.next_fragment() {
                Fragment::Eof => {
                    self.done = true;
                    return match self.pending.take() {
                        Some((offset, _)) => ReadEvent::TruncatedTail { offset },
                        None => ReadEvent::Eof,
                    };
                }
                Fragment::Truncated(at) => {
                    self.done = true;
                    let offset = self.pending.take().map_or(at, |(o, _)| o);
                    return ReadEvent::TruncatedTail { offset };
                }
                Fragment::Bad(offset, reason) => {
                    self.pending = None;
                    return ReadEvent::Corruption { offset, reason };
                }
                Fragment::Data(ty, s, e) => {
                    let start = (s - HEADER_SIZE) as u64;
                    match ty {
                        RecordType::Full => {
                            if let Some((offset, _)) = self.pending.take() {
                                self.pos = s - HEADER_SIZE;
                                return ReadEvent::Corruption {
                                    offset,
                                    reason: "partial record without LAST fragment",
                                };
                            }
                            return ReadEvent::Record(self.data[s..e].to_vec());
                        }
                        RecordType::First => {
                            if let Some((offset, _)) = self.pending.take() {
                                self.pos = s - HEADER_SIZE;
                                return ReadEvent::Corruption {
                                    offset,
                                    reason: "partial record without LAST fragment",
                                };
                            }
                            self.pending = Some((start, self.data[s..e].to_vec()));
                        }
                        RecordType::Middle | RecordType::Last => match self.pending.as_mut() {
                            None => {
                                return ReadEvent::Corruption {
                                    offset: start,
                                    reason: "continuation fragment without FIRST",
                                }
                            }
                            Some((_, buf)) => {
                                buf.extend_from_slice(&self.data[s..e]);
                                if ty == RecordType::Last {
                                    let (_, rec) = self.pending.take().unwrap_or_default();
                                    return ReadEvent::Record(rec);
                                }
                            }
                        },
                        RecordType::Zero => unreachable!("filtered in next_fragment"),
                    }
                }
            }
        }
    }

    /// Reads all remaining events and reports whether any complete record
    /// follows; used to tell tail damage from mid-log corruption.
    pub fn any_record_follows(&mut self) -> bool {
        loop {
            match self.next_event() {
                ReadEvent::Record(_) => return true,
                ReadEvent::Eof | ReadEvent::TruncatedTail { .. } => return false,
                ReadEvent::Corruption { .. } => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::writer::encode_fragment;

    #[test]
    fn any_record_follows_distinguishes_tail_damage() {
        let mut w = Vec::new();
        encode_fragment(&mut w, RecordType::Full, b"one");
        let mut bad = w.clone();
        bad[HEADER_SIZE] ^= 1;
        let mut r = LogReader::new(bad.clone());
        assert!(matches!(r.next_event(), ReadEvent::Corruption { .. }));
        assert!(!r.any_record_follows());
        // Damage in block 0, a valid record in block 1.
        bad.resize(BLOCK_SIZE, 0);
        encode_fragment(&mut bad, RecordType::Full, b"two");
        let mut r = LogReader::new(bad);
        assert!(matches!(r.next_event(), ReadEvent::Corruption { .. }));
        assert!(r.any_record_follows());
        assert_eq!(r.next_event(), ReadEvent::Eof);
    }

    #[test]
    fn torn_header_vs_zero_padding() {
        let mut w = Vec::new();
        encode_fragment(&mut w, RecordType::Full, b"one");
        let mut torn = w.clone();
        torn.extend_from_slice(&[1, 2, 3]);
        let mut r = LogReader::new(torn);
        assert!(matches!(r.next_event(), ReadEvent::Record(_)));
        assert!(matches!(r.next_event(), ReadEvent::TruncatedTail { .. }));
        assert_eq!(r.next_event(), ReadEvent::Eof);
        let mut padded = w;
        padded.extend_from_slice(&[0, 0, 0]);
        let mut r = LogReader::new(padded);
        assert!(matches!(r.next_event(), ReadEvent::Record(_)));
        assert_eq!(r.next_event(), ReadEvent::Eof);
        // A FIRST fragment at EOF is a torn multi-fragment record.
        let mut part = Vec::new();
        encode_fragment(&mut part, RecordType::First, b"abc");
        let mut r = LogReader::new(part);
        assert_eq!(r.next_event(), ReadEvent::TruncatedTail { offset: 0 });
    }
}
