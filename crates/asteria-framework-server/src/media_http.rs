//! `GET /media/{id}/original` and `GET /media/{id}/{preset}`.

use std::path::PathBuf;

use asteria_framework::paths::is_sidecar;
use asteria_framework::{MediaKind, UserId};
use asteria_framework_api::ItemId;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::cache::ItemRecord;
use crate::ext::{ExtContext, ThumbSource};
use crate::process::{Preset, ProcessError};
use crate::scan::{is_hidden, mtime_ns};
use crate::state::AppState;

/// 503 telling the client to come back shortly (the content hash is not ready, a tool is
/// missing, or the render queue is full).
fn not_ready() -> Response {
    let mut resp = StatusCode::SERVICE_UNAVAILABLE.into_response();
    resp.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
    resp
}

fn nosniff(mut resp: Response) -> Response {
    resp.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}

fn failure(status: StatusCode) -> Response {
    if status == StatusCode::SERVICE_UNAVAILABLE {
        not_ready()
    } else {
        status.into_response()
    }
}

/// The record, the file's absolute path and its current metadata; the file must exist and,
/// with symlinks resolved now (a file may have been swapped for a link since the scan), lie
/// inside the library outside any `.`-named directory and not be an XMP sidecar. `Err` is the status to answer with
/// instead (503 means "not ready").
async fn resolve(
    state: &AppState,
    id: String,
) -> Result<(ItemRecord, PathBuf, std::fs::Metadata), StatusCode> {
    let cache = state.cache().clone();
    let rec = tokio::task::spawn_blocking(move || cache.get_by_id(&ItemId(id)))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|e| {
            tracing::error!("cache read failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    let abs = state.library().root().join(&rec.path);
    let real = tokio::fs::canonicalize(&abs)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let inside = real
        .strip_prefix(state.canonical_root())
        .is_ok_and(|rel| !rel.components().any(|c| is_hidden(c.as_os_str())));
    if !inside || is_sidecar(&real) {
        tracing::warn!(path = %rec.path, "item resolves outside the library or to a sidecar; not serving it");
        return Err(StatusCode::NOT_FOUND);
    }
    let abs = real;
    let meta = tokio::fs::metadata(&abs)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    if !meta.is_file() {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok((rec, abs, meta))
}

pub async fn media(
    State(state): State<AppState>,
    Path((id, what)): Path<(String, String)>,
    req: Request,
) -> Response {
    if what == "original" {
        return match resolve(&state, id).await {
            Ok((_, abs, _)) => nosniff(
                ServeFile::new(abs)
                    .oneshot(req)
                    .await
                    .map(|r| r.map(Body::new))
                    .into_response(),
            ),
            Err(status) => failure(status),
        };
    }
    let Some(preset) = Preset::parse(&what) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match resolve(&state, id).await {
        Ok((rec, abs, meta)) => {
            // The record is out of date: whatever hash it holds describes other content.
            if (meta.len(), mtime_ns(&meta)) != (rec.size, rec.mtime_ns) {
                let _ = state.rescan().try_send(());
                return not_ready();
            }
            thumbnail(&state, rec, abs, preset, req).await
        }
        Err(status) => failure(status),
    }
}

async fn thumbnail(
    state: &AppState,
    rec: ItemRecord,
    abs: PathBuf,
    preset: Preset,
    req: Request,
) -> Response {
    /// What to render and under which key.
    enum Job {
        File(MediaKind, PathBuf),
        Bytes(Vec<u8>, &'static str),
    }
    let (key, job) = if preset == Preset::OfflineAudio {
        if rec.kind != MediaKind::Audio {
            return StatusCode::NOT_FOUND.into_response();
        }
        let Some(hash) = file_hash(&rec) else {
            return not_ready();
        };
        (hash, Job::File(MediaKind::Audio, abs))
    } else {
        // The app picks where the picture comes from (embedded cover art, a folder image).
        // The extension is synchronous and may read files, so it runs off the async threads.
        let ext = state.extension().clone();
        let ctx = ExtContext {
            library: state.library().clone(),
            cache: state.cache().clone(),
            user: req
                .extensions()
                .get::<UserId>()
                .copied()
                .unwrap_or_else(|| state.library().owner()),
            owner: state.library().owner(),
        };
        let (r, p) = (rec.clone(), abs.clone());
        let Ok(source) = tokio::task::spawn_blocking(move || ext.thumb_source(&ctx, &r, &p)).await
        else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        match source {
            ThumbSource::Original => {
                if !matches!(rec.kind, MediaKind::Image | MediaKind::Video) {
                    return StatusCode::NOT_FOUND.into_response();
                }
                let Some(hash) = file_hash(&rec) else {
                    return not_ready();
                };
                (hash, Job::File(rec.kind, abs))
            }
            ThumbSource::Image { key, bytes, ext } => (key, Job::Bytes(bytes, ext)),
            ThumbSource::File { key, path } => (key, Job::File(MediaKind::Image, path)),
            ThumbSource::None => return StatusCode::NOT_FOUND.into_response(),
        }
    };
    let processor = state.processor();
    let Some(thumb) = processor.thumb_path(&key, preset) else {
        tracing::error!("content hash is malformed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    if tokio::fs::try_exists(&thumb).await.unwrap_or(false) {
        return serve_thumb(thumb, preset, req).await;
    }
    let cache = state.cache().clone();
    let (h, p) = (key.clone(), preset.name());
    let known = tokio::task::spawn_blocking(move || cache.unprocessable(&h, p)).await;
    match known {
        Ok(Ok(Some(_))) => return StatusCode::UNPROCESSABLE_ENTITY.into_response(),
        Ok(Ok(None)) => {}
        _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    let rendered = match &job {
        Job::File(kind, path) => processor.render(path, *kind, &key, preset).await,
        Job::Bytes(bytes, ext) => processor.render_image_bytes(bytes, ext, &key, preset).await,
    };
    match rendered {
        Ok(path) => serve_thumb(path, preset, req).await,
        // The render task already recorded it (see `AppState::new`).
        Err(ProcessError::Unprocessable(_)) => StatusCode::UNPROCESSABLE_ENTITY.into_response(),
        Err(ProcessError::ToolMissing(_) | ProcessError::Busy) => not_ready(),
        Err(ProcessError::Io(e)) => {
            tracing::error!("thumbnail rendering failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// The whole-file hash of `rec`, if it describes the current content. `resolve` checked the
/// record against the file, so a hash with the record's stamp is the hash of the current
/// content. Renders are keyed on the whole-file hash so a metadata-only edit (EXIF
/// orientation, ICC) re-renders them.
fn file_hash(rec: &ItemRecord) -> Option<String> {
    rec.hash
        .as_ref()
        .filter(|h| h.size == rec.size && h.mtime_ns == rec.mtime_ns)
        .map(|h| h.file_hash.clone())
}

async fn serve_thumb(path: PathBuf, preset: Preset, req: Request) -> Response {
    let mut resp = nosniff(
        ServeFile::new_with_mime(path, &preset.mime().parse().expect("valid mime"))
            .oneshot(req)
            .await
            .map(|r| r.map(Body::new))
            .into_response(),
    );
    // The URL names an item, not a content hash: the thumbnail changes with the file.
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    resp
}
