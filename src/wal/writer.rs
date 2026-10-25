//! Appends records in the block format described in [`super`].

use super::{RecordType, BLOCK_SIZE, HEADER_SIZE};
use crate::error::Result;
use crate::fs::WritableFile;
use crate::util::crc32c;

/// Appends checksummed records to a log file.
pub struct LogWriter {
    file: Box<dyn WritableFile>,
    block_offset: usize,
    bytes_written: u64,
    scratch: Vec<u8>,
}

pub(crate) fn encode_fragment(dst: &mut Vec<u8>, ty: RecordType, payload: &[u8]) {
    let crc = crc32c::extend(crc32c::value(&[ty as u8]), payload);
    dst.extend_from_slice(&crc32c::mask(crc).to_le_bytes());
    dst.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    dst.push(ty as u8);
    dst.extend_from_slice(payload);
}

impl LogWriter {
    /// Wraps a freshly created (empty) file.
    pub fn new(file: Box<dyn WritableFile>) -> Self {
        LogWriter {
            file,
            block_offset: 0,
            bytes_written: 0,
            scratch: Vec::with_capacity(BLOCK_SIZE),
        }
    }

    /// Appends one record, fragmenting it across blocks as needed. The bytes
    /// reach the OS but are not durable until [`LogWriter::sync`].
    pub fn add_record(&mut self, mut data: &[u8]) -> Result<()> {
        self.scratch.clear();
        let mut begin = true;
        loop {
            let leftover = BLOCK_SIZE - self.block_offset;
            if leftover < HEADER_SIZE {
                self.scratch.resize(self.scratch.len() + leftover, 0);
                self.block_offset = 0;
            }
            let avail = BLOCK_SIZE - self.block_offset - HEADER_SIZE;
            let n = data.len().min(avail);
            let end = n == data.len();
            let ty = match (begin, end) {
                (true, true) => RecordType::Full,
                (true, false) => RecordType::First,
                (false, true) => RecordType::Last,
                (false, false) => RecordType::Middle,
            };
            encode_fragment(&mut self.scratch, ty, &data[..n]);
            self.block_offset += HEADER_SIZE + n;
            data = &data[n..];
            begin = false;
            if end {
                break;
            }
        }
        self.file.append(&self.scratch)?;
        self.file.flush()?;
        self.bytes_written += self.scratch.len() as u64;
        Ok(())
    }

    /// Makes all appended records durable.
    pub fn sync(&mut self) -> Result<()> {
        self.file.sync()
    }

    /// Total bytes appended so far (including headers and padding).
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}
