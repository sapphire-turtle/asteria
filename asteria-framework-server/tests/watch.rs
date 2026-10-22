use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asteria_framework::{Library, MediaKind};
use asteria_framework_api::MediaUpdateParams;
use asteria_framework_server::{Cache, MediaFilter, ScanLoop, spawn_scan_loop, spawn_writer};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Optionally blocks inside a scan until released.
#[derive(Default)]
struct Gate {
    armed: AtomicBool,
    entered: AtomicBool,
    release: Mutex<bool>,
    cv: std::sync::Condvar,
}
impl MediaFilter for Gate {
    fn accepts(&self, kind: MediaKind) -> bool {
        if self.armed.load(Ordering::SeqCst) {
            self.entered.store(true, Ordering::SeqCst);
            let mut r = self.release.lock().unwrap();
            while !*r {
                r = self.cv.wait(r).unwrap();
            }
        }
        matches!(kind, MediaKind::Image | MediaKind::Video)
    }
}

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    lib: Arc<Library>,
    cache: Arc<Cache>,
    gate: Arc<Gate>,
    passes: Arc<AtomicUsize>,
    scanning: Arc<AtomicBool>,
    tx: mpsc::Sender<()>,
    shutdown: CancellationToken,
    writer: asteria_framework_server::Writer,
    handle: tokio::task::JoinHandle<()>,
}

fn red(to: &Path) {
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../asteria-framework/tests/fixtures/red.jpg"),
        to,
    )
    .unwrap();
}

/// Prepares the library with `setup`, then starts the loop (unless `gate_armed`, which
/// arms the blocking filter before the first pass).
fn start(setup: impl FnOnce(&Path), gate_armed: bool) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lib");
    fs::create_dir(&root).unwrap();
    let lib = Arc::new(Library::init(&root, "owner").unwrap());
    let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
    setup(&root);
    let gate = Arc::new(Gate::default());
    gate.armed.store(gate_armed, Ordering::SeqCst);
    let (writer, _h) = spawn_writer(lib.clone(), cache.clone());
    let (tx, rx) = mpsc::channel(1);
    let passes = Arc::new(AtomicUsize::new(0));
    let scanning = Arc::new(AtomicBool::new(false));
    let shutdown = CancellationToken::new();
    let handle = spawn_scan_loop(ScanLoop {
        library: lib.clone(),
        cache: cache.clone(),
        filter: gate.clone(),
        writer: writer.clone(),
        scanning: scanning.clone(),
        trigger: rx,
        debounce: Duration::from_millis(100),
        period: Duration::from_secs(3600),
        shutdown: shutdown.clone(),
        passes: passes.clone(),
    });
    Env {
        _dir: dir,
        root,
        lib,
        cache,
        gate,
        passes,
        scanning,
        tx,
        shutdown,
        writer,
        handle,
    }
}

async fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn initial_pass_scans_and_clears_scanning() {
    let e = start(|r| red(&r.join("a.jpg")), false);
    wait_for("first pass", || e.passes.load(Ordering::SeqCst) >= 1).await;
    assert_eq!(e.cache.count().unwrap(), 1);
    assert!(!e.scanning.load(Ordering::SeqCst));
    assert!(
        e.cache
            .get_by_path("a.jpg")
            .unwrap()
            .unwrap()
            .hash
            .is_some()
    );
}

#[tokio::test]
async fn new_file_appears_after_debounce() {
    let e = start(|_| {}, false);
    wait_for("first pass", || e.passes.load(Ordering::SeqCst) >= 1).await;
    red(&e.root.join("new.jpg"));
    wait_for("new item", || e.cache.count().unwrap() == 1).await;
}

#[tokio::test]
async fn journal_writes_do_not_trigger_scans() {
    let e = start(|_| {}, false);
    wait_for("first pass", || e.passes.load(Ordering::SeqCst) >= 1).await;
    let dir = e.root.join(".asteria/journal");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("x.jsonl"), b"{}\n").unwrap();
    fs::write(e.root.join(".asteria/.x.asteria-tmp"), b"t").unwrap();
    fs::write(e.root.join(".y.jpg.xmp.asteria-tmp"), b"t").unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(e.passes.load(Ordering::SeqCst), 1);
    // control: a sidecar change does trigger
    fs::write(e.root.join("z.jpg.xmp"), b"x").unwrap();
    wait_for("sidecar-triggered pass", || {
        e.passes.load(Ordering::SeqCst) >= 2
    })
    .await;
}

#[tokio::test]
async fn triggers_coalesce() {
    let e = start(|r| red(&r.join("a.jpg")), true);
    wait_for("scan in progress", || e.gate.entered.load(Ordering::SeqCst)).await;
    assert!(e.scanning.load(Ordering::SeqCst));
    for _ in 0..5 {
        let _ = e.tx.try_send(());
    }
    e.gate.armed.store(false, Ordering::SeqCst);
    *e.gate.release.lock().unwrap() = true;
    e.gate.cv.notify_all();
    wait_for("second pass", || e.passes.load(Ordering::SeqCst) >= 2).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(e.passes.load(Ordering::SeqCst), 2);
    // and a later trigger still works
    e.tx.send(()).await.unwrap();
    wait_for("third pass", || e.passes.load(Ordering::SeqCst) >= 3).await;
}

#[tokio::test]
async fn moved_file_sidecar_is_relinked() {
    let e = start(
        |r| {
            red(&r.join("a.jpg"));
            fs::create_dir(r.join("sub")).unwrap();
        },
        false,
    );
    wait_for("first pass", || e.passes.load(Ordering::SeqCst) >= 1).await;
    let id = e.cache.get_by_path("a.jpg").unwrap().unwrap().id;
    e.writer
        .update(
            e.lib.owner(),
            MediaUpdateParams {
                id,
                rating: Some(5),
                subjects: None,
                hierarchical_subjects: None,
                description: None,
                label: None,
            },
        )
        .await
        .unwrap();
    wait_for("settled", || !e.scanning.load(Ordering::SeqCst)).await;
    fs::rename(e.root.join("a.jpg"), e.root.join("sub/a.jpg")).unwrap();
    wait_for("rating at new path", || {
        e.cache
            .get_by_path("sub/a.jpg")
            .unwrap()
            .is_some_and(|r| r.sidecar.owner_rating == Some(5))
    })
    .await;
    assert!(e.cache.get_by_path("a.jpg").unwrap().is_none());
}

#[tokio::test]
async fn shutdown_stops_the_loop() {
    let e = start(|_| {}, false);
    wait_for("first pass", || e.passes.load(Ordering::SeqCst) >= 1).await;
    e.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), e.handle)
        .await
        .unwrap()
        .unwrap();
}
