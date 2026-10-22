//! Shared server foundation for Asteria apps: configuration, tokens, errors, cache location.

pub mod auth;
pub mod config;
pub mod error;

pub use auth::{Tokens, new_token, token_hash};
pub use config::{ServerConfig, TokenEntry, cache_dir_for};
pub use error::ServerError;
