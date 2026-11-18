//! The one error type of this crate.

use std::fmt;

/// What went wrong, broadly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The caller asked for something the format does not allow, such as a
    /// page out of order or a page size that is not a power of two.
    Invalid,
    /// Bytes that should be an LTX file (or an LZ4 frame inside one) are not.
    Corrupt,
    /// Files do not follow on from each other: a TXID gap, a page size change,
    /// or a restore with no snapshot to start from.
    Discontiguous,
    /// The writer that output went to failed.
    Io,
}

/// An error from reading, writing, compacting or restoring LTX files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

/// `Result` with this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// An error of `kind` with `message`.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Invalid, message)
    }

    pub(crate) fn corrupt(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Corrupt, message)
    }

    pub(crate) fn discontiguous(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Discontiguous, message)
    }

    /// What kind of error this is.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The message, without the kind.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::new(ErrorKind::Io, e.to_string())
    }
}
