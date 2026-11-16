//! The rating queue kept on disk, so a rating made offline is sent once the server is back
//! (spec §5.2). [`OpQueue`] is its side on the UI thread; one sender thread per queue appends
//! to the file and sends with `ops.apply`.
//!
//! The UI side only touches memory: [`OpQueue::rate`] records the rating at once (so the star
//! shows it) and hands it to the thread. The thread alone owns the file, and never holds the
//! memory lock across I/O, so the UI never waits on the disk or the network.
//!
//! One queue file has one owner in the process ([`Ticket`]): a queue that opens on a file whose
//! previous queue is still finishing waits for it to let go, and queues get the file in the
//! order they were started, so the ratings of a dropped queue are saved before a newer queue
//! reads the file.

mod file;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use asteria_common_api::{
    ItemId, LibraryInfo, MAX_RATING_OPS, MEDIA_UPDATE, MediaItem, MediaUpdateParams, OPS_APPLY,
    OpKind, OpsApplyParams, OpsApplyResult, RatingOp, RatingOpStatus, RejectReason, codes,
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

struct Inner {
    state: Mutex<State>,
    ctx: egui::Context,
    dropped: AtomicBool,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
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

enum Msg {
    Rate(QueuedRating),
    Wake,
}

/// The rating queue of one session, for the UI. Every method returns at once; the file and
/// the server are handled on the queue's thread, which stops when this is dropped (with its
/// [`Session`](crate::session::Session)).
pub struct OpQueue {
    inner: Arc<Inner>,
    tx: mpsc::Sender<Msg>,
}

impl OpQueue {
    /// Starts the sender thread for the queue at `cfg.path`. It claims the file (waiting for a
    /// previous queue on it to let go), opens it, and sends whatever is pending whenever the
    /// gate is open.
    pub(crate) fn start(cfg: QueueConfig) -> OpQueue {
        let inner = Arc::new(Inner {
            state: Mutex::new(State::default()),
            ctx: cfg.ctx.clone(),
            dropped: AtomicBool::new(false),
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

    /// Records the rating in memory at once and hands it to the thread to append and send.
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
        if let Err(mpsc::SendError(Msg::Rate(op))) = self.tx.send(Msg::Rate(op)) {
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

    /// Stops the thread as dropping the queue does (see its [`Drop`]). For the session, whose
    /// queue may be shared a little longer with a job on its worker.
    pub(crate) fn stop(&self) {
        self.inner.dropped.store(true, Ordering::Release);
        // Wakes the thread even while another sender (a test's gate) keeps the channel open.
        let _ = self.tx.send(Msg::Wake);
    }
}

/// The thread still appends what was rated before the drop and finishes a request in flight,
/// then exits, releasing the file.
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

/// The thread: claims and opens the file, then appends and sends until the queue is dropped.
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
        return;
    };
    let file = match QueueFile::open(&cfg.path) {
        Ok(file) => Some(file),
        Err(e) => {
            tracing::warn!(path = %cfg.path.display(), error = %e, "rating queue unavailable");
            None
        }
    };
    Sender::new(inner, rx, file, cfg).run();
}

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

struct Sender {
    inner: Arc<Inner>,
    rx: mpsc::Receiver<Msg>,
    /// `None` if it could not be opened: every rating is then reported not saved.
    file: Option<QueueFile>,
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
    /// The time of the newest rating in the file, so each one appended is later.
    last_at: Option<DateTime<Utc>>,
}

impl Sender {
    fn new(
        inner: Arc<Inner>,
        rx: mpsc::Receiver<Msg>,
        file: Option<QueueFile>,
        cfg: QueueConfig,
    ) -> Sender {
        let mut sender = Sender {
            inner,
            rx,
            file,
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
            last_at: None,
        };
        sender.load();
        sender
    }

    /// Shows what the file holds. A rating made while the file was being opened is newer than
    /// any in it, so it keeps its place.
    fn load(&mut self) {
        let Some(file) = &self.file else { return };
        self.last_at = file.pending().iter().map(|p| p.at).max();
        let (send, _) = file.coalesced();
        if send.is_empty() {
            return;
        }
        self.inner.change(|s| {
            for op in send {
                s.pending.entry(op.item).or_insert(Unsettled {
                    id: op.id,
                    value: op.value,
                });
            }
        });
    }

    fn stopped(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || self.inner.dropped.load(Ordering::Acquire)
    }

    fn run(mut self) {
        self.serve();
        self.finish();
    }

    /// Once the thread stops: a rating that reached neither the file nor the thread (one made
    /// after a cancel, as the thread let go) is reported not saved rather than shown forever.
    fn finish(self) {
        let Sender {
            inner, rx, file, ..
        } = self;
        // From here on `rate` reports a failed send itself.
        drop(rx);
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
    }

    fn serve(&mut self) {
        loop {
            // Appends what was rated before a drop, too.
            if !self.pump() || self.stopped() {
                return;
            }
            let wait = match self.step() {
                Step::Worked => continue,
                Step::Idle => None,
                Step::Wait(d) => Some(d),
            };
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
                // The channel reports itself closed only once nothing is left in it.
                None => return,
            }
        }
    }

    /// Handles the messages that have arrived, without waiting. `false` once the
    /// [`OpQueue`] is gone.
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
            Msg::Rate(op) => {
                self.append(op);
                // A new rating goes at once (spec §5.2). The backoff stays, so a further
                // failure still waits longer.
                self.retry_at = None;
            }
            Msg::Wake => {
                // The server answers again: no reason to sit out the rest of a wait.
                self.backoff = None;
                self.retry_at = None;
            }
        }
    }

    fn append(&mut self, mut op: QueuedRating) {
        let Some(file) = &mut self.file else {
            self.inner.change(|s| not_saved(s, &op));
            return;
        };
        // Later than every rating before it, even if the clock went back, so the newest click
        // wins both the coalescing here and the server's merge by time.
        if let Some(last) = self.last_at
            && op.at <= last
        {
            op.at = last + TimeDelta::milliseconds(1);
        }
        match file.append(op.clone()) {
            Ok(()) => self.last_at = Some(op.at),
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
                self.inner.change(|s| {
                    not_saved(s, &op);
                    if let Some(older) = older {
                        s.pending.entry(op.item.clone()).or_insert(older);
                    }
                });
            }
        }
    }

    /// One exchange with the server, if one is due.
    fn step(&mut self) -> Step {
        let Some(file) = &self.file else {
            return Step::Idle;
        };
        if self.halted || file.pending().is_empty() {
            return Step::Idle;
        }
        if let Some(at) = self.retry_at {
            let now = Instant::now();
            if now < at {
                return Step::Wait(at - now);
            }
        }
        match &*lock_gate(&self.gate) {
            Gate::Open { .. } => {}
            Gate::Pending => return Step::Wait(GATE_PENDING_POLL),
            // Offline: a wake brings the sender back. Closed: nothing more goes to this
            // server, and the file keeps the ratings for the next session.
            Gate::Offline { .. } | Gate::Closed(_) => return Step::Idle,
        }
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
        match self.send_pending() {
            Ok(()) => {
                self.backoff = None;
                self.retry_at = None;
                Step::Worked
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

    /// Sends the newest pending rating of each item, in batches of [`MAX_RATING_OPS`]; the
    /// older ones are settled unsent. Stops at the first batch that failed.
    fn send_pending(&mut self) -> Result<(), ClientError> {
        let file = self.file.as_mut().expect("checked by step");
        let (send, obsolete) = file.coalesced();
        if let Err(e) = file.mark_done(&obsolete) {
            // They stay pending and are coalesced away again next time.
            tracing::warn!(error = %e, "cannot settle older ratings in the queue");
        }
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
    /// that cannot record the answers, is an `Err`, so the rest goes again after a wait.
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
    /// other as read-only); only a failed exchange is an `Err`.
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
        let file = self.file.as_mut().expect("checked by step");
        // The server has them either way. If the file cannot say so, they go again after a
        // wait and come back superseded (a refused one is refused, and noticed, again).
        let saved = file.mark_done(&ids);
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
