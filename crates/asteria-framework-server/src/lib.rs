//! Shared server foundation for Asteria apps: configuration, tokens, errors, cache location.

pub mod app;
pub mod auth;
mod batches;
pub mod cache;
pub mod cli;
pub mod config;
pub mod error;
pub mod ext;
mod fsutil;
pub mod hashing;
mod mcp;
mod media_http;
pub(crate) mod organize;
pub mod process;
mod rpc;
pub mod scan;
pub mod state;
pub mod upload;
pub mod upload_commit;
mod upload_http;
pub mod watch;
pub mod writer;

pub use app::router;
pub use auth::{Tokens, new_token, token_hash};
pub use cache::{
    CACHE_SCHEMA, Cache, ExtraRecord, ItemRecord, SidecarSnapshot, StoredHash, item_id_for,
};
pub use config::{ServerConfig, TokenEntry, cache_dir_for, data_dir_for};
pub use error::ServerError;
pub use ext::{AppExtension, ExtContext, McpTool, Rewrite, ThumbSource};
pub use hashing::{RelinkReport, hash_pending, relink_orphans};
pub use process::{Preset, ProcessError, Processor};
pub use rpc::{media_item, rpc_error};
pub use scan::{ScanReport, scan, scan_cancellable};
pub use state::AppState;
pub use watch::{ScanLoop, spawn_scan_loop};
pub use writer::{WriteError, Writer, spawn_writer};
