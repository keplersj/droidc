//! Error type carrying the contract's error `kind`.

use serde::Serialize;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorKind {
    Network,
    Signature,
    Storage,
    NotFound,
    Invalid,
    Locked,
    Internal,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Network => "network",
            ErrorKind::Signature => "signature",
            ErrorKind::Storage => "storage",
            ErrorKind::NotFound => "not-found",
            ErrorKind::Invalid => "invalid",
            ErrorKind::Locked => "locked",
            ErrorKind::Internal => "internal",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Error {
            kind,
            message: message.into(),
        }
    }
    pub fn network(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Network, m)
    }
    pub fn signature(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Signature, m)
    }
    pub fn storage(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Storage, m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, m)
    }
    pub fn invalid(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Invalid, m)
    }
    pub fn internal(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, m)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.message)
    }
}

impl std::error::Error for Error {}

/// ENOSPC (28) and EDQUOT (122) are storage errors.
pub fn io_kind(e: &std::io::Error) -> ErrorKind {
    match e.raw_os_error() {
        Some(28) | Some(122) => ErrorKind::Storage,
        _ => ErrorKind::Internal,
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::new(io_kind(&e), e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::internal(format!("json: {e}"))
    }
}

/// Add context (usually a path) to an io error while keeping its kind.
pub trait Context<T> {
    fn context(self, what: impl fmt::Display) -> Result<T>;
}

impl<T> Context<T> for std::io::Result<T> {
    fn context(self, what: impl fmt::Display) -> Result<T> {
        self.map_err(|e| Error::new(io_kind(&e), format!("{what}: {e}")))
    }
}

impl<T> Context<T> for Result<T> {
    fn context(self, what: impl fmt::Display) -> Result<T> {
        self.map_err(|e| Error::new(e.kind, format!("{what}: {}", e.message)))
    }
}
