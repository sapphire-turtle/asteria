//! The connection to one server profile. Everything tied to a server (its requests and their
//! results) lives in a [`Session`]; switching servers means dropping it and opening another.

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Instant;

use asteria_common_api::{
    ClientInfo, FOLDER_LIST, FolderListParams, FolderListing, LIBRARY_INFO, LIBRARY_RESCAN,
    LibraryInfo, MEDIA_GET, MEDIA_UPDATE, MediaGetParams, MediaItem, MediaUpdateParams,
    SERVER_INFO, ServerInfo, check_handshake,
};
use corundum_framework::egui;
use serde::Serialize;
use serde::de::IgnoredAny;
use url::Url;

use crate::images::ImageLoader;
use crate::media_cache::MediaCache;
use crate::offline::{
    KnownLibraries, OfflineCatalog, OfflineHandle, OfflineStore, RECONNECT_EVERY,
};
use crate::profiles::{ProfileId, ServerProfile, Token};
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
    /// The server could not be reached, and the profile has been connected to `library` before
    /// (as recorded then): requests are answered from the saved copies, and the handshake is
    /// tried again every [`RECONNECT_EVERY`] (or at once, see [`Session::reconnect`]).
    Offline { library: LibraryInfo },
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
    /// The server could not be reached; the profile last saw this library.
    Offline(LibraryInfo),
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
    /// The server could not be reached: requests are answered from the saved copies of
    /// `library` (as recorded when the profile last connected), and nothing goes to the server.
    Offline { library: LibraryInfo },
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
    /// Root of the app's data; the upload queues live in its `uploads/` directory, the offline
    /// copies in `offline/`.
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
    /// Filled by the handshake job once the library id is known (online or offline), if a data
    /// root and a catalog were given.
    offline: Arc<OnceLock<OfflineHandle>>,
    /// What the handshake job needs, to run it again; `None` if the worker could not start.
    handshake: Option<Arc<Handshake>>,
    /// When the latest handshake ended (or the session opened).
    last_handshake: Instant,
    /// A handshake from [`reconnect`](Self::reconnect) is queued or running.
    reconnecting: bool,
    /// Whether the offline sync was last told the server answers.
    sync_online: bool,
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
        Self::open_with(profile, client, ctx, dirs, None)
    }

    /// Like [`open_with_dirs`](Self::open_with_dirs), and with `dirs.data` and a `catalog`,
    /// the session also works offline:
    /// - each successful handshake records the library for the profile
    ///   ([`KnownLibraries`]);
    /// - once the library id is known, the library's [`OfflineHandle`] starts
    ///   ([`offline`](Self::offline)); it syncs while the server answers;
    /// - a handshake that cannot reach the server, for a profile with a recorded library that
    ///   has something kept offline (a pin), ends in [`SessionState::Offline`] instead of
    ///   `Failed`;
    /// - a request the server does not answer is answered from the saved copies when the
    ///   catalog can (see [`request`](Self::request)), and images come from the saved copies
    ///   first.
    pub fn open_with(
        profile: &ServerProfile,
        client: ClientInfo,
        ctx: egui::Context,
        dirs: SessionDirs,
        catalog: Option<Arc<dyn OfflineCatalog>>,
    ) -> Session {
        let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
        let (events_tx, events) = mpsc::channel();
        let gate = Arc::new(Mutex::new(Gate::Pending));
        let cache = Arc::new(OnceLock::new());
        let uploads = Arc::new(OnceLock::new());
        let offline = Arc::new(OnceLock::new());
        let health: Health = Arc::new(Mutex::new(None));
        let images = ImageLoader::new(
            ctx.clone(),
            profile.url.clone(),
            profile.token.clone(),
            gate.clone(),
            cache.clone(),
            offline.clone(),
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
            offline,
            handshake: None,
            last_handshake: Instant::now(),
            reconnecting: false,
            sync_online: false,
            images,
        };
        let (url, token) = (profile.url.clone(), profile.token.clone());
        // The client itself is built on the worker thread.
        match Worker::spawn("asteria-session", 1, move || {
            RpcClient::new(&url, token.clone())
        }) {
            Ok(worker) => {
                let handshake = Arc::new(Handshake {
                    profile: profile.id,
                    url: profile.url.clone(),
                    token: profile.token.clone(),
                    client,
                    dirs,
                    catalog,
                    ctx: session.ctx.clone(),
                    gate: session.gate.clone(),
                    health: session.health.clone(),
                    cancelled: worker.cancelled(),
                    cache: session.cache.clone(),
                    uploads: session.uploads.clone(),
                    offline: session.offline.clone(),
                });
                session.worker = Some(worker);
                session.handshake = Some(handshake.clone());
                session.submit(move |rpc| handshake.run(rpc));
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

    /// This library's thumbnail cache, once the handshake has succeeded (or the session is
    /// offline) and a cache root was given to [`open_with_dirs`](Self::open_with_dirs). Hand
    /// the `Arc` to jobs that run on worker threads: its methods do disk I/O, so not the UI
    /// thread.
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

    /// This library's offline copies and their sync, once the library id is known (from the
    /// handshake, or from the record when offline), if a data root and a catalog were given to
    /// [`open_with`](Self::open_with). The same handle online and offline; it stops when the
    /// session is dropped.
    pub fn offline(&self) -> Option<&OfflineHandle> {
        self.offline.get()
    }

    /// Whether what is on screen comes from the saved copies: the session is
    /// [`SessionState::Offline`], or connected but the server no longer answers
    /// ([`degraded`](Self::degraded) is [`ClientError::Unreachable`]).
    pub fn is_offline(&self) -> bool {
        match self.state {
            SessionState::Offline { .. } => true,
            SessionState::Ready { .. } => {
                matches!(self.degraded, Some(ClientError::Unreachable(_)))
            }
            _ => false,
        }
    }

    /// While [`SessionState::Offline`], tries the server again now instead of at the next
    /// [`RECONNECT_EVERY`]; otherwise (or while a try is under way) does nothing. The outcome
    /// comes through [`poll`](Self::poll): `Ready`, still `Offline`, or `Failed` if the server
    /// answered but refused (such as [`ClientError::LibraryChanged`]).
    pub fn reconnect(&mut self) {
        if !matches!(self.state, SessionState::Offline { .. }) || self.reconnecting {
            return;
        }
        let Some(handshake) = self.handshake.clone() else {
            return;
        };
        self.reconnecting = true;
        self.submit(move |rpc| handshake.run(rpc));
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
    ///
    /// While offline, nothing is sent either: the catalog answers from the saved copies;
    /// what it cannot answer fails with [`ClientError::NotSaved`], or with
    /// [`ClientError::Offline`] for a change (`media.update`, `library.rescan`).
    /// `library.info` answers with the recorded library. While connected, a request that
    /// cannot reach the server is answered from the saved copies if the catalog can, unless
    /// it asks for a later page (it carries a `cursor`).
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
        let offline = self.offline.clone();
        self.submit(move |rpc| {
            Event::Reply(Reply {
                id,
                result: gated(&gate, &health, &offline, rpc, request),
            })
        });
        id
    }

    /// Takes the results that have arrived, updating [`state`](Self::state) on the way.
    /// Replies to [`request`](Self::request)s are returned in arrival order; the handshake's
    /// result only changes the state. Call it every frame; while offline, it also starts the
    /// next try of the server when it is due.
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
                    self.handshake_ended();
                    self.state = match result {
                        Ok(library) => {
                            // Back from offline: what is on screen came from the saved copies.
                            if matches!(self.state, SessionState::Offline { .. }) {
                                self.refetch_listing = true;
                            }
                            SessionState::Ready { library }
                        }
                        Err(e) => SessionState::Failed(e),
                    };
                }
                Event::Offline(library) => {
                    self.handshake_ended();
                    self.state = SessionState::Offline { library };
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
        let health = self
            .health
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // The server answers again: what is on screen may have come from the saved copies.
        if matches!(self.state, SessionState::Ready { .. })
            && matches!(self.degraded, Some(ClientError::Unreachable(_)))
            && health.is_none()
        {
            self.refetch_listing = true;
        }
        self.degraded = health;
        self.tell_sync();
        self.reconnect_when_due();
        replies
    }

    fn handshake_ended(&mut self) {
        self.reconnecting = false;
        self.last_handshake = Instant::now();
    }

    /// Tells the offline sync when the server starts or stops answering.
    fn tell_sync(&mut self) {
        let online = match &self.state {
            SessionState::Ready { library }
                if !matches!(self.degraded, Some(ClientError::Unreachable(_))) =>
            {
                Some(library)
            }
            _ => None,
        };
        if online.is_some() == self.sync_online {
            return;
        }
        self.sync_online = online.is_some();
        if let Some(handle) = self.offline.get() {
            handle.set_online(online);
        }
    }

    /// While offline, tries the server again [`RECONNECT_EVERY`] after the last handshake, and
    /// asks egui to wake up for it.
    fn reconnect_when_due(&mut self) {
        if !matches!(self.state, SessionState::Offline { .. }) || self.reconnecting {
            return;
        }
        let since = self.last_handshake.elapsed();
        if since >= RECONNECT_EVERY {
            self.reconnect();
        } else {
            self.ctx.request_repaint_after(RECONNECT_EVERY - since);
        }
    }

    /// Whether the library's contents may have changed since the last call, so the listing on
    /// screen should be fetched again. Clears the mark. A `library.info` answer raises it when
    /// its epoch moved (the server rebuilt its cache), when a scan ended, or when, with no scan
    /// before or now, the item count or the revision changed (the server's watcher found
    /// changes, or a write went through). Coming back from offline raises it too, as does the
    /// server answering again after [`degraded`](Self::degraded) was
    /// [`ClientError::Unreachable`]: what is on screen came from the saved copies.
    pub fn take_refetch_listing(&mut self) -> bool {
        std::mem::take(&mut self.refetch_listing)
    }

    fn library_info_arrived(&mut self, info: &LibraryInfo) {
        if let SessionState::Ready { library } = &mut self.state {
            let scan_ended = library.scanning && !info.scanning;
            let idle_change =
                !library.scanning && !info.scanning && library.item_count != info.item_count;
            // A write that keeps the count (a rating, a move) still moves the revision.
            let idle_write =
                !library.scanning && !info.scanning && library.revision != info.revision;
            if library.epoch != info.epoch || scan_ended || idle_change || idle_write {
                self.refetch_listing = true;
            }
            *library = info.clone();
            if self.sync_online
                && let Some(handle) = self.offline.get()
            {
                handle.set_online(Some(info));
            }
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

impl Drop for Session {
    /// Stops the offline sync at once: it must not go on talking to a server the app left.
    fn drop(&mut self) {
        if let Some(handle) = self.offline.get() {
            handle.stop();
        }
    }
}

/// The handshake job and what it opens, kept by the session to run it again when it
/// reconnects. Runs on the worker.
struct Handshake {
    profile: ProfileId,
    url: Url,
    token: Token,
    client: ClientInfo,
    dirs: SessionDirs,
    catalog: Option<Arc<dyn OfflineCatalog>>,
    ctx: egui::Context,
    gate: Arc<Mutex<Gate>>,
    health: Health,
    /// The worker's flag: set once the session is dropped.
    cancelled: Arc<AtomicBool>,
    cache: Arc<OnceLock<Arc<MediaCache>>>,
    uploads: Arc<OnceLock<Uploads>>,
    offline: Arc<OnceLock<OfflineHandle>>,
}

impl Handshake {
    /// `server.info`, [`check_handshake`], then `library.info`; then opens what the library
    /// needs and sets the gate. When the session is offline, a server that now serves another
    /// library fails with [`ClientError::LibraryChanged`]: the saved copies are not its.
    fn run(&self, rpc: Result<&RpcClient, ClientError>) -> Event {
        let expected = match &*lock_gate(&self.gate) {
            Gate::Offline { library } => Some(library.library_id.clone()),
            _ => None,
        };
        let already_offline = expected.is_some();
        let result = rpc
            .and_then(|rpc| handshake(rpc, &self.client))
            .and_then(|library| match expected {
                Some(id) if id != library.library_id => Err(ClientError::LibraryChanged),
                _ => Ok(library),
            });
        let (gate, event) = match result {
            Ok(library) => {
                self.connected(&library);
                let gate = Gate::Open {
                    library_id: library.library_id.clone(),
                };
                (gate, Event::Connected(Ok(library)))
            }
            // Offline only with something saved: otherwise the app shows that the server
            // cannot be reached, not an empty library. A session already offline stays so.
            Err(e @ ClientError::Unreachable(_)) => match self
                .known_library()
                .filter(|library| already_offline || self.has_pins(&library.library_id))
            {
                Some(library) => {
                    // Thumbnails come from the saved copies, then from the temporary cache.
                    self.open_cache(&library.library_id);
                    self.start_offline(&library.library_id);
                    let gate = Gate::Offline {
                        library: library.clone(),
                    };
                    (gate, Event::Offline(library))
                }
                None => (Gate::Closed(e.clone()), Event::Connected(Err(e))),
            },
            Err(e) => (Gate::Closed(e.clone()), Event::Connected(Err(e))),
        };
        *lock_gate(&self.gate) = gate;
        event
    }

    /// After a successful handshake: the server answers, the library is recorded, and what
    /// was not started yet (by an earlier handshake of this session) starts. Uploads start
    /// only here: they need the server.
    fn connected(&self, library: &LibraryInfo) {
        *self.health.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.open_cache(&library.library_id);
        if let (None, Some(root)) = (self.uploads.get(), &self.dirs.data) {
            match queue_path(root, &library.library_id, &self.profile.to_string()) {
                Some(path) => {
                    let _ = self.uploads.set(Uploads::start(EngineConfig {
                        url: self.url.clone(),
                        token: self.token.clone(),
                        app: self.client.app.clone(),
                        ctx: self.ctx.clone(),
                        gate: self.gate.clone(),
                        health: self.health.clone(),
                        cancelled: self.cancelled.clone(),
                        path,
                    }));
                }
                None => tracing::warn!("library or profile id is not a safe file name"),
            }
        }
        if let (Some(root), Some(_)) = (&self.dirs.data, &self.catalog) {
            if let Err(e) = KnownLibraries::new(root).record(self.profile, library) {
                tracing::warn!("cannot record the library for offline use: {e}");
            }
            self.start_offline(&library.library_id);
        }
    }

    /// Opens the library's thumbnail cache (and trims it), the first time its id is known.
    fn open_cache(&self, library_id: &str) {
        let (None, Some(root)) = (self.cache.get(), &self.dirs.cache) else {
            return;
        };
        match MediaCache::open(root, library_id) {
            Some(opened) => {
                let opened = Arc::new(opened);
                let _ = self.cache.set(opened.clone());
                trim_in_background(opened, self.cancelled.clone());
            }
            None => tracing::warn!("library id is not a safe cache directory name"),
        }
    }

    /// The library the profile was last connected to, if the session can work offline.
    fn known_library(&self) -> Option<LibraryInfo> {
        let (Some(root), Some(_)) = (&self.dirs.data, &self.catalog) else {
            return None;
        };
        KnownLibraries::new(root).get(self.profile)
    }

    /// Whether anything of `library_id` is kept offline (reads its pins: on the worker).
    fn has_pins(&self, library_id: &str) -> bool {
        self.dirs
            .data
            .as_deref()
            .and_then(|root| OfflineStore::open(root, library_id))
            .is_some_and(|store| !store.load_pins().is_empty())
    }

    /// Starts the library's offline handle, the first time its id is known.
    fn start_offline(&self, library_id: &str) {
        let (Some(root), Some(catalog)) = (&self.dirs.data, &self.catalog) else {
            return;
        };
        // A dropped session starts nothing: its `Drop` has already stopped what there was.
        if self.offline.get().is_some() || self.cancelled.load(Ordering::Acquire) {
            return;
        }
        let Some(store) = OfflineStore::open(root, library_id) else {
            tracing::warn!("library id is not a safe offline directory name");
            return;
        };
        let _ = self.offline.set(OfflineHandle::start(
            store,
            catalog.clone(),
            self.url.clone(),
            self.token.clone(),
            root.join("offline").join("settings.toml"),
            self.ctx.clone(),
        ));
    }
}

pub(crate) fn lock_gate(gate: &Mutex<Gate>) -> std::sync::MutexGuard<'_, Gate> {
    gate.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs `request` only if the handshake let it through. A `library.info` naming another
/// library closes the gate: this request and every later one fail with
/// [`ClientError::LibraryChanged`], and `poll` moves the session to `Failed`. Offline, the
/// saved copies answer (see [`answer_offline`]); a request that cannot reach the server is
/// answered from them too, if they can.
fn gated(
    gate: &Mutex<Gate>,
    health: &Mutex<Option<ClientError>>,
    offline: &OnceLock<OfflineHandle>,
    rpc: Result<&RpcClient, ClientError>,
    request: Request,
) -> Result<Response, ClientError> {
    let library_id = match lock_gate(gate).clone() {
        Gate::Open { library_id } => library_id,
        Gate::Offline { library } => return answer_offline(offline.get(), &request, library),
        Gate::Closed(e) => return Err(e),
        // With one worker thread the handshake always runs first; kept as a guard.
        Gate::Pending => {
            return Err(ClientError::Protocol(
                "request ran before the handshake finished".into(),
            ));
        }
    };
    // Kept only when the saved copies could answer it. A later page is not: a copy is the
    // whole answer, which would repeat the pages already shown; it fails, and is retried.
    let fallback = offline
        .get()
        .filter(|_| !is_later_page(&request))
        .map(|handle| (handle, request.clone()));
    let response = run(rpc?, request);
    note_health(health, &response);
    if let (Err(ClientError::Unreachable(_)), Some((handle, request))) = (&response, fallback)
        && let Some(saved) = saved_answer(handle, &request)
    {
        return saved;
    }
    let response = response?;
    if let Response::LibraryInfo(info) = &response
        && info.library_id != library_id
    {
        *lock_gate(gate) = Gate::Closed(ClientError::LibraryChanged);
        return Err(ClientError::LibraryChanged);
    }
    Ok(response)
}

/// Whether `request` asks for a page after the first: it carries a cursor.
fn is_later_page(request: &Request) -> bool {
    match request {
        Request::FolderList(p) => p.cursor.is_some(),
        Request::Call { params, .. } => params.get("cursor").is_some_and(|c| !c.is_null()),
        _ => false,
    }
}

/// The answer to `request` while offline: `library.info` is the recorded `library`; the
/// catalog answers the rest from the saved copies, and what it cannot answer is
/// [`ClientError::NotSaved`], or [`ClientError::Offline`] for a change.
fn answer_offline(
    offline: Option<&OfflineHandle>,
    request: &Request,
    library: LibraryInfo,
) -> Result<Response, ClientError> {
    if let Request::LibraryInfo = request {
        return Ok(Response::LibraryInfo(library));
    }
    offline
        .and_then(|handle| saved_answer(handle, request))
        .unwrap_or(Err(match request {
            Request::MediaUpdate(_) | Request::LibraryRescan => ClientError::Offline,
            _ => ClientError::NotSaved,
        }))
}

/// The catalog's answer from the saved copies, once they are read.
fn saved_answer(
    handle: &OfflineHandle,
    request: &Request,
) -> Option<Result<Response, ClientError>> {
    handle.wait_loaded();
    handle.catalog().answer(request, &handle.docs())
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
