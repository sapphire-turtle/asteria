//! The music app's plug-in for the shared server.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use asteria_framework::MediaKind;
use asteria_framework_server::{
    AppExtension, ExtContext, ItemRecord, McpTool, Rewrite, ServerError, ThumbSource,
};
use serde_json::json;

use asteria_music_api::{
    ALBUM_GET, ALBUM_LIST, ARTIST_GET, ARTIST_LIST, PLAYLIST_GET, PLAYLIST_LIST, RpcError,
    TrackTags,
};

use crate::cover::FolderImages;
use crate::index::Index;
use crate::playlist::{self, Roots};
use crate::{cover, rpc, tags};

/// The index built at one cache generation, and the extra files of that moment.
pub type Snapshot = (u64, Arc<Index>, Arc<Extras>);

/// The extra files: every path, and the cover images grouped by folder.
#[derive(Debug, Default)]
pub struct Extras {
    pub paths: Vec<String>,
    pub covers: FolderImages,
}

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
        let paths: Vec<String> = ctx
            .cache
            .extras()?
            .into_iter()
            .map(|(rel, _)| rel)
            .collect();
        let covers = FolderImages::new(paths.iter().map(String::as_str));
        let snap = (
            generation,
            Arc::new(index),
            Arc::new(Extras { paths, covers }),
        );
        *slot = Some(snap.clone());
        Ok(snap)
    }

    /// Whether an index has been built (tests check that thumbnails do not build one).
    #[cfg(test)]
    pub fn built(&self) -> bool {
        self.snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
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

fn paging_schema() -> serde_json::Map<String, serde_json::Value> {
    schema(
        json!({
            "cursor": {"type": "string", "description": "next_cursor of the previous page."},
            "limit": {"type": "integer", "minimum": 1, "description": "The most entries to return."},
        }),
        &[],
    )
}

fn id_schema(what: &str) -> serde_json::Map<String, serde_json::Value> {
    schema(
        json!({"id": {"type": "string", "description": format!("The {what}'s id, as listed.")}}),
        &["id"],
    )
}

fn schema(
    properties: serde_json::Value,
    required: &[&str],
) -> serde_json::Map<String, serde_json::Value> {
    let serde_json::Value::Object(schema) = json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    }) else {
        unreachable!("json! of an object literal is an object")
    };
    schema
}

fn tool(
    name: &str,
    method: &str,
    description: &str,
    input_schema: serde_json::Map<String, serde_json::Value>,
) -> McpTool {
    McpTool {
        name: name.into(),
        description: description.into(),
        input_schema,
        method: method.into(),
    }
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
        // The extras straight from the cache: a thumbnail never builds the index. Without
        // them only the embedded picture can be found; that is still a cover.
        let images = match ctx.cache.extras() {
            Ok(extras) => {
                FolderImages::for_track(&rec.path, extras.iter().map(|(rel, _)| rel.as_str()))
            }
            Err(e) => {
                tracing::warn!(error = %e, "extras unavailable; looking for embedded cover only");
                FolderImages::default()
            }
        };
        cover::thumb_source(ctx.library.root(), &rec.path, abs, &images)
    }

    fn mcp_tools(&self) -> Vec<McpTool> {
        vec![
            tool(
                "album_list",
                ALBUM_LIST,
                "Lists the albums of the music library with their title, artist, year and \
                 track count. Pass next_cursor back as cursor for the next page.",
                paging_schema(),
            ),
            tool(
                "album_get",
                ALBUM_GET,
                "Returns one album by id with its tracks in track order. Each track carries \
                 its media item id and tags.",
                id_schema("album"),
            ),
            tool(
                "artist_list",
                ARTIST_LIST,
                "Lists the artists of the music library with their album counts. Pass \
                 next_cursor back as cursor for the next page.",
                paging_schema(),
            ),
            tool(
                "artist_get",
                ARTIST_GET,
                "Returns one artist by id with the albums they made and the albums they \
                 appear on.",
                id_schema("artist"),
            ),
            tool(
                "playlist_list",
                PLAYLIST_LIST,
                "Lists the playlists (.m3u and .m3u8 files) found in the library. Pass \
                 next_cursor back as cursor for the next page.",
                paging_schema(),
            ),
            tool(
                "playlist_get",
                PLAYLIST_GET,
                "Returns one playlist by id with its entries; each entry says whether its \
                 track was found and, if so, which track it is.",
                id_schema("playlist"),
            ),
        ]
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

    fn after_moves(&self, ctx: &ExtContext, moves: &[(String, String)]) -> Vec<Rewrite> {
        let moves: HashMap<String, String> = moves.iter().cloned().collect();
        let extras = match ctx.cache.extras() {
            Ok(extras) => extras,
            Err(e) => {
                tracing::warn!(error = %e, "extras unavailable; playlists are not rewritten");
                return Vec::new();
            }
        };
        let root = ctx.library.root();
        let roots = Roots::new(root, root.canonicalize().ok().as_deref());
        let mut rewrites = Vec::new();
        for (rel, _) in extras {
            if !is_playlist(&rel) {
                continue;
            }
            // Same rule as reading a playlist: only a file inside the library, in full.
            let Some((bytes, cut)) = cover::canonical_in_library(root, &rel)
                .and_then(|canon| rpc::read_playlist(&canon).ok())
            else {
                continue;
            };
            if cut {
                tracing::info!(playlist = %rel, "playlist is too large; not rewritten");
                continue;
            }
            let Ok(before) = String::from_utf8(bytes) else {
                tracing::info!(playlist = %rel, "playlist is not UTF-8; not rewritten");
                continue;
            };
            if let Some(after) = playlist::rewrite(&before, &rel, &roots, &moves) {
                rewrites.push(Rewrite {
                    path: rel,
                    before,
                    after,
                });
            }
        }
        rewrites
    }
}

#[cfg(test)]
mod tests {
    use asteria_framework_server::AppExtension;
    use asteria_music_api::*;
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};

    use super::MusicExtension;

    type Check = fn(&Value);

    fn parses<T: DeserializeOwned>(v: &Value) {
        serde_json::from_value::<T>(v.clone()).unwrap();
    }

    #[test]
    fn schema_examples_parse() {
        let tools = MusicExtension::new().mcp_tools();
        let examples: Vec<(&str, &str, Value, Check)> = vec![
            (
                "album_list",
                ALBUM_LIST,
                json!({"cursor": "c", "limit": 50}),
                parses::<ListParams>,
            ),
            (
                "album_get",
                ALBUM_GET,
                json!({"id": "a1"}),
                parses::<AlbumGetParams>,
            ),
            (
                "artist_list",
                ARTIST_LIST,
                json!({"cursor": "c", "limit": 50}),
                parses::<ListParams>,
            ),
            (
                "artist_get",
                ARTIST_GET,
                json!({"id": "r1"}),
                parses::<ArtistGetParams>,
            ),
            (
                "playlist_list",
                PLAYLIST_LIST,
                json!({"cursor": "c", "limit": 50}),
                parses::<ListParams>,
            ),
            (
                "playlist_get",
                PLAYLIST_GET,
                json!({"id": "p1"}),
                parses::<PlaylistGetParams>,
            ),
        ];
        assert_eq!(tools.len(), examples.len());
        for (name, method, example, check) in examples {
            let t = tools
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("{name} missing"));
            assert_eq!(t.method, method, "{name}");
            assert!(!t.description.is_empty(), "{name}");
            assert_eq!(t.input_schema["type"], "object", "{name}");
            assert_eq!(t.input_schema["additionalProperties"], false, "{name}");
            let props = t.input_schema["properties"].as_object().unwrap();
            for key in example.as_object().unwrap().keys() {
                assert!(props.contains_key(key), "{name}: {key} not in properties");
            }
            for key in t.input_schema["required"].as_array().unwrap() {
                assert!(example.get(key.as_str().unwrap()).is_some(), "{name}");
            }
            check(&example);
        }
    }
}
