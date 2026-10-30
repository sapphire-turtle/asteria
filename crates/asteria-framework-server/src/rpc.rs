//! `POST /rpc`: one JSON-RPC 2.0 request per body, answered with HTTP 200 (the body limit in
//! `app.rs` adds 413 for oversized bodies, and the auth layer 401).

use std::sync::atomic::Ordering;

use asteria_framework::UserId;
use asteria_framework::hash::ContentHash;
use asteria_framework_api::{
    ExistingMedia, FOLDER_LIST, FolderListParams, FolderListing, ItemId, LIBRARY_INFO,
    LIBRARY_RESCAN, LibraryInfo, MEDIA_GET, MEDIA_UPDATE, MediaGetParams, MediaItem,
    MediaUpdateParams, RpcError, RpcOutcome, RpcResponse, SERVER_INFO, ServerInfo, UPLOAD_ABORT,
    UPLOAD_BEGIN, UPLOAD_CHECK, UPLOAD_COMMIT, UPLOAD_STATUS, UploadBeginParams, UploadCheckParams,
    UploadCheckResult, UploadCommitParams, UploadCommitResult, UploadSessionParams, codes,
};
use axum::Extension;
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use chrono::{DateTime, SecondsFormat};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::mpsc::error::TrySendError;

use crate::cache::ItemRecord;
use crate::error::ServerError;
use crate::ext::ExtContext;
use crate::state::AppState;
use crate::upload::UploadError;
use crate::writer::WriteError;

const DEFAULT_LIMIT: u32 = 200;
const MAX_LIMIT: u32 = 1000;
/// Most hashes one `upload.check` may carry.
const MAX_CHECK_HASHES: usize = 1000;

/// `RpcRequest<Value>` whose `params` may be absent (treated as `{}`).
#[derive(Deserialize)]
struct Request {
    jsonrpc: String,
    /// Required, but the response id is taken from the raw value (see `rpc`).
    #[serde(rename = "id")]
    _id: u64,
    method: String,
    #[serde(default = "empty_params")]
    params: Value,
}

fn empty_params() -> Value {
    json!({})
}

/// An RPC error with `code` and `message`.
pub fn rpc_error(code: i32, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
    }
}

fn internal(detail: impl std::fmt::Display) -> RpcError {
    tracing::error!(error = %detail, "rpc internal error");
    rpc_error(codes::INTERNAL_ERROR, "internal error")
}

fn respond(id: u64, outcome: Result<Value, RpcError>) -> Json<RpcResponse<Value>> {
    Json(RpcResponse {
        jsonrpc: "2.0".into(),
        id,
        outcome: match outcome {
            Ok(v) => RpcOutcome::Ok(v),
            Err(e) => RpcOutcome::Err(e),
        },
    })
}

pub(crate) async fn rpc(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    body: Bytes,
) -> Json<RpcResponse<Value>> {
    // Id 0 is used when the request's own id cannot be read (JSON-RPC says `null` there, but
    // this API's ids are plain u64).
    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return respond(0, Err(rpc_error(codes::PARSE_ERROR, "parse error"))),
    };
    let id = value.get("id").and_then(Value::as_u64).unwrap_or(0);
    let req: Request = match serde_json::from_value(value) {
        Ok(r) => r,
        Err(_) => {
            return respond(
                id,
                Err(rpc_error(codes::INVALID_REQUEST, "invalid request")),
            );
        }
    };
    if req.jsonrpc != "2.0" {
        return respond(
            id,
            Err(rpc_error(codes::INVALID_REQUEST, "jsonrpc must be \"2.0\"")),
        );
    }
    respond(id, dispatch(&state, user, &req.method, req.params).await)
}

fn params<P: DeserializeOwned>(v: Value) -> Result<P, RpcError> {
    serde_json::from_value(v)
        .map_err(|e| rpc_error(codes::INVALID_PARAMS, format!("invalid params: {e}")))
}

fn to_value<R: serde::Serialize>(r: R) -> Result<Value, RpcError> {
    serde_json::to_value(r).map_err(internal)
}

/// Runs blocking cache work off the async threads.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ServerError> + Send + 'static,
) -> Result<T, RpcError> {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(ServerError::InvalidPath(p))) => {
            Err(rpc_error(codes::INVALID_PATH, format!("invalid path: {p}")))
        }
        Ok(Err(e)) => Err(internal(e)),
        Err(e) => Err(internal(e)),
    }
}

fn write_err(e: WriteError) -> RpcError {
    match e {
        WriteError::NotFound => rpc_error(codes::NOT_FOUND, "not found"),
        WriteError::InvalidPath(p) => rpc_error(codes::INVALID_PATH, format!("invalid path: {p}")),
        // Details can carry server paths, so they are logged, never sent.
        WriteError::ReadOnly(m) => {
            tracing::warn!(detail = %m, "write refused: read-only");
            rpc_error(
                codes::READ_ONLY,
                "sidecar is malformed; fix or remove it to edit",
            )
        }
        WriteError::Busy(m) => {
            tracing::warn!(detail = %m, "write refused: busy");
            rpc_error(codes::BUSY, "another write is in progress; retry")
        }
        WriteError::Invalid(m) => rpc_error(codes::INVALID_PARAMS, m),
        WriteError::Internal(m) => internal(m),
    }
}

fn upload_err(e: UploadError) -> RpcError {
    match e {
        UploadError::NotFound => rpc_error(codes::NOT_FOUND, "no such upload session"),
        UploadError::Invalid(m) => rpc_error(codes::INVALID_PARAMS, m),
        UploadError::Quota(m) => rpc_error(codes::QUOTA, m),
        UploadError::Incomplete => rpc_error(codes::UPLOAD_INCOMPLETE, "upload is incomplete"),
        UploadError::HashMismatch => rpc_error(codes::HASH_MISMATCH, "file hash does not match"),
        UploadError::Busy(m) => rpc_error(codes::BUSY, m),
        UploadError::Io(m) => internal(m),
    }
}

/// Runs blocking upload-store work off the async threads.
async fn upload_blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, UploadError> + Send + 'static,
) -> Result<T, RpcError> {
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r.map_err(upload_err),
        Err(e) => Err(internal(e)),
    }
}

async fn dispatch(
    state: &AppState,
    user: UserId,
    method: &str,
    p: Value,
) -> Result<Value, RpcError> {
    let owner = state.library().owner();
    match method {
        SERVER_INFO => {
            let cache = state.cache().clone();
            let epoch = blocking(move || Ok(cache.epoch())).await?;
            to_value(ServerInfo {
                app: state.app().into(),
                app_api: state.app_api(),
                server_version: env!("CARGO_PKG_VERSION").into(),
                epoch,
            })
        }
        LIBRARY_INFO => {
            let cache = state.cache().clone();
            let (epoch, item_count) = blocking(move || Ok((cache.epoch(), cache.count()?))).await?;
            let lib = state.library();
            let owner_name = lib
                .users()
                .user
                .iter()
                .find(|u| u.id == owner)
                .map_or_else(|| owner.to_string(), |u| u.name.clone());
            to_value(LibraryInfo {
                library_id: lib.config().id.0.to_string(),
                owner: owner_name,
                epoch,
                item_count,
                scanning: state.scanning().load(Ordering::Relaxed),
                revision: 0,
            })
        }
        LIBRARY_RESCAN => {
            match state.rescan().try_send(()) {
                // Full: a rescan is already pending.
                Ok(()) | Err(TrySendError::Full(())) => {}
                Err(TrySendError::Closed(())) => return Err(internal("rescan task stopped")),
            }
            Ok(json!({}))
        }
        FOLDER_LIST => {
            let p: FolderListParams = params(p)?;
            let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
            let cache = state.cache().clone();
            let (folders, recs, next_cursor) =
                blocking(move || cache.list_folder(&p.path, p.cursor.as_deref(), limit)).await?;
            to_value(FolderListing {
                folders,
                items: recs
                    .into_iter()
                    .map(|r| media_item(r, user, owner))
                    .collect(),
                next_cursor,
            })
        }
        MEDIA_GET => {
            let p: MediaGetParams = params(p)?;
            let cache = state.cache().clone();
            let rec = blocking(move || cache.get_by_id(&p.id)).await?;
            let rec = rec.ok_or_else(|| rpc_error(codes::NOT_FOUND, "not found"))?;
            to_value(media_item(rec, user, owner))
        }
        MEDIA_UPDATE => {
            let p: MediaUpdateParams = params(p)?;
            let rec = state.writer().update(user, p).await.map_err(write_err)?;
            to_value(media_item(rec, user, owner))
        }
        UPLOAD_CHECK => {
            let p: UploadCheckParams = params(p)?;
            if p.hashes.len() > MAX_CHECK_HASHES {
                return Err(rpc_error(
                    codes::INVALID_PARAMS,
                    format!("at most {MAX_CHECK_HASHES} hashes per check"),
                ));
            }
            let cache = state.cache().clone();
            let existing = blocking(move || {
                let mut out = Vec::new();
                for h in p.hashes {
                    let paths = cache.paths_with_hash(&h.hash, &h.scope)?;
                    if !paths.is_empty() {
                        out.push(ExistingMedia {
                            hash: h.hash,
                            scope: h.scope,
                            paths,
                        });
                    }
                }
                Ok(out)
            })
            .await?;
            to_value(UploadCheckResult { existing })
        }
        UPLOAD_BEGIN => {
            let p: UploadBeginParams = params(p)?;
            let hash: ContentHash = p
                .file_hash
                .parse()
                .map_err(|_| rpc_error(codes::INVALID_PARAMS, "file_hash is not a blake3 hash"))?;
            let store = state.uploads().clone();
            let begun = upload_blocking(move || store.begin(user, p.size, hash)).await?;
            to_value(begun)
        }
        UPLOAD_STATUS => {
            let p: UploadSessionParams = params(p)?;
            let store = state.uploads().clone();
            let status = upload_blocking(move || store.status(user, &p.session)).await?;
            to_value(status)
        }
        UPLOAD_ABORT => {
            let p: UploadSessionParams = params(p)?;
            let store = state.uploads().clone();
            upload_blocking(move || store.abort(user, &p.session)).await?;
            Ok(json!({}))
        }
        UPLOAD_COMMIT => {
            let p: UploadCommitParams = params(p)?;
            let result = state
                .writer()
                .commit_upload(state.uploads().clone(), user, p)
                .await
                .map_err(upload_err)?;
            if matches!(result, UploadCommitResult::Placed { .. }) {
                // Same as `library.rescan`: a full queue means one is already pending. The
                // files are placed and their sessions gone, so a failure here must not
                // turn the response into an error (a retry would only get NotFound).
                if let Err(TrySendError::Closed(())) = state.rescan().try_send(()) {
                    tracing::warn!("rescan task stopped; the placed files wait for the next scan");
                }
            }
            to_value(result)
        }
        _ => {
            // Not a common method: the app may know it. The extension is synchronous and
            // may read the cache or files, so it runs off the async threads.
            let ctx = ExtContext {
                library: state.library().clone(),
                cache: state.cache().clone(),
                user,
                owner,
            };
            let ext = state.extension().clone();
            let method = method.to_owned();
            let answer = tokio::task::spawn_blocking(move || ext.rpc(&ctx, &method, p))
                .await
                .map_err(internal)?;
            answer.unwrap_or_else(|| Err(rpc_error(codes::METHOD_NOT_FOUND, "method not found")))
        }
    }
}

/// The record as `user` sees it.
pub fn media_item(rec: ItemRecord, user: UserId, owner: UserId) -> MediaItem {
    let rating = if user == owner {
        rec.sidecar.owner_rating
    } else {
        rec.sidecar.user_ratings.get(&user.to_string()).copied()
    };
    let (content_hash, file_hash) = rec
        .hash
        .filter(|h| h.size == rec.size && h.mtime_ns == rec.mtime_ns)
        .map(|h| (h.hash, h.file_hash))
        .unzip();
    let secs = rec.mtime_ns.div_euclid(1_000_000_000);
    let nanos = rec.mtime_ns.rem_euclid(1_000_000_000) as u32;
    let modified = i64::try_from(secs)
        .ok()
        .and_then(|s| DateTime::from_timestamp(s, nanos))
        .unwrap_or(DateTime::UNIX_EPOCH)
        .to_rfc3339_opts(SecondsFormat::AutoSi, true);
    MediaItem {
        id: ItemId(rec.id.0),
        path: rec.path,
        kind: rec.kind,
        size: rec.size,
        modified,
        rating,
        subjects: rec.sidecar.subjects,
        hierarchical_subjects: rec.sidecar.hierarchical_subjects,
        description: rec.sidecar.description,
        label: rec.sidecar.label,
        content_hash,
        file_hash,
        read_only_reason: rec.sidecar_error,
    }
}
