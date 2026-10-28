//! A Range-serving HTTP server for the stream tests: it records the requests and can be
//! scripted to answer 503, cut a connection, or stall.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use url::Url;

/// What the server does. The request numbers are 1-based, over all requests.
#[derive(Clone, Default)]
pub struct Script {
    pub body: Vec<u8>,
    /// The first this many requests get 503.
    pub busy: usize,
    pub retry_after: Option<&'static str>,
    /// This request gets half its body, then the connection is cut.
    pub cut_on: Option<usize>,
    /// This request gets no answer.
    pub stall_on: Option<usize>,
    /// The first request is answered this late.
    pub first_delay: Duration,
    /// Every request gets this status and no body.
    pub status: Option<u16>,
    /// The token the server wants; others get 401.
    pub token: Option<&'static str>,
    /// Answer 200 with the whole body whatever the `Range`.
    pub ignore_range: bool,
}

pub struct Recorded {
    /// The request path.
    pub path: String,
    /// The `Range` header's value.
    pub range: Option<String>,
    pub at: Instant,
}

struct Inner {
    script: Script,
    log: Mutex<Vec<Recorded>>,
}

pub struct TestServer {
    addr: SocketAddr,
    inner: Arc<Inner>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl TestServer {
    pub fn start(script: Script) -> TestServer {
        let inner = Arc::new(Inner {
            script,
            log: Mutex::new(Vec::new()),
        });
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let state = inner.clone();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let app = Router::new()
                    .route("/media/{id}/{preset}", get(serve))
                    .with_state(state);
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                addr_tx.send(listener.local_addr().unwrap()).unwrap();
                tokio::select! {
                    _ = axum::serve(listener, app) => {}
                    _ = stopped => {}
                }
            });
        });
        TestServer {
            addr: addr_rx.recv().unwrap(),
            inner,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    /// `<base>/media/<id>/original`.
    pub fn url(&self) -> Url {
        Url::parse(&format!("http://{}/media/track/original", self.addr)).unwrap()
    }

    pub fn inner_body(&self) -> &[u8] {
        &self.inner.script.body
    }

    pub fn ranges(&self) -> Vec<Option<String>> {
        self.inner
            .log
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.range.clone())
            .collect()
    }

    pub fn paths(&self) -> Vec<String> {
        self.inner
            .log
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.path.clone())
            .collect()
    }

    pub fn times(&self) -> Vec<Instant> {
        self.inner
            .log
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.at)
            .collect()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Reproducible bytes that do not compress or repeat on chunk boundaries.
pub fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

async fn serve(
    State(inner): State<Arc<Inner>>,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> Response {
    let s = &inner.script;
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let n = {
        let mut log = inner.log.lock().unwrap();
        log.push(Recorded {
            path: uri.path().to_owned(),
            range: range.clone(),
            at: Instant::now(),
        });
        log.len()
    };
    if n == 1 && !s.first_delay.is_zero() {
        tokio::time::sleep(s.first_delay).await;
    }
    if s.stall_on == Some(n) {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
    if let Some(token) = s.token {
        let want = format!("Bearer {token}");
        if headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            != Some(&want)
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    if let Some(code) = s.status {
        return StatusCode::from_u16(code).unwrap().into_response();
    }
    if n <= s.busy {
        let mut resp = StatusCode::SERVICE_UNAVAILABLE.into_response();
        if let Some(after) = s.retry_after {
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static(after));
        }
        return resp;
    }
    let total = s.body.len();
    let parsed = range.as_deref().and_then(|r| {
        let (a, b) = r.strip_prefix("bytes=")?.split_once('-')?;
        let a: usize = a.parse().ok()?;
        let b: usize = b.parse().ok()?;
        Some((a, b.min(total.saturating_sub(1))))
    });
    let (status, start, end) = match parsed {
        Some((a, b)) if !s.ignore_range => (StatusCode::PARTIAL_CONTENT, a, b),
        _ => (StatusCode::OK, 0, total.saturating_sub(1)),
    };
    let part = s.body.get(start..=end).unwrap_or_default().to_vec();
    let mut resp = Response::builder()
        .status(status)
        .header(header::CONTENT_LENGTH, part.len());
    if status == StatusCode::PARTIAL_CONTENT {
        resp = resp.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{total}"),
        );
    }
    // A body shorter than its Content-Length makes the server drop the connection.
    let sent = if s.cut_on == Some(n) {
        part[..part.len() / 2].to_vec()
    } else {
        part
    };
    resp.body(axum::body::Body::from(sent)).unwrap()
}
