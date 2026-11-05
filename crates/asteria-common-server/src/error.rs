#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error(transparent)]
    Framework(#[from] asteria_common_core::Error),
    #[error("config error: {0}")]
    Config(String),
    #[error("cache error: {0}")]
    Cache(String),
    /// The library directory does not hold this library (unmounted, replaced, unreadable).
    #[error("library unavailable: {0}")]
    LibraryUnavailable(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    /// The API keys' store.
    #[error("api keys: {0}")]
    Keys(#[from] corundum_framework::keys::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ServerError>;
