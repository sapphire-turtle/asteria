//! The music app's plug-in for the shared server.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use asteria_framework::MediaKind;
use asteria_framework_server::{AppExtension, ExtContext, ItemRecord, ServerError, ThumbSource};
use asteria_music_api::{
    ALBUM_GET, ALBUM_LIST, ARTIST_GET, ARTIST_LIST, PLAYLIST_GET, PLAYLIST_LIST, RpcError,
    TrackTags,
};

use crate::index::Index;
use crate::{cover, rpc, tags};

/// The index built at one cache generation, and the extra files' paths of that moment.
pub type Snapshot = (u64, Arc<Index>, Arc<Vec<String>>);

/// Music shows audio; the tags are read during the scan, and the album and artist index is
/// built from them when a method needs it.
pub struct MusicExtension {
    snapshot: Mutex<Option<Snapshot>>,
}

impl MusicExtension {
    pub fn new() -> Self {
        Self {
            snapshot: Mutex::new(None),
        }
    }

    /// The index for the cache as it is now, rebuilt when app data or extras changed since
    /// the last build.
    pub fn snapshot(&self, ctx: &ExtContext) -> Result<Snapshot, ServerError> {
        // One build at a time: concurrent callers wait for it instead of repeating it.
        let mut slot = self.snapshot.lock().unwrap_or_else(PoisonError::into_inner);
        // Read before the tables: a write during the build leaves the stored generation
        // behind, so the next call rebuilds.
        let generation = ctx.cache.app_generation();
        if let Some(snap) = slot.as_ref()
            && snap.0 == generation
        {
            return Ok(snap.clone());
        }
        let tracks = ctx
            .cache
            .all_app_data()?
            .into_iter()
            .map(|(rel, bytes)| (rel, decode_tags(&bytes)));
        let index = Index::build(&ctx.library.config().id, tracks);
        let extras = ctx
            .cache
            .extras()?
            .into_iter()
            .map(|(rel, _)| rel)
            .collect();
        let snap = (generation, Arc::new(index), Arc::new(extras));
        *slot = Some(snap.clone());
        Ok(snap)
    }
}

/// Tags as cached by `extract`; an entry that does not decode still makes a track.
pub fn decode_tags(bytes: &[u8]) -> TrackTags {
    serde_json::from_slice(bytes).unwrap_or_else(|_| TrackTags {
        tag_error: Some("unreadable cache entry".into()),
        ..TrackTags::default()
    })
}

/// True for a playlist file name or path: `.m3u8` or `.m3u` in any case, with a stem.
pub fn is_playlist(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty() && (ext.eq_ignore_ascii_case("m3u8") || ext.eq_ignore_ascii_case("m3u"))
    })
}

impl AppExtension for MusicExtension {
    fn accepts(&self, kind: MediaKind) -> bool {
        kind == MediaKind::Audio
    }

    fn extract(&self, _rel: &str, abs: &Path) -> Option<Vec<u8>> {
        serde_json::to_vec(&tags::read(abs)).ok()
    }

    fn wants_extra(&self, rel: &str) -> bool {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        is_playlist(name) || cover::is_cover_name(name)
    }

    fn thumb_source(&self, ctx: &ExtContext, rec: &ItemRecord, abs: &Path) -> ThumbSource {
        // Without the extras only the embedded picture can be found; that is still a cover.
        let extras = self
            .snapshot(ctx)
            .map(|(_, _, extras)| extras)
            .unwrap_or_default();
        cover::thumb_source(ctx.library.root(), &rec.path, abs, &extras)
    }

    fn rpc(
        &self,
        ctx: &ExtContext,
        method: &str,
        params: serde_json::Value,
    ) -> Option<Result<serde_json::Value, RpcError>> {
        let handler = match method {
            ALBUM_LIST => rpc::album_list,
            ALBUM_GET => rpc::album_get,
            ARTIST_LIST => rpc::artist_list,
            ARTIST_GET => rpc::artist_get,
            PLAYLIST_LIST => rpc::playlist_list,
            PLAYLIST_GET => rpc::playlist_get,
            _ => return None,
        };
        Some(handler(self, ctx, params))
    }
}
