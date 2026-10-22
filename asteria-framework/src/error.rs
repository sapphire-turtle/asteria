use std::path::PathBuf;

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
}

pub type Result<T> = std::result::Result<T, Error>;
