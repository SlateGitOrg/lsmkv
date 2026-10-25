//! Block handles, the footer, and checksummed block I/O.

use crate::error::{Error, Result};
use crate::fs::RandomAccessFile;
use crate::util::coding::{put_varint64, Decoder};
use crate::util::crc32c;

/// Bytes after each block: compression type + masked CRC.
pub const BLOCK_TRAILER_SIZE: usize = 5;
/// Fixed footer size.
pub const FOOTER_SIZE: usize = 48;
/// Footer magic ("lsmkv sst v1").
pub const TABLE_MAGIC: u64 = 0x6c73_6d6b_7673_7431;
/// The only compression type: none.
pub const NO_COMPRESSION: u8 = 0;

/// Location of a block within a table file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockHandle {
    /// Offset of the block's first byte.
    pub offset: u64,
    /// Size of the block contents, excluding the trailer.
    pub size: u64,
}

impl BlockHandle {
    /// Appends the varint encoding.
    pub fn encode_to(&self, dst: &mut Vec<u8>) {
        put_varint64(dst, self.offset);
        put_varint64(dst, self.size);
    }

    /// Encodes into a fresh vector.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(20);
        self.encode_to(&mut v);
        v
    }

    /// Decodes from the front of a decoder.
    pub fn decode_from(d: &mut Decoder<'_>) -> Result<BlockHandle> {
        match (d.varint64(), d.varint64()) {
            (Some(offset), Some(size)) => Ok(BlockHandle { offset, size }),
            _ => Err(Error::corruption("bad block handle")),
        }
    }

    /// Decodes a buffer that holds exactly one handle.
    pub fn decode(buf: &[u8]) -> Result<BlockHandle> {
        BlockHandle::decode_from(&mut Decoder::new(buf))
    }
}

/// The fixed-size table footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footer {
    /// Handle of the metaindex block.
    pub metaindex: BlockHandle,
    /// Handle of the index block.
    pub index: BlockHandle,
}

impl Footer {
    /// Encodes to exactly [`FOOTER_SIZE`] bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(FOOTER_SIZE);
        self.metaindex.encode_to(&mut v);
        self.index.encode_to(&mut v);
        v.resize(FOOTER_SIZE - 8, 0);
        v.extend_from_slice(&TABLE_MAGIC.to_le_bytes());
        v
    }

    /// Decodes and validates the magic number.
    pub fn decode(buf: &[u8]) -> Result<Footer> {
        if buf.len() != FOOTER_SIZE {
            return Err(Error::corruption("footer has wrong size"));
        }
        let mut d = Decoder::new(&buf[FOOTER_SIZE - 8..]);
        if d.fixed64() != Some(TABLE_MAGIC) {
            return Err(Error::corruption("not an sstable (bad magic number)"));
        }
        let mut d = Decoder::new(&buf[..FOOTER_SIZE - 8]);
        Ok(Footer {
            metaindex: BlockHandle::decode_from(&mut d)?,
            index: BlockHandle::decode_from(&mut d)?,
        })
    }
}

/// Computes the trailer for `contents`.
pub fn block_trailer(contents: &[u8]) -> [u8; BLOCK_TRAILER_SIZE] {
    let crc = crc32c::extend(crc32c::value(contents), &[NO_COMPRESSION]);
    let m = crc32c::mask(crc).to_le_bytes();
    [NO_COMPRESSION, m[0], m[1], m[2], m[3]]
}

/// Reads and verifies the block at `handle`.
pub fn read_block(file: &dyn RandomAccessFile, handle: BlockHandle) -> Result<Vec<u8>> {
    let size = usize::try_from(handle.size).map_err(|_| Error::corruption("block too large"))?;
    let mut buf = file.read_at(handle.offset, size + BLOCK_TRAILER_SIZE)?;
    if buf.len() != size + BLOCK_TRAILER_SIZE {
        return Err(Error::corruption(format!(
            "truncated block read at offset {}",
            handle.offset
        )));
    }
    let trailer = buf.split_off(size);
    if trailer[0] != NO_COMPRESSION {
        return Err(Error::corruption(format!(
            "unknown block compression type {}",
            trailer[0]
        )));
    }
    if trailer != block_trailer(&buf) {
        return Err(Error::corruption(format!(
            "block checksum mismatch at offset {}",
            handle.offset
        )));
    }
    Ok(buf)
}
