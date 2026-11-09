//! [`OfflineHandle`]: one library's offline copies and the thread that keeps them in step with
//! the server.
//!
//! A sync runs on the control thread (`asteria-offline-sync`), which also does every other
//! disk write the handle asks for, one command at a time:
//! 1. answers: each pin's documents, through [`OfflineCatalog::fetch`]; a pin the server no
//!    longer has is [`PinStatus::Gone`] and keeps its copies;
//! 2. the media those documents point to and the store lacks, in the order the pins were added;
//! 3. downloads, [`DOWNLOADS_AT_ONCE`] at a time, stopping at the size limit;
//! 4. clean-up of the copies and media no pin needs, only when every step succeeded.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use asteria_common_api::{ItemId, LibraryInfo, codes};
use bytes::Bytes;
use corundum_framework::egui;
use serde::{Deserialize, Serialize};
use url::Url;

use super::catalog::{Docs, Fetch, OfflineCatalog};
use super::store::{DocKey, MediaKey, OfflineStore, Pin, PinStatus, PinTarget, SyncMark};
use super::{AUDIO_TIMEOUT, DEFAULT_LIMIT_BYTES, DOWNLOADS_AT_ONCE, RETRY_ATTEMPTS};
use crate::media_cache::write_atomic;
use crate::profiles::Token;
use crate::rpc::{ClientError, RpcClient};
use crate::saver::Saver;

#[derive(Clone, Debug, PartialEq)]
pub enum SyncPhase {
    Idle,
    /// Downloading media: `done` of `total` files.
    Syncing {
        done: u32,
        total: u32,
    },
    /// The size limit stopped the downloads.
    Full,
    /// The sync stopped: the server could not be reached, or the disk could not be written.
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SyncProgress {
    pub phase: SyncPhase,
    /// Media files the server could not render (404, 422) in the latest sync.
    pub unsavable: u32,
    pub used_bytes: u64,
    pub limit_bytes: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PinInfo {
    pub pin: Pin,
    /// The stored media the pin's documents point to (a file shared with other pins counts
    /// for each). While a sync downloads, it may lag a few files behind.
    pub bytes: u64,
    /// The estimated size of all of the pin's media; `None` until its answers are fetched.
    pub estimate: Option<u64>,
}

/// `<data>/offline/settings.toml`.
#[derive(Serialize, Deserialize)]
struct Settings {
    limit_bytes: u64,
}

/// What the control thread is asked to do.
enum Command {
    /// Sync, if one is still asked for (requests made while one waits are merged).
    Sync,
    SavePins,
    /// Delete what only the unpinned `(kind, id)` needed.
    Unpin(DocKey),
    Clear,
    Stop,
}

/// A stored media file: `(file_hash, preset)`.
type MediaId = (String, String);

/// The media one pin's documents point to, each once.
struct PinMedia {
    ids: Vec<MediaId>,
    estimate: u64,
}

/// How many downloads go by between two measures of the pins' stored bytes.
const MEASURE_EVERY: u32 = 32;

struct State {
    /// In the order added.
    pins: Vec<Pin>,
    /// Which copies each pin needed when last fetched.
    pin_docs: HashMap<DocKey, Vec<DocKey>>,
    docs: Arc<Docs>,
    /// The size of each stored file a document points to.
    stored: HashMap<MediaId, u64>,
    /// The media of each pin whose answers are saved, worked out on the control thread
    /// whenever the copies change, so [`OfflineHandle::pin_info`] need not.
    pin_media: Arc<HashMap<DocKey, PinMedia>>,
    /// The stored bytes of each pin in `pin_media`, measured whenever `stored` changes (during
    /// the downloads, every [`MEASURE_EVERY`] files).
    pin_bytes: HashMap<DocKey, u64>,
    phase: SyncPhase,
    unsavable: u32,
    used_bytes: u64,
    limit_bytes: u64,
    /// The library as the session last saw it; `None` while offline.
    online: Option<LibraryInfo>,
    /// The mark of the last sync that finished.
    mark: SyncMark,
    /// The mark of the sync asked for or running.
    pending: Option<SyncMark>,
    /// A sync waits in the command queue.
    requested: bool,
    /// A sync was asked for while offline (a pin, "sync now"): run it once online.
    forced: bool,
    /// The sync asked for or running was forced: if it is cut short or fails, it runs again
    /// on the next connection.
    pending_forced: bool,
    /// A forced sync was cut short or failed: run it again once the server comes back after
    /// being lost.
    resume: bool,
    /// The last sync that ended without finishing (full, failed), so that the same mark does
    /// not run it again on every `library.info`.
    tried: Option<Tried>,
    /// Failed syncs in a row; each doubles the wait before the next try.
    failures: u32,
    /// The wait after the first failure.
    retry_backoff: Duration,
    /// The control thread has read the copies (or could not start).
    loaded: bool,
}

/// How the last sync that did not finish ended, for its mark.
enum Tried {
    /// It stopped at the size limit: run again only when something changes.
    Full(SyncMark),
    /// It failed: run again after a reconnect, or once `retry_at` has passed.
    Failed { mark: SyncMark, retry_at: Instant },
}

/// The wait before retrying a failed sync, after the first failure; it doubles with each
/// failure in a row, up to [`RETRY_BACKOFF_CAP`] times this.
const RETRY_BACKOFF: Duration = Duration::from_secs(60);
const RETRY_BACKOFF_CAP: u32 = 30;

/// What the handle and its control thread share.
struct Shared {
    store: OfflineStore,
    catalog: Arc<dyn OfflineCatalog>,
    url: Url,
    token: Token,
    ctx: egui::Context,
    state: Mutex<State>,
    /// Wakes a retry's wait when the sync is stopped, and [`OfflineHandle::wait_loaded`] once
    /// the copies are read. Paired with `state`.
    wake: Condvar,
    /// Set for good once the handle stops.
    stopped: AtomicBool,
    /// Cuts the running sync short (offline, everything cleared); reset when the next starts.
    interrupt: AtomicBool,
}

/// The window onto one library's offline copies and its sync thread. Clones share it;
/// the thread stops when the last clone is dropped or `stop` is called.
#[derive(Clone)]
pub struct OfflineHandle {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for OfflineHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineHandle").finish_non_exhaustive()
    }
}

struct Inner {
    shared: Arc<Shared>,
    commands: mpsc::Sender<Command>,
    /// Writes the size limit off the UI thread.
    settings: Option<Mutex<Saver<u64>>>,
}

impl OfflineHandle {
    /// Reads the pins, the last sync mark and the size limit from `settings`
    /// (`<data>/offline/settings.toml`, shared by every library; [`DEFAULT_LIMIT_BYTES`] when
    /// missing or unreadable), then starts the control thread, which loads the rest. Offline
    /// until [`set_online`](Self::set_online).
    pub fn start(
        store: OfflineStore,
        catalog: Arc<dyn OfflineCatalog>,
        url: Url,
        token: Token,
        settings: PathBuf,
        ctx: egui::Context,
    ) -> OfflineHandle {
        let state = State {
            pins: store.load_pins(),
            pin_docs: HashMap::new(),
            docs: Arc::default(),
            stored: HashMap::new(),
            pin_media: Arc::default(),
            pin_bytes: HashMap::new(),
            phase: SyncPhase::Idle,
            unsavable: 0,
            used_bytes: 0,
            limit_bytes: read_limit(&settings),
            online: None,
            mark: store.load_mark(),
            pending: None,
            requested: false,
            forced: false,
            pending_forced: false,
            resume: false,
            tried: None,
            failures: 0,
            retry_backoff: RETRY_BACKOFF,
            loaded: false,
        };
        let shared = Arc::new(Shared {
            store,
            catalog,
            url,
            token,
            ctx: ctx.clone(),
            state: Mutex::new(state),
            wake: Condvar::new(),
            stopped: AtomicBool::new(false),
            interrupt: AtomicBool::new(false),
        });
        let (commands, rx) = mpsc::channel();
        let thread_shared = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("asteria-offline-sync".into())
            .spawn(move || control(&thread_shared, &rx));
        if let Err(e) = spawned {
            tracing::warn!("cannot start the offline sync: {e}");
            shared.update(|s| {
                s.phase = SyncPhase::Failed(e.to_string());
                s.loaded = true;
            });
        }
        let saver = Saver::spawn("asteria-offline-settings", ctx, move |bytes: &u64| {
            write_limit(&settings, *bytes)
        });
        let settings = match saver {
            Ok(saver) => Some(Mutex::new(saver)),
            Err(e) => {
                tracing::warn!("cannot start saving the offline settings: {e}");
                None
            }
        };
        OfflineHandle {
            inner: Arc::new(Inner {
                shared,
                commands,
                settings,
            }),
        }
    }

    /// Keeps `target` offline; syncs at once if online. Pinning it again only syncs.
    pub fn pin(&self, target: PinTarget) {
        self.shared().update(|s| {
            if !s
                .pins
                .iter()
                .any(|p| p.target.kind == target.kind && p.target.id == target.id)
            {
                s.pins.push(Pin {
                    target,
                    added_unix: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs()),
                    status: PinStatus::Pending,
                });
            }
        });
        self.send(Command::SavePins);
        self.want_sync(true);
    }

    /// Drops the pin; its copies and the media no other pin needs are deleted in the background.
    pub fn unpin(&self, kind: &str, id: &str) {
        let removed = self.shared().update(|s| {
            let before = s.pins.len();
            s.pins
                .retain(|p| !(p.target.kind == kind && p.target.id == id));
            s.pins.len() != before
        });
        if removed {
            self.send(Command::Unpin((kind.into(), id.into())));
        }
    }

    pub fn status(&self, kind: &str, id: &str) -> Option<PinStatus> {
        let s = self.shared().lock();
        find(&s.pins, kind, id).map(|p| p.status)
    }

    /// Every pin, in the order added. Cheap: the sizes are worked out when the copies change.
    pub fn pins(&self) -> Vec<PinInfo> {
        let s = self.shared().lock();
        s.pins.iter().map(|p| info(&s, p)).collect()
    }

    pub fn pin_info(&self, kind: &str, id: &str) -> Option<PinInfo> {
        let s = self.shared().lock();
        find(&s.pins, kind, id).map(|p| info(&s, p))
    }

    pub fn progress(&self) -> SyncProgress {
        let s = self.shared().lock();
        SyncProgress {
            phase: s.phase.clone(),
            unsavable: s.unsavable,
            used_bytes: s.used_bytes,
            limit_bytes: s.limit_bytes,
        }
    }

    /// Syncs now (or once online), even if nothing seems to have changed.
    pub fn sync_now(&self) {
        self.want_sync(true);
    }

    /// Unpins everything and deletes every copy and media file of the library.
    pub fn clear_all(&self) {
        {
            let mut s = self.shared().lock();
            s.pins.clear();
            s.pin_docs.clear();
            s.docs = Arc::default();
            s.stored.clear();
            s.pin_media = Arc::default();
            s.pin_bytes.clear();
            s.mark = SyncMark::default();
            s.pending = None;
            self.shared().interrupt.store(true, Ordering::Release);
            self.shared().wake.notify_all();
        }
        self.shared().ctx.request_repaint();
        self.send(Command::Clear);
    }

    pub fn limit_bytes(&self) -> u64 {
        self.shared().lock().limit_bytes
    }

    /// Sets the size limit of every library's offline copies. Raising it past a sync that
    /// stopped [`Full`](SyncPhase::Full) syncs again; lowering it keeps the files there are.
    pub fn set_limit_bytes(&self, bytes: u64) {
        let raised_past_full = self.shared().update(|s| {
            let raised = s.phase == SyncPhase::Full && bytes > s.limit_bytes;
            s.limit_bytes = bytes;
            raised
        });
        if let Some(saver) = &self.inner.settings {
            saver.lock().unwrap_or_else(|e| e.into_inner()).save(bytes);
        }
        if raised_past_full {
            self.want_sync(true);
        }
    }

    /// The saved answer copies, as of the latest change.
    pub fn docs(&self) -> Arc<Docs> {
        self.shared().lock().docs.clone()
    }

    /// Waits until the control thread has read the saved copies, which it does first, so
    /// [`docs`](Self::docs) has them; returns at once if the handle is stopped. Blocks: only
    /// on worker threads.
    pub(crate) fn wait_loaded(&self) {
        let shared = self.shared();
        let mut s = shared.lock();
        while !s.loaded && !shared.is_stopped() {
            s = shared.wake.wait(s).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// The stored file for `(file_hash, preset)`, if there is one.
    pub fn media(&self, file_hash: &str, preset: &str) -> Option<PathBuf> {
        self.shared().store.media(file_hash, preset)
    }

    /// Whether `(file_hash, preset)` is stored, as the handle last measured: no disk I/O, so
    /// the UI thread may ask it for every row it draws.
    pub fn is_stored(&self, file_hash: &str, preset: &str) -> bool {
        self.shared()
            .lock()
            .stored
            .contains_key(&(file_hash.to_owned(), preset.to_owned()))
    }

    /// The stored file of `item` (as the saved documents know it) at `preset`.
    pub fn media_of_item(&self, item: &ItemId, preset: &str) -> Option<PathBuf> {
        let docs = self.docs();
        self.media(docs.hash_of(item)?, preset)
    }

    pub fn catalog(&self) -> &Arc<dyn OfflineCatalog> {
        &self.shared().catalog
    }

    /// The session's view of the server: syncs (if the mark changed) only while online.
    /// `None` stops a sync that is running. A sync that stopped full is not run again for the
    /// same mark; one that failed (or a forced one cut short) runs again when the server comes
    /// back after a `None`, or for the same mark once its retry is due (one minute, doubling
    /// with each failure in a row, up to 30).
    pub fn set_online(&self, library: Option<&LibraryInfo>) {
        match library {
            Some(info) => {
                self.shared().update(|s| {
                    // Back after the server was lost: a failed or cut-off sync runs again.
                    if s.online.is_none() {
                        if matches!(s.tried, Some(Tried::Failed { .. })) {
                            s.tried = None;
                        }
                        s.forced |= std::mem::take(&mut s.resume);
                    }
                    s.online = Some(info.clone());
                });
                self.want_sync(false);
            }
            None => {
                {
                    let mut s = self.shared().lock();
                    s.online = None;
                    s.pending = None;
                    self.shared().interrupt.store(true, Ordering::Release);
                    self.shared().wake.notify_all();
                }
                self.shared().ctx.request_repaint();
            }
        }
    }

    /// Sets the wait before a failed sync is tried again (one minute unless set; it doubles
    /// with each failure in a row). For tests.
    #[doc(hidden)]
    pub fn set_retry_backoff(&self, first: Duration) {
        self.shared().lock().retry_backoff = first;
    }

    /// Stops the control thread (after the request it is making, if any).
    pub fn stop(&self) {
        self.inner.stop();
    }

    fn shared(&self) -> &Shared {
        &self.inner.shared
    }

    fn send(&self, command: Command) {
        // Fails only once the thread has stopped.
        let _ = self.inner.commands.send(command);
    }

    /// See [`Shared::want_sync`].
    fn want_sync(&self, force: bool) {
        if self.shared().want_sync(force) {
            self.send(Command::Sync);
        }
    }
}

impl Inner {
    fn stop(&self) {
        {
            let _s = self.shared.lock();
            self.shared.stopped.store(true, Ordering::Release);
            self.shared.wake.notify_all();
        }
        let _ = self.commands.send(Command::Stop);
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.stop();
    }
}

fn info(s: &State, pin: &Pin) -> PinInfo {
    let key = (pin.target.kind.clone(), pin.target.id.clone());
    PinInfo {
        pin: pin.clone(),
        bytes: s.pin_bytes.get(&key).copied().unwrap_or(0),
        estimate: s.pin_media.get(&key).map(|m| m.estimate),
    }
}

/// Measures the stored bytes of each pin in `s.pin_media` (a file shared with other pins
/// counts for each).
fn measure(s: &mut State) {
    s.pin_bytes = s
        .pin_media
        .iter()
        .map(|(key, media)| {
            let bytes = media.ids.iter().filter_map(|id| s.stored.get(id)).sum();
            (key.clone(), bytes)
        })
        .collect();
}

fn find<'a>(pins: &'a [Pin], kind: &str, id: &str) -> Option<&'a Pin> {
    pins.iter()
        .find(|p| p.target.kind == kind && p.target.id == id)
}

/// The mark a sync of `pins` against `info` leaves: its epoch and revision, and a digest of
/// the pins' `(kind, id)`s.
fn mark_of(info: &LibraryInfo, pins: &[Pin]) -> SyncMark {
    let mut digest = blake3::Hasher::new();
    for pin in pins {
        for part in [&pin.target.kind, &pin.target.id] {
            digest.update(&(part.len() as u64).to_le_bytes());
            digest.update(part.as_bytes());
        }
    }
    SyncMark {
        epoch: info.epoch.clone(),
        revision: info.revision,
        pins_digest: digest.finalize().to_hex().to_string(),
    }
}

/// Whether the server says it no longer has what was asked: HTTP 404, or the JSON-RPC
/// `NOT_FOUND` error.
fn is_gone(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::NotFound
            | ClientError::Rpc {
                code: codes::NOT_FOUND,
                ..
            }
    )
}

/// Whether `e` means the server cannot be talked to at all, so the sync should stop.
fn is_fatal(e: &ClientError) -> bool {
    matches!(e, ClientError::Unreachable(_) | ClientError::Unauthorized)
}

fn disk_error(e: std::io::Error) -> String {
    format!("cannot write the offline copy: {e}")
}

fn read_limit(path: &Path) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<Settings>(&text).ok())
        .map_or(DEFAULT_LIMIT_BYTES, |s| s.limit_bytes)
}

fn write_limit(path: &Path, limit_bytes: u64) -> Result<(), String> {
    let text = toml::to_string(&Settings { limit_bytes }).map_err(|e| e.to_string())?;
    write_atomic(path, text.as_bytes(), true).map_err(|e| {
        tracing::warn!("cannot save the offline settings: {e}");
        e.to_string()
    })
}

/// The control thread: loads what [`OfflineHandle::start`] left, then runs commands until the
/// handle stops.
fn control(shared: &Shared, commands: &mpsc::Receiver<Command>) {
    shared.load();
    while let Ok(command) = commands.recv() {
        if shared.is_stopped() {
            return;
        }
        match command {
            Command::Sync => shared.sync(),
            Command::SavePins => {
                let pins = shared.lock().pins.clone();
                if let Err(e) = shared.store.save_pins(&pins) {
                    shared.fail(disk_error(e));
                }
            }
            Command::Unpin(key) => {
                shared.forget(&key);
                // The room it made may let the downloads the limit stopped go on.
                if shared.lock().phase == SyncPhase::Full && shared.want_sync(true) {
                    shared.sync();
                }
            }
            Command::Clear => shared.clear(),
            Command::Stop => return,
        }
    }
}

/// How a sync ended.
enum End {
    Done,
    Full,
    Failed(String),
    /// Stopped from outside (offline, cleared, the handle stopped).
    Interrupted,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Changes the state and repaints, so the UI shows it.
    fn update<R>(&self, change: impl FnOnce(&mut State) -> R) -> R {
        let r = change(&mut self.lock());
        self.ctx.request_repaint();
        r
    }

    /// Asks for a sync; `true` if one should run (the caller sends [`Command::Sync`], or runs
    /// it on the control thread). Online, it runs if `force`, or if the mark changed since
    /// the last sync that finished and is not the one asked for already, nor the one that
    /// last stopped full, nor the one that last failed while its retry is not due. Offline,
    /// `force` runs it once online.
    fn want_sync(&self, force: bool) -> bool {
        let mut s = self.lock();
        let Some(info) = &s.online else {
            s.forced |= force;
            return false;
        };
        let mark = mark_of(info, &s.pins);
        if !force && !s.forced {
            if mark == s.mark || s.pending.as_ref() == Some(&mark) {
                return false;
            }
            match &s.tried {
                Some(Tried::Full(m)) if *m == mark => return false,
                Some(Tried::Failed { mark: m, retry_at })
                    if *m == mark && Instant::now() < *retry_at =>
                {
                    return false;
                }
                _ => {}
            }
        }
        s.pending_forced |= force || s.forced;
        s.forced = false;
        s.pending = Some(mark);
        s.requested = true;
        true
    }

    fn fail(&self, reason: String) {
        tracing::warn!("offline sync: {reason}");
        self.update(|s| s.phase = SyncPhase::Failed(reason));
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    fn cancelled(&self) -> bool {
        self.is_stopped() || self.interrupt.load(Ordering::Acquire)
    }

    /// Sleeps for `wait` unless the sync is stopped first; `false` if it was.
    fn sleep(&self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        let mut s = self.lock();
        loop {
            if self.cancelled() {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            s = self
                .wake
                .wait_timeout(s, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Runs `request`, again after each 503's `Retry-After`, up to [`RETRY_ATTEMPTS`] times.
    fn retrying<T>(
        &self,
        mut request: impl FnMut() -> Result<T, ClientError>,
    ) -> Result<T, ClientError> {
        let stopped = || ClientError::Io("the offline sync was stopped".into());
        let mut retries = 0;
        loop {
            if self.cancelled() {
                return Err(stopped());
            }
            match request() {
                Err(ClientError::Retry { after }) if retries < RETRY_ATTEMPTS => {
                    retries += 1;
                    if !self.sleep(after) {
                        return Err(stopped());
                    }
                }
                result => return result,
            }
        }
    }

    /// Works out each saved pin's media from the copies (off the lock: for some apps it
    /// decodes every document), then measures them. On the control thread.
    fn index_pins(&self) {
        let (docs, pin_docs) = {
            let s = self.lock();
            (s.docs.clone(), s.pin_docs.clone())
        };
        let index: HashMap<DocKey, PinMedia> = pin_docs
            .iter()
            .map(|(key, refs)| {
                let mut seen = HashSet::new();
                let mut estimate = 0;
                let mut ids = Vec::new();
                for media in self.media_of_refs(&docs, refs) {
                    let id = (media.file_hash, media.preset);
                    if seen.insert(id.clone()) {
                        estimate += media.estimate;
                        ids.push(id);
                    }
                }
                (key.clone(), PinMedia { ids, estimate })
            })
            .collect();
        self.update(|s| {
            s.pin_media = Arc::new(index);
            measure(s);
        });
    }

    /// The media the documents `refs` point to (repeats included).
    fn media_of_refs<'a>(
        &'a self,
        docs: &'a Docs,
        refs: &'a [DocKey],
    ) -> impl Iterator<Item = MediaKey> + 'a {
        refs.iter()
            .filter_map(|(kind, id)| docs.get(kind, id))
            .flat_map(|doc| self.catalog.media_of(doc))
    }

    /// Every `(file_hash, preset)` the documents point to.
    fn media_ids(&self, docs: &Docs) -> HashSet<MediaId> {
        docs.iter()
            .flat_map(|doc| self.catalog.media_of(doc))
            .map(|m| (m.file_hash, m.preset))
            .collect()
    }

    /// The sizes of the stored files among `ids`.
    fn sizes(&self, ids: impl IntoIterator<Item = MediaId>) -> HashMap<MediaId, u64> {
        ids.into_iter()
            .filter_map(|id| {
                let len = fs::metadata(self.store.media(&id.0, &id.1)?).ok()?.len();
                Some((id, len))
            })
            .collect()
    }

    fn read_docs(&self) -> Arc<Docs> {
        Arc::new(Docs::new(self.store.read_docs(), &*self.catalog))
    }

    /// Reads the copies and measures the media, on the control thread.
    fn load(&self) {
        self.store.sweep_stale_tmp();
        let docs = self.read_docs();
        let stored = self.sizes(self.media_ids(&docs));
        let pin_docs = self.store.load_pin_docs();
        let used = self.store.media_bytes();
        self.update(|s| {
            s.docs = docs;
            s.stored = stored;
            s.pin_docs = pin_docs;
            s.used_bytes = used;
        });
        self.index_pins();
        self.lock().loaded = true;
        self.wake.notify_all();
    }

    /// Deletes the copies only the unpinned `key` needed, then every media file no copy
    /// points to.
    fn forget(&self, key: &DocKey) {
        let (pins, pin_docs, dropped) = {
            let mut s = self.lock();
            let dropped = s.pin_docs.remove(key).unwrap_or_default();
            (s.pins.clone(), s.pin_docs.clone(), dropped)
        };
        let still_needed: HashSet<&DocKey> = pin_docs.values().flatten().collect();
        let result = (|| {
            self.store.save_pins(&pins)?;
            self.store.save_pin_docs(&pin_docs)?;
            for (kind, id) in dropped.iter().filter(|r| !still_needed.contains(r)) {
                self.store.remove_doc(kind, id)?;
            }
            self.collect_garbage()
        })();
        if let Err(e) = result {
            self.fail(disk_error(e));
        }
    }

    /// Rereads the copies and deletes the media none of them points to.
    fn collect_garbage(&self) -> std::io::Result<()> {
        let docs = self.read_docs();
        let keep = self.media_ids(&docs);
        self.store.collect_garbage(&keep)?;
        let used = self.store.media_bytes();
        self.update(|s| {
            s.docs = docs;
            s.stored.retain(|id, _| keep.contains(id));
            s.used_bytes = used;
        });
        self.index_pins();
        Ok(())
    }

    fn clear(&self) {
        let result = self.store.clear();
        // A sync queued before `clear_all` may have run since, refilling the state (and the
        // mark) from what is now deleted.
        let pins = self.update(|s| {
            s.pin_docs.clear();
            s.docs = Arc::default();
            s.stored.clear();
            s.pin_media = Arc::default();
            s.pin_bytes.clear();
            s.used_bytes = 0;
            s.unsavable = 0;
            s.phase = SyncPhase::Idle;
            s.mark = SyncMark::default();
            s.pending = None;
            s.tried = None;
            s.failures = 0;
            for pin in &mut s.pins {
                pin.status = PinStatus::Pending;
            }
            s.pins.clone()
        });
        if pins.is_empty() {
            if let Err(e) = result {
                self.fail(disk_error(e));
            }
            return;
        }
        // Pins added since `clear_all` survive it, and are copied again.
        if let Err(e) = result.and_then(|()| self.store.save_pins(&pins)) {
            self.fail(disk_error(e));
            return;
        }
        {
            let mut s = self.lock();
            s.requested = true;
            s.pending_forced = true;
        }
        self.sync();
    }

    fn sync(&self) {
        let (pins, mark, forced) = {
            let mut s = self.lock();
            if !s.requested {
                return;
            }
            s.requested = false;
            let forced = std::mem::take(&mut s.pending_forced);
            let Some(info) = &s.online else {
                // Went offline since it was asked for: run it on the next connection.
                s.pending = None;
                s.forced = true;
                return;
            };
            let mark = mark_of(info, &s.pins);
            s.pending = Some(mark.clone());
            // Under the lock: going offline after this point interrupts this sync.
            self.interrupt.store(false, Ordering::Release);
            s.phase = SyncPhase::Syncing { done: 0, total: 0 };
            s.unsavable = 0;
            let pins: Vec<PinTarget> = s.pins.iter().map(|p| p.target.clone()).collect();
            (pins, mark, forced)
        };
        self.ctx.request_repaint();
        let mut end = self.run(&pins);
        if self.is_stopped() {
            end = End::Interrupted;
        }
        if matches!(end, End::Done)
            && let Err(e) = self.store.save_mark(&mark)
        {
            end = End::Failed(disk_error(e));
        }
        if let End::Failed(reason) = &end {
            tracing::warn!("offline sync: {reason}");
        }
        self.update(|s| {
            measure(s);
            if s.pending.as_ref() == Some(&mark) {
                s.pending = None;
            }
            s.phase = match end {
                End::Done => {
                    s.mark = mark;
                    s.tried = None;
                    s.failures = 0;
                    s.resume = false;
                    SyncPhase::Idle
                }
                End::Full => {
                    s.tried = Some(Tried::Full(mark));
                    s.failures = 0;
                    s.resume = false;
                    SyncPhase::Full
                }
                End::Failed(reason) => {
                    s.failures = s.failures.saturating_add(1);
                    let doublings = 2u32.saturating_pow(s.failures - 1).min(RETRY_BACKOFF_CAP);
                    s.tried = Some(Tried::Failed {
                        mark,
                        retry_at: Instant::now() + s.retry_backoff * doublings,
                    });
                    s.resume |= forced;
                    SyncPhase::Failed(reason)
                }
                End::Interrupted => {
                    s.resume |= forced;
                    SyncPhase::Idle
                }
            };
        });
    }

    /// One sync of `pins`. Deletes nothing unless every pin answered (or is gone) and every
    /// download either succeeded, was unsavable or was left for the size limit.
    fn run(&self, pins: &[PinTarget]) -> End {
        let rpc = match RpcClient::new(&self.url, self.token.clone()) {
            Ok(rpc) => rpc,
            Err(e) => return End::Failed(e.to_string()),
        };

        // 1. Answers.
        let mut problem: Option<String> = None;
        let mut gone = HashSet::new();
        let mut stopped_by: Option<End> = None;
        for pin in pins {
            if self.cancelled() {
                stopped_by = Some(End::Interrupted);
                break;
            }
            let result = self.catalog.fetch(
                pin,
                &mut SyncFetch {
                    shared: self,
                    rpc: &rpc,
                },
            );
            if self.cancelled() {
                stopped_by = Some(End::Interrupted);
                break;
            }
            let key = (pin.kind.clone(), pin.id.clone());
            match result {
                Ok(docs) => {
                    if let Err(e) = docs.iter().try_for_each(|d| self.store.write_doc(d)) {
                        stopped_by = Some(End::Failed(disk_error(e)));
                        break;
                    }
                    let refs = docs.into_iter().map(|d| (d.kind, d.id)).collect();
                    self.update(|s| {
                        s.pin_docs.insert(key.clone(), refs);
                        set_status(&mut s.pins, &key, PinStatus::Saved);
                    });
                }
                Err(e) if is_gone(&e) => {
                    gone.insert(key.clone());
                    self.update(|s| set_status(&mut s.pins, &key, PinStatus::Gone));
                }
                Err(e) if is_fatal(&e) => {
                    stopped_by = Some(End::Failed(e.to_string()));
                    break;
                }
                Err(e) => {
                    tracing::warn!("offline sync: cannot fetch {} {}: {e}", pin.kind, pin.id);
                    problem.get_or_insert(e.to_string());
                }
            }
        }
        // A stopped handle's files may already belong to a new one.
        if self.is_stopped() {
            return End::Interrupted;
        }
        let (saved_pins, pin_docs) = {
            let s = self.lock();
            (s.pins.clone(), s.pin_docs.clone())
        };
        if let Err(e) = self
            .store
            .save_pins(&saved_pins)
            .and_then(|()| self.store.save_pin_docs(&pin_docs))
        {
            return End::Failed(disk_error(e));
        }
        // New copies answer at once, while the media download.
        let docs = self.read_docs();
        self.update(|s| s.docs = docs.clone());
        self.index_pins();
        if let Some(end) = stopped_by {
            return end;
        }

        // 2. The media to download, in the order the pins were added.
        let mut seen = HashSet::new();
        let mut found = Vec::new();
        let mut needed = VecDeque::new();
        for pin in pins {
            let key = (pin.kind.clone(), pin.id.clone());
            let Some(refs) = pin_docs.get(&key).filter(|_| !gone.contains(&key)) else {
                continue;
            };
            for media in self.media_of_refs(&docs, refs) {
                if seen.insert((media.file_hash.clone(), media.preset.clone())) {
                    match self.store.media(&media.file_hash, &media.preset) {
                        Some(_) => found.push((media.file_hash, media.preset)),
                        None => needed.push_back(media),
                    }
                }
            }
        }
        let found = self.sizes(found);
        let total = u32::try_from(needed.len()).unwrap_or(u32::MAX);
        self.update(|s| {
            s.stored.extend(found);
            measure(s);
            s.phase = SyncPhase::Syncing { done: 0, total };
        });

        // 3. Downloads.
        let downloads = Downloads {
            rpc: &rpc,
            audio: OnceLock::new(),
            total,
            jobs: Mutex::new(Jobs {
                next: needed,
                reserved: 0,
                done: 0,
                full: false,
                fatal: None,
            }),
            problem: Mutex::new(problem),
        };
        std::thread::scope(|scope| {
            for _ in 1..DOWNLOADS_AT_ONCE {
                let spawned = std::thread::Builder::new()
                    .name("asteria-offline-download".into())
                    .spawn_scoped(scope, || self.download(&downloads));
                if let Err(e) = spawned {
                    tracing::warn!("cannot start an offline download thread: {e}");
                }
            }
            self.download(&downloads);
        });
        let jobs = downloads
            .jobs
            .into_inner()
            .unwrap_or_else(|e| e.into_inner());
        let problem = downloads
            .problem
            .into_inner()
            .unwrap_or_else(|e| e.into_inner());
        if self.cancelled() {
            return End::Interrupted;
        }
        if let Some(reason) = jobs.fatal {
            return End::Failed(reason);
        }
        if let Some(reason) = problem {
            return End::Failed(reason);
        }

        // 4. Clean-up: every pin answered, so what none of them needs can go.
        let needed_docs: HashSet<&DocKey> = pin_docs.values().flatten().collect();
        let result = (|| {
            for doc in docs.iter() {
                let key = (doc.kind.clone(), doc.id.clone());
                if !needed_docs.contains(&key) {
                    self.store.remove_doc(&doc.kind, &doc.id)?;
                }
            }
            self.collect_garbage()
        })();
        if let Err(e) = result {
            return End::Failed(disk_error(e));
        }
        if jobs.full { End::Full } else { End::Done }
    }

    /// A download thread: takes the next file until none is left, the size limit is reached
    /// or the sync stops.
    fn download(&self, downloads: &Downloads<'_>) {
        loop {
            if self.cancelled() {
                return;
            }
            let media = {
                let mut jobs = lock(&downloads.jobs);
                if jobs.full || jobs.fatal.is_some() {
                    return;
                }
                let Some(estimate) = jobs.next.front().map(|m| m.estimate) else {
                    return;
                };
                let (used, limit) = {
                    let s = self.lock();
                    (s.used_bytes, s.limit_bytes)
                };
                if used.saturating_add(jobs.reserved).saturating_add(estimate) > limit {
                    jobs.full = true;
                    return;
                }
                jobs.reserved += estimate;
                jobs.next.pop_front().expect("checked above")
            };
            let fetched = self.fetch_media(downloads, &media);
            if self.cancelled() {
                return;
            }
            let saved = match fetched {
                Ok(bytes) => self
                    .store
                    .put_media(&media.file_hash, &media.preset, &bytes)
                    .map(|_| bytes.len() as u64)
                    .map_err(|e| Fatal(disk_error(e))),
                Err(e) if is_gone(&e) || e == ClientError::Unprocessable => {
                    self.update(|s| s.unsavable += 1);
                    Ok(0)
                }
                Err(e) if is_fatal(&e) => Err(Fatal(e.to_string())),
                Err(e) => {
                    tracing::warn!("offline sync: cannot download a file: {e}");
                    lock(&downloads.problem).get_or_insert(e.to_string());
                    Ok(0)
                }
            };
            if let Ok(len) = saved
                && len > 0
            {
                let id = (media.file_hash.clone(), media.preset.clone());
                self.update(|s| {
                    s.used_bytes += len;
                    s.stored.insert(id, len);
                });
            }
            let mut jobs = lock(&downloads.jobs);
            jobs.reserved -= media.estimate;
            match saved {
                Ok(_) => {
                    jobs.done += 1;
                    let (done, total) = (jobs.done, downloads.total);
                    self.update(|s| {
                        if done % MEASURE_EVERY == 0 {
                            measure(s);
                        }
                        s.phase = SyncPhase::Syncing { done, total };
                    });
                }
                Err(Fatal(reason)) => {
                    jobs.fatal.get_or_insert(reason);
                    return;
                }
            }
        }
    }

    fn fetch_media(
        &self,
        downloads: &Downloads<'_>,
        media: &MediaKey,
    ) -> Result<Bytes, ClientError> {
        let rpc = if media.preset == "offline-audio" {
            downloads
                .audio
                .get_or_init(|| {
                    RpcClient::with_timeout(&self.url, self.token.clone(), AUDIO_TIMEOUT)
                })
                .as_ref()
                .map_err(Clone::clone)?
        } else {
            downloads.rpc
        };
        let path = format!("media/{}/{}", media.item.0, media.preset);
        self.retrying(|| rpc.get_bytes(&path))
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn set_status(pins: &mut [Pin], (kind, id): &DocKey, status: PinStatus) {
    if let Some(pin) = pins
        .iter_mut()
        .find(|p| &p.target.kind == kind && &p.target.id == id)
    {
        pin.status = status;
    }
}

/// A failure that stops every download of the sync.
struct Fatal(String);

/// One sync's downloads, shared by its download threads.
struct Downloads<'a> {
    rpc: &'a RpcClient,
    /// For `offline-audio`, which may take up to [`AUDIO_TIMEOUT`]; made when first needed.
    audio: OnceLock<Result<RpcClient, ClientError>>,
    total: u32,
    jobs: Mutex<Jobs>,
    /// The first failure that may pass (a 503 past the retries, an unexpected answer): the
    /// sync goes on, but ends failed and deletes nothing.
    problem: Mutex<Option<String>>,
}

struct Jobs {
    next: VecDeque<MediaKey>,
    /// The estimates of the files being downloaded, counted against the limit.
    reserved: u64,
    done: u32,
    full: bool,
    fatal: Option<String>,
}

/// The RPCs of a sync: retried after a 503, refused once the sync stops.
struct SyncFetch<'a> {
    shared: &'a Shared,
    rpc: &'a RpcClient,
}

impl Fetch for SyncFetch<'_> {
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ClientError> {
        self.shared.retrying(|| self.rpc.call(method, &params))
    }
}
