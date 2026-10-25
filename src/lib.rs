//! lsmkv: an LSM-tree key-value storage engine.
#![warn(missing_docs)]

pub mod error;
pub mod fs;
pub mod util;

pub use error::{Error, Result};
