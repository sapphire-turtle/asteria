//! `GET /media/{id}/original` and `GET /media/{id}/{preset}`.

use std::path::PathBuf;

use asteria_framework::MediaKind;
use asteria_framework_api::ItemId;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::cache::ItemRecord;
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
/// inside the library outside any `.`-named directory. `Err` is the status to answer with
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
    if !inside {
        tracing::warn!(path = %rec.path, "item resolves outside the library; not serving it");
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
    if !matches!(rec.kind, MediaKind::Image | MediaKind::Video) {
        return StatusCode::NOT_FOUND.into_response();
    }
    // `resolve` checked the record against the file, so a hash with the record's stamp
    // is the hash of the current content.
    let Some(hash) = rec
        .hash
        .filter(|h| h.size == rec.size && h.mtime_ns == rec.mtime_ns)
        .map(|h| h.hash)
    else {
        return not_ready();
    };
    let processor = state.processor();
    let Some(thumb) = processor.thumb_path(&hash, preset) else {
        tracing::error!("cached content hash is malformed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    if tokio::fs::try_exists(&thumb).await.unwrap_or(false) {
        return serve_thumb(thumb, req).await;
    }
    let cache = state.cache().clone();
    let (h, p) = (hash.clone(), preset.name());
    let known = tokio::task::spawn_blocking(move || cache.unprocessable(&h, p)).await;
    match known {
        Ok(Ok(Some(_))) => return StatusCode::UNPROCESSABLE_ENTITY.into_response(),
        Ok(Ok(None)) => {}
        _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    match processor.render(&abs, rec.kind, &hash, preset).await {
        Ok(path) => serve_thumb(path, req).await,
        // The render task already recorded it (see `AppState::new`).
        Err(ProcessError::Unprocessable(_)) => StatusCode::UNPROCESSABLE_ENTITY.into_response(),
        Err(ProcessError::ToolMissing(_) | ProcessError::Busy) => not_ready(),
        Err(ProcessError::Io(e)) => {
            tracing::error!("thumbnail rendering failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn serve_thumb(path: PathBuf, req: Request) -> Response {
    let mut resp = nosniff(
        ServeFile::new_with_mime(path, &"image/webp".parse().expect("valid mime"))
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
