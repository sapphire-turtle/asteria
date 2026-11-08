//! A fake app for the offline cache: [`FakeCatalog`] keeps "sets" of items offline, and
//! [`FakeServer`] is a stub that answers `set.get` and the items' media as each test decides.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use asteria_common_api::{ItemId, LibraryInfo, codes};
use asteria_common_client::offline::{Doc, Docs, Fetch, MediaKey, OfflineCatalog, PinTarget};
use asteria_common_client::rpc::ClientError;
use asteria_common_client::session::{Request, Response};
use serde_json::{Value, json};
use url::Url;

use super::{
    DEADLINE, StubRequest, StubResponse, library_info, photos_server_info, serve, serve_at,
};

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

/// The content hash the fake server gives `item`: shaped like a blake3 hash, made with std
/// only (the app crates include these helpers without depending on blake3).
pub fn file_hash(item: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    item.hash(&mut hasher);
    format!("blake3:{:064x}", hasher.finish())
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
    /// The set, with these item ids, once the hold is released.
    Held(Vec<String>, Hold),
}

/// Keeps a [`SetAnswer::Held`] answer back until [`release`](Self::release)d.
#[derive(Clone, Debug, Default)]
pub struct Hold(Arc<(Mutex<bool>, Condvar)>);

impl Hold {
    pub fn release(&self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }

    fn wait(&self) {
        let released = self.0.0.lock().unwrap();
        let (released, _) = self
            .0
            .1
            .wait_timeout_while(released, DEADLINE, |r| !*r)
            .unwrap();
        assert!(*released, "the held answer was never released");
    }
}

#[derive(Default)]
struct Fake {
    sets: HashMap<String, SetAnswer>,
    /// The HTTP status of an item's media; 200 when absent.
    media_status: HashMap<String, u16>,
    /// The body of every media file; [`MEDIA_BYTES`] when `None`.
    media_bytes: Option<Vec<u8>>,
    /// How long each media answer is held back.
    media_delay: Duration,
    /// Every connection is closed without an answer, as a server that went away.
    gone: bool,
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
        Self::start_with(|handler| serve(handler))
    }

    /// Like [`start`](Self::start), on `addr`.
    pub fn start_at(addr: SocketAddr) -> FakeServer {
        Self::start_with(|handler| serve_at(addr, handler))
    }

    fn start_with(
        serve: impl FnOnce(Box<dyn Fn(&StubRequest) -> StubResponse + Send + Sync>) -> SocketAddr,
    ) -> FakeServer {
        let fake = Arc::new(Mutex::new(Fake::default()));
        let shared = fake.clone();
        let addr = serve(Box::new(move |req: &StubRequest| answer(&shared, req)));
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

    /// Sends `bytes` as every media file (instead of [`MEDIA_BYTES`]).
    pub fn media_bytes(&self, bytes: Vec<u8>) {
        self.fake.lock().unwrap().media_bytes = Some(bytes);
    }

    /// Holds back each media answer for `delay`.
    pub fn media_delay(&self, delay: Duration) {
        self.fake.lock().unwrap().media_delay = delay;
    }

    /// While `gone`, every connection is closed without an answer (nothing is logged).
    pub fn gone(&self, gone: bool) {
        self.fake.lock().unwrap().gone = gone;
    }

    /// How many requests were logged starting with `prefix` (`"set.get"`, `"GET /media/a/"`).
    pub fn requests(&self, prefix: &str) -> usize {
        let fake = self.fake.lock().unwrap();
        fake.log.iter().filter(|l| l.starts_with(prefix)).count()
    }
}

fn answer(fake: &Mutex<Fake>, req: &StubRequest) -> StubResponse {
    let mut fake = fake.lock().unwrap();
    if fake.gone {
        return StubResponse::hang_up();
    }
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
                let answer = fake.sets.get(&set).cloned().unwrap_or(SetAnswer::NotFound);
                // Not under the lock: a held answer must not block the other requests.
                drop(fake);
                match answer {
                    SetAnswer::Held(items, hold) => {
                        hold.wait();
                        StubResponse::rpc_result(id, set_result(&items))
                    }
                    SetAnswer::Items(items) => StubResponse::rpc_result(id, set_result(&items)),
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
    let response = match fake.media_status.get(item).copied().unwrap_or(200) {
        200 => {
            let body = fake.media_bytes.clone().unwrap_or(MEDIA_BYTES.to_vec());
            StubResponse::new(200, body).header("content-type", "image/webp")
        }
        status => StubResponse::new(status, Vec::new()),
    };
    let delay = fake.media_delay;
    // Not under the lock: the other requests go on meanwhile.
    drop(fake);
    std::thread::sleep(delay);
    response
}

fn set_result(items: &[String]) -> Value {
    let items: Vec<Value> = items
        .iter()
        .map(|i| json!({"id": i, "file_hash": file_hash(i)}))
        .collect();
    json!({ "items": items })
}

/// Polls `done` until it holds; fails the test after [`DEADLINE`].
pub fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < DEADLINE, "timed out waiting for {what}");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
