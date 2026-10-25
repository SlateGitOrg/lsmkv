//! lsmkv: an LSM-tree key-value storage engine.
#![warn(missing_docs)]

pub mod batch;
pub mod error;
pub mod fs;
pub mod iterator;
pub mod key;
pub mod memtable;
pub mod util;
pub mod wal;

pub use error::{Error, Result};
