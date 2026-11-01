//! Bearer-token authentication against the library's API keys. A key of the
//! [owner profile](crate::profile::OWNER) acts as the library's owner.

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use corundum_framework::keys::Verdict;

use crate::profile::OWNER;
use crate::state::AppState;

/// Requires `Authorization: Bearer <token>` of an unretired key; on success stores the owner's
/// `UserId` in the request extensions. Anything else is a 401 with an empty body and
/// `WWW-Authenticate: Bearer`, and a key of a profile asteria does not offer is a 403 with an
/// empty body. Neither the header nor the token is ever logged.
pub(crate) async fn authenticate(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_token)
        .map(str::to_owned);
    let verdict = match token {
        // The verifier reads the key files on a cache miss.
        Some(token) => {
            let verifier = state.keys().clone();
            tokio::task::spawn_blocking(move || verifier.verify(&token))
                .await
                .ok()
        }
        None => None,
    };
    match verdict {
        Some(Verdict::Accepted(key)) if key.profile.as_str() == OWNER => {
            req.extensions_mut().insert(state.library().owner());
            next.run(req).await
        }
        Some(Verdict::Accepted(_)) => StatusCode::FORBIDDEN.into_response(),
        _ => unauthorized(),
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))],
    )
        .into_response()
}

/// The token of a `Bearer <token>` value; the scheme is case-insensitive (RFC 7235).
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("Bearer").then_some(token)
}
