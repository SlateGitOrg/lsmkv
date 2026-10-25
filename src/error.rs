//! Error type shared by every layer of the engine.

use std::fmt;
use std::io;

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong inside the engine.
#[derive(Debug)]
pub enum Error {
    /// An underlying filesystem operation failed (including injected faults).
    Io(io::Error),
    /// Persistent data failed a checksum or structural validation.
    Corruption(String),
    /// The caller passed an argument the engine cannot accept.
    InvalidArgument(String),
    /// A requested file or entity does not exist.
    NotFound(String),
    /// An earlier background error put the database into read-only mode.
    ReadOnly(String),
    /// The database has been closed.
    Closed,
}

impl Error {
    /// Builds a [`Error::Corruption`] from anything printable.
    pub fn corruption(msg: impl Into<String>) -> Self {
        Error::Corruption(msg.into())
    }

    /// Builds an [`Error::InvalidArgument`] from anything printable.
    pub fn invalid(msg: impl Into<String>) -> Self {
        Error::InvalidArgument(msg.into())
    }

    /// True for [`Error::Corruption`].
    pub fn is_corruption(&self) -> bool {
        matches!(self, Error::Corruption(_))
    }

    /// Produces an equivalent error value; `io::Error` is not `Clone`, so the
    /// kind and message are carried over.
    pub fn duplicate(&self) -> Error {
        match self {
            Error::Io(e) => Error::Io(io::Error::new(e.kind(), e.to_string())),
            Error::Corruption(m) => Error::Corruption(m.clone()),
            Error::InvalidArgument(m) => Error::InvalidArgument(m.clone()),
            Error::NotFound(m) => Error::NotFound(m.clone()),
            Error::ReadOnly(m) => Error::ReadOnly(m.clone()),
            Error::Closed => Error::Closed,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Corruption(m) => write!(f, "corruption: {m}"),
            Error::InvalidArgument(m) => write!(f, "invalid argument: {m}"),
            Error::NotFound(m) => write!(f, "not found: {m}"),
            Error::ReadOnly(m) => write!(f, "database is read-only after background error: {m}"),
            Error::Closed => write!(f, "database is closed"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_duplicate() {
        let errs = [
            Error::Io(io::Error::other("disk")),
            Error::corruption("bad crc"),
            Error::invalid("empty"),
            Error::NotFound("x".into()),
            Error::ReadOnly("y".into()),
            Error::Closed,
        ];
        for e in &errs {
            assert_eq!(e.to_string(), e.duplicate().to_string());
        }
        assert!(errs[1].is_corruption());
        assert!(!errs[0].is_corruption());
        assert!(std::error::Error::source(&errs[0]).is_some());
        assert!(std::error::Error::source(&errs[1]).is_none());
    }
}
