//! The extension point an app plugs into the shared server: which media become items,
//! per-item data extracted at scan time, extra files worth remembering, thumbnail sources
//! and app-specific RPC methods. Everything defaults to "nothing special".

use std::path::{Path, PathBuf};
use std::sync::Arc;

use asteria_framework::{Library, MediaKind, UserId};
use asteria_framework_api::RpcError;

use crate::cache::{Cache, ItemRecord};

/// What an app extension may look at while serving a request.
#[derive(Clone)]
pub struct ExtContext {
    pub library: Arc<Library>,
    pub cache: Arc<Cache>,
    /// The authenticated caller.
    pub user: UserId,
    /// The library owner.
    pub owner: UserId,
}

/// Where the `thumb`/`preview` of an item is rendered from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThumbSource {
    /// The item's own file.
    Original,
    /// Image bytes (for example embedded cover art). `key` is `blake3:<hex>` and names the
    /// derived media; `ext` is the file extension the bytes need for decoding.
    Image {
        key: String,
        bytes: Vec<u8>,
        ext: &'static str,
    },
    /// An image file elsewhere in the library. `key` is `blake3:<hex>`.
    File { key: String, path: PathBuf },
    /// There is nothing to show.
    None,
}

/// A change to an app file (a playlist, say) that follows a batch of moves: `path` is
/// rewritten from `before` to `after`, but only if it still holds exactly `before`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewrite {
    /// Library-relative, with `/` separators.
    pub path: String,
    pub before: String,
    pub after: String,
}

pub trait AppExtension: Send + Sync + 'static {
    /// Which kinds of media this app turns into items.
    fn accepts(&self, kind: MediaKind) -> bool;
    /// Called for each item the scan adds or whose (size, mtime) changed. `abs` is absolute.
    fn extract(&self, _rel: &str, _abs: &Path) -> Option<Vec<u8>> {
        None
    }
    /// Non-item files the app wants recorded in EXTRAS (checked before classification).
    fn wants_extra(&self, _rel: &str) -> bool {
        false
    }
    /// Source for the `thumb`/`preview` of `rec`.
    fn thumb_source(&self, _ctx: &ExtContext, _rec: &ItemRecord, _abs: &Path) -> ThumbSource {
        ThumbSource::Original
    }
    /// App-specific RPC methods. `None`: not this app's method.
    fn rpc(
        &self,
        _ctx: &ExtContext,
        _method: &str,
        _params: serde_json::Value,
    ) -> Option<Result<serde_json::Value, RpcError>> {
        None
    }
    /// Called once a batch has moved files. `moves` are the (old path, new path) pairs that
    /// completed; moves to the trash are not included. The returned rewrites are applied
    /// and journaled as part of the batch.
    fn after_moves(&self, _ctx: &ExtContext, _moves: &[(String, String)]) -> Vec<Rewrite> {
        Vec::new()
    }
}
