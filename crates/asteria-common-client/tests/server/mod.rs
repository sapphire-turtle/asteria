//! A real photos server on a temporary library, run in-process (as `tests/session.rs` does), that
//! can also listen on an address chosen before it started. Not in `common`: the GUI crates
//! include that through `#[path]`, without the server.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use asteria_common_api::ItemId;
use asteria_common_client::profiles::ServerProfile;
use asteria_common_core::{Library, MediaKind};
use asteria_common_server::keys::{KeyStore, KeyVerifier};
use asteria_common_server::upload::UploadStore;
use asteria_common_server::{AppExtension, AppState, Cache, Processor, router, scan, spawn_writer};

use crate::common::profile;

struct Photos;
impl AppExtension for Photos {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Image | MediaKind::Video)
    }
}

/// A photos server on a temporary library holding `a.jpg`, listening on a free local port.
/// Stops when dropped.
pub struct TestServer {
    _dir: tempfile::TempDir,
    rt: Option<tokio::runtime::Runtime>,
    app: axum::Router,
    cache: Arc<Cache>,
    pub addr: SocketAddr,
    pub token: String,
    pub library_id: String,
}

impl TestServer {
    pub fn start() -> TestServer {
        Self::start_with(|_| {})
    }

    /// Like [`start`](Self::start); `prepare` runs on the library root before the scan.
    pub fn start_with(prepare: impl FnOnce(&Path)) -> TestServer {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        std::fs::create_dir(&root).unwrap();
        let lib = Arc::new(
            Library::init(&root, "owner")
                .unwrap()
                .with_state_dir(dir.path().join("state")),
        );
        let library_id = lib.config().id.0.to_string();
        let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
        let red = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../asteria-common-core/tests/fixtures/red.jpg");
        std::fs::copy(&red, root.join("a.jpg")).unwrap();
        prepare(&root);
        scan(&lib, &cache, &Photos).unwrap();
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
        let state_cache = cache.clone();
        let (app, addr) = rt.block_on(async {
            let (writer, _h) = spawn_writer(lib.clone(), state_cache.clone());
            let (rescan, rescan_rx) = tokio::sync::mpsc::channel(1);
            let state = AppState::new(
                lib,
                state_cache,
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
            let app = router(state);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let served = app.clone();
            tokio::spawn(async move {
                let _rescan_rx = rescan_rx;
                axum::serve(listener, served).await.unwrap();
            });
            (app, addr)
        });
        TestServer {
            _dir: dir,
            rt: Some(rt),
            app,
            cache,
            addr,
            token,
            library_id,
        }
    }

    /// Serves the same library on `addr` too (such as one taken from
    /// [`free_addr`](crate::common::free_addr) before a session found nothing there).
    pub fn listen_at(&self, addr: SocketAddr) {
        let app = self.app.clone();
        let rt = self.rt.as_ref().unwrap();
        let listener = rt.block_on(tokio::net::TcpListener::bind(addr)).unwrap();
        rt.spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
    }

    /// A profile for this server, with its owner's token.
    pub fn profile(&self) -> ServerProfile {
        profile(self.addr, &self.token)
    }

    /// The id of the item at `path` (library-relative).
    pub fn item_id(&self, path: &str) -> ItemId {
        ItemId(self.cache.get_by_path(path).unwrap().unwrap().id.0)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}
