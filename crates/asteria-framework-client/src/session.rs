//! The connection to one server profile. Everything tied to a server (its requests and their
//! results) lives in a [`Session`]; switching servers means dropping it and opening another.

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};

use asteria_framework_api::{
    ClientInfo, FOLDER_LIST, FolderListParams, FolderListing, LIBRARY_INFO, LIBRARY_RESCAN,
    LibraryInfo, MEDIA_GET, MEDIA_UPDATE, MediaGetParams, MediaItem, MediaUpdateParams,
    SERVER_INFO, ServerInfo, check_handshake,
};
use corundum_framework::egui;
use serde::Serialize;
use serde::de::IgnoredAny;

use crate::images::ImageLoader;
use crate::media_cache::MediaCache;
use crate::profiles::{ProfileId, ServerProfile};
use crate::rpc::{ClientError, RpcClient};
use crate::upload::queue::queue_path;
use crate::upload::{EngineConfig, Uploads};
use crate::worker::{Job, Worker};

/// Source of session generations; each [`Session`] takes a fresh one, starting at 1.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Where the connection stands.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionState {
    /// The handshake (`server.info`, then `library.info`) is running.
    Connecting,
    /// Connected. `library` is the latest `library.info` answer.
    Ready { library: LibraryInfo },
    /// The handshake failed, or the server switched to another library
    /// ([`ClientError::LibraryChanged`]); the session will not recover. Open a new one to retry.
    Failed(ClientError),
}

/// Identifies a request within its session. Tags are only unique within one session, so a
/// tag is never compared across sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(pub u64);

/// A request a caller can make through [`Session::request`].
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// `library.info`. On success the session's [`SessionState::Ready`] library is updated,
    /// and [`Session::take_refetch_listing`] is raised when the listing may have changed: a
    /// new `epoch`, a scan that ended, or (between scans) a new `item_count`. A different
    /// `library_id` fails the request and the session with [`ClientError::LibraryChanged`].
    LibraryInfo,
    /// `library.rescan`.
    LibraryRescan,
    /// `folder.list`: one page of a folder.
    FolderList(FolderListParams),
    /// `media.get`.
    MediaGet(MediaGetParams),
    /// `media.update`.
    MediaUpdate(MediaUpdateParams),
    /// Any other method, such as an app's own: `method` called with `params`. The result comes
    /// back as it is, in [`Response::Value`]; decoding it is up to the app.
    Call {
        method: String,
        params: serde_json::Value,
    },
}

/// The successful result of a [`Request`], in the same variant order.
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    LibraryInfo(LibraryInfo),
    LibraryRescan,
    FolderList(FolderListing),
    /// From `media.get` and `media.update`.
    Media(MediaItem),
    /// From [`Request::Call`]: the method's result, undecoded.
    Value(serde_json::Value),
}

/// The outcome of one request, handed out by [`Session::poll`].
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub id: RequestId,
    pub result: Result<Response, ClientError>,
}

/// What a job reports back to its session.
enum Event {
    Connected(Result<LibraryInfo, ClientError>),
    Reply(Reply),
}

/// An [`Event`] stamped with the generation of the session that asked for it.
struct Stamped {
    generation: u64,
    event: Event,
}

/// What the worker knows about the handshake. Only worker jobs read or write it; it keeps a
/// request queued during `Connecting` from reaching a server the handshake rejected, without
/// waiting for the UI thread to `poll` the failure.
#[derive(Clone)]
pub(crate) enum Gate {
    /// The handshake has not finished.
    Pending,
    /// The handshake succeeded with this library.
    Open { library_id: String },
    /// No more requests go to the server; each gets this error instead.
    Closed(ClientError),
}

/// Whether the server still answers, as the latest request to it (an RPC on the worker or an
/// image fetch on the pool) found: set by a request that could not reach the server or was
/// refused its token ([`ClientError::Unreachable`], [`ClientError::Unauthorized`]), cleared
/// by one that succeeded. Shared by the worker and the image pool of one session.
pub(crate) type Health = Arc<Mutex<Option<ClientError>>>;

/// Records in `health` what a request to the server found (see [`Health`]); other failures
/// (the server answered, but not with a result) change nothing.
pub(crate) fn note_health<T>(health: &Mutex<Option<ClientError>>, result: &Result<T, ClientError>) {
    let mut slot = health.lock().unwrap_or_else(|e| e.into_inner());
    match result {
        Ok(_) => *slot = None,
        Err(e @ (ClientError::Unreachable(_) | ClientError::Unauthorized)) => {
            *slot = Some(e.clone());
        }
        Err(_) => {}
    }
}

/// Where a session keeps what it stores on this device. Each is optional; without one, the
/// session simply does without what goes there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionDirs {
    /// Root of the thumbnail caches ([`MediaCache`], one per library).
    pub cache: Option<PathBuf>,
    /// Root of the app's data; the upload queues live in its `uploads/` directory.
    pub data: Option<PathBuf>,
}

/// The `{}` sent as the params of methods that take none.
#[derive(Serialize)]
struct NoParams {}

/// One server connection, owned by the UI thread.
///
/// Opening it starts a worker thread that runs the handshake; requests made with
/// [`request`](Self::request) run on that worker in order, after the handshake. Results come
/// back only through [`poll`](Self::poll), and each arrival asks egui for a repaint.
///
/// Every session has its own result channel and a generation number that every result
/// carries. Dropping the session closes the worker's queue without waiting: a request that is
/// mid-flight finishes on its own and its result is thrown away, so nothing a dropped session
/// asked for can reach another session.
pub struct Session {
    generation: u64,
    profile: ProfileId,
    state: SessionState,
    /// Set when a `library.info` answer shows a new epoch.
    refetch_listing: bool,
    next_request: u64,
    ctx: egui::Context,
    events_tx: mpsc::Sender<Stamped>,
    events: mpsc::Receiver<Stamped>,
    /// `None` if the worker could not be started; the session is then `Failed`.
    worker: Option<Worker>,
    gate: Arc<Mutex<Gate>>,
    /// Written by the worker and the image pool; read into `degraded` by `poll`.
    health: Health,
    /// What `health` said at the last `poll`.
    degraded: Option<ClientError>,
    /// Filled by the handshake job once the library id is known (and only if a cache root was
    /// given and the id is a safe directory name).
    cache: Arc<OnceLock<Arc<MediaCache>>>,
    /// Filled by the handshake job once the library id is known, if a data root was given.
    uploads: Arc<OnceLock<Uploads>>,
    images: ImageLoader,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("generation", &self.generation)
            .field("profile", &self.profile)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Connects to `profile` as the app described by `client`, and starts the handshake:
    /// `server.info`, [`check_handshake`], then `library.info`. The session starts in
    /// [`SessionState::Connecting`]; watch [`state`](Self::state) after each
    /// [`poll`](Self::poll).
    pub fn open(profile: &ServerProfile, client: ClientInfo, ctx: egui::Context) -> Session {
        Self::open_with_dirs(profile, client, ctx, SessionDirs::default())
    }

    /// Like [`open`](Self::open), and as soon as the handshake learns the library id:
    /// - with `dirs.cache`, opens the library's [`MediaCache`] under it, and trims it on a
    ///   short-lived thread of its own (not the request worker, not the UI thread);
    /// - with `dirs.data`, starts the upload engine on the library's queue there (it loads
    ///   the queue on its own thread, once a previous session on the library has let go of
    ///   it; see [`UploadsState::Starting`](crate::upload::UploadsState::Starting)).
    ///
    /// Both are available, from [`media_cache`](Self::media_cache) and
    /// [`uploads`](Self::uploads), by the time the state is [`SessionState::Ready`].
    pub fn open_with_dirs(
        profile: &ServerProfile,
        client: ClientInfo,
        ctx: egui::Context,
        dirs: SessionDirs,
    ) -> Session {
        let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
        let (events_tx, events) = mpsc::channel();
        let gate = Arc::new(Mutex::new(Gate::Pending));
        let cache = Arc::new(OnceLock::new());
        let uploads = Arc::new(OnceLock::new());
        let health: Health = Arc::new(Mutex::new(None));
        let images = ImageLoader::new(
            ctx.clone(),
            profile.url.clone(),
            profile.token.clone(),
            gate.clone(),
            cache.clone(),
            health.clone(),
        );
        let mut session = Session {
            generation,
            profile: profile.id,
            state: SessionState::Connecting,
            refetch_listing: false,
            next_request: 0,
            ctx,
            events_tx,
            events,
            worker: None,
            gate,
            health,
            degraded: None,
            cache,
            uploads,
            images,
        };
        let (url, token) = (profile.url.clone(), profile.token.clone());
        // The client itself is built on the worker thread.
        match Worker::spawn("asteria-session", 1, move || {
            RpcClient::new(&url, token.clone())
        }) {
            Ok(worker) => {
                let cancelled = worker.cancelled();
                session.worker = Some(worker);
                let gate = session.gate.clone();
                let cache = session.cache.clone();
                let uploads = session.uploads.clone();
                let engine = EngineConfig {
                    url: profile.url.clone(),
                    token: profile.token.clone(),
                    app: client.app.clone(),
                    ctx: session.ctx.clone(),
                    gate: session.gate.clone(),
                    health: session.health.clone(),
                    cancelled: cancelled.clone(),
                    path: PathBuf::new(),
                };
                let profile_id = profile.id;
                session.submit(move |rpc| {
                    let result = rpc.and_then(|rpc| handshake(rpc, &client));
                    if let (Ok(library), Some(root)) = (&result, &dirs.cache) {
                        match MediaCache::open(root, &library.library_id) {
                            Some(opened) => {
                                let opened = Arc::new(opened);
                                let _ = cache.set(opened.clone());
                                trim_in_background(opened, cancelled);
                            }
                            None => tracing::warn!("library id is not a safe cache directory name"),
                        }
                    }
                    if let (Ok(library), Some(root)) = (&result, &dirs.data) {
                        match queue_path(root, &library.library_id, &profile_id.to_string()) {
                            Some(path) => {
                                let _ =
                                    uploads.set(Uploads::start(EngineConfig { path, ..engine }));
                            }
                            None => tracing::warn!("library or profile id is not a safe file name"),
                        }
                    }
                    *lock_gate(&gate) = match &result {
                        Ok(library) => Gate::Open {
                            library_id: library.library_id.clone(),
                        },
                        Err(e) => Gate::Closed(e.clone()),
                    };
                    Event::Connected(result)
                });
            }
            Err(e) => {
                let e = ClientError::Io(e.to_string());
                *lock_gate(&session.gate) = Gate::Closed(e.clone());
                session.state = SessionState::Failed(e);
            }
        }
        session
    }

    /// This session's generation: unique among the sessions of this process.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The profile this session connects to.
    pub fn profile(&self) -> ProfileId {
        self.profile
    }

    /// This library's thumbnail cache, once the handshake has succeeded and a cache root was
    /// given to [`open_with_dirs`](Self::open_with_dirs). Hand the `Arc` to jobs that run on
    /// worker threads: its methods do disk I/O, so not the UI thread.
    pub fn media_cache(&self) -> Option<Arc<MediaCache>> {
        self.cache.get().cloned()
    }

    /// This library's uploads, once the handshake has succeeded and a data root was given to
    /// [`open_with_dirs`](Self::open_with_dirs). The engine behind it stops when the session
    /// is dropped; a session for another server has its own queue, so switching servers
    /// pauses this one until a session for this library opens again.
    pub fn uploads(&self) -> Option<&Uploads> {
        self.uploads.get()
    }

    /// Thumbnails and previews of this session's server, as egui textures.
    pub fn images(&mut self) -> &mut ImageLoader {
        &mut self.images
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Why the server, connected so far, stopped answering: the latest request to it (a
    /// request or an image fetch) could not reach it ([`ClientError::Unreachable`]) or had its
    /// token refused ([`ClientError::Unauthorized`]). Cleared as soon as a request succeeds
    /// again. Only while [`SessionState::Ready`]; updated by [`poll`](Self::poll).
    pub fn degraded(&self) -> Option<&ClientError> {
        match self.state {
            SessionState::Ready { .. } => self.degraded.as_ref(),
            _ => None,
        }
    }

    /// Queues `request`; its [`Reply`] comes out of [`poll`](Self::poll) with the returned
    /// id. Requests run one at a time in the order made, after the handshake. If the
    /// handshake failed (even if [`poll`](Self::poll) has not reported it yet), or the session
    /// has failed since, the reply carries that failure and nothing is sent.
    pub fn request(&mut self, request: Request) -> RequestId {
        let id = RequestId(self.next_request);
        self.next_request += 1;
        if let SessionState::Failed(e) = &self.state {
            let reply = Reply {
                id,
                result: Err(e.clone()),
            };
            self.send_local(Event::Reply(reply));
            return id;
        }
        let gate = self.gate.clone();
        let health = self.health.clone();
        self.submit(move |rpc| {
            Event::Reply(Reply {
                id,
                result: gated(&gate, &health, rpc, request),
            })
        });
        id
    }

    /// Takes the results that have arrived, updating [`state`](Self::state) on the way.
    /// Replies to [`request`](Self::request)s are returned in arrival order; the handshake's
    /// result only changes the state. Call it every frame.
    pub fn poll(&mut self) -> Vec<Reply> {
        let mut replies = Vec::new();
        while let Ok(Stamped { generation, event }) = self.events.try_recv() {
            if generation != self.generation {
                // Cannot happen with a per-session channel; kept as a guard.
                tracing::debug!(generation, "discarding a result from another session");
                continue;
            }
            match event {
                Event::Connected(result) => {
                    self.state = match result {
                        Ok(library) => SessionState::Ready { library },
                        Err(e) => SessionState::Failed(e),
                    };
                }
                Event::Reply(reply) => {
                    match &reply.result {
                        Ok(Response::LibraryInfo(info)) => self.library_info_arrived(info),
                        Err(ClientError::LibraryChanged) => {
                            self.state = SessionState::Failed(ClientError::LibraryChanged);
                        }
                        _ => {}
                    }
                    replies.push(reply);
                }
            }
        }
        // Lets image requests through (or fails them) once the handshake has settled.
        self.images.poll();
        // Read after the replies: the worker records a request's outcome before sending it.
        self.degraded = self
            .health
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        replies
    }

    /// Whether the library's contents may have changed since the last call, so the listing on
    /// screen should be fetched again. Clears the mark. A `library.info` answer raises it when
    /// its epoch moved (the server rebuilt its cache), when a scan ended, or when, with no scan
    /// before or now, the item count changed (the server's watcher found changes).
    pub fn take_refetch_listing(&mut self) -> bool {
        std::mem::take(&mut self.refetch_listing)
    }

    fn library_info_arrived(&mut self, info: &LibraryInfo) {
        if let SessionState::Ready { library } = &mut self.state {
            let scan_ended = library.scanning && !info.scanning;
            let idle_change =
                !library.scanning && !info.scanning && library.item_count != info.item_count;
            if library.epoch != info.epoch || scan_ended || idle_change {
                self.refetch_listing = true;
            }
            *library = info.clone();
        }
    }

    /// Runs `work` on the worker and delivers its event to this session only.
    fn submit(&self, work: impl FnOnce(Result<&RpcClient, ClientError>) -> Event + Send + 'static) {
        let Some(worker) = &self.worker else { return };
        let generation = self.generation;
        let tx = self.events_tx.clone();
        let ctx = self.ctx.clone();
        let cancelled: Arc<AtomicBool> = worker.cancelled();
        let job: Job = Box::new(move |rpc| {
            let event = work(rpc);
            // Dropped while the request ran: the result belongs to no one.
            if cancelled.load(Ordering::Acquire) {
                return;
            }
            if tx.send(Stamped { generation, event }).is_ok() {
                ctx.request_repaint();
            }
        });
        worker.submit(job);
    }

    /// Delivers an event produced on the UI thread itself.
    fn send_local(&self, event: Event) {
        let _ = self.events_tx.send(Stamped {
            generation: self.generation,
            event,
        });
        self.ctx.request_repaint();
    }
}

pub(crate) fn lock_gate(gate: &Mutex<Gate>) -> std::sync::MutexGuard<'_, Gate> {
    gate.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs `request` only if the handshake let it through. A `library.info` naming another
/// library closes the gate: this request and every later one fail with
/// [`ClientError::LibraryChanged`], and `poll` moves the session to `Failed`.
fn gated(
    gate: &Mutex<Gate>,
    health: &Mutex<Option<ClientError>>,
    rpc: Result<&RpcClient, ClientError>,
    request: Request,
) -> Result<Response, ClientError> {
    let library_id = match lock_gate(gate).clone() {
        Gate::Open { library_id } => library_id,
        Gate::Closed(e) => return Err(e),
        // With one worker thread the handshake always runs first; kept as a guard.
        Gate::Pending => {
            return Err(ClientError::Protocol(
                "request ran before the handshake finished".into(),
            ));
        }
    };
    let response = run(rpc?, request);
    note_health(health, &response);
    let response = response?;
    if let Response::LibraryInfo(info) = &response
        && info.library_id != library_id
    {
        *lock_gate(gate) = Gate::Closed(ClientError::LibraryChanged);
        return Err(ClientError::LibraryChanged);
    }
    Ok(response)
}

/// Trims `cache` on a short-lived thread of its own: a big cache must not delay the session's
/// requests (or the UI). Skipped if the session is dropped before the thread starts.
fn trim_in_background(cache: Arc<MediaCache>, cancelled: Arc<AtomicBool>) {
    let spawned = std::thread::Builder::new()
        .name("asteria-cache-trim".into())
        .spawn(move || {
            if !cancelled.load(Ordering::Acquire) {
                cache.trim();
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("cannot start trimming the cache: {e}");
    }
}

fn handshake(rpc: &RpcClient, client: &ClientInfo) -> Result<LibraryInfo, ClientError> {
    let server: ServerInfo = rpc.call(SERVER_INFO, &NoParams {})?;
    check_handshake(client, &server)?;
    rpc.call(LIBRARY_INFO, &NoParams {})
}

fn run(rpc: &RpcClient, request: Request) -> Result<Response, ClientError> {
    Ok(match request {
        Request::LibraryInfo => Response::LibraryInfo(rpc.call(LIBRARY_INFO, &NoParams {})?),
        Request::LibraryRescan => {
            rpc.call::<_, IgnoredAny>(LIBRARY_RESCAN, &NoParams {})?;
            Response::LibraryRescan
        }
        Request::FolderList(p) => Response::FolderList(rpc.call(FOLDER_LIST, &p)?),
        Request::MediaGet(p) => Response::Media(rpc.call(MEDIA_GET, &p)?),
        Request::MediaUpdate(p) => Response::Media(rpc.call(MEDIA_UPDATE, &p)?),
        Request::Call { method, params } => Response::Value(rpc.call(&method, &params)?),
    })
}
