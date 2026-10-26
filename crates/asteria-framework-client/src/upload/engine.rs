//! The upload engine: one thread per session that takes the queue through hashing, the
//! duplicate check, chunked sending and commit (spec §4.7, §5.3). [`Uploads`] is its side on
//! the UI thread.
//!
//! The queue lives in memory behind a mutex shared by three sides, none of which holds it
//! across I/O:
//! - the UI side only changes it in memory (a decision, a cancel) and wakes the others;
//! - the engine thread plans, hashes and talks to the server;
//! - the saver thread writes the queue file whenever it changed (progress alone at most every
//!   [`PROGRESS_SAVE_EVERY`]), so a decision reaches the disk even while the engine waits on
//!   the network.
//!
//! Copies for the UI are published to a slot of their own, by the engine at most every
//! [`PUBLISH_EVERY`] (and before it waits), by the saver, and by the UI side after a command,
//! so taking one never waits on the queue.
//!
//! One queue file has one owner in the process ([`claim`]): a session that opens on a library
//! whose previous session is still finishing a request waits for it to release the file,
//! which it does once that request's outcome is applied and saved.

use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant, UNIX_EPOCH};

use asteria_framework::MediaKind;
use asteria_framework::hash::{HashScope, content_hash, file_hash};
use asteria_framework_api::{
    HashRef, UPLOAD_ABORT, UPLOAD_BEGIN, UPLOAD_CHECK, UPLOAD_CHUNK_SIZE, UPLOAD_COMMIT,
    UPLOAD_STATUS, UploadBegin, UploadBeginParams, UploadCheckParams, UploadCheckResult,
    UploadCommitParams, UploadCommitResult, UploadFile, UploadSessionParams, UploadStatus,
    UploadWarning, codes,
};
use corundum_framework::egui;
use serde::Serialize;
use serde::de::{DeserializeOwned, IgnoredAny};
use url::Url;

use super::plan::{PlanWarning, plan_drop};
use super::queue::{
    GroupId, GroupState, QueueSummary, QueuedFile, QueuedGroup, UploadQueue, save_groups,
};
use crate::profiles::Token;
use crate::rpc::{ClientError, RpcClient, UPLOAD_TIMEOUT};
use crate::session::{Gate, Health, lock_gate, note_health};

/// The first wait after the server could not be reached or asked to wait, in milliseconds;
/// each further failure doubles it, up to [`BACKOFF_MAX_MS`].
static BACKOFF_INITIAL_MS: AtomicU64 = AtomicU64::new(2_000);
static BACKOFF_MAX_MS: AtomicU64 = AtomicU64::new(60_000);

/// Sets the retry waits (normally 2 s doubling up to 60 s) for every engine in the process.
/// For tests, which cannot sit through the real ones.
#[doc(hidden)]
pub fn set_retry_backoff(initial: Duration, max: Duration) {
    BACKOFF_INITIAL_MS.store(initial.as_millis() as u64, Ordering::Relaxed);
    BACKOFF_MAX_MS.store(max.as_millis() as u64, Ordering::Relaxed);
}

/// Most hashes sent in one `upload.check` (the server takes up to 1000).
const CHECK_BATCH: usize = 500;
/// Chunk progress alone is saved at most this often.
const PROGRESS_SAVE_EVERY: Duration = Duration::from_secs(2);
/// How long one hashing pass runs before the engine looks at its messages.
const HASH_SLICE: Duration = Duration::from_millis(250);
/// While busy, the engine publishes a new snapshot at most this often.
const PUBLISH_EVERY: Duration = Duration::from_millis(100);
/// Groups taken per scan of the queue while hashing.
const HASH_PICK: usize = 64;

/// What the engine is doing, as far as the UI cares.
#[derive(Debug, Clone, PartialEq)]
pub enum UploadsState {
    /// The queue is not loaded yet: the previous session on this library may still be
    /// finishing a request.
    Starting,
    /// Sending, or nothing to send.
    Running,
    /// The server could not be reached or asked to wait; the engine tries again after a pause
    /// (2 s, doubling up to 60 s). Local work (planning, hashing) goes on meanwhile.
    Waiting { error: ClientError },
    /// The engine no longer talks to the server: the token was refused, the server now serves
    /// another library, or the client could not be built. Nothing is lost; a new session
    /// (after fixing the cause) carries on.
    Stopped { error: ClientError },
    /// The queue file could not be read. Nothing is sent, and the file is left as it is.
    Unavailable { reason: String },
}

/// A copy of the queue for the UI.
#[derive(Debug, Clone)]
pub struct UploadSnapshot {
    pub groups: Arc<Vec<QueuedGroup>>,
    pub summary: QueueSummary,
    /// Files left out of the latest drop.
    pub excluded: usize,
    /// Warnings from the latest drop.
    pub warnings: Vec<PlanWarning>,
    pub state: UploadsState,
}

/// Which media kinds the app `app` uploads (sidecars always go with their media).
fn upload_accept(app: &str) -> fn(MediaKind) -> bool {
    match app {
        "asteria-photos" => |k| matches!(k, MediaKind::Image | MediaKind::Video),
        "asteria-music" => |k| matches!(k, MediaKind::Audio),
        _ => |_| false,
    }
}

/// Everything the engine shares with [`Uploads`].
struct Shared {
    queue: UploadQueue,
    excluded: usize,
    warnings: Vec<PlanWarning>,
    state: UploadsState,
    /// Server sessions nothing refers to any more, to abort.
    orphans: Vec<String>,
    /// Bumped on every change the UI can see.
    version: u64,
    /// A group or file changed since the last save.
    dirty: bool,
    /// Only `sent` changed since the last save.
    progress: bool,
    /// The engine has stopped: the saver writes what is left and releases the file.
    closing: bool,
}

struct Inner {
    shared: Mutex<Shared>,
    /// Signalled on every change, for the saver.
    changed: Condvar,
    /// The latest snapshot, and the version of `shared` it shows.
    published: Mutex<(u64, UploadSnapshot)>,
    ctx: egui::Context,
    /// Set when the [`Uploads`] is dropped.
    dropped: AtomicBool,
}

/// How a change to [`Shared`] is to be saved.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// A group or file changed: saved at once.
    State,
    /// Only chunk progress: saved every [`PROGRESS_SAVE_EVERY`].
    Progress,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs `f`; if it reports a change, marks it for the next snapshot and for saving.
    fn change(&self, kind: Kind, f: impl FnOnce(&mut Shared) -> bool) -> bool {
        let mut s = self.lock();
        let changed = f(&mut s);
        if changed {
            s.version += 1;
            match kind {
                Kind::State => s.dirty = true,
                Kind::Progress => s.progress = true,
            }
            self.changed.notify_all();
        }
        changed
    }

    fn set_state(&self, state: UploadsState) {
        let mut s = self.lock();
        if s.state != state {
            s.state = state;
            s.version += 1;
        }
    }

    /// Publishes a snapshot of the queue if it changed since the last one, and asks egui to
    /// repaint. Returns the groups published.
    fn publish(&self) -> Arc<Vec<QueuedGroup>> {
        let (version, snap) = {
            let s = self.lock();
            {
                let published = self.published.lock().unwrap_or_else(|e| e.into_inner());
                if published.0 == s.version {
                    return published.1.groups.clone();
                }
            }
            (s.version, snapshot_of(&s))
        };
        let groups = snap.groups.clone();
        {
            let mut published = self.published.lock().unwrap_or_else(|e| e.into_inner());
            // Another thread may have published a later version meanwhile.
            if published.0 < version {
                *published = (version, snap);
            }
        }
        self.ctx.request_repaint();
        groups
    }
}

fn snapshot_of(s: &Shared) -> UploadSnapshot {
    UploadSnapshot {
        groups: Arc::new(s.queue.groups.clone()),
        summary: s.queue.summary(),
        excluded: s.excluded,
        warnings: s.warnings.clone(),
        state: s.state.clone(),
    }
}

enum Msg {
    Drop(Vec<PathBuf>),
    Wake,
}

/// The uploads of one session, for the UI. Every method returns at once: planning, hashing
/// and sending happen on the engine's thread, which stops when this is dropped (with its
/// [`Session`](crate::session::Session)).
pub struct Uploads {
    inner: Arc<Inner>,
    tx: mpsc::Sender<Msg>,
}

/// What the engine needs from its session.
pub(crate) struct EngineConfig {
    pub url: Url,
    pub token: Token,
    pub app: String,
    pub ctx: egui::Context,
    pub gate: Arc<Mutex<Gate>>,
    pub health: Health,
    /// The session's worker flag: set once the session is dropped.
    pub cancelled: Arc<AtomicBool>,
    /// `<data_root>/uploads/<library_id>.json`.
    pub path: PathBuf,
}

impl Uploads {
    /// Starts the engine thread for the queue at `cfg.path`. It claims the file (waiting for
    /// a previous session on this library to let go of it), loads it, and starts the saver.
    /// If the file cannot be read, nothing runs: the snapshot reports the queue unavailable
    /// and the file is never written.
    pub(crate) fn start(cfg: EngineConfig) -> Uploads {
        let shared = Shared {
            queue: UploadQueue::default(),
            excluded: 0,
            warnings: Vec::new(),
            state: UploadsState::Starting,
            orphans: Vec::new(),
            version: 0,
            dirty: false,
            progress: false,
            closing: false,
        };
        let first = snapshot_of(&shared);
        let inner = Arc::new(Inner {
            shared: Mutex::new(shared),
            changed: Condvar::new(),
            published: Mutex::new((0, first)),
            ctx: cfg.ctx.clone(),
            dropped: AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::channel();
        let engine_inner = inner.clone();
        let spawned = std::thread::Builder::new()
            .name("asteria-uploads".into())
            .spawn(move || run_engine(engine_inner, rx, cfg));
        if let Err(e) = spawned {
            inner.set_state(UploadsState::Stopped {
                error: ClientError::Io(format!("cannot start the upload thread: {e}")),
            });
            inner.publish();
        }
        Uploads { inner, tx }
    }

    /// Plans `paths` (files and folders) on the engine's thread and queues what they hold.
    pub fn drop_paths(&self, paths: Vec<PathBuf>) {
        let _ = self.tx.send(Msg::Drop(paths));
    }

    /// Answers a group waiting on a duplicate: upload it anyway (`keep`) or discard it.
    pub fn decide(&self, group: GroupId, keep: bool) -> bool {
        self.command(|q| q.decide(group, keep))
    }

    /// [`decide`](Self::decide) for every waiting group.
    pub fn decide_all(&self, keep: bool) -> bool {
        self.command(|q| q.decide_all(keep))
    }

    /// Starts a failed group again.
    pub fn retry(&self, group: GroupId) -> bool {
        self.command(|q| q.retry(group))
    }

    /// Discards a group that is not finished; its server sessions are aborted.
    pub fn cancel(&self, group: GroupId) -> bool {
        self.command(|q| q.cancel(group))
    }

    /// Removes the finished (done and discarded) groups from the queue. A discarded group
    /// whose server sessions are not aborted yet stays until they are.
    pub fn clear_finished(&self) -> bool {
        let changed = self.inner.change(Kind::State, |s| {
            let before = s.queue.groups.len();
            s.queue.groups.retain(|g| match g.state {
                GroupState::Done { .. } => false,
                GroupState::Discarded => g.files.iter().any(|f| f.session.is_some()),
                _ => true,
            });
            s.queue.groups.len() != before
        });
        if changed {
            self.inner.publish();
            let _ = self.tx.send(Msg::Wake);
        }
        changed
    }

    /// The latest published copy of the queue. Cheap, and never waits on the engine; while
    /// the engine is busy it may be up to [`PUBLISH_EVERY`] old.
    pub fn snapshot(&self) -> UploadSnapshot {
        let published = self
            .inner
            .published
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        published.1.clone()
    }

    /// Applies `f` at once (the next snapshot shows it) and wakes the engine to act on it.
    fn command(&self, f: impl FnOnce(&mut UploadQueue) -> bool) -> bool {
        let changed = self.inner.change(Kind::State, |s| f(&mut s.queue));
        if changed {
            self.inner.publish();
            let _ = self.tx.send(Msg::Wake);
        }
        changed
    }
}

/// Known limitation: the saver and the engine are detached threads. If the process exits
/// right after the drop (or at any time), a change the saver has not written yet is lost,
/// and so is the outcome of a request still in flight.
impl Drop for Uploads {
    fn drop(&mut self) {
        self.inner.dropped.store(true, Ordering::Release);
        // Dropping `tx` right after this wakes the engine, which then exits.
    }
}

/// The queue files owned by an engine of this process.
static OWNED: LazyLock<(Mutex<HashSet<PathBuf>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(HashSet::new()), Condvar::new()));

/// Ownership of one queue file; released when dropped.
struct Claim(PathBuf);

impl Drop for Claim {
    fn drop(&mut self) {
        let (owned, released) = &*OWNED;
        owned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
        released.notify_all();
    }
}

/// Takes ownership of the queue file `path`, waiting while another engine holds it. `None`
/// if `give_up` turns true first.
fn claim(path: &Path, give_up: impl Fn() -> bool) -> Option<Claim> {
    let (owned, released) = &*OWNED;
    let mut set = owned.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if give_up() {
            return None;
        }
        if set.insert(path.to_owned()) {
            return Some(Claim(path.to_owned()));
        }
        set = released
            .wait_timeout(set, Duration::from_millis(50))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

/// The engine thread: claims and loads the queue, starts the saver, runs the engine, and
/// on the way out lets the saver write the last state and release the file.
fn run_engine(inner: Arc<Inner>, rx: mpsc::Receiver<Msg>, cfg: EngineConfig) {
    let cancelled = cfg.cancelled.clone();
    let stopped = || cancelled.load(Ordering::Acquire) || inner.dropped.load(Ordering::Acquire);
    let Some(claim) = claim(&cfg.path, stopped) else {
        return;
    };
    match UploadQueue::load(&cfg.path) {
        Ok(queue) => {
            let mut s = inner.lock();
            s.queue = queue;
            s.state = UploadsState::Running;
            s.version += 1;
        }
        Err(e) => {
            tracing::warn!(path = %cfg.path.display(), error = %e, "upload queue unavailable");
            let reason = format!("{}: {e}", cfg.path.display());
            inner.set_state(UploadsState::Unavailable { reason });
            inner.publish();
            return;
        }
    }
    inner.publish();
    let saver_inner = inner.clone();
    let path = cfg.path.clone();
    let spawned = std::thread::Builder::new()
        .name("asteria-uploads-save".into())
        .spawn(move || save_loop(saver_inner, path, claim));
    if let Err(e) = spawned {
        inner.set_state(UploadsState::Stopped {
            error: ClientError::Io(format!("cannot start the upload saver: {e}")),
        });
        inner.publish();
        return;
    }
    // Set even if the engine panics, so the saver still saves and releases the claim.
    let _closing = Closing(inner.clone());
    Engine::new(inner, rx, cfg).run();
}

/// Tells the saver, when dropped (also on unwind), that the engine has stopped: it then
/// saves whatever the last step applied (a commit's Done, say) and releases the claim.
struct Closing(Arc<Inner>);

impl Drop for Closing {
    fn drop(&mut self) {
        self.0.lock().closing = true;
        self.0.changed.notify_all();
    }
}

/// Writes the queue file whenever the queue changed; chunk progress alone at most every
/// [`PROGRESS_SAVE_EVERY`]. Once the engine is closing, writes what is left and returns,
/// releasing `claim`.
fn save_loop(inner: Arc<Inner>, path: PathBuf, claim: Claim) {
    let mut last_save: Option<Instant> = None;
    loop {
        {
            let mut s = inner.lock();
            loop {
                let progress_due = s.progress
                    && (s.closing || last_save.is_none_or(|t| t.elapsed() >= PROGRESS_SAVE_EVERY));
                if s.dirty || progress_due {
                    break;
                }
                if s.closing {
                    drop(s);
                    drop(claim);
                    return;
                }
                let wait = match (s.progress, last_save) {
                    (true, Some(t)) => PROGRESS_SAVE_EVERY.saturating_sub(t.elapsed()),
                    _ => Duration::from_secs(3600),
                };
                s = inner
                    .changed
                    .wait_timeout(s, wait)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            s.dirty = false;
            s.progress = false;
        }
        let groups = inner.publish();
        if let Err(e) = save_groups(&groups, &path) {
            tracing::warn!(path = %path.display(), error = %e, "cannot save the upload queue");
        }
        last_save = Some(Instant::now());
    }
}

/// What a step of the engine came to.
enum Step {
    /// Something was done; look for more at once.
    Worked,
    /// Nothing to do until a message arrives.
    Idle,
    /// Nothing to do before this long has passed (or a message arrives).
    Wait(Duration),
}

/// Whether to try again later rather than give up on the group.
fn transient(e: &ClientError) -> bool {
    match e {
        ClientError::Unreachable(_) | ClientError::Retry { .. } => true,
        // BUSY from a call; 409 from a chunk: a commit holds the session.
        ClientError::Rpc { code, .. } => *code == codes::BUSY || *code == 409,
        _ => false,
    }
}

/// Whether the engine must stop talking to the server altogether.
fn halting(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::Unauthorized | ClientError::LibraryChanged | ClientError::Handshake(_)
    )
}

fn not_found(e: &ClientError) -> bool {
    match e {
        ClientError::NotFound => true,
        ClientError::Rpc { code, .. } => *code == codes::NOT_FOUND,
        _ => false,
    }
}

fn is_xmp(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("xmp"))
}

/// Size and modification time (nanoseconds since the Unix epoch) of `path`.
fn stat(path: &Path) -> io::Result<(u64, i128)> {
    let meta = fs::metadata(path)?;
    let mtime = meta.modified()?;
    let ns = match mtime.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    };
    Ok((meta.len(), ns))
}

/// Whether `f` on disk still has the size and time it was hashed with.
fn unchanged(f: &QueuedFile) -> bool {
    stat(&f.local).is_ok_and(|s| s == (f.size, f.mtime_ns))
}

/// Length of chunk `index` of a `size`-byte file.
fn chunk_len(size: u64, index: u64) -> u64 {
    size.saturating_sub(index.saturating_mul(UPLOAD_CHUNK_SIZE))
        .min(UPLOAD_CHUNK_SIZE)
}

fn read_chunk(path: &Path, index: u64, size: u64) -> io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(index * UPLOAD_CHUNK_SIZE))?;
    let mut buf = vec![0; chunk_len(size, index) as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// Hashes a group's files. Returns them filled in, and the sessions that no longer match
/// their file (to abort); or why a file could not be read.
fn hash_files(files: &[QueuedFile]) -> Result<(Vec<QueuedFile>, Vec<String>), String> {
    let mut out = Vec::with_capacity(files.len());
    let mut stale = Vec::new();
    for f in files {
        let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", f.local.display());
        // Taken before hashing: a change while hashing then shows up before sending.
        let (size, mtime_ns) = stat(&f.local).map_err(|e| fail(&e))?;
        let (whole, payload) = if is_xmp(&f.local) {
            (file_hash(&f.local).map_err(|e| fail(&e))?, None)
        } else {
            let o = content_hash(&f.local).map_err(|e| fail(&e))?;
            let whole = match o.scope {
                HashScope::File1 => o.hash,
                HashScope::MediaPayload1 => file_hash(&f.local).map_err(|e| fail(&e))?,
            };
            let payload = HashRef {
                hash: o.hash.to_string(),
                scope: o.scope.to_string(),
            };
            (whole, Some(payload))
        };
        let file_hash = Some(whole.to_string());
        let mut session = f.session.clone();
        // A session holds what was sent of the file as it was; it only fits the same file.
        if session.is_some() && (f.file_hash != file_hash || f.size != size) {
            stale.extend(session.take());
        }
        out.push(QueuedFile {
            local: f.local.clone(),
            inbox_path: f.inbox_path.clone(),
            size,
            mtime_ns,
            file_hash,
            payload,
            session,
        });
    }
    Ok((out, stale))
}

/// The group `id`, looked for at `hint` first.
fn group_mut(queue: &mut UploadQueue, id: GroupId, hint: usize) -> Option<&mut QueuedGroup> {
    let index = match queue.groups.get(hint) {
        Some(g) if g.id == id => hint,
        _ => queue.groups.iter().position(|g| g.id == id)?,
    };
    queue.groups.get_mut(index)
}

fn uploading(state: &GroupState) -> Option<bool> {
    match state {
        GroupState::Uploading { allow_duplicate } | GroupState::Committing { allow_duplicate } => {
            Some(*allow_duplicate)
        }
        _ => None,
    }
}

struct Engine {
    inner: Arc<Inner>,
    rx: mpsc::Receiver<Msg>,
    cancelled: Arc<AtomicBool>,
    /// Built on this thread; `None` (and `halted` set) if that failed.
    rpc: Option<RpcClient>,
    gate: Arc<Mutex<Gate>>,
    health: Health,
    accept: fn(MediaKind) -> bool,
    /// Why the engine stopped talking to the server, if it did.
    halted: Option<ClientError>,
    /// The latest retry wait; `None` after a success.
    backoff: Option<Duration>,
    /// No server request before this.
    retry_at: Option<Instant>,
    last_publish: Instant,
}

impl Engine {
    fn new(inner: Arc<Inner>, rx: mpsc::Receiver<Msg>, cfg: EngineConfig) -> Engine {
        let mut engine = Engine {
            inner,
            rx,
            cancelled: cfg.cancelled,
            rpc: None,
            gate: cfg.gate,
            health: cfg.health,
            accept: upload_accept(&cfg.app),
            halted: None,
            backoff: None,
            retry_at: None,
            last_publish: Instant::now(),
        };
        match RpcClient::with_timeout(&cfg.url, cfg.token, UPLOAD_TIMEOUT) {
            Ok(rpc) => engine.rpc = Some(rpc),
            Err(e) => engine.halt(e),
        }
        engine
    }

    fn stopped(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || self.inner.dropped.load(Ordering::Acquire)
    }

    fn run(mut self) {
        loop {
            if !self.pump() || self.stopped() {
                return;
            }
            let step = self.step();
            if self.stopped() {
                return;
            }
            let wait = match step {
                Step::Worked => {
                    self.publish(false);
                    continue;
                }
                Step::Idle => None,
                Step::Wait(d) => Some(d),
            };
            self.publish(true);
            let msg = match wait {
                None => self.rx.recv().ok(),
                Some(d) => match self.rx.recv_timeout(d) {
                    Ok(m) => Some(m),
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => None,
                },
            };
            match msg {
                Some(m) => self.handle(m),
                None => return,
            }
        }
    }

    /// Handles the messages that have arrived, without waiting. `false` once the
    /// [`Uploads`] is gone.
    fn pump(&mut self) -> bool {
        loop {
            match self.rx.try_recv() {
                Ok(m) => self.handle(m),
                Err(mpsc::TryRecvError::Empty) => return true,
                Err(mpsc::TryRecvError::Disconnected) => return false,
            }
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Drop(paths) => {
                let plan = plan_drop(&paths, self.accept);
                self.inner.change(Kind::State, |s| {
                    s.excluded = plan.excluded;
                    s.warnings = plan.warnings.clone();
                    s.queue.push(plan);
                    true
                });
            }
            Msg::Wake => {}
        }
    }

    /// Publishes a snapshot if the queue changed, and `force` is set or the last one is old
    /// enough.
    fn publish(&mut self, force: bool) {
        if force || self.last_publish.elapsed() >= PUBLISH_EVERY {
            self.inner.publish();
            self.last_publish = Instant::now();
        }
    }

    fn halt(&mut self, error: ClientError) {
        self.halted = Some(error.clone());
        self.inner.set_state(UploadsState::Stopped { error });
    }

    /// One unit of work: local work first, then (unless waiting) one exchange with the server.
    fn step(&mut self) -> Step {
        if self.hash_some() {
            return Step::Worked;
        }
        if self.halted.is_some() {
            return Step::Idle;
        }
        if let Some(at) = self.retry_at {
            let now = Instant::now();
            if now < at {
                return Step::Wait(at - now);
            }
        }
        let gate = lock_gate(&self.gate).clone();
        if let Gate::Closed(e) = gate {
            self.halt(e);
            return Step::Idle;
        }
        let outcome = match self.abort_some() {
            Some(r) => r,
            None => match self.check_some() {
                Some(r) => r,
                None => match self.upload_some() {
                    Some(r) => r,
                    None => return Step::Idle,
                },
            },
        };
        match outcome {
            Ok(()) => {
                self.backoff = None;
                self.retry_at = None;
                self.inner.set_state(UploadsState::Running);
                Step::Worked
            }
            Err(e) if halting(&e) => {
                self.halt(e);
                Step::Idle
            }
            Err(e) => {
                let initial = Duration::from_millis(BACKOFF_INITIAL_MS.load(Ordering::Relaxed));
                let max = Duration::from_millis(BACKOFF_MAX_MS.load(Ordering::Relaxed));
                let mut wait = self.backoff.map_or(initial, |b| (b * 2).min(max));
                self.backoff = Some(wait);
                if let ClientError::Retry { after } = &e {
                    wait = wait.max(*after).min(max);
                }
                self.retry_at = Some(Instant::now() + wait);
                self.inner.set_state(UploadsState::Waiting { error: e });
                Step::Wait(wait)
            }
        }
    }

    fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R, ClientError> {
        let Some(rpc) = &self.rpc else {
            return Err(ClientError::Io("no client".into()));
        };
        let result = rpc.call(method, params);
        note_health(&self.health, &result);
        result
    }

    fn put(&self, session: &str, index: u64, body: Vec<u8>) -> Result<(), ClientError> {
        let Some(rpc) = &self.rpc else {
            return Err(ClientError::Io("no client".into()));
        };
        let result = rpc.put_chunk(session, index, body);
        note_health(&self.health, &result);
        result
    }

    /// Hashes groups waiting for it, for up to [`HASH_SLICE`]. Whether any was.
    fn hash_some(&mut self) -> bool {
        let start = Instant::now();
        let mut any = false;
        loop {
            let picked: Vec<(usize, GroupId, Vec<QueuedFile>)> = {
                let s = self.inner.lock();
                s.queue
                    .groups
                    .iter()
                    .enumerate()
                    .filter(|(_, g)| g.state == GroupState::Hashing)
                    .take(HASH_PICK)
                    .map(|(i, g)| (i, g.id, g.files.clone()))
                    .collect()
            };
            if picked.is_empty() {
                return any;
            }
            for (hint, id, files) in picked {
                let hashed = hash_files(&files);
                self.inner.change(Kind::State, |s| {
                    let Some(g) = group_mut(&mut s.queue, id, hint) else {
                        return false;
                    };
                    if g.state != GroupState::Hashing {
                        return false;
                    }
                    g.sent = 0;
                    match hashed {
                        Ok((files, stale)) => {
                            g.files = files;
                            g.state = GroupState::Checking;
                            s.orphans.extend(stale);
                        }
                        Err(reason) => g.state = GroupState::Failed { reason },
                    }
                    true
                });
                any = true;
                self.publish(false);
                if self.stopped() || start.elapsed() >= HASH_SLICE {
                    return true;
                }
            }
        }
    }

    /// Aborts one server session nothing needs any more. If the server could not be reached
    /// (or the engine must stop), the session is kept for a later try; any other failure is
    /// ignored (the server drops idle sessions after 7 days).
    fn abort_some(&mut self) -> Option<Result<(), ClientError>> {
        let (session, orphan) = {
            let mut s = self.inner.lock();
            match s.orphans.pop() {
                Some(session) => (session, true),
                None => s
                    .queue
                    .groups
                    .iter()
                    .filter(|g| g.state == GroupState::Discarded)
                    .flat_map(|g| &g.files)
                    .find_map(|f| f.session.clone())
                    .map(|session| (session, false))?,
            }
        };
        let result = self.call::<_, IgnoredAny>(
            UPLOAD_ABORT,
            &UploadSessionParams {
                session: session.clone(),
            },
        );
        if let Err(e) = result
            && (transient(&e) || halting(&e))
        {
            if orphan {
                self.inner.lock().orphans.push(session);
            }
            return Some(Err(e));
        }
        self.inner.change(Kind::State, |s| {
            let mut changed = false;
            for g in s.queue.groups.iter_mut() {
                if g.state != GroupState::Discarded {
                    continue;
                }
                for f in g.files.iter_mut() {
                    if f.session.as_deref() == Some(session.as_str()) {
                        f.session = None;
                        changed = true;
                    }
                }
            }
            changed
        });
        Some(Ok(()))
    }

    /// Asks the server which payloads of the groups being checked it already holds, up to
    /// [`CHECK_BATCH`] hashes at a time.
    fn check_some(&mut self) -> Option<Result<(), ClientError>> {
        let batch: Vec<(usize, GroupId, Vec<HashRef>)> = {
            let s = self.inner.lock();
            let mut batch = Vec::new();
            let mut count = 0;
            for (i, g) in s.queue.groups.iter().enumerate() {
                if g.state != GroupState::Checking {
                    continue;
                }
                let refs: Vec<HashRef> = g.files.iter().filter_map(|f| f.payload.clone()).collect();
                if !batch.is_empty() && count + refs.len() > CHECK_BATCH {
                    break;
                }
                count += refs.len();
                batch.push((i, g.id, refs));
            }
            batch
        };
        if batch.is_empty() {
            return None;
        }
        let hashes = batch.iter().flat_map(|(_, _, r)| r.clone()).collect();
        let result: Result<UploadCheckResult, ClientError> =
            self.call(UPLOAD_CHECK, &UploadCheckParams { hashes });
        let existing = match result {
            Ok(r) => r.existing,
            Err(e) if transient(&e) || halting(&e) => return Some(Err(e)),
            Err(e) => {
                let reason = e.to_string();
                self.inner.change(Kind::State, |s| {
                    for (hint, id, _) in &batch {
                        if let Some(g) = group_mut(&mut s.queue, *id, *hint)
                            && g.state == GroupState::Checking
                        {
                            g.state = GroupState::Failed {
                                reason: reason.clone(),
                            };
                        }
                    }
                    true
                });
                return Some(Ok(()));
            }
        };
        self.inner.change(Kind::State, |s| {
            for (hint, id, refs) in &batch {
                let Some(g) = group_mut(&mut s.queue, *id, *hint) else {
                    continue;
                };
                if g.state != GroupState::Checking {
                    continue;
                }
                let paths: BTreeSet<String> = existing
                    .iter()
                    .filter(|e| refs.iter().any(|r| r.hash == e.hash && r.scope == e.scope))
                    .flat_map(|e| e.paths.iter().cloned())
                    .collect();
                g.state = if paths.is_empty() {
                    GroupState::Uploading {
                        allow_duplicate: false,
                    }
                } else {
                    GroupState::AwaitingDecision {
                        existing: paths.into_iter().collect(),
                    }
                };
            }
            true
        });
        Some(Ok(()))
    }

    /// Takes the first group being sent one exchange (or a run of chunks) further.
    fn upload_some(&mut self) -> Option<Result<(), ClientError>> {
        let (hint, id, files, allow_duplicate) = {
            let s = self.inner.lock();
            s.queue
                .groups
                .iter()
                .enumerate()
                .find_map(|(i, g)| uploading(&g.state).map(|a| (i, g.id, g.files.clone(), a)))?
        };
        Some(self.upload_group(hint, id, files, allow_duplicate))
    }

    /// Whether group `id` is still to be sent (the user may have cancelled it meanwhile).
    fn still_uploading(&self, id: GroupId, hint: usize) -> bool {
        let mut s = self.inner.lock();
        group_mut(&mut s.queue, id, hint).is_some_and(|g| uploading(&g.state).is_some())
    }

    /// Sends what the server is missing of each file of the group, then commits it. Returns
    /// early (to be called again) whenever the plan changes: a file changed, a session is
    /// gone, the user cancelled, or the session is closing.
    fn upload_group(
        &mut self,
        hint: usize,
        id: GroupId,
        files: Vec<QueuedFile>,
        allow_duplicate: bool,
    ) -> Result<(), ClientError> {
        let mut done_before = 0u64;
        let mut placed = Vec::with_capacity(files.len());
        for (i, f) in files.iter().enumerate() {
            if !unchanged(f) {
                self.restart(hint, id, i);
                return Ok(());
            }
            let session = match &f.session {
                Some(s) => s.clone(),
                None => match self.begin(hint, id, i, f)? {
                    Some(s) => s,
                    None => return Ok(()),
                },
            };
            let status: UploadStatus = match self.call(
                UPLOAD_STATUS,
                &UploadSessionParams {
                    session: session.clone(),
                },
            ) {
                Ok(s) => s,
                Err(e) if not_found(&e) => {
                    // Gone from the server (expired, or never there): begin again.
                    self.forget_session(hint, id, i, &session, false);
                    return Ok(());
                }
                Err(e) => return self.group_error(hint, id, e),
            };
            if status.size != f.size {
                self.forget_session(hint, id, i, &session, true);
                return Ok(());
            }
            let missing: u64 = status.missing.iter().map(|&k| chunk_len(f.size, k)).sum();
            let mut sent = done_before + f.size - missing.min(f.size);
            self.set_sent(hint, id, sent);
            for k in status.missing {
                if !self.pump() || self.stopped() || !self.still_uploading(id, hint) {
                    return Ok(());
                }
                if !unchanged(f) {
                    self.restart(hint, id, i);
                    return Ok(());
                }
                let body = match read_chunk(&f.local, k, f.size) {
                    Ok(b) => b,
                    // Changed since the check above.
                    Err(_) if !unchanged(f) => {
                        self.restart(hint, id, i);
                        return Ok(());
                    }
                    Err(e) => {
                        self.fail(hint, id, format!("{}: {e}", f.local.display()));
                        return Ok(());
                    }
                };
                let len = body.len() as u64;
                match self.put(&session, k, body) {
                    Ok(()) => {}
                    Err(e) if not_found(&e) => {
                        self.forget_session(hint, id, i, &session, false);
                        return Ok(());
                    }
                    Err(e) => return self.group_error(hint, id, e),
                }
                if self.stopped() {
                    return Ok(());
                }
                sent += len;
                self.set_sent(hint, id, sent);
                self.publish(false);
            }
            done_before += f.size;
            placed.push(UploadFile {
                session,
                path: f.inbox_path.clone(),
            });
        }
        // What was sent must still be what is on disk.
        for (i, f) in files.iter().enumerate() {
            if !unchanged(f) {
                self.restart(hint, id, i);
                return Ok(());
            }
        }
        self.commit(hint, id, placed, allow_duplicate)
    }

    /// `upload.begin` for file `i`; the saver writes the session at once, so a restart
    /// resumes it.
    /// `None` if the group is no longer being sent.
    fn begin(
        &mut self,
        hint: usize,
        id: GroupId,
        i: usize,
        f: &QueuedFile,
    ) -> Result<Option<String>, ClientError> {
        let Some(file_hash) = f.file_hash.clone() else {
            self.restart(hint, id, i);
            return Ok(None);
        };
        let begun: UploadBegin = match self.call(
            UPLOAD_BEGIN,
            &UploadBeginParams {
                size: f.size,
                file_hash,
            },
        ) {
            Ok(b) => b,
            Err(e) => return self.group_error(hint, id, e).map(|()| None),
        };
        let session = begun.session;
        if begun.chunk_size != UPLOAD_CHUNK_SIZE {
            self.inner.change(Kind::State, |s| {
                s.orphans.push(session.clone());
                true
            });
            self.fail(
                hint,
                id,
                format!(
                    "the server uses {}-byte chunks, not {UPLOAD_CHUNK_SIZE}",
                    begun.chunk_size
                ),
            );
            return Ok(None);
        }
        let mut recorded = false;
        self.inner.change(Kind::State, |s| {
            match group_mut(&mut s.queue, id, hint) {
                Some(g) if uploading(&g.state).is_some() && g.files[i].session.is_none() => {
                    g.files[i].session = Some(session.clone());
                    recorded = true;
                }
                // Cancelled meanwhile: nothing refers to the new session.
                _ => s.orphans.push(session.clone()),
            }
            true
        });
        Ok(recorded.then_some(session))
    }

    fn commit(
        &mut self,
        hint: usize,
        id: GroupId,
        files: Vec<UploadFile>,
        allow_duplicate: bool,
    ) -> Result<(), ClientError> {
        let ready = self
            .inner
            .change(Kind::State, |s| match group_mut(&mut s.queue, id, hint) {
                Some(g) if uploading(&g.state).is_some() => {
                    g.state = GroupState::Committing { allow_duplicate };
                    true
                }
                _ => false,
            });
        if !ready {
            return Ok(());
        }
        let result: Result<UploadCommitResult, ClientError> = self.call(
            UPLOAD_COMMIT,
            &UploadCommitParams {
                files,
                allow_duplicate,
            },
        );
        match result {
            Ok(UploadCommitResult::Placed { files, warnings }) => {
                self.inner.change(Kind::State, |s| {
                    let Some(g) = group_mut(&mut s.queue, id, hint) else {
                        return false;
                    };
                    // Placed even if cancelled meanwhile: the files are in the library.
                    for f in g.files.iter_mut() {
                        f.session = None;
                    }
                    g.sent = g.files.iter().map(|f| f.size).sum();
                    g.state = GroupState::Done {
                        placed: files.into_iter().map(|p| p.path).collect(),
                        warnings: warnings
                            .into_iter()
                            .map(|w| match w {
                                UploadWarning::SidecarDropped { reason, .. } => {
                                    format!("sidecar dropped: {reason}")
                                }
                            })
                            .collect(),
                    };
                    true
                });
                Ok(())
            }
            Ok(UploadCommitResult::Duplicate { existing }) => {
                // The sessions stay, so keeping the group later commits them as they are.
                self.inner
                    .change(Kind::State, |s| match group_mut(&mut s.queue, id, hint) {
                        Some(g) if uploading(&g.state).is_some() => {
                            g.state = GroupState::AwaitingDecision { existing };
                            true
                        }
                        _ => false,
                    });
                Ok(())
            }
            Err(ClientError::Rpc { code, .. }) if code == codes::HASH_MISMATCH => {
                // The server dropped the session that did not match; hash again.
                self.set_group(hint, id, |g| {
                    g.state = GroupState::Hashing;
                    g.sent = 0;
                });
                Ok(())
            }
            Err(e @ ClientError::Rpc { code, .. }) if code == codes::UPLOAD_INCOMPLETE => {
                // Status found nothing missing, yet the server says otherwise: send what is
                // missing again, after a pause, so a server that keeps saying so is not
                // asked in a tight loop.
                self.set_group(hint, id, |g| {
                    g.state = GroupState::Uploading { allow_duplicate };
                });
                Err(e)
            }
            Err(e) if not_found(&e) => {
                // A session is gone: start the group's sessions over.
                self.inner.change(Kind::State, |s| {
                    let Some(g) = group_mut(&mut s.queue, id, hint) else {
                        return false;
                    };
                    if uploading(&g.state).is_none() {
                        return false;
                    }
                    let sessions: Vec<String> = g
                        .files
                        .iter_mut()
                        .filter_map(|f| f.session.take())
                        .collect();
                    g.state = GroupState::Uploading { allow_duplicate };
                    g.sent = 0;
                    s.orphans.extend(sessions);
                    true
                });
                Ok(())
            }
            Err(e) => self.group_error(hint, id, e),
        }
    }

    /// Changes group `id` if it is still being sent.
    fn set_group(&self, hint: usize, id: GroupId, f: impl FnOnce(&mut QueuedGroup)) {
        self.inner
            .change(Kind::State, |s| match group_mut(&mut s.queue, id, hint) {
                Some(g) if uploading(&g.state).is_some() => {
                    f(g);
                    true
                }
                _ => false,
            });
    }

    fn set_sent(&self, hint: usize, id: GroupId, sent: u64) {
        self.inner.change(Kind::Progress, |s| {
            match group_mut(&mut s.queue, id, hint) {
                Some(g) if uploading(&g.state).is_some() && g.sent != sent => {
                    g.sent = sent;
                    true
                }
                _ => false,
            }
        });
    }

    fn fail(&self, hint: usize, id: GroupId, reason: String) {
        self.set_group(hint, id, |g| g.state = GroupState::Failed { reason });
    }

    /// A server error while sending group `id`: passed up if it is worth retrying (or stops
    /// the engine), otherwise the group fails with it.
    fn group_error(&self, hint: usize, id: GroupId, e: ClientError) -> Result<(), ClientError> {
        if transient(&e) || halting(&e) {
            return Err(e);
        }
        self.fail(hint, id, e.to_string());
        Ok(())
    }

    /// File `i` changed on disk: its session is aborted, its record cleared, and the group
    /// hashed again.
    fn restart(&self, hint: usize, id: GroupId, i: usize) {
        self.inner.change(Kind::State, |s| {
            let Some(g) = group_mut(&mut s.queue, id, hint) else {
                return false;
            };
            if uploading(&g.state).is_none() {
                return false;
            }
            let f = &mut g.files[i];
            let session = f.session.take();
            f.size = 0;
            f.mtime_ns = 0;
            f.file_hash = None;
            f.payload = None;
            g.state = GroupState::Hashing;
            g.sent = 0;
            s.orphans.extend(session);
            true
        });
    }

    /// Forgets file `i`'s session `session` (aborting it on the server if `abort`), so the
    /// next pass begins a new one.
    fn forget_session(&self, hint: usize, id: GroupId, i: usize, session: &str, abort: bool) {
        self.inner.change(Kind::State, |s| {
            let Some(g) = group_mut(&mut s.queue, id, hint) else {
                return false;
            };
            if g.files[i].session.as_deref() != Some(session) {
                return false;
            }
            g.files[i].session = None;
            if abort {
                s.orphans.push(session.to_owned());
            }
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inner() -> Arc<Inner> {
        let shared = Shared {
            queue: UploadQueue::default(),
            excluded: 0,
            warnings: Vec::new(),
            state: UploadsState::Running,
            orphans: Vec::new(),
            version: 0,
            dirty: false,
            progress: false,
            closing: false,
        };
        let first = snapshot_of(&shared);
        Arc::new(Inner {
            shared: Mutex::new(shared),
            changed: Condvar::new(),
            published: Mutex::new((0, first)),
            ctx: egui::Context::default(),
            dropped: AtomicBool::new(false),
        })
    }

    #[test]
    fn a_panicking_engine_still_releases_the_queue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uploads").join("lib.json");
        let inner = inner();
        let held = claim(&path, || false).unwrap();
        let saver_inner = inner.clone();
        let saver_path = path.clone();
        let saver = std::thread::spawn(move || save_loop(saver_inner, saver_path, held));
        inner.change(Kind::State, |_| true);

        let engine_inner = inner.clone();
        let engine = std::thread::spawn(move || {
            let _closing = Closing(engine_inner);
            panic!("engine failed");
        });
        assert!(engine.join().is_err());
        saver.join().unwrap();
        // Saved before letting go, and the next engine can claim the file.
        assert_eq!(UploadQueue::load(&path).unwrap(), UploadQueue::default());
        let start = Instant::now();
        let again = claim(&path, || start.elapsed() > Duration::from_secs(5));
        assert!(again.is_some());
    }
}
