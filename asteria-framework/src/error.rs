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
    #[error("temporary file already exists (concurrent writer or crash leftover): {0}")]
    TmpExists(PathBuf),
    #[error("refusing to write sidecar: {0}")]
    UnsafeSidecarWrite(String),
    #[error("required file is missing: {0}")]
    MissingFile(PathBuf),
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
    #[error("media file not found: {0}")]
    MediaNotFound(PathBuf),
    #[error("rating out of range (-1..=5): {0}")]
    InvalidRating(i8),
    #[error("failed to serialize journal entry: {0}")]
    JournalSerialize(String),
    #[error("malformed journal entry at {}:{line}", .file.display())]
    MalformedJournal { file: PathBuf, line: usize },
}

pub type Result<T> = std::result::Result<T, Error>;
