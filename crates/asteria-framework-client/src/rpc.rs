//! A blocking JSON-RPC and media client for one server. It is only used off the UI thread,
//! from a [`Session`](crate::session::Session)'s worker.

use std::error::Error as _;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use asteria_framework_api::{HandshakeError, RpcOutcome, RpcRequest, RpcResponse};
use bytes::Bytes;
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, RETRY_AFTER};
use serde::Serialize;
use serde::de::DeserializeOwned;
use url::Url;

use crate::profiles::Token;

/// How long to wait for a TCP (and TLS) connection to the server.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a whole request, body included, may take.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The wait used for a 503 that has no usable `Retry-After` (the server sends `2`).
pub const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(2);

/// Why a request to the server did not produce a result.
///
/// How failures map onto the variants:
/// - no answer at all (connection refused, DNS failure, connect or request timeout, the
///   connection dropped while sending or reading the body) is [`Unreachable`](Self::Unreachable);
/// - HTTP 401 is [`Unauthorized`](Self::Unauthorized), 404 [`NotFound`](Self::NotFound),
///   422 [`Unprocessable`](Self::Unprocessable) (do not retry) and 503
///   [`Retry`](Self::Retry) (retry after the given time);
/// - a JSON-RPC `error` member (sent with HTTP 200) is [`Rpc`](Self::Rpc);
/// - any other answer the client cannot make sense of (another status, a body that is not the
///   expected JSON, a mismatched id) is [`Protocol`](Self::Protocol);
/// - a failure on this device (a worker thread or the HTTP client could not be created,
///   reading or writing local files) is [`Io`](Self::Io).
///
/// The texts carry no token: the token only ever travels in the `Authorization` header.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ClientError {
    #[error("cannot reach the server: {0}")]
    Unreachable(String),
    #[error("the server did not accept the access token")]
    Unauthorized,
    #[error(transparent)]
    Handshake(#[from] HandshakeError),
    #[error("the server answered with error {code}: {message}")]
    Rpc { code: i32, message: String },
    #[error("the server is busy; retry in {}s", after.as_secs())]
    Retry { after: Duration },
    #[error("the server cannot process this media")]
    Unprocessable,
    #[error("not found on the server")]
    NotFound,
    #[error("unexpected answer from the server: {0}")]
    Protocol(String),
    #[error("local error: {0}")]
    Io(String),
}

/// Talks to one server with one token. Blocking: never call it on the UI thread.
pub struct RpcClient {
    http: Client,
    /// Always ends in `/`, so relative paths join below it.
    base: Url,
    token: Token,
    next_id: AtomicU64,
}

impl fmt::Debug for RpcClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcClient")
            .field("base", &self.base.as_str())
            .finish_non_exhaustive()
    }
}

impl RpcClient {
    /// A client for the server at `base` (`http(s)://host:port`, optionally with a path
    /// prefix), using [`CONNECT_TIMEOUT`] and [`REQUEST_TIMEOUT`].
    pub fn new(base: &Url, token: Token) -> Result<RpcClient, ClientError> {
        let http = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| ClientError::Io(describe(e)))?;
        let mut base = base.clone();
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        Ok(RpcClient {
            http,
            base,
            token,
            next_id: AtomicU64::new(1),
        })
    }

    /// Calls `method` with `params` and decodes its result as `R`.
    pub fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = RpcRequest {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params,
        };
        let resp = self
            .http
            .post(self.url("rpc")?)
            .header(AUTHORIZATION, self.bearer())
            .json(&request)
            .send()
            .map_err(transport)?;
        let body = check_status(resp)?.bytes().map_err(transport)?;
        let response: RpcResponse<R> = serde_json::from_slice(&body)
            .map_err(|e| ClientError::Protocol(format!("{method}: {e}")))?;
        if response.id != id {
            return Err(ClientError::Protocol(format!(
                "{method}: answer has id {}, expected {id}",
                response.id
            )));
        }
        match response.outcome {
            RpcOutcome::Ok(r) => Ok(r),
            RpcOutcome::Err(e) => Err(ClientError::Rpc {
                code: e.code,
                message: e.message,
            }),
        }
    }

    /// `GET`s `path` (relative to the server's base, e.g. `media/<id>/thumb`) and returns the
    /// body.
    pub fn get_bytes(&self, path: &str) -> Result<Bytes, ClientError> {
        let resp = self
            .http
            .get(self.url(path.trim_start_matches('/'))?)
            .header(AUTHORIZATION, self.bearer())
            .send()
            .map_err(transport)?;
        check_status(resp)?.bytes().map_err(transport)
    }

    fn url(&self, path: &str) -> Result<Url, ClientError> {
        self.base
            .join(path)
            .map_err(|e| ClientError::Protocol(format!("bad path {path:?}: {e}")))
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.token.expose())
    }
}

/// Turns the non-success statuses the server uses into errors.
fn check_status(resp: Response) -> Result<Response, ClientError> {
    match resp.status() {
        s if s.is_success() => Ok(resp),
        StatusCode::UNAUTHORIZED => Err(ClientError::Unauthorized),
        StatusCode::NOT_FOUND => Err(ClientError::NotFound),
        StatusCode::UNPROCESSABLE_ENTITY => Err(ClientError::Unprocessable),
        StatusCode::SERVICE_UNAVAILABLE => Err(ClientError::Retry {
            after: retry_after(&resp),
        }),
        s => Err(ClientError::Protocol(format!("HTTP {s}"))),
    }
}

/// `Retry-After` in seconds; an HTTP date or a missing header means [`DEFAULT_RETRY_AFTER`].
fn retry_after(resp: &Response) -> Duration {
    resp.headers()
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(DEFAULT_RETRY_AFTER, Duration::from_secs)
}

/// A failure of the HTTP exchange itself.
fn transport(e: reqwest::Error) -> ClientError {
    if e.is_connect() || e.is_timeout() || e.is_request() || e.is_body() {
        ClientError::Unreachable(describe(e))
    } else {
        ClientError::Protocol(describe(e))
    }
}

/// The error and its causes on one line, without the URL.
fn describe(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        text.push_str(": ");
        text.push_str(&s.to_string());
        source = s.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_the_token() {
        let client = RpcClient::new(
            &Url::parse("http://127.0.0.1:1/x").unwrap(),
            Token::new("s3cret-token"),
        )
        .unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains("s3cret-token"), "{debug}");
        assert!(debug.contains("http://127.0.0.1:1/x/"), "{debug}");
    }

    #[test]
    fn paths_join_below_a_prefix() {
        let client =
            RpcClient::new(&Url::parse("http://h:1/asteria").unwrap(), Token::new("t")).unwrap();
        assert_eq!(
            client.url("rpc").unwrap().as_str(),
            "http://h:1/asteria/rpc"
        );
    }
}
