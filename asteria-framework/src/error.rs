use std::path::PathBuf;

use crate::hash::MediaFormat;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not an asteria library: {0}")]
    NotALibrary(PathBuf),
    #[error("library already initialized: {0}")]
    AlreadyInitialized(PathBuf),
    #[error("unsupported library format: {0}")]
    UnsupportedFormat(u32),
    #[error("toml error: {0}")]
    Toml(String),
    #[error("invalid content hash: {0}")]
    InvalidHash(String),
    #[error("invalid hash scope: {0}")]
    InvalidHashScope(String),
    #[error("malformed {format:?} media: {reason}")]
    MalformedMedia { format: MediaFormat, reason: String },
    #[error("malformed XMP sidecar{}: {reason}", .path.as_ref().map(|p| format!(" {}", p.display())).unwrap_or_default())]
    MalformedSidecar {
        path: Option<PathBuf>,
        reason: String,
    },
}

pub type Result<T> = std::result::Result<T, Error>;
