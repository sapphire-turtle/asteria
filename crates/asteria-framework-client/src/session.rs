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
    /// and a changed `epoch` raises [`Session::take_refetch_listing`]. A different
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
}

/// The successful result of a [`Request`], in the same variant order.
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    LibraryInfo(LibraryInfo),
    LibraryRescan,
    FolderList(FolderListing),
    /// From `media.get` and `media.update`.
    Media(MediaItem),
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
    /// Filled by the handshake job once the library id is known (and only if a cache root was
    /// given and the id is a safe directory name).
    cache: Arc<OnceLock<Arc<MediaCache>>>,
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
        Self::open_with_cache(profile, client, ctx, None)
    }

    /// Like [`open`](Self::open), and with `cache_root` given, opens the library's
    /// [`MediaCache`] under it as soon as the handshake learns the library id, trimming it on
    /// the worker thread right after the handshake. The cache is available from [`media_cache`](Self::media_cache)
    /// by the time the state is [`SessionState::Ready`].
    pub fn open_with_cache(
        profile: &ServerProfile,
        client: ClientInfo,
        ctx: egui::Context,
        cache_root: Option<PathBuf>,
    ) -> Session {
        let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
        let (events_tx, events) = mpsc::channel();
        let gate = Arc::new(Mutex::new(Gate::Pending));
        let cache = Arc::new(OnceLock::new());
        let images = ImageLoader::new(
            ctx.clone(),
            profile.url.clone(),
            profile.token.clone(),
            gate.clone(),
            cache.clone(),
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
            cache,
            images,
        };
        let (url, token) = (profile.url.clone(), profile.token.clone());
        // The client itself is built on the worker thread.
        match Worker::spawn("asteria-session", 1, move || {
            RpcClient::new(&url, token.clone())
        }) {
            Ok(worker) => {
                session.worker = Some(worker);
                let gate = session.gate.clone();
                let cache = session.cache.clone();
                session.submit(move |rpc| {
                    let result = rpc.and_then(|rpc| handshake(rpc, &client));
                    if let (Ok(library), Some(root)) = (&result, &cache_root) {
                        match MediaCache::open(root, &library.library_id) {
                            Some(opened) => {
                                let _ = cache.set(Arc::new(opened));
                            }
                            None => tracing::warn!("library id is not a safe cache directory name"),
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
                // A separate job, queued after the handshake's: a big cache must not delay Ready.
                // It runs on the worker and is skipped if the session is dropped first.
                let cache = session.cache.clone();
                if let Some(worker) = &session.worker {
                    worker.submit(Box::new(move |_| {
                        if let Some(cache) = cache.get() {
                            cache.trim();
                        }
                    }));
                }
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
    /// given to [`open_with_cache`](Self::open_with_cache). Hand the `Arc` to jobs that run on
    /// worker threads: its methods do disk I/O, so not the UI thread.
    pub fn media_cache(&self) -> Option<Arc<MediaCache>> {
        self.cache.get().cloned()
    }

    /// Thumbnails and previews of this session's server, as egui textures.
    pub fn images(&mut self) -> &mut ImageLoader {
        &mut self.images
    }

    pub fn state(&self) -> &SessionState {
        &self.state
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
        self.submit(move |rpc| {
            Event::Reply(Reply {
                id,
                result: gated(&gate, rpc, request),
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
        replies
    }

    /// Whether the library's contents changed (its epoch moved) since the last call, so the
    /// listing on screen should be fetched again. Clears the mark.
    pub fn take_refetch_listing(&mut self) -> bool {
        std::mem::take(&mut self.refetch_listing)
    }

    fn library_info_arrived(&mut self, info: &LibraryInfo) {
        if let SessionState::Ready { library } = &mut self.state {
            if library.epoch != info.epoch {
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
    let response = run(rpc?, request)?;
    if let Response::LibraryInfo(info) = &response
        && info.library_id != library_id
    {
        *lock_gate(gate) = Gate::Closed(ClientError::LibraryChanged);
        return Err(ClientError::LibraryChanged);
    }
    Ok(response)
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
    })
}
