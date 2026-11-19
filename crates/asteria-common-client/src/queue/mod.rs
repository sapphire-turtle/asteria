//! The rating queue kept on disk, so a rating made offline is sent once the server is back
//! (spec §5.2). [`OpQueue`] is its side on the UI thread. Each queue has two threads: the
//! appender writes each rating to the file as it comes, and the sender sends what the file
//! holds with `ops.apply`.
//!
//! The UI side only touches memory: [`OpQueue::rate`] records the rating at once (so the star
//! shows it) and hands it to the appender. The two threads share the file under a lock taken
//! only for disk work, never across a request, so a rating reaches the file even while a send
//! waits on the server; neither holds the memory lock across I/O, so the UI never waits on the
//! disk or the network (except [`OpQueue::stop`], which waits for the appends, not the
//! server).
//!
//! One queue file has one owner in the process ([`Ticket`], held by the appender): a queue that
//! opens on a file whose previous queue is still finishing waits for it to let go, and queues
//! get the file in the order they were started, so the ratings of a dropped queue are saved
//! before a newer queue reads the file. A stopped queue lets go once its appends are done,
//! without waiting for a send in flight: what that send settled goes again next time.

mod file;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use asteria_common_api::{
    ItemId, LIBRARY_INFO, LibraryInfo, MAX_RATING_OPS, MEDIA_UPDATE, MediaItem, MediaUpdateParams,
    OPS_APPLY, OpKind, OpsApplyParams, OpsApplyResult, RatingOp, RatingOpStatus, RejectReason,
    codes,
};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use corundum_framework::egui;
use grain_id::GrainId;
use serde::Serialize;
use serde::de::DeserializeOwned;
use url::Url;

use self::file::{QueueFile, QueuedRating};
use crate::claim::Ticket;
use crate::media_cache::safe_component;
use crate::profiles::Token;
use crate::rpc::{ClientError, RpcClient, ends_session};
use crate::session::{Gate, Health, lock_gate, note_health};

/// `<data_root>/queue/<library_id>/<profile_id>.jsonl`, or `None` if either id is not a safe
/// file name. One queue per library and profile, so two users of one server never share one.
pub fn queue_path(data_root: &Path, library_id: &str, profile_id: &str) -> Option<PathBuf> {
    (safe_component(library_id) && safe_component(profile_id)).then(|| {
        data_root
            .join("queue")
            .join(library_id)
            .join(format!("{profile_id}.jsonl"))
    })
}

/// The first wait after the server could not be reached or asked to wait, in milliseconds;
/// each further failure doubles it, up to [`BACKOFF_MAX_MS`]: 1, 2, 4, 8, 16, then 30 minutes.
static BACKOFF_INITIAL_MS: AtomicU64 = AtomicU64::new(60_000);
static BACKOFF_MAX_MS: AtomicU64 = AtomicU64::new(30 * 60_000);

/// Sets the retry waits (normally 1 min doubling up to 30 min) for every queue in the process.
/// For tests, which cannot sit through the real ones.
#[doc(hidden)]
pub fn set_retry_backoff(initial: Duration, max: Duration) {
    BACKOFF_INITIAL_MS.store(initial.as_millis() as u64, Ordering::Relaxed);
    BACKOFF_MAX_MS.store(max.as_millis() as u64, Ordering::Relaxed);
}

/// While the handshake has not finished, the sender looks at the gate again this often.
const GATE_PENDING_POLL: Duration = Duration::from_millis(200);

/// The longest [`OpQueue::stop`] waits for the appender: only a queue file another live queue
/// keeps could take longer, and the app must still close.
const FLUSH_LIMIT: Duration = Duration::from_secs(5);

/// Something about a rating the user should hear of.
#[derive(Debug, Clone, PartialEq)]
pub enum Notice {
    /// The server refused the rating; the item keeps the server's value.
    RatingRejected { item: ItemId, reason: RejectReason },
    /// The rating could not be stored on this device, so it will not be sent.
    RatingNotSaved { item: ItemId },
}

/// What the queue needs from its session.
pub(crate) struct QueueConfig {
    pub url: Url,
    pub token: Token,
    pub ctx: egui::Context,
    pub gate: Arc<Mutex<Gate>>,
    pub health: Health,
    /// The session's worker flag: set once the session is dropped.
    pub cancelled: Arc<AtomicBool>,
    /// `<data_root>/queue/<library_id>/<profile_id>.jsonl` ([`queue_path`]).
    pub path: PathBuf,
}

/// The newest rating of an item that the server has not settled yet.
struct Unsettled {
    /// The operation that carries it.
    id: String,
    value: i8,
}

/// What the UI reads; both sides keep it current.
#[derive(Default)]
struct State {
    /// Per item, the latest [`OpQueue::rate`] the server has not settled yet.
    pending: HashMap<ItemId, Unsettled>,
    /// The rating the server returned this session, for the items it settled.
    confirmed: HashMap<ItemId, i8>,
    notices: Vec<Notice>,
}

/// The ratings handed to the appender and not handled yet, for [`OpQueue::stop`] to wait on.
#[derive(Default)]
struct Handoff {
    waiting: usize,
    /// The appender has stopped: nothing more will be handled.
    closed: bool,
}

struct Inner {
    state: Mutex<State>,
    ctx: egui::Context,
    dropped: AtomicBool,
    handoff: Mutex<Handoff>,
    handled: Condvar,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn handoff(&self, f: impl FnOnce(&mut Handoff)) {
        f(&mut self.handoff.lock().unwrap_or_else(|e| e.into_inner()));
        self.handled.notify_all();
    }

    /// Applies `f` to the state and asks for a repaint.
    fn change(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.lock());
        self.ctx.request_repaint();
    }
}

/// Forgets `op` as `item`'s unsettled rating, unless a newer one has taken its place.
/// Whether it did.
fn settle(s: &mut State, item: &ItemId, op: &str) -> bool {
    let current = s.pending.get(item).is_some_and(|u| u.id == op);
    if current {
        s.pending.remove(item);
    }
    current
}

/// `op` could not be stored, so it will never be sent. Noticed once, and only while it is
/// still the item's newest rating: a newer click has its own fate.
fn not_saved(s: &mut State, op: &QueuedRating) {
    if settle(s, &op.item, &op.id) {
        s.notices.push(Notice::RatingNotSaved {
            item: op.item.clone(),
        });
    }
}

/// For the appender.
enum Msg {
    Rate(QueuedRating),
    Wake,
}

/// From the appender to the sender.
enum Nudge {
    /// A rating was appended, the file's `n`th: it goes at once, unless the sender has
    /// seen it already.
    Rated(u64),
    /// The server answers again (or the queue stops).
    Wake,
}

/// The rating queue of one session, for the UI. Every method but [`stop`](Self::stop) returns
/// at once; the file and the server are handled on the queue's threads, which stop when this
/// is dropped (with its [`Session`](crate::session::Session)).
pub struct OpQueue {
    inner: Arc<Inner>,
    tx: mpsc::Sender<Msg>,
}

impl OpQueue {
    /// Starts the threads for the queue at `cfg.path`. The appender claims the file (waiting
    /// for a previous queue on it to let go), opens it and starts the sender, which sends
    /// whatever is pending whenever the gate is open.
    pub(crate) fn start(cfg: QueueConfig) -> OpQueue {
        let inner = Arc::new(Inner {
            state: Mutex::new(State::default()),
            ctx: cfg.ctx.clone(),
            dropped: AtomicBool::new(false),
            handoff: Mutex::new(Handoff::default()),
            handled: Condvar::new(),
        });
        let (tx, rx) = mpsc::channel();
        let thread_inner = inner.clone();
        let ticket = Ticket::new(&cfg.path);
        let spawned = std::thread::Builder::new()
            .name("asteria-ratings".into())
            .spawn(move || run(thread_inner, rx, ticket, cfg));
        if let Err(e) = spawned {
            // The receiver went with the closure, so every `rate` reports itself not saved.
            tracing::warn!(error = %e, "cannot start the rating queue thread");
        }
        OpQueue { inner, tx }
    }

    /// Records the rating in memory at once and hands it to the appender to save, then to the
    /// sender.
    pub fn rate(&self, item: ItemId, value: i8) {
        let op = QueuedRating {
            id: GrainId::random().to_string(),
            item,
            value,
            at: Utc::now(),
        };
        self.inner.change(|s| {
            s.pending.insert(
                op.item.clone(),
                Unsettled {
                    id: op.id.clone(),
                    value,
                },
            );
        });
        self.inner.handoff(|h| h.waiting += 1);
        if let Err(mpsc::SendError(Msg::Rate(op))) = self.tx.send(Msg::Rate(op)) {
            self.inner.handoff(|h| h.waiting -= 1);
            self.inner.change(|s| not_saved(s, &op));
        }
    }

    /// Not yet confirmed: the newest pending value of `item`, else the value the server returned
    /// for it this session (applied or superseded), else None.
    pub fn rating(&self, item: &ItemId) -> Option<i8> {
        let s = self.inner.lock();
        s.pending
            .get(item)
            .map(|u| u.value)
            .or_else(|| s.confirmed.get(item).copied())
    }

    /// Pending ratings, counted after coalescing (one per item).
    pub fn pending(&self) -> usize {
        self.inner.lock().pending.len()
    }

    /// The notices since the last call, oldest first.
    pub fn take_notices(&self) -> Vec<Notice> {
        std::mem::take(&mut self.inner.lock().notices)
    }

    /// Send now if anything is pending (reconnect).
    pub(crate) fn wake(&self) {
        let _ = self.tx.send(Msg::Wake);
    }

    /// Stops the threads as dropping the queue does (see its [`Drop`]), and waits until every
    /// rating handed to [`rate`](Self::rate) is in the file (or noticed as not saved): the
    /// disk, never the server. For the session, whose queue may be shared a little longer with
    /// a job on its worker, and which must not lose a rating when the app quits.
    pub(crate) fn stop(&self) {
        self.inner.dropped.store(true, Ordering::Release);
        // Wakes the thread even while another sender (a test's gate) keeps the channel open.
        let _ = self.tx.send(Msg::Wake);
        let deadline = Instant::now() + FLUSH_LIMIT;
        let mut h = self.inner.handoff.lock().unwrap_or_else(|e| e.into_inner());
        while h.waiting > 0 && !h.closed {
            let now = Instant::now();
            if now >= deadline {
                tracing::warn!(waiting = h.waiting, "ratings not saved before closing");
                return;
            }
            h = self
                .inner
                .handled
                .wait_timeout(h, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// The appender saves what was rated before the drop, then lets go of the file; a request in
/// flight finishes on its own, and what it settled is sent again next time.
impl Drop for OpQueue {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The gate of a queue started by [`start_for_test`].
#[doc(hidden)]
pub struct TestGate {
    gate: Arc<Mutex<Gate>>,
    tx: mpsc::Sender<Msg>,
}

#[doc(hidden)]
impl TestGate {
    pub fn open(&self) {
        *lock_gate(&self.gate) = Gate::Open {
            library_id: "test".into(),
        };
    }

    pub fn close(&self, error: ClientError) {
        *lock_gate(&self.gate) = Gate::Closed(error);
    }

    pub fn offline(&self) {
        *lock_gate(&self.gate) = Gate::Offline {
            library: LibraryInfo {
                library_id: "test".into(),
                owner: "test".into(),
                epoch: "test".into(),
                item_count: 0,
                scanning: false,
                revision: 0,
            },
        };
    }

    /// What [`OpQueue::wake`] does.
    pub fn wake(&self) {
        let _ = self.tx.send(Msg::Wake);
    }
}

/// Starts a queue without a session: its gate is open, or pending until [`TestGate`] sets it.
#[doc(hidden)]
pub fn start_for_test(
    url: Url,
    token: Token,
    path: PathBuf,
    ctx: egui::Context,
    gate_open: bool,
) -> (OpQueue, TestGate) {
    let gate = Arc::new(Mutex::new(Gate::Pending));
    let test_gate_gate = gate.clone();
    if gate_open {
        *lock_gate(&gate) = Gate::Open {
            library_id: "test".into(),
        };
    }
    let queue = OpQueue::start(QueueConfig {
        url,
        token,
        ctx,
        gate,
        health: Arc::new(Mutex::new(None)),
        cancelled: Arc::new(AtomicBool::new(false)),
        path,
    });
    let test_gate = TestGate {
        gate: test_gate_gate,
        tx: queue.tx.clone(),
    };
    (queue, test_gate)
}

/// The appender thread: claims and opens the file, starts the sender, then appends until the
/// queue is dropped.
fn run(inner: Arc<Inner>, rx: mpsc::Receiver<Msg>, ticket: Ticket, cfg: QueueConfig) {
    let cancelled = cfg.cancelled.clone();
    // Ratings already made are not given up: they wait for the file however long it takes.
    // Runs with the claims locked, so the lock order is claims, then state; nothing takes
    // the claims while holding the state.
    let give_up = || {
        (cancelled.load(Ordering::Acquire) || inner.dropped.load(Ordering::Acquire))
            && inner.lock().pending.is_empty()
    };
    let Some(_claim) = ticket.claim(give_up) else {
        inner.handoff(|h| h.closed = true);
        return;
    };
    let file = match QueueFile::open(&cfg.path) {
        Ok(file) => Some(file),
        Err(e) => {
            tracing::warn!(path = %cfg.path.display(), error = %e, "rating queue unavailable");
            None
        }
    };
    let disk = Arc::new(Mutex::new(Disk::new(file, &inner)));
    let (nudge, nudges) = mpsc::channel();
    let sender = Sender::new(inner.clone(), disk.clone(), nudges, cfg);
    let spawned = std::thread::Builder::new()
        .name("asteria-ratings-send".into())
        .spawn(move || sender.run());
    if let Err(e) = spawned {
        // Ratings are still saved, for the next session to send.
        tracing::warn!(error = %e, "cannot start the rating queue's sender");
    }
    Appender {
        inner,
        rx,
        disk,
        nudge,
        cancelled,
    }
    .run();
}

/// The queue file and what goes with it, shared by the appender and the sender. Locked only
/// for disk work, never across a request.
struct Disk {
    /// `None` if it could not be opened (every rating is then reported not saved), or once the
    /// appender has let go of it.
    file: Option<QueueFile>,
    /// The time of the newest rating in the file, so each one appended is later.
    last_at: Option<DateTime<Utc>>,
    /// Ratings appended so far.
    appended: u64,
}

impl Disk {
    /// Shows what the file holds. A rating made while the file was being opened is newer than
    /// any in it, so it keeps its place.
    fn new(file: Option<QueueFile>, inner: &Inner) -> Disk {
        let last_at = file
            .as_ref()
            .and_then(|f| f.pending().iter().map(|p| p.at).max());
        let send = file.as_ref().map(|f| f.coalesced().0).unwrap_or_default();
        if !send.is_empty() {
            inner.change(|s| {
                for op in send {
                    s.pending.entry(op.item).or_insert(Unsettled {
                        id: op.id,
                        value: op.value,
                    });
                }
            });
        }
        Disk {
            file,
            last_at,
            appended: 0,
        }
    }
}

fn lock_disk(disk: &Mutex<Disk>) -> MutexGuard<'_, Disk> {
    disk.lock().unwrap_or_else(|e| e.into_inner())
}

/// Saves each rating as it comes; owns the file's claim.
struct Appender {
    inner: Arc<Inner>,
    rx: mpsc::Receiver<Msg>,
    disk: Arc<Mutex<Disk>>,
    nudge: mpsc::Sender<Nudge>,
    cancelled: Arc<AtomicBool>,
}

impl Appender {
    fn stopped(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || self.inner.dropped.load(Ordering::Acquire)
    }

    fn run(self) {
        loop {
            // Appends what was rated before a drop, too.
            let open = loop {
                match self.rx.try_recv() {
                    Ok(m) => self.handle(m),
                    Err(mpsc::TryRecvError::Empty) => break true,
                    Err(mpsc::TryRecvError::Disconnected) => break false,
                }
            };
            if !open || self.stopped() {
                break;
            }
            match self.rx.recv() {
                Ok(m) => self.handle(m),
                // The channel reports itself closed only once nothing is left in it.
                Err(mpsc::RecvError) => break,
            }
        }
        self.finish();
    }

    fn handle(&self, msg: Msg) {
        match msg {
            Msg::Rate(op) => {
                let appended = self.append(op);
                self.inner
                    .handoff(|h| h.waiting = h.waiting.saturating_sub(1));
                if let Some(n) = appended {
                    let _ = self.nudge.send(Nudge::Rated(n));
                }
            }
            Msg::Wake => {
                let _ = self.nudge.send(Nudge::Wake);
            }
        }
    }

    /// Which rating of the file `op` became; `None` if it could not be saved.
    fn append(&self, mut op: QueuedRating) -> Option<u64> {
        let mut disk = lock_disk(&self.disk);
        let last_at = disk.last_at;
        let Some(file) = &mut disk.file else {
            drop(disk);
            self.inner.change(|s| not_saved(s, &op));
            return None;
        };
        // Later than every rating before it, even if the clock went back, so the newest click
        // wins both the coalescing here and the server's merge by time.
        if let Some(last) = last_at
            && op.at <= last
        {
            op.at = last + TimeDelta::milliseconds(1);
        }
        match file.append(op.clone()) {
            Ok(()) => {
                disk.last_at = Some(op.at);
                disk.appended += 1;
                Some(disk.appended)
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot save a rating");
                // An older rating of the item still in the file is the one that will be sent.
                let older = file
                    .pending()
                    .iter()
                    .rev()
                    .find(|p| p.item == op.item)
                    .map(|p| Unsettled {
                        id: p.id.clone(),
                        value: p.value,
                    });
                drop(disk);
                self.inner.change(|s| {
                    not_saved(s, &op);
                    if let Some(older) = older {
                        s.pending.entry(op.item.clone()).or_insert(older);
                    }
                });
                None
            }
        }
    }

    /// Once the queue stops: lets go of the file (a send in flight then settles nothing in
    /// it), and reports a rating that reached neither the file nor the thread (one made after
    /// a cancel, as the thread let go) as not saved rather than showing it forever.
    fn finish(self) {
        let Appender {
            inner,
            rx,
            disk,
            nudge,
            ..
        } = self;
        // From here on `rate` reports a failed send itself.
        drop(rx);
        let file = lock_disk(&disk).file.take();
        // The sender sees its channel close and ends.
        drop(nudge);
        let saved: HashSet<&str> = file
            .iter()
            .flat_map(|f| f.pending())
            .map(|p| p.id.as_str())
            .collect();
        inner.change(|s| {
            let lost: Vec<ItemId> = s
                .pending
                .iter()
                .filter(|(_, u)| !saved.contains(u.id.as_str()))
                .map(|(item, _)| item.clone())
                .collect();
            for item in lost {
                s.pending.remove(&item);
                s.notices.push(Notice::RatingNotSaved { item });
            }
        });
        inner.handoff(|h| h.closed = true);
    }
}

/// The `{}` sent as the params of `library.info`.
#[derive(Serialize)]
struct NoParams {}

/// What a step of the sender came to.
enum Step {
    /// Something was sent; look for more at once.
    Worked,
    /// Nothing to do until a message arrives.
    Idle,
    /// Nothing to do before this long has passed (or a message arrives).
    Wait(Duration),
}

/// What the server did with one rating.
enum Outcome {
    /// Applied, or superseded by one at least as new; the item's rating is now this.
    Rated(Option<i8>),
    Rejected(RejectReason),
}

/// Sends what the file holds, whenever the gate is open.
struct Sender {
    inner: Arc<Inner>,
    disk: Arc<Mutex<Disk>>,
    rx: mpsc::Receiver<Nudge>,
    /// Built on the first send, so building it never holds up an append.
    rpc: Option<RpcClient>,
    url: Url,
    token: Option<Token>,
    gate: Arc<Mutex<Gate>>,
    health: Health,
    cancelled: Arc<AtomicBool>,
    /// The server has no `ops.apply` (older than API 2.4): rate with `media.update`.
    legacy: bool,
    /// The server takes nothing more from this session.
    halted: bool,
    /// The latest retry wait; `None` after a success.
    backoff: Option<Duration>,
    /// No server request before this.
    retry_at: Option<Instant>,
    /// [`Disk::appended`] when the sender last took what to send from the file: a rating
    /// already sent does not end the wait after a failure.
    seen: u64,
}

impl Sender {
    fn new(
        inner: Arc<Inner>,
        disk: Arc<Mutex<Disk>>,
        rx: mpsc::Receiver<Nudge>,
        cfg: QueueConfig,
    ) -> Sender {
        Sender {
            inner,
            disk,
            rx,
            rpc: None,
            url: cfg.url,
            token: Some(cfg.token),
            gate: cfg.gate,
            health: cfg.health,
            cancelled: cfg.cancelled,
            legacy: false,
            halted: false,
            backoff: None,
            retry_at: None,
            seen: 0,
        }
    }

    fn stopped(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || self.inner.dropped.load(Ordering::Acquire)
    }

    fn run(mut self) {
        loop {
            if !self.pump() || self.stopped() {
                return;
            }
            let wait = match self.step() {
                Step::Worked => continue,
                Step::Idle => None,
                Step::Wait(d) => Some(d),
            };
            let nudge = match wait {
                None => self.rx.recv().ok(),
                Some(d) => match self.rx.recv_timeout(d) {
                    Ok(m) => Some(m),
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => None,
                },
            };
            match nudge {
                Some(m) => self.handle(m),
                // The appender has stopped.
                None => return,
            }
        }
    }

    /// Handles the nudges that have arrived, without waiting. `false` once the appender is
    /// gone.
    fn pump(&mut self) -> bool {
        loop {
            match self.rx.try_recv() {
                Ok(m) => self.handle(m),
                Err(mpsc::TryRecvError::Empty) => return true,
                Err(mpsc::TryRecvError::Disconnected) => return false,
            }
        }
    }

    fn handle(&mut self, nudge: Nudge) {
        match nudge {
            Nudge::Rated(n) if n > self.seen => {
                // A new rating goes at once (spec §5.2). The backoff stays, so a further
                // failure still waits longer.
                self.retry_at = None;
            }
            // Already in the last send (which may have failed and be waiting).
            Nudge::Rated(_) => {}
            Nudge::Wake => {
                // The server answers again: no reason to sit out the rest of a wait.
                self.backoff = None;
                self.retry_at = None;
            }
        }
    }

    /// One exchange with the server, if one is due.
    fn step(&mut self) -> Step {
        let has_pending = lock_disk(&self.disk)
            .file
            .as_ref()
            .is_some_and(|f| !f.pending().is_empty());
        if self.halted || !has_pending {
            return Step::Idle;
        }
        if let Some(at) = self.retry_at {
            let now = Instant::now();
            if now < at {
                return Step::Wait(at - now);
            }
        }
        let library_id = match &*lock_gate(&self.gate) {
            Gate::Open { library_id } => library_id.clone(),
            Gate::Pending => return Step::Wait(GATE_PENDING_POLL),
            // Offline: a wake brings the sender back. Closed: nothing more goes to this
            // server, and the file keeps the ratings for the next session.
            Gate::Offline { .. } | Gate::Closed(_) => return Step::Idle,
        };
        if self.rpc.is_none()
            && let Some(token) = self.token.take()
        {
            match RpcClient::new(&self.url, token) {
                Ok(rpc) => self.rpc = Some(rpc),
                Err(e) => {
                    // Kept in the file for the next session.
                    tracing::warn!(error = %e, "cannot build a client for the rating queue");
                    self.halted = true;
                    return Step::Idle;
                }
            }
        }
        let result = self.same_library(&library_id).and_then(|same| {
            if same {
                self.send_pending().map(|()| true)
            } else {
                Ok(false)
            }
        });
        match result {
            Ok(true) => {
                self.backoff = None;
                self.retry_at = None;
                Step::Worked
            }
            Ok(false) => {
                tracing::warn!("the server serves another library; ratings kept");
                {
                    let mut gate = lock_gate(&self.gate);
                    if matches!(&*gate, Gate::Open { library_id: id } if *id == library_id) {
                        *gate = Gate::Closed(ClientError::LibraryChanged);
                    }
                }
                // The session sees the closed gate at its next poll and fails.
                self.inner.ctx.request_repaint();
                self.halted = true;
                Step::Idle
            }
            Err(e) if ends_session(&e) => {
                tracing::warn!(error = %e, "the server refused the rating queue");
                self.halted = true;
                Step::Idle
            }
            Err(e) => {
                tracing::debug!(error = %e, "ratings not sent; retrying later");
                let initial = Duration::from_millis(BACKOFF_INITIAL_MS.load(Ordering::Relaxed));
                let max = Duration::from_millis(BACKOFF_MAX_MS.load(Ordering::Relaxed));
                let mut wait = self.backoff.map_or(initial, |b| (b * 2).min(max));
                self.backoff = Some(wait);
                if let ClientError::Retry { after } = &e {
                    wait = wait.max(*after);
                }
                self.retry_at = Some(Instant::now() + wait);
                Step::Wait(wait)
            }
        }
    }

    /// Whether the server still serves the library the session opened (`library.info`), so
    /// a server that came back with another library never gets this one's ratings: its ids
    /// would all be refused `not_found`, and the ratings lost.
    fn same_library(&self, library_id: &str) -> Result<bool, ClientError> {
        let info: LibraryInfo = self.call(LIBRARY_INFO, &NoParams {})?;
        Ok(info.library_id == library_id)
    }

    /// Sends the newest pending rating of each item, in batches of [`MAX_RATING_OPS`]; the
    /// older ones are settled unsent. Stops at the first batch that failed.
    fn send_pending(&mut self) -> Result<(), ClientError> {
        let send = {
            let mut disk = lock_disk(&self.disk);
            self.seen = disk.appended;
            let Some(file) = disk.file.as_mut() else {
                return Ok(());
            };
            let (send, obsolete) = file.coalesced();
            if let Err(e) = file.mark_done(&obsolete) {
                // They stay pending and are coalesced away again next time.
                tracing::warn!(error = %e, "cannot settle older ratings in the queue");
            }
            send
        };
        for batch in send.chunks(MAX_RATING_OPS) {
            if self.stopped() {
                return Ok(());
            }
            if !self.legacy {
                match self.apply(batch) {
                    Err(ClientError::Rpc { code, .. }) if code == codes::METHOD_NOT_FOUND => {
                        tracing::info!("the server has no ops.apply; rating with media.update");
                        self.legacy = true;
                    }
                    other => {
                        other?;
                        continue;
                    }
                }
            }
            for op in batch {
                if self.stopped() {
                    return Ok(());
                }
                self.update(op)?;
            }
        }
        Ok(())
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

    /// One `ops.apply`. Every answered operation is settled; one left unanswered, or a file
    /// that cannot record the answers, is an `Err`, so the rest goes again after a wait. So
    /// is an error answer for the whole call, `BUSY` among them (a long batch holds the
    /// server's writes): nothing is refused or noticed.
    fn apply(&mut self, batch: &[QueuedRating]) -> Result<(), ClientError> {
        let params = OpsApplyParams {
            ops: batch
                .iter()
                .map(|op| RatingOp {
                    id: op.id.clone(),
                    kind: OpKind::Rating,
                    item: op.item.clone(),
                    value: op.value,
                    at: op.at.to_rfc3339_opts(SecondsFormat::Millis, true),
                })
                .collect(),
        };
        let result: OpsApplyResult = self.call(OPS_APPLY, &params)?;
        let by_id: HashMap<&str, &QueuedRating> =
            batch.iter().map(|op| (op.id.as_str(), op)).collect();
        let mut outcomes = Vec::new();
        for r in result.results {
            let Some(op) = by_id.get(r.id.as_str()) else {
                continue;
            };
            let outcome = match r.status {
                RatingOpStatus::Applied | RatingOpStatus::Superseded => {
                    Outcome::Rated(r.item.and_then(|i| i.rating))
                }
                RatingOpStatus::Rejected => {
                    Outcome::Rejected(r.reason.unwrap_or(RejectReason::Invalid))
                }
            };
            outcomes.push(((*op).clone(), outcome));
        }
        let answered = outcomes.len();
        self.settle(outcomes)?;
        if answered < batch.len() {
            return Err(ClientError::Protocol(format!(
                "{OPS_APPLY}: {answered} of {} operations answered",
                batch.len()
            )));
        }
        Ok(())
    }

    /// One rating with `media.update`, for a server without `ops.apply`. An error answer
    /// refuses this rating (`NOT_FOUND`, `READ_ONLY` and `INVALID_PARAMS` as themselves, any
    /// other but `BUSY` as read-only); a `BUSY` answer and a failed exchange are an `Err`.
    fn update(&mut self, op: &QueuedRating) -> Result<(), ClientError> {
        let params = MediaUpdateParams {
            id: op.item.clone(),
            rating: Some(op.value),
            ..MediaUpdateParams::default()
        };
        let outcome = match self.call::<_, MediaItem>(MEDIA_UPDATE, &params) {
            Ok(item) => Outcome::Rated(item.rating),
            Err(ClientError::Rpc { code, .. }) if code == codes::NOT_FOUND => {
                Outcome::Rejected(RejectReason::NotFound)
            }
            Err(ClientError::Rpc { code, .. }) if code == codes::READ_ONLY => {
                Outcome::Rejected(RejectReason::ReadOnly)
            }
            Err(ClientError::Rpc { code, .. }) if code == codes::INVALID_PARAMS => {
                Outcome::Rejected(RejectReason::Invalid)
            }
            // The server's writes are held up (a long batch): this rating and the rest wait
            // and go again.
            Err(e @ ClientError::Rpc { code, .. }) if code == codes::BUSY => return Err(e),
            // The server answered for this item alone: refused, so the others still go.
            Err(ClientError::Rpc { code, message }) => {
                tracing::warn!(code, %message, "the server refused a rating");
                Outcome::Rejected(RejectReason::ReadOnly)
            }
            // The server could not be reached or refused the session: the batch waits.
            Err(e) => return Err(e),
        };
        self.settle(vec![(op.clone(), outcome)])
    }

    /// Marks the answered operations done in the file, then shows what the server said.
    fn settle(&mut self, outcomes: Vec<(QueuedRating, Outcome)>) -> Result<(), ClientError> {
        if outcomes.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = outcomes.iter().map(|(op, _)| op.id.clone()).collect();
        // The server has them either way. If the file cannot say so, they go again after a
        // wait and come back superseded (a refused one is refused, and noticed, again). A
        // file let go of (the queue stopped) is left alone: the next session sends them again.
        let saved = match lock_disk(&self.disk).file.as_mut() {
            Some(file) => file.mark_done(&ids),
            None => Ok(()),
        };
        self.inner.change(|s| {
            for (op, outcome) in outcomes {
                settle(s, &op.item, &op.id);
                match outcome {
                    Outcome::Rated(Some(value)) => {
                        s.confirmed.insert(op.item, value);
                    }
                    // Not something `rating` can say; the item's own rating shows instead.
                    Outcome::Rated(None) => {
                        s.confirmed.remove(&op.item);
                    }
                    Outcome::Rejected(reason) => s.notices.push(Notice::RatingRejected {
                        item: op.item,
                        reason,
                    }),
                }
            }
        });
        saved.map_err(|e| ClientError::Io(format!("cannot update the rating queue: {e}")))
    }
}
