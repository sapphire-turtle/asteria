//! Bearer-token authentication against the library's API keys. A key acts as the user its
//! profile names.

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use corundum_framework::keys::Verdict;

use crate::profile::user_of;
use crate::state::AppState;

/// Requires `Authorization: Bearer <token>` of an unretired key; on success stores the
/// `UserId` of the user its profile names in the request extensions. Anything else is a 401
/// with an empty body and `WWW-Authenticate: Bearer`, and a key whose profile names no user
/// of the library is a 403 with an empty body. Neither the header nor the token is ever
/// logged.
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
        Some(Verdict::Accepted(key)) => match user_of(state.library(), &key.profile) {
            Some(user) => {
                req.extensions_mut().insert(user);
                next.run(req).await
            }
            None => StatusCode::FORBIDDEN.into_response(),
        },
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
