//! Helpers shared by the integration tests: profiles, waiting on a session, and a small HTTP
//! stub server whose answers each test decides.

// Each test binary uses its own subset of these helpers.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use asteria_framework_api::ClientInfo;
use asteria_framework_client::profiles::{ProfileId, ServerProfile, Token};
use asteria_framework_client::session::{Reply, Session, SessionState};
use grain_id::GrainId;
use serde_json::{Value, json};
use url::Url;

/// How long a test waits for something that should happen promptly.
pub const DEADLINE: Duration = Duration::from_secs(20);

pub fn profile(addr: SocketAddr, token: &str) -> ServerProfile {
    ServerProfile {
        id: ProfileId(GrainId::random()),
        name: "test".into(),
        url: Url::parse(&format!("http://{addr}")).unwrap(),
        token: Token::new(token),
    }
}

pub fn photos_client() -> ClientInfo {
    asteria_photos_api::client_info()
}

/// Polls until the session leaves `Connecting`, collecting any replies on the way.
pub fn wait_settled(session: &mut Session) -> Vec<Reply> {
    let start = Instant::now();
    let mut replies = Vec::new();
    loop {
        replies.extend(session.poll());
        if !matches!(session.state(), SessionState::Connecting) {
            return replies;
        }
        assert!(start.elapsed() < DEADLINE, "session never settled");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// One HTTP request as the stub received it.
pub struct StubRequest {
    /// `GET`, `POST`, ...
    pub method: String,
    /// The request target, e.g. `/rpc` or `/media/<id>/thumb`.
    pub path: String,
    pub body: Vec<u8>,
}

impl StubRequest {
    /// The JSON-RPC id and method, if this is a call to `/rpc`.
    pub fn rpc(&self) -> Option<(u64, String)> {
        if self.path != "/rpc" {
            return None;
        }
        let req: Value = serde_json::from_slice(&self.body).unwrap();
        Some((
            req["id"].as_u64().unwrap(),
            req["method"].as_str().unwrap().to_string(),
        ))
    }
}

/// The stub's answer to one request.
pub struct StubResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Runs once the answer has been written to the socket.
    pub after_write: Option<Box<dyn FnOnce() + Send>>,
    /// Hang up without answering, as a server that went away mid-request.
    pub hang_up: bool,
}

impl StubResponse {
    pub fn new(status: u16, body: Vec<u8>) -> StubResponse {
        StubResponse {
            status,
            headers: Vec::new(),
            body,
            after_write: None,
            hang_up: false,
        }
    }

    /// No answer at all: the connection is closed.
    pub fn hang_up() -> StubResponse {
        StubResponse {
            hang_up: true,
            ..StubResponse::new(500, Vec::new())
        }
    }

    /// A JSON-RPC success answer.
    pub fn rpc_result(id: u64, result: Value) -> StubResponse {
        let body = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
        let mut resp = StubResponse::new(200, body.into_bytes());
        resp.headers
            .push(("content-type".into(), "application/json".into()));
        resp
    }

    pub fn header(mut self, name: &str, value: &str) -> StubResponse {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// The `server.info` result of an asteria-photos server.
pub fn photos_server_info() -> Value {
    json!({
        "app": "asteria-photos",
        "app_api": asteria_photos_api::APP_API,
        "server_version": "0.0.0",
        "epoch": "e1",
    })
}

/// A `library.info` result for `library_id`.
pub fn library_info(library_id: &str) -> Value {
    json!({
        "library_id": library_id,
        "owner": "owner",
        "epoch": "e1",
        "item_count": 0,
        "scanning": false,
    })
}

/// Starts a stub HTTP/1.1 server on a free local port that answers every request with
/// `handler`, one thread per connection. It runs until the test process ends.
pub fn serve(handler: impl Fn(&StubRequest) -> StubResponse + Send + Sync + 'static) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let handler = handler.clone();
            std::thread::spawn(move || serve_connection(stream, &*handler));
        }
    });
    addr
}

fn serve_connection(stream: TcpStream, handler: &(dyn Fn(&StubRequest) -> StubResponse + Sync)) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts.next().unwrap_or_default().to_string();
        let mut content_length = 0usize;
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; content_length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let resp = handler(&StubRequest { method, path, body });
        if resp.hang_up {
            return;
        }
        let mut head = format!(
            "HTTP/1.1 {} Stub\r\ncontent-length: {}\r\n",
            resp.status,
            resp.body.len()
        );
        for (name, value) in &resp.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let written = writer
            .write_all(head.as_bytes())
            .and_then(|()| writer.write_all(&resp.body))
            .and_then(|()| writer.flush());
        if let Some(after) = resp.after_write {
            after();
        }
        if written.is_err() {
            return;
        }
    }
}
