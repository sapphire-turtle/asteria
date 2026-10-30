//! Thumbnails and previews as egui textures: fetched (or read from the disk cache) and decoded
//! on a pool of threads, uploaded on the UI thread.
//!
//! An [`ImageLoader`] belongs to a [`Session`](crate::session::Session) and is reached through
//! [`Session::images`](crate::session::Session::images). Call [`ImageLoader::request`] for each
//! image you draw, every frame; it returns at once with what there is so far.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, mpsc};
use std::time::{Duration, Instant};

use asteria_framework_api::{ItemId, MediaItem};
use corundum_framework::egui;
use url::Url;

use crate::media_cache::MediaCache;
use crate::profiles::Token;
use crate::rpc::{ClientError, RpcClient};
use crate::session::{Gate, Health, lock_gate, note_health};

/// How many images are fetched (or read from disk) and decoded at the same time.
pub const MAX_CONCURRENT: usize = 6;
/// How many fetches one image gets in total (the first one included) while the server keeps
/// answering 503; after the last one it fails with [`ClientError::Retry`].
pub const MAX_ATTEMPTS: u32 = 30;
/// The wait after an image first fails for a reason that may pass (the server could not be
/// reached, an unexpected status, a local error). It doubles with each further failure, up to
/// [`MAX_FAILURE_BACKOFF`].
pub const FIRST_FAILURE_BACKOFF: Duration = Duration::from_secs(5);
/// The longest wait before a failed image is tried again.
pub const MAX_FAILURE_BACKOFF: Duration = Duration::from_secs(30);
/// A queued image that has not been requested in this many of the latest egui passes has
/// scrolled out of view: the pool drops it instead of fetching it.
pub const STALE_AFTER_PASSES: u64 = 2;

/// The sizes the server renders (`GET /media/{id}/{preset}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Preset {
    /// Long side 256 px.
    Thumb,
    /// Long side 1920 px.
    Preview,
}

impl Preset {
    /// The name the server and the disk cache use.
    pub fn as_str(self) -> &'static str {
        match self {
            Preset::Thumb => "thumb",
            Preset::Preview => "preview",
        }
    }

    /// How many textures of this preset are kept; beyond it, the least recently requested
    /// ones are dropped.
    pub fn texture_budget(self) -> usize {
        match self {
            Preset::Thumb => 1_000,
            Preset::Preview => 8,
        }
    }
}

/// What [`ImageLoader::request`] has for an image.
#[derive(Clone)]
pub enum ImageState {
    /// Queued, or being fetched or decoded.
    Loading,
    Ready(egui::TextureHandle),
    /// It could not be loaded. Some failures are final for the session
    /// ([`ClientError::Unprocessable`]: the server cannot render this file; also
    /// [`ClientError::NotFound`], [`ClientError::Unauthorized`], a failed handshake, a changed
    /// library and an image that does not decode). Others (the server unreachable, still busy
    /// after [`MAX_ATTEMPTS`] fetches, an unexpected answer) are tried again by a
    /// [`request`](ImageLoader::request) made after a backoff of [`FIRST_FAILURE_BACKOFF`],
    /// doubling up to [`MAX_FAILURE_BACKOFF`].
    Failed(ClientError),
}

impl fmt::Debug for ImageState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageState::Loading => f.write_str("Loading"),
            ImageState::Ready(texture) => f
                .debug_tuple("Ready")
                .field(&texture.id())
                .field(&texture.size())
                .finish(),
            ImageState::Failed(e) => f.debug_tuple("Failed").field(e).finish(),
        }
    }
}

type Key = (ItemId, Preset);

/// One (item, preset) the loader knows about.
struct Entry {
    /// Identifies the job filling this entry; results for an older ticket are ignored.
    ticket: u64,
    file_hash: Option<String>,
    state: EntryState,
    /// The egui pass in which it was last requested.
    last_frame: u64,
    /// Orders requests, for eviction.
    last_touch: u64,
    /// Read by the pool to choose the next job: the pass it was last requested in.
    priority: Arc<AtomicU64>,
    /// Failures in a row that may pass, for the backoff.
    failures: u32,
}

enum EntryState {
    /// Waiting for the handshake before it is queued.
    Held,
    Loading,
    Ready(egui::TextureHandle),
    /// `retry_at` is `None` if the failure is final for the session.
    Failed {
        error: ClientError,
        retry_at: Option<Instant>,
    },
}

/// Loads images for one session. See the [module docs](self).
///
/// - Each (item, preset) is loaded once; repeated requests return the same state. If the
///   item's `file_hash` changes, it is loaded again.
/// - Nothing is sent before the session's handshake succeeds; if it fails, or the session
///   fails later, requests fail with that error.
/// - With a `file_hash` and a cache, the disk cache is read first and a fetched image is
///   stored there.
/// - Up to [`MAX_CONCURRENT`] images load at once, on threads of their own (not the
///   session's request worker). Images requested in the latest egui pass are taken from the
///   queue first; one not requested in the last [`STALE_AFTER_PASSES`] passes has scrolled
///   out of view and is dropped from the queue unfetched (a later request queues it again).
/// - A 503 is retried after its `Retry-After`, up to [`MAX_ATTEMPTS`] fetches in total. For
///   other failures, see [`ImageState::Failed`].
/// - At most [`Preset::texture_budget`] textures per preset are kept; past that, the least
///   recently requested ones (not requested in the current pass) are dropped and loaded again
///   if requested later.
/// - Dropping the loader (with its session) stops the threads without waiting; results still
///   in flight are thrown away.
pub struct ImageLoader {
    ctx: egui::Context,
    url: Url,
    token: Token,
    gate: Arc<Mutex<Gate>>,
    cache: Arc<OnceLock<Arc<MediaCache>>>,
    health: Health,
    entries: HashMap<Key, Entry>,
    /// Entries waiting for the handshake, in request order.
    held: Vec<Key>,
    /// The egui pass last seen by `request`.
    frame: Option<u64>,
    /// The same, for the pool.
    newest_pass: Arc<AtomicU64>,
    /// The first and the longest wait before a failure that may pass is retried.
    backoff: (Duration, Duration),
    next_ticket: u64,
    touch: u64,
    /// Started on the first fetch.
    pool: Option<Pool>,
    results_tx: mpsc::Sender<Done>,
    results: mpsc::Receiver<Done>,
}

impl fmt::Debug for ImageLoader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageLoader")
            .field("entries", &self.entries.len())
            .finish_non_exhaustive()
    }
}

impl ImageLoader {
    pub(crate) fn new(
        ctx: egui::Context,
        url: Url,
        token: Token,
        gate: Arc<Mutex<Gate>>,
        cache: Arc<OnceLock<Arc<MediaCache>>>,
        health: Health,
    ) -> ImageLoader {
        let (results_tx, results) = mpsc::channel();
        ImageLoader {
            ctx,
            url,
            token,
            gate,
            cache,
            health,
            entries: HashMap::new(),
            held: Vec::new(),
            frame: None,
            newest_pass: Arc::new(AtomicU64::new(0)),
            backoff: (FIRST_FAILURE_BACKOFF, MAX_FAILURE_BACKOFF),
            next_ticket: 0,
            touch: 0,
            pool: None,
            results_tx,
            results,
        }
    }

    /// The image of `item` at `preset`: starts loading it if needed, and marks it as on screen
    /// in this egui pass. Call it for every image drawn, every frame; the first call in a pass
    /// also uploads the images that have arrived since the previous pass.
    pub fn request(&mut self, item: &MediaItem, preset: Preset) -> ImageState {
        self.request_with(&item.id, item.file_hash.as_deref(), preset)
    }

    /// Like [`request`](Self::request), for an image known only by its item id, such as an
    /// album's cover. Without a `file_hash` the disk cache is neither read nor written, and
    /// the entry is the one `request` would use for the same id, with no `file_hash`.
    pub fn request_id(&mut self, id: &ItemId, preset: Preset) -> ImageState {
        self.request_with(id, None, preset)
    }

    fn request_with(&mut self, id: &ItemId, file_hash: Option<&str>, preset: Preset) -> ImageState {
        let frame = self.ctx.cumulative_pass_nr();
        if self.frame != Some(frame) {
            self.frame = Some(frame);
            self.newest_pass.store(frame, Ordering::Relaxed);
            self.poll();
        }
        self.touch += 1;
        let key = (id.clone(), preset);
        if let Some(entry) = self.entries.get_mut(&key)
            && entry.file_hash.as_deref() == file_hash
        {
            entry.last_frame = frame;
            entry.last_touch = self.touch;
            entry.priority.store(frame, Ordering::Relaxed);
            let cooled_down = matches!(
                entry.state,
                EntryState::Failed { retry_at: Some(at), .. } if Instant::now() >= at
            );
            if !cooled_down {
                return entry.state.public();
            }
            // Try again, as a new job: a late result of the old one is ignored.
            entry.ticket = self.next_ticket;
            self.next_ticket += 1;
            entry.state = EntryState::Held;
            self.held.push(key.clone());
            self.release_held();
            return self.entries[&key].state.public();
        }
        // New, or the file changed since it was loaded.
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        let state = if safe_id(&id.0) {
            self.held.push(key.clone());
            EntryState::Held
        } else {
            EntryState::Failed {
                error: ClientError::Protocol("the item id cannot be used in a URL".into()),
                retry_at: None,
            }
        };
        self.entries.insert(
            key.clone(),
            Entry {
                ticket,
                file_hash: file_hash.map(str::to_owned),
                state,
                last_frame: frame,
                last_touch: self.touch,
                priority: Arc::new(AtomicU64::new(frame)),
                failures: 0,
            },
        );
        self.release_held();
        self.entries[&key].state.public()
    }

    /// Uploads the images that have arrived and queues requests that were waiting for the
    /// handshake. [`request`](Self::request) and [`Session::poll`](crate::session::Session::poll)
    /// call it; there is no need to call it yourself.
    pub fn poll(&mut self) {
        let (mut thumbs, mut previews) = (false, false);
        let (first, longest) = self.backoff;
        while let Ok(done) = self.results.try_recv() {
            let Some(entry) = self.entries.get_mut(&done.key) else {
                continue; // Evicted while loading.
            };
            if entry.ticket != done.ticket {
                continue; // Replaced while loading.
            }
            entry.state = match done.outcome {
                Outcome::Skipped => {
                    // Out of view when its turn came; a later request starts it afresh.
                    self.entries.remove(&done.key);
                    continue;
                }
                Outcome::Image(image) => {
                    let (id, preset) = &done.key;
                    match preset {
                        Preset::Thumb => thumbs = true,
                        Preset::Preview => previews = true,
                    }
                    entry.failures = 0;
                    let name = format!("asteria-media/{}/{}", id.0, preset.as_str());
                    EntryState::Ready(self.ctx.load_texture(
                        name,
                        image,
                        egui::TextureOptions::LINEAR,
                    ))
                }
                Outcome::Failed(Failure { error, permanent }) => {
                    let retry_at = (!permanent).then(|| {
                        entry.failures += 1;
                        Instant::now() + backoff(first, longest, entry.failures)
                    });
                    EntryState::Failed { error, retry_at }
                }
            };
        }
        self.release_held();
        if thumbs {
            self.evict(Preset::Thumb);
        }
        if previews {
            self.evict(Preset::Preview);
        }
    }

    /// Sets the wait before an image that failed for a reason that may pass is tried again:
    /// `first` after the first failure, doubling up to `longest`. The defaults are
    /// [`FIRST_FAILURE_BACKOFF`] and [`MAX_FAILURE_BACKOFF`].
    pub fn set_failure_backoff(&mut self, first: Duration, longest: Duration) {
        self.backoff = (first, longest);
    }

    /// Queues the held entries once the handshake has succeeded, or fails them if it has not.
    fn release_held(&mut self) {
        if self.held.is_empty() {
            return;
        }
        let gate = lock_gate(&self.gate).clone();
        let failure = match gate {
            Gate::Pending => return,
            Gate::Open { .. } => self.start_pool().err().map(|error| Failure {
                error,
                permanent: false,
            }),
            Gate::Closed(error) => Some(Failure {
                error,
                permanent: true,
            }),
        };
        let (first, longest) = self.backoff;
        for key in std::mem::take(&mut self.held) {
            let Some(entry) = self.entries.get_mut(&key) else {
                continue;
            };
            if !matches!(entry.state, EntryState::Held) {
                continue;
            }
            if let Some(f) = &failure {
                let retry_at = (!f.permanent).then(|| {
                    entry.failures += 1;
                    Instant::now() + backoff(first, longest, entry.failures)
                });
                entry.state = EntryState::Failed {
                    error: f.error.clone(),
                    retry_at,
                };
                continue;
            }
            entry.state = EntryState::Loading;
            let job = Job {
                ticket: entry.ticket,
                id: key.0.clone(),
                preset: key.1,
                file_hash: entry.file_hash.clone(),
                priority: entry.priority.clone(),
                attempts: 0,
            };
            if let Some(pool) = &self.pool {
                pool.push(job);
            }
        }
    }

    fn start_pool(&mut self) -> Result<(), ClientError> {
        if self.pool.is_none() {
            let (url, token) = (self.url.clone(), self.token.clone());
            let pool = Pool::spawn(
                PoolContext {
                    connect: Box::new(move || RpcClient::new(&url, token.clone())),
                    client: Mutex::new(None),
                    newest_pass: self.newest_pass.clone(),
                    gate: self.gate.clone(),
                    cache: self.cache.clone(),
                    health: self.health.clone(),
                    results: Mutex::new(self.results_tx.clone()),
                    ui: self.ctx.clone(),
                },
                MAX_CONCURRENT,
            )
            .map_err(|e| ClientError::Io(e.to_string()))?;
            self.pool = Some(pool);
        }
        Ok(())
    }

    /// Drops the least recently requested textures of `preset` beyond its budget, sparing
    /// those requested in the current pass.
    fn evict(&mut self, preset: Preset) {
        let ready = |e: &Entry| matches!(e.state, EntryState::Ready(_));
        let count = self
            .entries
            .iter()
            .filter(|((_, p), e)| *p == preset && ready(e))
            .count();
        let Some(excess) = count.checked_sub(preset.texture_budget()) else {
            return;
        };
        let frame = self.frame;
        let mut candidates: Vec<(u64, Key)> = self
            .entries
            .iter()
            .filter(|((_, p), e)| *p == preset && ready(e) && Some(e.last_frame) != frame)
            .map(|(k, e)| (e.last_touch, k.clone()))
            .collect();
        candidates.sort_unstable_by_key(|(touch, _)| *touch);
        for (_, key) in candidates.into_iter().take(excess) {
            // Dropping the handle frees the texture.
            self.entries.remove(&key);
        }
    }
}

impl EntryState {
    fn public(&self) -> ImageState {
        match self {
            EntryState::Held | EntryState::Loading => ImageState::Loading,
            EntryState::Ready(texture) => ImageState::Ready(texture.clone()),
            EntryState::Failed { error, .. } => ImageState::Failed(error.clone()),
        }
    }
}

/// The wait after the `failures`-th failure in a row: `first`, doubling up to `longest`.
fn backoff(first: Duration, longest: Duration, failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    first.saturating_mul(1 << doublings).min(longest)
}

/// Item ids are UUID strings; anything else is refused rather than put into a URL path.
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// One image to load, on the pool.
struct Job {
    ticket: u64,
    id: ItemId,
    preset: Preset,
    file_hash: Option<String>,
    priority: Arc<AtomicU64>,
    /// Fetches made so far.
    attempts: u32,
}

/// A finished job, sent to the UI thread.
struct Done {
    key: Key,
    ticket: u64,
    outcome: Outcome,
}

enum Outcome {
    Image(egui::ColorImage),
    Failed(Failure),
    /// Dropped unfetched: out of view when its turn came.
    Skipped,
}

/// Why an image did not load, and whether that is final for the session.
#[derive(Clone)]
struct Failure {
    error: ClientError,
    permanent: bool,
}

impl Failure {
    fn permanent(error: ClientError) -> Failure {
        Failure {
            error,
            permanent: true,
        }
    }
}

impl From<ClientError> for Failure {
    /// Failures of the exchange with the server: final if the server's answer will not change
    /// in this session.
    fn from(error: ClientError) -> Failure {
        let permanent = matches!(
            error,
            ClientError::Unprocessable
                | ClientError::NotFound
                | ClientError::Unauthorized
                | ClientError::Handshake(_)
                | ClientError::LibraryChanged
        );
        Failure { error, permanent }
    }
}

/// What the pool threads share besides the queue.
struct PoolContext {
    connect: Box<dyn Fn() -> Result<RpcClient, ClientError> + Send + Sync>,
    /// Built by the first job that needs the network, on a pool thread. A failed build is not
    /// kept: the next job tries again.
    client: Mutex<Option<Arc<RpcClient>>>,
    /// The latest egui pass in which the UI requested an image.
    newest_pass: Arc<AtomicU64>,
    gate: Arc<Mutex<Gate>>,
    cache: Arc<OnceLock<Arc<MediaCache>>>,
    /// The session's view of whether the server answers; each fetch reports to it.
    health: Health,
    results: Mutex<mpsc::Sender<Done>>,
    ui: egui::Context,
}

struct Queue {
    /// Jobs that may run now.
    ready: VecDeque<Job>,
    /// Jobs waiting for a 503's `Retry-After` to pass.
    deferred: Vec<(Instant, Job)>,
    /// Set when the loader is dropped.
    closed: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
    cancelled: AtomicBool,
    cx: PoolContext,
}

/// The loader's threads. Dropping it stops them without waiting.
struct Pool {
    shared: Arc<Shared>,
}

impl Pool {
    fn spawn(cx: PoolContext, threads: usize) -> std::io::Result<Pool> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                ready: VecDeque::new(),
                deferred: Vec::new(),
                closed: false,
            }),
            wake: Condvar::new(),
            cancelled: AtomicBool::new(false),
            cx,
        });
        let pool = Pool { shared };
        for _ in 0..threads.max(1) {
            let shared = pool.shared.clone();
            // On failure `pool` is dropped, which stops the threads already started.
            std::thread::Builder::new()
                .name("asteria-images".into())
                .spawn(move || run(&shared))?;
        }
        Ok(pool)
    }

    fn push(&self, job: Job) {
        lock(&self.shared.queue).ready.push_back(job);
        self.shared.wake.notify_one();
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shared.cancelled.store(true, Ordering::Release);
        lock(&self.shared.queue).closed = true;
        self.shared.wake.notify_all();
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A pool thread: takes the most wanted job, runs it, repeats until the pool is dropped.
fn run(shared: &Shared) {
    while let Some(next) = next_job(shared) {
        let (job, outcome) = match next {
            Next::Skip(job) => (job, Outcome::Skipped),
            Next::Run(mut job) => {
                let result = load(shared, &job);
                job.attempts += 1;
                if let Err(Failure {
                    error: ClientError::Retry { after },
                    ..
                }) = &result
                    && job.attempts < MAX_ATTEMPTS
                {
                    // Back in the queue after the wait; no thread sleeps through it.
                    lock(&shared.queue)
                        .deferred
                        .push((Instant::now() + *after, job));
                    shared.wake.notify_one();
                    continue;
                }
                let outcome = match result {
                    Ok(image) => Outcome::Image(image),
                    Err(failure) => Outcome::Failed(failure),
                };
                (job, outcome)
            }
        };
        // Dropped while the job ran: the result belongs to no one.
        if shared.cancelled.load(Ordering::Acquire) {
            return;
        }
        let done = Done {
            key: (job.id, job.preset),
            ticket: job.ticket,
            outcome,
        };
        if lock(&shared.cx.results).send(done).is_ok() {
            shared.cx.ui.request_repaint();
        }
    }
}

/// What a pool thread does next.
enum Next {
    Run(Job),
    /// Not requested in the last [`STALE_AFTER_PASSES`] passes: report it, fetch nothing.
    Skip(Job),
}

/// Waits for a job: the one requested in the latest pass, the oldest first among equals.
/// Deferred jobs join the queue when their time comes. `None` once the pool is dropped.
///
/// Staleness is measured against the latest pass in which the UI requested anything, so an
/// idle UI (no new passes) never makes the images it asked for stale.
fn next_job(shared: &Shared) -> Option<Next> {
    let mut queue = lock(&shared.queue);
    loop {
        if queue.closed {
            return None;
        }
        let now = Instant::now();
        let mut i = 0;
        while i < queue.deferred.len() {
            if queue.deferred[i].0 <= now {
                let (_, job) = queue.deferred.swap_remove(i);
                queue.ready.push_back(job);
            } else {
                i += 1;
            }
        }
        // `max_by_key` keeps the last maximum, so iterate from the back to prefer the oldest.
        let best = queue
            .ready
            .iter()
            .enumerate()
            .rev()
            .max_by_key(|(_, job)| job.priority.load(Ordering::Relaxed))
            .map(|(i, _)| i);
        if let Some(job) = best.and_then(|i| queue.ready.remove(i)) {
            let newest = shared.cx.newest_pass.load(Ordering::Relaxed);
            let requested = job.priority.load(Ordering::Relaxed);
            return Some(if newest.saturating_sub(requested) >= STALE_AFTER_PASSES {
                Next::Skip(job)
            } else {
                Next::Run(job)
            });
        }
        let next_due = queue.deferred.iter().map(|(due, _)| *due).min();
        queue = match next_due {
            Some(due) => {
                shared
                    .wake
                    .wait_timeout(queue, due.saturating_duration_since(now))
                    .unwrap_or_else(|e| e.into_inner())
                    .0
            }
            None => shared.wake.wait(queue).unwrap_or_else(|e| e.into_inner()),
        };
    }
}

/// Reads the image from the disk cache, or fetches it (and caches it), then decodes it.
fn load(shared: &Shared, job: &Job) -> Result<egui::ColorImage, Failure> {
    let cx = &shared.cx;
    check_gate(&cx.gate).map_err(Failure::permanent)?;
    let preset = job.preset.as_str();
    let cache = cx.cache.get();
    if let Some(cache) = cache
        && let Some(path) = cache.get(job.file_hash.as_deref(), preset)
    {
        // A file trimmed away or damaged since is a miss.
        match std::fs::read(&path) {
            Ok(bytes) => match decode(&bytes) {
                Ok(image) => return Ok(image),
                Err(e) => tracing::debug!("cached image unusable, fetching it: {e}"),
            },
            Err(e) => tracing::debug!("cached image unreadable, fetching it: {e}"),
        }
    }
    let client = client(cx)?;
    let fetched = client.get_bytes(&format!("media/{}/{preset}", job.id.0));
    note_health(&cx.health, &fetched);
    let bytes = fetched?;
    let image = decode(&bytes).map_err(Failure::permanent)?;
    // Only images that decode are kept, only while the session still serves this library, and
    // not once the session is gone (its results belong to no one).
    if let Some(cache) = cache
        && !shared.cancelled.load(Ordering::Acquire)
        && check_gate(&cx.gate).is_ok()
        && let Err(e) = cache.put(job.file_hash.as_deref(), preset, &bytes)
    {
        tracing::warn!("cannot store an image in the cache: {e}");
    }
    Ok(image)
}

/// The pool's client, built on first use.
fn client(cx: &PoolContext) -> Result<Arc<RpcClient>, ClientError> {
    let mut slot = lock(&cx.client);
    if let Some(client) = &*slot {
        return Ok(client.clone());
    }
    let client = Arc::new((cx.connect)()?);
    *slot = Some(client.clone());
    Ok(client)
}

/// Lets a job through only while the handshake's library is being served.
fn check_gate(gate: &Mutex<Gate>) -> Result<(), ClientError> {
    match &*lock_gate(gate) {
        Gate::Open { .. } => Ok(()),
        Gate::Closed(e) => Err(e.clone()),
        // The loader queues nothing before the gate opens; kept as a guard.
        Gate::Pending => Err(ClientError::Protocol(
            "image requested before the handshake finished".into(),
        )),
    }
}

fn decode(bytes: &[u8]) -> Result<egui::ColorImage, ClientError> {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::WebP)
        .map_err(|e| ClientError::Protocol(format!("cannot decode the image: {e}")))?
        .to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        size,
        image.as_raw(),
    ))
}
