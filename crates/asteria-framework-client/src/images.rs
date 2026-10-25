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
use std::time::Instant;

use asteria_framework_api::{ItemId, MediaItem};
use corundum_framework::egui;
use url::Url;

use crate::media_cache::MediaCache;
use crate::profiles::Token;
use crate::rpc::{ClientError, RpcClient};
use crate::session::{Gate, lock_gate};

/// How many images are fetched (or read from disk) and decoded at the same time.
pub const MAX_CONCURRENT: usize = 6;
/// How many times one image is fetched while the server keeps answering 503, before it fails.
pub const MAX_ATTEMPTS: u32 = 30;

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
    /// Will not be tried again in this session. [`ClientError::Unprocessable`] means the
    /// server cannot render this file; [`ClientError::Retry`] means it stayed busy for
    /// [`MAX_ATTEMPTS`] attempts.
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
}

enum EntryState {
    /// Waiting for the handshake before it is queued.
    Held,
    Loading,
    Ready(egui::TextureHandle),
    Failed(ClientError),
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
///   session's request worker). Images requested in the current egui pass are taken from the
///   queue before those that were not: these have scrolled out of view.
/// - A 503 is retried after its `Retry-After`, up to [`MAX_ATTEMPTS`] fetches. Any other
///   failure (422 included) is final for this session.
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
    entries: HashMap<Key, Entry>,
    /// Entries waiting for the handshake, in request order.
    held: Vec<Key>,
    /// The egui pass last seen by `request`.
    frame: Option<u64>,
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
    ) -> ImageLoader {
        let (results_tx, results) = mpsc::channel();
        ImageLoader {
            ctx,
            url,
            token,
            gate,
            cache,
            entries: HashMap::new(),
            held: Vec::new(),
            frame: None,
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
        let frame = self.ctx.cumulative_pass_nr();
        if self.frame != Some(frame) {
            self.frame = Some(frame);
            self.poll();
        }
        self.touch += 1;
        let key = (item.id.clone(), preset);
        if let Some(entry) = self.entries.get_mut(&key)
            && entry.file_hash == item.file_hash
        {
            entry.last_frame = frame;
            entry.last_touch = self.touch;
            entry.priority.store(frame, Ordering::Relaxed);
            return entry.state.public();
        }
        // New, or the file changed since it was loaded.
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        let state = if safe_id(&item.id.0) {
            self.held.push(key.clone());
            EntryState::Held
        } else {
            EntryState::Failed(ClientError::Protocol(
                "the item id cannot be used in a URL".into(),
            ))
        };
        self.entries.insert(
            key.clone(),
            Entry {
                ticket,
                file_hash: item.file_hash.clone(),
                state,
                last_frame: frame,
                last_touch: self.touch,
                priority: Arc::new(AtomicU64::new(frame)),
            },
        );
        self.release_held();
        self.entries[&key].state.public()
    }

    /// Uploads the images that have arrived and queues requests that were waiting for the
    /// handshake. [`request`](Self::request) and [`Session::poll`](crate::session::Session::poll)
    /// call it; there is no need to call it yourself.
    pub fn poll(&mut self) {
        let mut uploaded = [false; 2];
        while let Ok(done) = self.results.try_recv() {
            let Some(entry) = self.entries.get_mut(&done.key) else {
                continue; // Evicted while loading.
            };
            if entry.ticket != done.ticket {
                continue; // Replaced while loading.
            }
            entry.state = match done.result {
                Ok(image) => {
                    let (id, preset) = &done.key;
                    uploaded[*preset as usize] = true;
                    let name = format!("asteria-media/{}/{}", id.0, preset.as_str());
                    EntryState::Ready(self.ctx.load_texture(
                        name,
                        image,
                        egui::TextureOptions::LINEAR,
                    ))
                }
                Err(e) => EntryState::Failed(e),
            };
        }
        self.release_held();
        for preset in [Preset::Thumb, Preset::Preview] {
            if uploaded[preset as usize] {
                self.evict(preset);
            }
        }
    }

    /// Queues the held entries once the handshake has succeeded, or fails them if it has not.
    fn release_held(&mut self) {
        if self.held.is_empty() {
            return;
        }
        let gate = lock_gate(&self.gate).clone();
        let failure = match gate {
            Gate::Pending => return,
            Gate::Open { .. } => self.start_pool().err(),
            Gate::Closed(e) => Some(e),
        };
        for key in std::mem::take(&mut self.held) {
            let Some(entry) = self.entries.get_mut(&key) else {
                continue;
            };
            if !matches!(entry.state, EntryState::Held) {
                continue;
            }
            if let Some(e) = &failure {
                entry.state = EntryState::Failed(e.clone());
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
                    client: OnceLock::new(),
                    gate: self.gate.clone(),
                    cache: self.cache.clone(),
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
            EntryState::Failed(e) => ImageState::Failed(e.clone()),
        }
    }
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
    result: Result<egui::ColorImage, ClientError>,
}

/// What the pool threads share besides the queue.
struct PoolContext {
    connect: Box<dyn Fn() -> Result<RpcClient, ClientError> + Send + Sync>,
    /// Built by the first job that needs the network, on a pool thread.
    client: OnceLock<Result<RpcClient, ClientError>>,
    gate: Arc<Mutex<Gate>>,
    cache: Arc<OnceLock<Arc<MediaCache>>>,
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
    while let Some(mut job) = next_job(shared) {
        let result = load(&shared.cx, &job);
        job.attempts += 1;
        if let Err(ClientError::Retry { after }) = &result
            && job.attempts < MAX_ATTEMPTS
        {
            // Back in the queue after the wait; no thread sleeps through it.
            lock(&shared.queue)
                .deferred
                .push((Instant::now() + *after, job));
            shared.wake.notify_one();
            continue;
        }
        // Dropped while the job ran: the result belongs to no one.
        if shared.cancelled.load(Ordering::Acquire) {
            return;
        }
        let done = Done {
            key: (job.id, job.preset),
            ticket: job.ticket,
            result,
        };
        if lock(&shared.cx.results).send(done).is_ok() {
            shared.cx.ui.request_repaint();
        }
    }
}

/// Waits for a job to run: the one requested in the latest pass, the oldest first among
/// equals. Deferred jobs join the queue when their time comes. `None` once the pool is dropped.
fn next_job(shared: &Shared) -> Option<Job> {
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
        if let Some(i) = best {
            return queue.ready.remove(i);
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
fn load(cx: &PoolContext, job: &Job) -> Result<egui::ColorImage, ClientError> {
    check_gate(&cx.gate)?;
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
    let client = cx
        .client
        .get_or_init(|| (cx.connect)())
        .as_ref()
        .map_err(Clone::clone)?;
    let bytes = client.get_bytes(&format!("media/{}/{preset}", job.id.0))?;
    let image = decode(&bytes)?;
    // Only images that decode are kept, and only while the session still serves this library.
    if let Some(cache) = cache
        && check_gate(&cx.gate).is_ok()
        && let Err(e) = cache.put(job.file_hash.as_deref(), preset, &bytes)
    {
        tracing::warn!("cannot store an image in the cache: {e}");
    }
    Ok(image)
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
