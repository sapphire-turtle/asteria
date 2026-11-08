//! A fake app for the offline cache: [`FakeCatalog`] keeps "sets" of items offline, and
//! [`FakeServer`] is a stub that answers `set.get` and the items' media as each test decides.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use asteria_common_api::{ItemId, LibraryInfo, codes};
use asteria_common_client::offline::{Doc, Docs, Fetch, MediaKey, OfflineCatalog, PinTarget};
use asteria_common_client::rpc::ClientError;
use asteria_common_client::session::{Request, Response};
use serde_json::{Value, json};
use url::Url;

use super::{DEADLINE, StubRequest, StubResponse, library_info, photos_server_info, serve};

/// The body of every media file the fake server sends: as long as [`ESTIMATE`].
pub const MEDIA_BYTES: &[u8] = b"img";
/// The size [`FakeCatalog`] estimates for each media file.
pub const ESTIMATE: u64 = 3;
/// The library id the fake server reports.
pub const LIBRARY_ID: &str = "fake-lib";

/// Pins of kind `"set"`: one `set.get` call per pin, whose answer
/// `{"items":[{"id","file_hash"}]}` points to each item's `thumb`.
pub struct FakeCatalog;

impl OfflineCatalog for FakeCatalog {
    fn fetch(&self, pin: &PinTarget, rpc: &mut dyn Fetch) -> Result<Vec<Doc>, ClientError> {
        if pin.kind != "set" {
            return Err(ClientError::Protocol(format!("no such kind {}", pin.kind)));
        }
        let value = rpc.call("set.get", json!({"id": pin.id}))?;
        Ok(vec![Doc {
            kind: "set".into(),
            id: pin.id.clone(),
            value,
        }])
    }

    fn media_of(&self, doc: &Doc) -> Vec<MediaKey> {
        doc.value["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                Some(MediaKey {
                    item: ItemId(item["id"].as_str()?.into()),
                    file_hash: item["file_hash"].as_str()?.into(),
                    preset: "thumb".into(),
                    estimate: ESTIMATE,
                })
            })
            .collect()
    }

    fn answer(&self, request: &Request, docs: &Docs) -> Option<Result<Response, ClientError>> {
        let Request::Call { method, params } = request else {
            return None;
        };
        if method != "set.get" {
            return None;
        }
        let doc = docs.get("set", params["id"].as_str()?)?;
        Some(Ok(Response::Value(doc.value.clone())))
    }
}

/// A pin of the set `id`.
pub fn set_pin(id: &str) -> PinTarget {
    PinTarget {
        kind: "set".into(),
        id: id.into(),
        name: id.into(),
    }
}

/// The content hash the fake server gives `item`.
pub fn file_hash(item: &str) -> String {
    format!("blake3:{}", blake3::hash(item.as_bytes()).to_hex())
}

/// The library the fake server serves, at `revision`.
pub fn library(revision: u64) -> LibraryInfo {
    LibraryInfo {
        library_id: LIBRARY_ID.into(),
        owner: "owner".into(),
        epoch: "e1".into(),
        item_count: 0,
        scanning: false,
        revision,
    }
}

/// How the fake server answers `set.get` for one set.
#[derive(Clone, Debug)]
pub enum SetAnswer {
    /// The set, with these item ids.
    Items(Vec<String>),
    /// A JSON-RPC `NOT_FOUND` error.
    NotFound,
    /// The connection is closed without an answer.
    HangUp,
}

#[derive(Default)]
struct Fake {
    sets: HashMap<String, SetAnswer>,
    /// The HTTP status of an item's media; 200 when absent.
    media_status: HashMap<String, u16>,
    /// Every request: `set.get <id>`, `<method>` for other calls, or `GET <path>`.
    log: Vec<String>,
}

/// A stub server for [`FakeCatalog`]. It also answers the handshake (`server.info` of a photos
/// server, `library.info` of [`LIBRARY_ID`]). Unknown sets are `NOT_FOUND`.
#[derive(Clone)]
pub struct FakeServer {
    pub addr: SocketAddr,
    fake: Arc<Mutex<Fake>>,
}

impl FakeServer {
    pub fn start() -> FakeServer {
        let fake = Arc::new(Mutex::new(Fake::default()));
        let shared = fake.clone();
        let addr = serve(move |req: &StubRequest| answer(&shared, req));
        FakeServer { addr, fake }
    }

    pub fn url(&self) -> Url {
        Url::parse(&format!("http://{}", self.addr)).unwrap()
    }

    /// Serves the set `id` with `items`.
    pub fn set(&self, id: &str, items: &[&str]) {
        self.answer(
            id,
            SetAnswer::Items(items.iter().map(|s| s.to_string()).collect()),
        );
    }

    pub fn answer(&self, id: &str, answer: SetAnswer) {
        self.fake.lock().unwrap().sets.insert(id.into(), answer);
    }

    /// Answers `item`'s media with `status` (and no body unless it is 200).
    pub fn media_status(&self, item: &str, status: u16) {
        self.fake
            .lock()
            .unwrap()
            .media_status
            .insert(item.into(), status);
    }

    /// How many requests were logged starting with `prefix` (`"set.get"`, `"GET /media/a/"`).
    pub fn requests(&self, prefix: &str) -> usize {
        let fake = self.fake.lock().unwrap();
        fake.log.iter().filter(|l| l.starts_with(prefix)).count()
    }
}

fn answer(fake: &Mutex<Fake>, req: &StubRequest) -> StubResponse {
    let mut fake = fake.lock().unwrap();
    if let Some((id, method)) = req.rpc() {
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        return match method.as_str() {
            "server.info" => {
                fake.log.push(method);
                StubResponse::rpc_result(id, photos_server_info())
            }
            "library.info" => {
                fake.log.push(method);
                StubResponse::rpc_result(id, library_info(LIBRARY_ID))
            }
            "set.get" => {
                let set = body["params"]["id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                fake.log.push(format!("set.get {set}"));
                match fake.sets.get(&set).cloned().unwrap_or(SetAnswer::NotFound) {
                    SetAnswer::Items(items) => {
                        let items: Vec<Value> = items
                            .iter()
                            .map(|i| json!({"id": i, "file_hash": file_hash(i)}))
                            .collect();
                        StubResponse::rpc_result(id, json!({ "items": items }))
                    }
                    SetAnswer::NotFound => {
                        StubResponse::rpc_error(id, codes::NOT_FOUND, "no such set")
                    }
                    SetAnswer::HangUp => StubResponse::hang_up(),
                }
            }
            other => panic!("fake server got unexpected method {other}"),
        };
    }
    fake.log.push(format!("{} {}", req.method, req.path));
    let item = req
        .path
        .strip_prefix("/media/")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_default();
    match fake.media_status.get(item).copied().unwrap_or(200) {
        200 => StubResponse::new(200, MEDIA_BYTES.to_vec()).header("content-type", "image/webp"),
        status => StubResponse::new(status, Vec::new()),
    }
}

/// Polls `done` until it holds; fails the test after [`DEADLINE`].
pub fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < DEADLINE, "timed out waiting for {what}");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
