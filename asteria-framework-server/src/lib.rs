//! Shared server foundation for Asteria apps: configuration, tokens, errors, cache location.

pub mod app;
pub mod auth;
pub mod cache;
pub mod config;
pub mod error;
pub mod hashing;
mod rpc;
pub mod scan;
pub mod state;
pub mod writer;

pub use app::router;
pub use auth::{Tokens, new_token, token_hash};
pub use cache::{CACHE_SCHEMA, Cache, ItemRecord, SidecarSnapshot, StoredHash, item_id_for};
pub use config::{ServerConfig, TokenEntry, cache_dir_for};
pub use error::ServerError;
pub use hashing::{RelinkReport, hash_pending, relink_orphans};
pub use scan::{MediaFilter, ScanReport, scan};
pub use state::AppState;
pub use writer::{WriteError, Writer, spawn_writer};
