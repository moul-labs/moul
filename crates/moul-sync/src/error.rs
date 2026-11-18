//! Errors from sync.

use std::fmt;

/// What went wrong, coarse enough to decide whether to retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The replica could not be reached, or answered with a server error.
    /// Retry later.
    Network,
    /// The replica refused the credentials or the request.
    Denied,
    /// Another device changed what this one meant to change, such as the
    /// writer record. Read again and decide.
    Conflict,
    /// The replica does not hold what was asked for.
    NotFound,
    /// The replica holds something sync cannot use: a gap in the TXIDs, a
    /// damaged file, a record it cannot parse.
    Replica,
    /// The local database or its file layer failed.
    Local,
    /// The caller asked for something that cannot be done, such as taking
    /// over writing before catching up.
    Invalid,
}

/// An error from sync, a replica transport or the local database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

/// The result type of this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// Makes an error of `kind` with a human-readable message.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn network(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Network, message)
    }

    pub(crate) fn replica(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Replica, message)
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Invalid, message)
    }

    /// What went wrong.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The message, without the kind.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Whether trying again later may succeed without anything else changing.
    pub fn is_transient(&self) -> bool {
        self.kind == ErrorKind::Network
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            ErrorKind::Network => "network error",
            ErrorKind::Denied => "access denied",
            ErrorKind::Conflict => "conflict",
            ErrorKind::NotFound => "not found",
            ErrorKind::Replica => "replica error",
            ErrorKind::Local => "local database error",
            ErrorKind::Invalid => "invalid request",
        };
        write!(f, "{kind}: {}", self.message)
    }
}

impl std::error::Error for Error {}

impl From<moul_vfs::Error> for Error {
    fn from(e: moul_vfs::Error) -> Self {
        Self::new(ErrorKind::Local, e.to_string())
    }
}

impl From<moul_store::Error> for Error {
    fn from(e: moul_store::Error) -> Self {
        Self::new(ErrorKind::Local, e.to_string())
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::new(ErrorKind::Local, e.to_string())
    }
}

impl From<moul_ltx::Error> for Error {
    fn from(e: moul_ltx::Error) -> Self {
        Self::replica(format!("LTX: {e}"))
    }
}
