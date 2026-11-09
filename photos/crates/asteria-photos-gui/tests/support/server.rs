//! A real photos server on a temporary library, run in-process on a free local port (as
//! `asteria-photos-server` runs it, without the watcher and the background scan).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asteria_common_core::{Library, MediaKind};
use asteria_common_server::keys::{KeyStore, KeyVerifier};
use asteria_common_server::upload::UploadStore;
use asteria_common_server::{
    AppExtension, AppState, Cache, Processor, hash_pending, router, scan, spawn_writer,
};

struct Photos;
impl AppExtension for Photos {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Image | MediaKind::Video)
    }
}

/// The small JPEG the libraries are made of.
pub fn red_jpg() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../crates/asteria-common-core/tests/fixtures/red.jpg")
}

/// The server, its library and its owner's token. Stops when dropped.
pub struct TestServer {
    _dir: tempfile::TempDir,
    rt: Option<tokio::runtime::Runtime>,
    pub addr: SocketAddr,
    pub token: String,
    /// The library's root folder.
    pub root: PathBuf,
    pub lib: Arc<Library>,
    /// The path of every request the server received, in order.
    pub requests: Arc<Mutex<Vec<String>>>,
}

impl TestServer {
    /// A library holding a copy of a small JPEG under each of `files`; `prepare` may then
    /// change the folder (break a sidecar, say) before the scan.
    pub fn start(files: &[&str], prepare: impl FnOnce(&Path)) -> TestServer {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        std::fs::create_dir(&root).unwrap();
        let lib = Arc::new(
            Library::init(&root, "owner")
                .unwrap()
                .with_state_dir(dir.path().join("state")),
        );
        let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
        let red = red_jpg();
        for f in files {
            std::fs::copy(&red, root.join(f)).unwrap();
        }
        prepare(&root);
        scan(&lib, &cache, &Photos).unwrap();
        // The duplicate check compares content hashes.
        hash_pending(&lib, &cache, &AtomicBool::new(false)).unwrap();
        // The keys live in the temporary directory, not in the app's data directory.
        let keys = dir.path().join("keys");
        let token = KeyStore::open(&keys)
            .unwrap()
            .add(
                "owner",
                None,
                asteria_common_server::keys::ProfileId::new("owner").unwrap(),
            )
            .unwrap()
            .1
            .expose()
            .to_owned();
        let processor = Arc::new(Processor::new(
            dir.path().join("cache/thumbs"),
            1,
            Duration::from_secs(60),
        ));
        let uploads = Arc::new(UploadStore::open(lib.state_dir().unwrap()).unwrap());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let state_lib = lib.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let addr = rt.block_on(async {
            let (writer, _h) = spawn_writer(state_lib.clone(), cache.clone());
            let (rescan, rescan_rx) = tokio::sync::mpsc::channel(1);
            let state = AppState::new(
                state_lib,
                cache,
                writer,
                Arc::new(KeyVerifier::new(&keys)),
                asteria_photos_api::APP,
                asteria_photos_api::APP_API,
                Arc::new(AtomicBool::new(false)),
                rescan,
                Arc::new(Photos),
                processor,
                uploads,
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let _rescan_rx = rescan_rx;
                let app = router(state).layer(axum::middleware::from_fn(
                    move |req: axum::extract::Request, next: axum::middleware::Next| {
                        log.lock().unwrap().push(req.uri().path().to_owned());
                        next.run(req)
                    },
                ));
                axum::serve(listener, app).await.unwrap();
            });
            addr
        });
        TestServer {
            _dir: dir,
            rt: Some(rt),
            addr,
            token,
            root,
            lib,
            requests,
        }
    }

    /// Whether the server was asked for a path ending in `suffix`.
    pub fn was_asked(&self, suffix: &str) -> bool {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.ends_with(suffix))
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}
