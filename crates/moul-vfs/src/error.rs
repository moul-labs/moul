//! Errors from the file layer.

use std::fmt;

/// What went wrong, coarse enough for a caller to decide what to do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The storage back end failed to read, write, flush or truncate.
    Io,
    /// Stored bytes are not what the file layer wrote: a bad checksum where
    /// one cannot be explained by a crash, or a header it does not recognise.
    Corrupt,
    /// The caller asked for something the file layer does not allow, such as
    /// a misaligned page write or a name with a path separator.
    Invalid,
    /// The file is in use, for example open by SQLite while an import runs.
    Busy,
    /// What was asked for is not held, such as a level-0 file already
    /// released to the replica.
    NotFound,
    /// An earlier failure left the volume in a state it cannot trust. Reopen
    /// the volume: recovery puts the files back in order.
    Poisoned,
}

/// An error from the file layer or one of its storage back ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

impl Error {
    /// Makes an error of `kind` with a human-readable message.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// A storage failure.
    pub fn io(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Io, message)
    }

    /// Stored bytes that cannot be trusted.
    pub fn corrupt(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Corrupt, message)
    }

    /// A request the file layer refuses.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Invalid, message)
    }

    /// What kind of failure this is.
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
        let kind = match self.kind {
            ErrorKind::Io => "storage error",
            ErrorKind::Corrupt => "corrupt file",
            ErrorKind::Invalid => "invalid request",
            ErrorKind::Busy => "file busy",
            ErrorKind::NotFound => "not found",
            ErrorKind::Poisoned => "volume poisoned",
        };
        write!(f, "{kind}: {}", self.message)
    }
}

impl std::error::Error for Error {}

#[cfg(not(target_arch = "wasm32"))]
impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::io(err.to_string())
    }
}

/// Result type of the file layer.
pub type Result<T, E = Error> = std::result::Result<T, E>;
