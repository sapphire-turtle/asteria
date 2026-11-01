//! Shared server foundation for Asteria apps: configuration, API keys, errors, cache location.

pub mod app;
mod auth;
mod batches;
pub mod cache;
pub mod cli;
pub mod config;
pub mod error;
pub mod ext;
mod fsutil;
pub mod hashing;
pub mod local;
mod mcp;
mod media_http;
pub(crate) mod organize;
pub mod process;
pub mod profile;
mod rpc;
pub mod scan;
pub mod state;
pub mod upload;
pub mod upload_commit;
mod upload_http;
pub mod watch;
pub mod writer;

pub use app::{router, router_with_shutdown};
pub use cache::{
    CACHE_SCHEMA, Cache, ExtraRecord, ItemRecord, SidecarSnapshot, StoredHash, item_id_for,
};
pub use config::{
    ServerConfig, cache_dir_for, data_dir_for, default_config_path, keys_dir_for, load_and_migrate,
    load_and_migrate_in, open_keys,
};
/// corundum's API keys: the store, its verifier and its command line.
pub use corundum_framework::keys;
pub use error::ServerError;
pub use ext::{AppExtension, ExtContext, McpTool, Rewrite, ThumbSource};
pub use hashing::{RelinkReport, hash_pending, relink_orphans};
pub use local::{LocalServer, setup_local, setup_local_at};
pub use process::{Preset, ProcessError, Processor, Tool, find_tool, tool_program};
pub use rpc::{media_item, rpc_error};
pub use scan::{ScanReport, scan, scan_cancellable};
pub use state::AppState;
pub use watch::{ScanLoop, spawn_scan_loop};
pub use writer::{WriteError, Writer, spawn_writer};
