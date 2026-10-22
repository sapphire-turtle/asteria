//! The axum router and its bearer-token authentication.

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;

use crate::rpc::rpc;
use crate::state::AppState;

/// Builds the app. The auth layer wraps every route added before it, so routes added later
/// must be registered before the `layer` call below.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/rpc", post(rpc))
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
        .and_then(|v| v.strip_prefix("Bearer "))
        .and_then(|token| state.tokens().verify(token));
    match user {
        Some(user) => {
            req.extensions_mut().insert(user);
            next.run(req).await
        }
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}
