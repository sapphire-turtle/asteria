//! A track fetched in `Range` chunks into a temporary file while it is read.
//!
//! A fetch thread asks for one chunk at a time, starting at the chunk the reader last
//! wanted; a [`StreamReader`] over the file waits for the bytes it needs. Requests are
//! chunks because the blocking client only has whole-request timeouts.

use std::error::Error as _;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use asteria_framework_client::rpc::{CONNECT_TIMEOUT, DEFAULT_RETRY_AFTER, MAX_RETRY_AFTER};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, CONTENT_RANGE, HeaderValue, RANGE, RETRY_AFTER};
use symphonia::core::io::MediaSource;
use url::Url;

use super::engine::{TrackError, Wait};

/// The size of one `Range` request.
pub const CHUNK_BYTES: u64 = 1 << 20;
/// How long one chunk's request may take.
pub const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the first request of an offline-audio track may take: the server renders it first.
pub const PREPARE_TIMEOUT: Duration = Duration::from_secs(600);

/// The request timeouts; tests shorten them.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    pub chunk: Duration,
    pub prepare: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            chunk: CHUNK_TIMEOUT,
            prepare: PREPARE_TIMEOUT,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum StreamStatus {
    Waiting(Wait),
    Fetching,
    Complete,
    Failed(TrackError),
}

pub struct StreamRequest {
    /// `<base>/media/<id>/<preset>`.
    pub url: Url,
    /// `Bearer <token>`, marked sensitive.
    pub auth: HeaderValue,
    /// The temporary file; created here, removed when the Stream is dropped.
    pub path: PathBuf,
    /// The first request's timeout: `PREPARE_TIMEOUT` for offline-audio, else `CHUNK_TIMEOUT`.
    pub first_timeout: Duration,
    /// Fetch nothing until this one is finished (complete, failed or cancelled): the
    /// preloaded next track.
    pub after: Option<Arc<Stream>>,
}

/// What a failed [`StreamReader`] read carries (as the `io::Error`'s inner error).
#[derive(Debug)]
pub struct StreamError(pub TrackError);

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            TrackError::Unplayable(r) => f.write_str(r),
            TrackError::Network(m) => f.write_str(m),
            TrackError::Unauthorized => f.write_str("the server refused the token"),
        }
    }
}

impl std::error::Error for StreamError {}

#[derive(Default)]
struct State {
    total: Option<u64>,
    /// Per chunk, once `total` is known.
    have: Vec<bool>,
    /// Where the reader last wanted to be.
    wanted: u64,
    first_done: bool,
    waiting_readers: usize,
    /// Set while a 503 is waited out.
    retry_wait: Option<Wait>,
    complete: bool,
    failed: Option<TrackError>,
    cancelled: bool,
}

impl State {
    fn finished(&self) -> bool {
        self.complete || self.failed.is_some() || self.cancelled
    }
}

struct Shared {
    file: File,
    state: Mutex<State>,
    /// Notified at every change of the state.
    changed: Condvar,
    /// Whether the first request is an offline-audio render.
    preparing: bool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait<'a>(&self, st: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.changed.wait(st).unwrap_or_else(|e| e.into_inner())
    }

    fn wait_for<'a>(&self, st: MutexGuard<'a, State>, d: Duration) -> MutexGuard<'a, State> {
        self.changed
            .wait_timeout(st, d)
            .unwrap_or_else(|e| e.into_inner())
            .0
    }

    fn fail(&self, e: TrackError) {
        let mut st = self.lock();
        if st.failed.is_none() {
            st.failed = Some(e);
        }
        drop(st);
        self.changed.notify_all();
    }
}

pub struct Stream {
    shared: Arc<Shared>,
    path: PathBuf,
}

impl Stream {
    /// Starts the fetch thread.
    pub fn start(req: StreamRequest) -> io::Result<Arc<Stream>> {
        Stream::start_with(req, Timeouts::default())
    }

    pub(crate) fn start_with(
        mut req: StreamRequest,
        timeouts: Timeouts,
    ) -> io::Result<Arc<Stream>> {
        req.auth.set_sensitive(true);
        // Only its shared state is held: a predecessor the engine drops is cancelled by its
        // `Drop`, which counts as finished.
        let before = req.after.take().map(|s| s.shared.clone());
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&req.path)?;
        let shared = Arc::new(Shared {
            file,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            preparing: req.first_timeout == timeouts.prepare,
        });
        let stream = Arc::new(Stream {
            shared: shared.clone(),
            path: req.path.clone(),
        });
        let spawned = std::thread::Builder::new()
            .name("asteria-stream".into())
            .spawn(move || {
                let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    fetch(&shared, req, before, timeouts)
                }));
                match run {
                    Ok(Err(Stop::Failed(e))) => shared.fail(e),
                    Ok(_) => {}
                    Err(_) => shared.fail(TrackError::Network("the fetch thread panicked".into())),
                }
            });
        // On a spawn failure `stream` is dropped here, which removes the file.
        spawned?;
        Ok(stream)
    }

    pub fn reader(self: &Arc<Self>) -> StreamReader {
        StreamReader {
            stream: self.clone(),
            pos: 0,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn status(&self) -> StreamStatus {
        let sh = &self.shared;
        let st = sh.lock();
        if st.cancelled {
            return StreamStatus::Failed(TrackError::Network("the stream was cancelled".into()));
        }
        if let Some(e) = &st.failed {
            return StreamStatus::Failed(e.clone());
        }
        if st.complete {
            return StreamStatus::Complete;
        }
        if let Some(w) = st.retry_wait {
            return StreamStatus::Waiting(w);
        }
        if st.waiting_readers > 0 {
            return StreamStatus::Waiting(if !st.first_done && sh.preparing {
                Wait::Preparing
            } else {
                Wait::Buffering
            });
        }
        StreamStatus::Fetching
    }

    /// The whole length, once the first response has told it.
    pub fn len(&self) -> Option<u64> {
        self.shared.lock().total
    }

    pub fn is_empty(&self) -> bool {
        self.len() == Some(0)
    }

    /// Stops the fetch and wakes every reader, which then fail.
    pub fn cancel(&self) {
        self.shared.lock().cancelled = true;
        self.shared.changed.notify_all();
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.cancel();
        let _ = std::fs::remove_file(&self.path);
    }
}

enum Stop {
    Cancelled,
    Failed(TrackError),
}

impl From<TrackError> for Stop {
    fn from(e: TrackError) -> Stop {
        Stop::Failed(e)
    }
}

fn fetch(
    shared: &Shared,
    req: StreamRequest,
    before: Option<Arc<Shared>>,
    timeouts: Timeouts,
) -> Result<(), Stop> {
    let StreamRequest {
        url,
        auth,
        first_timeout,
        ..
    } = req;
    if let Some(before) = before {
        let mut st = before.lock();
        while !st.finished() {
            if shared.lock().cancelled {
                return Err(Stop::Cancelled);
            }
            st = before.wait_for(st, Duration::from_millis(100));
        }
    }
    let client = Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(net_error)?;
    loop {
        let Some((start, end)) = next_range(shared)? else {
            shared.lock().complete = true;
            shared.changed.notify_all();
            return Ok(());
        };
        let first = !shared.lock().first_done;
        let timeout = if first { first_timeout } else { timeouts.chunk };
        // 503s are waited out for this long all told.
        let busy_budget = if first {
            first_timeout
        } else {
            timeouts.chunk * 4
        };
        let began = Instant::now();
        let resp = loop {
            if shared.lock().cancelled {
                return Err(Stop::Cancelled);
            }
            let resp = client
                .get(url.clone())
                .header(AUTHORIZATION, auth.clone())
                .header(RANGE, format!("bytes={start}-{end}"))
                .timeout(timeout)
                .send()
                .map_err(net_error)?;
            if resp.status() != StatusCode::SERVICE_UNAVAILABLE {
                break resp;
            }
            if began.elapsed() >= busy_budget {
                return Err(TrackError::Network("the server stayed busy".into()).into());
            }
            sleep_for(shared, retry_after(&resp), first)?;
        };
        receive(shared, resp, start, end, first)?;
    }
}

/// The next chunk to ask for: from the reader's position on, then from the start; the first
/// request, before the length is known, is the first chunk.
fn next_range(shared: &Shared) -> Result<Option<(u64, u64)>, Stop> {
    let st = shared.lock();
    if st.cancelled {
        return Err(Stop::Cancelled);
    }
    let Some(total) = st.total else {
        return Ok(Some((0, CHUNK_BYTES - 1)));
    };
    let from = usize::try_from(st.wanted / CHUNK_BYTES).unwrap_or(usize::MAX);
    let missing = (from..st.have.len())
        .chain(0..from.min(st.have.len()))
        .find(|&c| !st.have[c]);
    Ok(missing.map(|c| {
        let start = c as u64 * CHUNK_BYTES;
        (start, (start + CHUNK_BYTES).min(total) - 1)
    }))
}

/// Waits out a 503, or until cancelled.
fn sleep_for(shared: &Shared, d: Duration, first: bool) -> Result<(), Stop> {
    let deadline = Instant::now() + d;
    let mut st = shared.lock();
    st.retry_wait = Some(if first {
        Wait::Preparing
    } else {
        Wait::Buffering
    });
    shared.changed.notify_all();
    let result = loop {
        if st.cancelled {
            break Err(Stop::Cancelled);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break Ok(());
        }
        st = shared.wait_for(st, left);
    };
    st.retry_wait = None;
    drop(st);
    shared.changed.notify_all();
    result
}

/// `Retry-After` in seconds, at most `MAX_RETRY_AFTER`; anything else is `DEFAULT_RETRY_AFTER`.
fn retry_after(resp: &Response) -> Duration {
    resp.headers()
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(DEFAULT_RETRY_AFTER, Duration::from_secs)
        .min(MAX_RETRY_AFTER)
}

const NO_RANGES: &str = "the server does not support ranges";

/// Takes the response to a request for `start..=end`.
fn receive(shared: &Shared, resp: Response, start: u64, end: u64, first: bool) -> Result<(), Stop> {
    let status = resp.status();
    let unsupported = || Stop::Failed(TrackError::Unplayable(NO_RANGES.into()));
    match status {
        StatusCode::PARTIAL_CONTENT => {
            let (s, e, total) = resp
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range)
                .ok_or_else(unsupported)?;
            if s != start || e != end.min(total.saturating_sub(1)) {
                return Err(unsupported());
            }
            if shared.lock().total.is_some_and(|t| t != total) {
                return Err(unsupported());
            }
            let body = resp.bytes().map_err(net_error)?;
            if body.len() as u64 != e - s + 1 {
                return Err(TrackError::Network("the response ended early".into()).into());
            }
            write_all_at(&shared.file, &body, s).map_err(write_error)?;
            let mut st = shared.lock();
            set_total(&mut st, total);
            st.have[(s / CHUNK_BYTES) as usize] = true;
            st.first_done = true;
        }
        StatusCode::OK if first => {
            if resp.content_length().is_some_and(|n| n > CHUNK_BYTES) {
                return Err(unsupported());
            }
            let mut body = Vec::new();
            let mut resp = resp;
            io::Read::take(&mut resp, CHUNK_BYTES + 1)
                .read_to_end(&mut body)
                .map_err(|e| Stop::Failed(TrackError::Network(e.to_string())))?;
            if body.len() as u64 > CHUNK_BYTES {
                return Err(unsupported());
            }
            write_all_at(&shared.file, &body, 0).map_err(write_error)?;
            let mut st = shared.lock();
            set_total(&mut st, body.len() as u64);
            st.have.fill(true);
            st.first_done = true;
        }
        StatusCode::OK => return Err(unsupported()),
        StatusCode::UNAUTHORIZED => return Err(TrackError::Unauthorized.into()),
        // An empty file has no byte to start at.
        StatusCode::RANGE_NOT_SATISFIABLE if first => {
            return Err(TrackError::Unplayable("the track is empty".into()).into());
        }
        StatusCode::NOT_FOUND | StatusCode::UNPROCESSABLE_ENTITY => {
            return Err(TrackError::Unplayable(format!(
                "the server cannot serve the track ({status})"
            ))
            .into());
        }
        s => return Err(TrackError::Network(format!("HTTP {s}")).into()),
    }
    shared.changed.notify_all();
    Ok(())
}

fn set_total(st: &mut State, total: u64) {
    if st.total.is_none() {
        st.total = Some(total);
        st.have = vec![false; total.div_ceil(CHUNK_BYTES) as usize];
    }
}

/// `bytes s-e/total`.
fn parse_content_range(v: &str) -> Option<(u64, u64, u64)> {
    let (range, total) = v.trim().strip_prefix("bytes ")?.split_once('/')?;
    let (s, e) = range.split_once('-')?;
    let (s, e, total) = (s.parse().ok()?, e.parse().ok()?, total.parse().ok()?);
    (s <= e && e < total).then_some((s, e, total))
}

fn net_error(e: reqwest::Error) -> Stop {
    // The URL has no secret in it, but the text is shown to the person; keep it short.
    let e = e.without_url();
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        text.push_str(": ");
        text.push_str(&s.to_string());
        source = s.source();
    }
    Stop::Failed(TrackError::Network(text))
}

fn write_error(e: io::Error) -> Stop {
    Stop::Failed(TrackError::Network(format!(
        "cannot write the temporary file: {e}"
    )))
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], pos: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, pos)
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], pos: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, pos)
}

#[cfg(unix)]
fn write_all_at(file: &File, buf: &[u8], pos: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, pos)
}

#[cfg(windows)]
fn write_all_at(file: &File, mut buf: &[u8], mut pos: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = std::os::windows::fs::FileExt::seek_write(file, buf, pos)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n..];
        pos += n as u64;
    }
    Ok(())
}

/// Read + Seek over the stream. A read of bytes not yet fetched waits for them, and moves
/// the fetch there. After a failure the bytes already fetched can still be read; a read of
/// others fails at once, and after a cancel every read does. The failure is the
/// `io::Error`'s inner [`StreamError`].
pub struct StreamReader {
    stream: Arc<Stream>,
    pos: u64,
    cancelled: Arc<AtomicBool>,
}

/// Cancels one [`StreamReader`] (not the stream): its reads and seeks fail from then on,
/// and one that is waiting wakes at once.
#[derive(Clone)]
pub struct ReaderCancel {
    flag: Arc<AtomicBool>,
    shared: Arc<Shared>,
}

impl ReaderCancel {
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
        // Under the lock, so a reader between its check and its wait does not miss it.
        drop(self.shared.lock());
        self.shared.changed.notify_all();
    }
}

impl StreamReader {
    pub fn canceller(&self) -> ReaderCancel {
        ReaderCancel {
            flag: self.cancelled.clone(),
            shared: self.stream.shared.clone(),
        }
    }

    fn check_cancelled(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(io::Error::other("the reader was cancelled"))
        } else {
            Ok(())
        }
    }

    fn failure(st: &State) -> Option<io::Error> {
        if st.cancelled {
            return Some(io::Error::other("the stream was cancelled"));
        }
        st.failed.clone().map(|e| io::Error::other(StreamError(e)))
    }

    fn want(&self, st: &mut State) {
        st.wanted = self.pos;
    }
}

impl Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.check_cancelled()?;
        let sh = &self.stream.shared;
        let mut st = sh.lock();
        self.want(&mut st);
        let n = loop {
            self.check_cancelled()?;
            if st.cancelled {
                return Err(Self::failure(&st).unwrap());
            }
            if let Some(total) = st.total {
                if self.pos >= total {
                    return Ok(0);
                }
                let c = (self.pos / CHUNK_BYTES) as usize;
                if st.have[c] {
                    let run = st.have[c..].iter().take_while(|h| **h).count() as u64;
                    let end = ((c as u64 + run) * CHUNK_BYTES).min(total);
                    break buf.len().min((end - self.pos) as usize);
                }
            }
            if let Some(e) = Self::failure(&st) {
                return Err(e);
            }
            st.waiting_readers += 1;
            st = sh.wait(st);
            st.waiting_readers -= 1;
        };
        drop(st);
        let n = read_at(&sh.file, &mut buf[..n], self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for StreamReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.check_cancelled()?;
        let sh = &self.stream.shared;
        let target = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => {
                let mut st = sh.lock();
                let total = loop {
                    self.check_cancelled()?;
                    if let Some(t) = st.total {
                        break t;
                    }
                    if let Some(e) = Self::failure(&st) {
                        return Err(e);
                    }
                    st.waiting_readers += 1;
                    st = sh.wait(st);
                    st.waiting_readers -= 1;
                };
                total.checked_add_signed(d)
            }
        };
        let Some(target) = target else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a seek before the start",
            ));
        };
        self.pos = target;
        let mut st = sh.lock();
        self.want(&mut st);
        Ok(target)
    }
}

impl MediaSource for StreamReader {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.stream.len()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};
    use std::time::Instant;

    use tempfile::TempDir;

    use super::super::test_server::{Script, TestServer, pseudo_random};
    use super::*;

    const MIB: usize = 1 << 20;
    const SECRET: &str = "secret-token-value";

    fn timeouts() -> Timeouts {
        Timeouts {
            chunk: Duration::from_secs(10),
            prepare: Duration::from_secs(20),
        }
    }

    fn request(server: &TestServer, dir: &TempDir, name: &str) -> StreamRequest {
        let mut auth = HeaderValue::from_str(&format!("Bearer {SECRET}")).unwrap();
        auth.set_sensitive(true);
        StreamRequest {
            url: server.url(),
            auth,
            path: dir.path().join(name),
            first_timeout: timeouts().chunk,
            after: None,
        }
    }

    fn script(len: usize) -> Script {
        Script {
            body: pseudo_random(len, 7),
            ..Script::default()
        }
    }

    fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn range(s: usize, e: usize) -> Option<String> {
        Some(format!("bytes={s}-{e}"))
    }

    #[test]
    fn reads_back_the_whole_body_in_order() {
        let body_len = 3 * MIB + MIB / 2;
        let server = TestServer::start(script(body_len));
        let dir = TempDir::new().unwrap();
        let stream = Stream::start_with(request(&server, &dir, "t"), timeouts()).unwrap();
        let mut out = Vec::new();
        stream.reader().read_to_end(&mut out).unwrap();
        assert!(out == server.inner_body());
        assert_eq!(stream.len(), Some(body_len as u64));
        wait_until("complete", || stream.status() == StreamStatus::Complete);
        assert_eq!(
            server.ranges(),
            vec![
                range(0, MIB - 1),
                range(MIB, 2 * MIB - 1),
                range(2 * MIB, 3 * MIB - 1),
                range(3 * MIB, body_len - 1),
            ]
        );
    }

    #[test]
    fn a_seek_ahead_fetches_from_there_first() {
        let body_len = 3 * MIB + MIB / 2;
        let server = TestServer::start(Script {
            first_delay: Duration::from_millis(300),
            ..script(body_len)
        });
        let dir = TempDir::new().unwrap();
        let stream = Stream::start_with(request(&server, &dir, "t"), timeouts()).unwrap();
        let mut r = stream.reader();
        r.seek(SeekFrom::Start(3 * MIB as u64)).unwrap();
        let mut buf = [0u8; 16];
        r.read_exact(&mut buf).unwrap();
        assert_eq!(buf[..], server.inner_body()[3 * MIB..3 * MIB + 16]);
        assert_eq!(server.ranges()[1], range(3 * MIB, body_len - 1));
        wait_until("complete", || stream.status() == StreamStatus::Complete);
        assert_eq!(server.ranges().len(), 4);
    }

    #[test]
    fn a_seek_back_does_not_refetch() {
        let server = TestServer::start(script(2 * MIB + 10));
        let dir = TempDir::new().unwrap();
        let stream = Stream::start_with(request(&server, &dir, "t"), timeouts()).unwrap();
        let mut r = stream.reader();
        let mut all = Vec::new();
        r.read_to_end(&mut all).unwrap();
        wait_until("complete", || stream.status() == StreamStatus::Complete);
        let before = server.ranges().len();
        r.seek(SeekFrom::Start(5)).unwrap();
        let mut buf = [0u8; 100];
        r.read_exact(&mut buf).unwrap();
        assert_eq!(buf[..], all[5..105]);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(server.ranges().len(), before);
    }

    #[test]
    fn retry_after_is_waited_for() {
        let server = TestServer::start(Script {
            busy: 2,
            retry_after: Some("1"),
            ..script(MIB / 2)
        });
        let dir = TempDir::new().unwrap();
        let mut req = request(&server, &dir, "t");
        req.first_timeout = timeouts().prepare;
        let started = Instant::now();
        let stream = Stream::start_with(req, timeouts()).unwrap();
        wait_until("preparing", || {
            stream.status() == StreamStatus::Waiting(Wait::Preparing)
        });
        let mut out = Vec::new();
        stream.reader().read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), MIB / 2);
        assert!(started.elapsed() >= Duration::from_secs(2));
        let times = server.times();
        assert_eq!(times.len(), 3);
        assert!(times[2] - times[1] >= Duration::from_millis(990));
        // The retried requests are the same one.
        let ranges = server.ranges();
        assert!(ranges.iter().all(|r| *r == range(0, MIB - 1)));
    }

    #[test]
    fn a_dropped_connection_fails_the_reader() {
        let server = TestServer::start(Script {
            cut_on: Some(2),
            ..script(2 * MIB)
        });
        let dir = TempDir::new().unwrap();
        let stream = Stream::start_with(request(&server, &dir, "t"), timeouts()).unwrap();
        let mut out = Vec::new();
        let err = stream.reader().read_to_end(&mut out).unwrap_err();
        assert_eq!(out.len(), MIB);
        assert!(err.get_ref().unwrap().is::<StreamError>());
        assert!(matches!(
            stream.status(),
            StreamStatus::Failed(TrackError::Network(_))
        ));
    }

    #[test]
    fn a_stalled_chunk_fails_after_the_timeout() {
        let server = TestServer::start(Script {
            stall_on: Some(1),
            ..script(MIB)
        });
        let dir = TempDir::new().unwrap();
        let short = Timeouts {
            chunk: Duration::from_millis(300),
            prepare: Duration::from_secs(20),
        };
        let mut req = request(&server, &dir, "t");
        req.first_timeout = short.chunk;
        let started = Instant::now();
        let stream = Stream::start_with(req, short).unwrap();
        let mut buf = [0u8; 8];
        assert!(stream.reader().read(&mut buf).is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(matches!(
            stream.status(),
            StreamStatus::Failed(TrackError::Network(_))
        ));
    }

    #[test]
    fn not_found_is_unplayable_and_unauthorized_is_kept() {
        let dir = TempDir::new().unwrap();
        for code in [404, 422] {
            let server = TestServer::start(Script {
                status: Some(code),
                ..script(10)
            });
            let stream = Stream::start_with(request(&server, &dir, "t"), timeouts()).unwrap();
            let mut buf = [0u8; 8];
            assert!(stream.reader().read(&mut buf).is_err());
            assert!(matches!(
                stream.status(),
                StreamStatus::Failed(TrackError::Unplayable(_))
            ));
        }
        let server = TestServer::start(Script {
            token: Some("another"),
            ..script(10)
        });
        let stream = Stream::start_with(request(&server, &dir, "u"), timeouts()).unwrap();
        let mut buf = [0u8; 8];
        assert!(stream.reader().read(&mut buf).is_err());
        assert_eq!(
            stream.status(),
            StreamStatus::Failed(TrackError::Unauthorized)
        );
    }

    #[test]
    fn range_not_satisfiable_on_the_first_request_is_unplayable() {
        // An empty file: the server answers 416 to any Range request.
        let dir = TempDir::new().unwrap();
        let server = TestServer::start(Script {
            status: Some(416),
            ..script(0)
        });
        let stream = Stream::start_with(request(&server, &dir, "t"), timeouts()).unwrap();
        let mut buf = [0u8; 8];
        assert!(stream.reader().read(&mut buf).is_err());
        assert!(matches!(
            stream.status(),
            StreamStatus::Failed(TrackError::Unplayable(_))
        ));
    }

    #[test]
    fn a_server_that_ignores_ranges_is_accepted_when_it_sends_it_all() {
        let server = TestServer::start(Script {
            ignore_range: true,
            ..script(MIB / 2 + 3)
        });
        let dir = TempDir::new().unwrap();
        let stream = Stream::start_with(request(&server, &dir, "t"), timeouts()).unwrap();
        let mut out = Vec::new();
        stream.reader().read_to_end(&mut out).unwrap();
        assert!(out == server.inner_body());
        wait_until("complete", || stream.status() == StreamStatus::Complete);
        assert_eq!(server.ranges().len(), 1);
    }

    #[test]
    fn a_large_200_is_not_buffered() {
        let server = TestServer::start(Script {
            ignore_range: true,
            ..script(2 * MIB)
        });
        let dir = TempDir::new().unwrap();
        let stream = Stream::start_with(request(&server, &dir, "t"), timeouts()).unwrap();
        let mut buf = [0u8; 4];
        assert!(stream.reader().read(&mut buf).is_err());
        assert_eq!(
            stream.status(),
            StreamStatus::Failed(TrackError::Unplayable(NO_RANGES.into()))
        );
    }

    #[test]
    fn a_server_that_stays_busy_fails() {
        let server = TestServer::start(Script {
            busy: 1000,
            retry_after: Some("1"),
            ..script(10)
        });
        let dir = TempDir::new().unwrap();
        let short = Timeouts {
            chunk: Duration::from_millis(200),
            prepare: Duration::from_millis(500),
        };
        let mut req = request(&server, &dir, "t");
        req.first_timeout = short.prepare;
        let stream = Stream::start_with(req, short).unwrap();
        let mut buf = [0u8; 4];
        assert!(stream.reader().read(&mut buf).is_err());
        assert_eq!(
            stream.status(),
            StreamStatus::Failed(TrackError::Network("the server stayed busy".into()))
        );
        assert!(server.ranges().len() <= 3);
    }

    #[test]
    fn a_dropped_predecessor_lets_the_next_start() {
        let slow = TestServer::start(Script {
            first_delay: Duration::from_millis(500),
            ..script(MIB)
        });
        let next = TestServer::start(script(MIB / 2));
        let dir = TempDir::new().unwrap();
        let first = Stream::start_with(request(&slow, &dir, "a"), timeouts()).unwrap();
        let mut req = request(&next, &dir, "b");
        req.after = Some(first.clone());
        let second = Stream::start_with(req, timeouts()).unwrap();
        drop(first);
        assert!(!dir.path().join("a").exists());
        let mut out = Vec::new();
        second.reader().read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), MIB / 2);
    }

    #[test]
    fn the_preloaded_stream_waits_for_the_one_before() {
        let slow = TestServer::start(Script {
            first_delay: Duration::from_millis(600),
            ..script(2 * MIB)
        });
        let next = TestServer::start(script(MIB));
        let dir = TempDir::new().unwrap();
        let first = Stream::start_with(request(&slow, &dir, "a"), timeouts()).unwrap();
        let mut req = request(&next, &dir, "b");
        req.after = Some(first.clone());
        let second = Stream::start_with(req, timeouts()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!matches!(first.status(), StreamStatus::Complete));
        assert!(next.ranges().is_empty());
        let mut out = Vec::new();
        first.reader().read_to_end(&mut out).unwrap();
        wait_until("first complete", || {
            first.status() == StreamStatus::Complete
        });
        let mut out = Vec::new();
        second.reader().read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), MIB);
        assert!(next.times()[0] >= slow.times()[1]);
    }

    #[test]
    fn dropping_the_stream_removes_its_file() {
        let server = TestServer::start(script(10));
        let dir = TempDir::new().unwrap();
        let req = request(&server, &dir, "t");
        let path = req.path.clone();
        let stream = Stream::start_with(req, timeouts()).unwrap();
        assert!(path.exists());
        let mut out = Vec::new();
        stream.reader().read_to_end(&mut out).unwrap();
        drop(stream);
        assert!(!path.exists());
    }

    #[test]
    fn cancel_fails_waiting_readers() {
        let server = TestServer::start(Script {
            stall_on: Some(1),
            ..script(10)
        });
        let dir = TempDir::new().unwrap();
        let stream = Stream::start_with(request(&server, &dir, "t"), timeouts()).unwrap();
        let mut r = stream.reader();
        let s2 = stream.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            s2.cancel();
        });
        let mut buf = [0u8; 4];
        assert!(r.read(&mut buf).is_err());
        t.join().unwrap();
    }

    #[test]
    fn the_token_is_not_in_any_error_text() {
        let dir = TempDir::new().unwrap();
        let scripts = [
            Script {
                status: Some(404),
                ..script(10)
            },
            Script {
                token: Some("another"),
                ..script(10)
            },
            Script {
                cut_on: Some(1),
                ..script(2 * MIB)
            },
            Script {
                status: Some(500),
                ..script(10)
            },
        ];
        for s in scripts {
            let server = TestServer::start(s);
            let stream = Stream::start_with(request(&server, &dir, "t"), timeouts()).unwrap();
            let mut out = Vec::new();
            let err = stream.reader().read_to_end(&mut out).unwrap_err();
            let text = format!("{err} {err:?} {:?}", stream.status());
            assert!(!text.contains(SECRET), "{text}");
        }
        // And one that cannot connect.
        let gone = TestServer::start(script(10));
        let mut req = request(&gone, &dir, "g");
        drop(gone);
        req.url.set_port(Some(1)).unwrap();
        let stream = Stream::start_with(req, timeouts()).unwrap();
        let mut buf = [0u8; 4];
        let err = stream.reader().read(&mut buf).unwrap_err();
        assert!(!format!("{err} {err:?} {:?}", stream.status()).contains(SECRET));
        assert!(matches!(
            stream.status(),
            StreamStatus::Failed(TrackError::Network(_))
        ));
    }
}
