//! The axum router and its bearer-token authentication.

use std::sync::Arc;

use asteria_framework_api::UPLOAD_CHUNK_SIZE;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};

use crate::mcp::AsteriaMcp;
use crate::media_http::media;
use crate::rpc::rpc;
use crate::state::AppState;
use crate::upload_http::upload_chunk;

/// Largest accepted request body. Bigger bodies get 413 before reaching `/rpc`: the only
/// non-200 answer from `/rpc` besides the 401 of the auth layer. Sized so that an
/// `organize.apply` of `MAX_OPS` operations (about 150 bytes each) fits. `/mcp` has the
/// same limit.
const BODY_LIMIT: usize = 4 * 1024 * 1024;

/// Builds the app. The auth layer wraps every route added before it, so routes added later
/// must be registered before the `layer` call below.
pub fn router(state: AppState) -> Router {
    let server = AsteriaMcp::new(state.clone());
    // `DefaultBodyLimit` only reaches axum extractors, so rmcp gets the limit itself. The
    // bearer token, not the Host header, is what keeps other sites out, and the server is
    // reached by whatever name its host has, so rmcp's loopback-only Host check is off.
    let mcp = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_max_request_body_bytes(BODY_LIMIT)
            .disable_allowed_hosts(),
    );
    Router::new()
        .route("/rpc", post(rpc))
        .nest_service("/mcp", mcp)
        .route("/media/{id}/{what}", get(media))
        // A chunk is far bigger than an RPC body; only this route lifts the limit.
        .route(
            "/upload/{session}/{chunk}",
            put(upload_chunk).layer(DefaultBodyLimit::max(UPLOAD_CHUNK_SIZE as usize)),
        )
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state)
}

/// Requires `Authorization: Bearer <token>`; on success stores the `UserId` in the request
/// extensions. Anything else is a 401 with an empty body. Neither the header nor the token is
/// ever logged.
async fn authenticate(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let user = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_token)
        .and_then(|token| state.tokens().verify(token));
    match user {
        Some(user) => {
            req.extensions_mut().insert(user);
            next.run(req).await
        }
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// The token of a `Bearer <token>` value; the scheme is case-insensitive (RFC 7235).
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("Bearer").then_some(token)
}
