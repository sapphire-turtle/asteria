//! `PUT /upload/{session}/{chunk}`: one chunk of an upload session.

use asteria_framework::UserId;
use axum::Extension;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;

use crate::state::AppState;
use crate::upload::UploadError;

/// The HTTP status for an upload failure.
fn status_of(e: &UploadError) -> StatusCode {
    match e {
        UploadError::NotFound => StatusCode::NOT_FOUND,
        UploadError::Invalid(_) => StatusCode::BAD_REQUEST,
        UploadError::Quota(_) => StatusCode::PAYLOAD_TOO_LARGE,
        UploadError::Incomplete | UploadError::HashMismatch | UploadError::Busy(_) => {
            StatusCode::CONFLICT
        }
        UploadError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Stores the chunk; 204 on success. The body limit (set in `app.rs`) answers 413 for a body
/// bigger than a chunk before this runs.
pub(crate) async fn upload_chunk(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path((session, chunk)): Path<(String, u64)>,
    body: Bytes,
) -> StatusCode {
    let store = state.uploads().clone();
    match tokio::task::spawn_blocking(move || store.put_chunk(user, &session, chunk, &body)).await {
        Ok(Ok(())) => StatusCode::NO_CONTENT,
        Ok(Err(e)) => {
            if let UploadError::Io(m) = &e {
                tracing::error!(error = %m, "storing an upload chunk failed");
            }
            status_of(&e)
        }
        Err(e) => {
            tracing::error!(error = %e, "upload chunk task failed");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
