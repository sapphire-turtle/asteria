//! The real [`Opener`]: tracks fetched from the server by [`Stream`]s and decoded on a
//! thread each.

use std::io::{self, Read, Seek, SeekFrom};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::time::{Duration, SystemTime};

use asteria_framework_client::media_cache::safe_component;
use asteria_music_api::ItemId;
use reqwest::header::HeaderValue;
use symphonia::core::io::MediaSource;

use super::decode::{self, DecodeError};
use super::engine::{Feed, FeedEvent, Opener, OutputConfig, TrackError, Wait};
use super::resample::Converter;
use super::stream::{ReaderCancel, Stream, StreamReader, StreamRequest, StreamStatus, Timeouts};
use super::{Endpoint, Quality, QueueEntry};

/// Messages the decoder thread can hold before it waits for the player.
const CHANNEL_CAPACITY: usize = 32;
/// A per-process stream directory this long unmodified belongs to a dead process.
const STALE_STREAM_DIR: Duration = Duration::from_secs(24 * 60 * 60);
/// A file in the stream directory whose mtime is refreshed whenever a stream starts.
const ALIVE_MARKER: &str = ".alive";
/// No duration known yet.
const NO_DURATION: u64 = u64::MAX;

/// Makes `<cache root>/<library id>/stream/<process tag>/` and removes, beside it, the
/// directories last modified more than STALE_STREAM_DIR ago. The process tag is the pid and
/// 8 random hex digits. Returns the directory; the caller removes it at exit.
/// A `library_id` that is not one ordinary path component is `InvalidInput`: the server
/// chose it, and it must not lead the directory (or the sweep) outside `cache_root`.
pub fn stream_dir(cache_root: &Path, library_id: &str) -> io::Result<PathBuf> {
    if !safe_component(library_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the library id is not usable as a directory name",
        ));
    }
    let base = cache_root.join(library_id).join("stream");
    std::fs::create_dir_all(&base)?;
    let now = SystemTime::now();
    if let Ok(siblings) = std::fs::read_dir(&base) {
        for sibling in siblings.flatten() {
            let stale = sibling
                .metadata()
                .ok()
                .filter(|m| m.is_dir())
                .and_then(|m| {
                    let marker = std::fs::metadata(sibling.path().join(ALIVE_MARKER))
                        .and_then(|m| m.modified())
                        .ok();
                    m.modified().ok().max(marker)
                })
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|age| age > STALE_STREAM_DIR);
            if stale {
                let _ = std::fs::remove_dir_all(sibling.path());
            }
        }
    }
    let mut random = [0u8; 4];
    getrandom::fill(&mut random).map_err(|e| io::Error::other(e.to_string()))?;
    let dir = base.join(format!(
        "{}-{:08x}",
        std::process::id(),
        u32::from_le_bytes(random)
    ));
    std::fs::create_dir(&dir)?;
    Ok(dir)
}

/// A stream the opener holds: dropping it cancels the fetch and, once the decoder thread
/// lets go too, removes the file.
struct Slot {
    id: ItemId,
    quality: Quality,
    stream: Arc<Stream>,
}

impl Slot {
    /// Whether this is the track, and its stream can still deliver it: one that failed
    /// (or was cancelled) is started afresh, which is what a retry needs.
    fn is(&self, id: &ItemId, quality: Quality) -> bool {
        self.id == *id
            && self.quality == quality
            && !matches!(self.stream.status(), StreamStatus::Failed(_))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.stream.cancel();
    }
}

/// Never reuse a path within the process: a dropped `Stream` removes its file by name.
static FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The real Opener: fetches from the endpoint's server into `stream_dir`.
pub struct HttpOpener {
    endpoint: Option<Endpoint>,
    quality: Quality,
    timeouts: Timeouts,
    current: Option<Slot>,
    preloaded: Option<Slot>,
}

impl Default for HttpOpener {
    fn default() -> Self {
        HttpOpener::new()
    }
}

impl HttpOpener {
    pub fn new() -> HttpOpener {
        HttpOpener {
            endpoint: None,
            quality: Quality::default(),
            timeouts: Timeouts::default(),
            current: None,
            preloaded: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_timeouts(mut self, timeouts: Timeouts) -> HttpOpener {
        self.timeouts = timeouts;
        self
    }

    fn start(&self, entry: &QueueEntry, after: Option<Arc<Stream>>) -> Result<Slot, TrackError> {
        let Some(endpoint) = &self.endpoint else {
            return Err(TrackError::Network("not connected".into()));
        };
        let id = &entry.media.id;
        let mut url = endpoint.url.clone();
        url.path_segments_mut()
            .map_err(|()| TrackError::Network("the server address cannot be a base".into()))?
            .pop_if_empty()
            .extend(["media", id.0.as_str(), self.quality.preset()]);
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", endpoint.token.expose()))
            .map_err(|_| TrackError::Unauthorized)?;
        auth.set_sensitive(true);
        // Tells other instances' sweeps that this directory is alive.
        let _ = std::fs::write(endpoint.stream_dir.join(ALIVE_MARKER), b"");
        let n = FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let stream = Stream::start_with(
            StreamRequest {
                url,
                auth,
                path: endpoint.stream_dir.join(format!("{n}.part")),
                first_timeout: match self.quality {
                    Quality::Low => self.timeouts.prepare,
                    Quality::Original => self.timeouts.chunk,
                },
                after,
            },
            self.timeouts,
        )
        // A local disk problem is not the track's fault.
        .map_err(|e| TrackError::Network(format!("cannot write the stream file: {e}")))?;
        Ok(Slot {
            id: id.clone(),
            quality: self.quality,
            stream,
        })
    }
}

impl Opener for HttpOpener {
    fn open(&mut self, entry: &QueueEntry, at_ms: u64, target: OutputConfig) -> Box<dyn Feed> {
        let id = &entry.media.id;
        let quality = self.quality;
        if self.current.as_ref().is_some_and(|s| s.is(id, quality)) {
            // Reuse it.
        } else if self.preloaded.as_ref().is_some_and(|s| s.is(id, quality)) {
            self.current = self.preloaded.take();
        } else {
            match self.start(entry, None) {
                Ok(slot) => self.current = Some(slot),
                Err(e) => {
                    self.current = None;
                    return Box::new(FailedFeed(e));
                }
            }
        }
        let stream = self.current.as_ref().expect("just set").stream.clone();
        let ext = Path::new(&entry.media.path)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_owned);
        match TrackFeed::spawn(stream, ext, at_ms, target) {
            Ok(feed) => Box::new(feed),
            Err(e) => Box::new(FailedFeed(TrackError::Unplayable(e))),
        }
    }

    fn preload(&mut self, next: Option<&QueueEntry>) {
        let Some(next) = next else {
            self.preloaded = None;
            return;
        };
        let (id, quality) = (&next.media.id, self.quality);
        if self.current.as_ref().is_some_and(|s| s.is(id, quality))
            || self.preloaded.as_ref().is_some_and(|s| s.is(id, quality))
        {
            return;
        }
        self.preloaded = None;
        let after = self.current.as_ref().map(|s| s.stream.clone());
        match self.start(next, after) {
            Ok(slot) => self.preloaded = Some(slot),
            Err(e) => tracing::debug!("cannot preload the next track: {e:?}"),
        }
    }

    fn set_endpoint(&mut self, endpoint: Option<Endpoint>) {
        self.clear();
        self.endpoint = endpoint;
    }

    fn set_quality(&mut self, quality: Quality) {
        self.quality = quality;
    }

    fn clear(&mut self) {
        self.current = None;
        self.preloaded = None;
    }
}

/// A feed that is already over.
struct FailedFeed(TrackError);

impl Feed for FailedFeed {
    fn poll(&mut self) -> FeedEvent {
        FeedEvent::Failed(self.0.clone())
    }

    fn duration_ms(&self) -> Option<u64> {
        None
    }
}

enum Message {
    Pcm(Vec<f32>),
    End,
    Failed(TrackError),
}

/// One track being decoded on its own thread.
struct TrackFeed {
    rx: Receiver<Message>,
    stream: Arc<Stream>,
    stop: Arc<AtomicBool>,
    reader: ReaderCancel,
    duration: Arc<AtomicU64>,
    /// How the feed ended, repeated by any later poll.
    finished: Option<FeedEvent>,
}

impl TrackFeed {
    fn spawn(
        stream: Arc<Stream>,
        ext: Option<String>,
        at_ms: u64,
        target: OutputConfig,
    ) -> Result<TrackFeed, String> {
        let (tx, rx) = sync_channel(CHANNEL_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let duration = Arc::new(AtomicU64::new(NO_DURATION));
        let reader = stream.reader();
        let canceller = reader.canceller();
        let job = Job {
            reader: Some(reader),
            stream: stream.clone(),
            ext,
            at_ms,
            target,
            tx,
            stop: stop.clone(),
            duration: duration.clone(),
        };
        std::thread::Builder::new()
            .name("asteria-decode".into())
            .spawn(move || {
                let tx = job.tx.clone();
                if catch_unwind(AssertUnwindSafe(|| job.run())).is_err() {
                    let _ = tx.send(Message::Failed(TrackError::Unplayable(
                        "the decoder crashed".into(),
                    )));
                }
            })
            .map_err(|e| format!("cannot start the decoder: {e}"))?;
        Ok(TrackFeed {
            rx,
            stream,
            stop,
            reader: canceller,
            duration,
            finished: None,
        })
    }
}

impl Drop for TrackFeed {
    fn drop(&mut self) {
        // The thread leaves at once, even from a wait for bytes. The stream is the opener's
        // and may be opened again (a seek), so only this feed's reader is cancelled.
        self.stop.store(true, Ordering::Release);
        self.reader.cancel();
    }
}

impl Feed for TrackFeed {
    fn poll(&mut self) -> FeedEvent {
        if let Some(done) = &self.finished {
            return done.clone();
        }
        let done = match self.rx.try_recv() {
            Ok(Message::Pcm(p)) => return FeedEvent::Pcm(p),
            Ok(Message::End) => FeedEvent::End,
            Ok(Message::Failed(e)) => FeedEvent::Failed(e),
            Err(TryRecvError::Empty) => {
                return FeedEvent::Waiting(match self.stream.status() {
                    StreamStatus::Waiting(w) => w,
                    _ => Wait::Buffering,
                });
            }
            Err(TryRecvError::Disconnected) => {
                FeedEvent::Failed(TrackError::Unplayable("the decoder stopped".into()))
            }
        };
        self.finished = Some(done.clone());
        done
    }

    fn duration_ms(&self) -> Option<u64> {
        match self.duration.load(Ordering::Acquire) {
            NO_DURATION => None,
            ms => Some(ms),
        }
    }
}

/// Everything the decoder thread needs.
struct Job {
    reader: Option<StreamReader>,
    stream: Arc<Stream>,
    ext: Option<String>,
    at_ms: u64,
    target: OutputConfig,
    tx: SyncSender<Message>,
    stop: Arc<AtomicBool>,
    duration: Arc<AtomicU64>,
}

impl Job {
    fn run(mut self) {
        let message = match self.decode() {
            Ok(()) => Message::End,
            Err(Some(e)) => Message::Failed(e),
            // Cancelled: nobody is listening.
            Err(None) => return,
        };
        let _ = self.tx.send(message);
    }

    /// `Err(None)` when the feed was dropped.
    fn decode(&mut self) -> Result<(), Option<TrackError>> {
        let reader = Cancellable {
            inner: self.reader.take().expect("decoded once"),
            stop: self.stop.clone(),
        };
        let mut decoder =
            decode::open(Box::new(reader), self.ext.as_deref()).map_err(|e| self.failure(e))?;
        if let Some(ms) = decoder.duration_ms() {
            self.duration.store(ms, Ordering::Release);
        }
        if self.at_ms > 0 {
            decoder.seek(self.at_ms).map_err(|e| self.failure(e))?;
        }
        let mut converter = Converter::new(decoder.sample_rate(), decoder.channels(), self.target)
            .map_err(|e| Some(TrackError::Unplayable(e)))?;
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Err(None);
            }
            match decoder.next_block() {
                Ok(Some(block)) => {
                    let pcm = converter.process(&block);
                    self.send(pcm)?;
                }
                Ok(None) => break,
                Err(e) => return Err(self.failure(e)),
            }
        }
        // A fetch that failed under a decoder that took the cut-off for the end.
        if let StreamStatus::Failed(e) = self.stream.status() {
            return Err((!self.stop.load(Ordering::Acquire)).then_some(e));
        }
        let rest = converter.flush();
        self.send(rest)
    }

    fn send(&self, pcm: Vec<f32>) -> Result<(), Option<TrackError>> {
        if pcm.is_empty() {
            return Ok(());
        }
        self.tx.send(Message::Pcm(pcm)).map_err(|_| None)
    }

    /// The stream's own failure if it has one, else the decoder's reason.
    fn failure(&self, e: DecodeError) -> Option<TrackError> {
        if self.stop.load(Ordering::Acquire) {
            return None;
        }
        Some(match self.stream.status() {
            StreamStatus::Failed(failed) => failed,
            _ => TrackError::Unplayable(e.to_string()),
        })
    }
}

/// A reader that fails once its feed is dropped.
struct Cancellable {
    inner: StreamReader,
    stop: Arc<AtomicBool>,
}

impl Cancellable {
    fn check(&self) -> io::Result<()> {
        if self.stop.load(Ordering::Acquire) {
            Err(io::Error::other("the feed was dropped"))
        } else {
            Ok(())
        }
    }
}

impl Read for Cancellable {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        let n = self.inner.read(buf)?;
        self.check()?;
        Ok(n)
    }
}

impl Seek for Cancellable {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.check()?;
        self.inner.seek(to)
    }
}

impl MediaSource for Cancellable {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.inner.byte_len()
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use asteria_framework_client::profiles::Token;
    use tempfile::TempDir;

    use super::super::engine::{Feed, FeedEvent, Opener, OutputConfig, TrackError, Wait};
    use super::super::fakes::entry;
    use super::super::stream::Timeouts;
    use super::super::test_server::{Script, TestServer};
    use super::super::{Endpoint, Quality};
    use super::*;

    const TARGET: OutputConfig = OutputConfig {
        sample_rate: 48_000,
        channels: 2,
    };

    fn tone() -> Vec<u8> {
        std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.flac"))
            .unwrap()
    }

    fn endpoint(server: &TestServer, dir: &Path) -> Endpoint {
        Endpoint {
            url: server.url().join("/").unwrap(),
            token: Token::new("secret"),
            library_id: "lib".into(),
            stream_dir: dir.to_owned(),
        }
    }

    fn opener(server: &TestServer, dir: &TempDir) -> HttpOpener {
        let mut o = HttpOpener::new().with_timeouts(Timeouts {
            chunk: Duration::from_secs(10),
            prepare: Duration::from_secs(20),
        });
        o.set_endpoint(Some(endpoint(server, dir.path())));
        o
    }

    fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Polls to the end: the samples, and how it finished.
    fn drain(feed: &mut dyn Feed) -> (Vec<f32>, FeedEvent) {
        let mut pcm = Vec::new();
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(Instant::now() < end, "the feed did not finish");
            match feed.poll() {
                FeedEvent::Pcm(p) => pcm.extend(p),
                FeedEvent::Waiting(_) => std::thread::sleep(Duration::from_millis(5)),
                last => return (pcm, last),
            }
        }
    }

    fn files(dir: &TempDir) -> usize {
        std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .count()
    }

    #[test]
    fn a_track_from_the_server_comes_out_as_pcm_then_end() {
        let server = TestServer::start(Script {
            body: tone(),
            ..Script::default()
        });
        let dir = TempDir::new().unwrap();
        let mut o = opener(&server, &dir);
        let mut feed = o.open(&entry("track"), 0, TARGET);
        let (pcm, last) = drain(feed.as_mut());
        assert_eq!(last, FeedEvent::End);
        let frames = pcm.len() / 2;
        assert!(frames.abs_diff(96_000) < 2_000, "{frames} frames");
        assert!(pcm.iter().fold(0.0f32, |m, v| m.max(v.abs())) > 0.2);
        assert_eq!(feed.duration_ms().map(|d| d / 100), Some(20));
    }

    #[test]
    fn opening_again_at_a_position_reuses_the_fetched_file() {
        let server = TestServer::start(Script {
            body: tone(),
            ..Script::default()
        });
        let dir = TempDir::new().unwrap();
        let mut o = opener(&server, &dir);
        let e = entry("track");
        let (_, last) = drain(o.open(&e, 0, TARGET).as_mut());
        assert_eq!(last, FeedEvent::End);
        let (pcm, last) = drain(o.open(&e, 1_000, TARGET).as_mut());
        assert_eq!(last, FeedEvent::End);
        let frames = pcm.len() / 2;
        assert!(frames.abs_diff(48_000) < 3_000, "{frames} frames");
        assert_eq!(server.ranges().len(), 1);
        assert_eq!(files(&dir), 1);
    }

    #[test]
    fn low_quality_asks_for_offline_audio() {
        let server = TestServer::start(Script {
            body: tone(),
            ..Script::default()
        });
        let dir = TempDir::new().unwrap();
        let mut o = opener(&server, &dir);
        o.set_quality(Quality::Low);
        let (_, last) = drain(o.open(&entry("track"), 0, TARGET).as_mut());
        assert_eq!(last, FeedEvent::End);
        let paths = server.paths();
        assert!(
            paths[0].ends_with("/media/track/offline-audio"),
            "{paths:?}"
        );
    }

    #[test]
    fn a_404_is_unplayable() {
        let server = TestServer::start(Script {
            status: Some(404),
            ..Script::default()
        });
        let dir = TempDir::new().unwrap();
        let mut o = opener(&server, &dir);
        let (_, last) = drain(o.open(&entry("track"), 0, TARGET).as_mut());
        assert!(
            matches!(last, FeedEvent::Failed(TrackError::Unplayable(_))),
            "{last:?}"
        );
    }

    #[test]
    fn a_stalled_server_is_a_network_failure() {
        let server = TestServer::start(Script {
            body: tone(),
            stall_on: Some(1),
            ..Script::default()
        });
        let dir = TempDir::new().unwrap();
        let mut o = HttpOpener::new().with_timeouts(Timeouts {
            chunk: Duration::from_millis(300),
            prepare: Duration::from_secs(20),
        });
        o.set_endpoint(Some(endpoint(&server, dir.path())));
        let (_, last) = drain(o.open(&entry("track"), 0, TARGET).as_mut());
        assert!(
            matches!(last, FeedEvent::Failed(TrackError::Network(_))),
            "{last:?}"
        );
    }

    #[test]
    fn preload_waits_for_the_current_track() {
        let server = TestServer::start(Script {
            body: tone(),
            first_delay: Duration::from_secs(2),
            ..Script::default()
        });
        let dir = TempDir::new().unwrap();
        let mut o = opener(&server, &dir);
        let mut feed = o.open(&entry("a"), 0, TARGET);
        o.preload(Some(&entry("b")));
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            server.paths().len(),
            1,
            "the next track was asked for early"
        );
        let (_, last) = drain(feed.as_mut());
        assert_eq!(last, FeedEvent::End);
        wait_until("the preload's request", || server.paths().len() == 2);
        let paths = server.paths();
        assert!(
            paths[0].contains("/a/") && paths[1].contains("/b/"),
            "{paths:?}"
        );
        // Opening it now uses the preloaded file.
        let (_, last) = drain(o.open(&entry("b"), 0, TARGET).as_mut());
        assert_eq!(last, FeedEvent::End);
        assert_eq!(server.paths().len(), 2);
    }

    #[test]
    fn clear_removes_the_temporary_files() {
        let server = TestServer::start(Script {
            body: tone(),
            ..Script::default()
        });
        let dir = TempDir::new().unwrap();
        let mut o = opener(&server, &dir);
        let (_, last) = drain(o.open(&entry("a"), 0, TARGET).as_mut());
        assert_eq!(last, FeedEvent::End);
        o.preload(Some(&entry("b")));
        assert!(files(&dir) >= 1);
        o.clear();
        wait_until("the files to go", || files(&dir) == 0);
    }

    #[test]
    fn no_endpoint_is_not_connected() {
        let mut o = HttpOpener::new();
        let mut feed = o.open(&entry("a"), 0, TARGET);
        assert_eq!(
            feed.poll(),
            FeedEvent::Failed(TrackError::Network("not connected".into()))
        );
    }

    #[test]
    fn other_tracks_are_dropped_when_a_new_one_opens() {
        let server = TestServer::start(Script {
            body: tone(),
            ..Script::default()
        });
        let dir = TempDir::new().unwrap();
        let mut o = opener(&server, &dir);
        for id in ["a", "b", "c"] {
            let (_, last) = drain(o.open(&entry(id), 0, TARGET).as_mut());
            assert_eq!(last, FeedEvent::End);
        }
        wait_until("older files to go", || files(&dir) == 1);
    }

    #[test]
    fn waiting_reports_preparing_for_a_slow_first_response() {
        let server = TestServer::start(Script {
            body: tone(),
            first_delay: Duration::from_secs(2),
            ..Script::default()
        });
        let dir = TempDir::new().unwrap();
        let mut o = opener(&server, &dir);
        o.set_quality(Quality::Low);
        let mut feed = o.open(&entry("track"), 0, TARGET);
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < end, "never reported preparing");
            if feed.poll() == FeedEvent::Waiting(Wait::Preparing) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_failed_track_is_fetched_afresh_when_opened_again() {
        // The first request is refused with 401, later ones are served.
        let server = TestServer::start(Script {
            body: tone(),
            fail_first: 1,
            ..Script::default()
        });
        let dir = TempDir::new().unwrap();
        let mut o = opener(&server, &dir);
        let e = entry("track");
        let (_, last) = drain(o.open(&e, 0, TARGET).as_mut());
        assert!(matches!(last, FeedEvent::Failed(_)), "{last:?}");
        let (pcm, last) = drain(o.open(&e, 0, TARGET).as_mut());
        assert_eq!(last, FeedEvent::End);
        assert!(!pcm.is_empty());
        assert_eq!(files(&dir), 1);
    }

    #[test]
    fn a_preload_that_failed_is_fetched_afresh() {
        let server = TestServer::start(Script {
            body: tone(),
            fail_first: 1,
            ..Script::default()
        });
        let dir = TempDir::new().unwrap();
        let mut o = opener(&server, &dir);
        o.preload(Some(&entry("b")));
        wait_until("the failed request", || server.paths().len() == 1);
        std::thread::sleep(Duration::from_millis(100));
        let (_, last) = drain(o.open(&entry("b"), 0, TARGET).as_mut());
        assert_eq!(last, FeedEvent::End);
    }

    #[test]
    fn a_dropped_feed_stops_waiting_for_bytes() {
        let server = TestServer::start(Script {
            body: tone(),
            first_delay: Duration::from_secs(2),
            ..Script::default()
        });
        let dir = TempDir::new().unwrap();
        let mut o = opener(&server, &dir);
        let mut feed = o.open(&entry("track"), 0, TARGET);
        let stream = o.current.as_ref().unwrap().stream.clone();
        wait_until("the reader to wait", || {
            matches!(feed.poll(), FeedEvent::Waiting(Wait::Buffering))
                && stream.status() == StreamStatus::Waiting(Wait::Buffering)
        });
        drop(feed);
        wait_until("the reader to stop waiting", || {
            stream.status() == StreamStatus::Fetching
        });
        // Only the opener and this test hold the stream once the thread has gone.
        wait_until("the decoder thread to go", || {
            Arc::strong_count(&stream) <= 3
        });
    }

    #[test]
    #[cfg(unix)]
    fn a_live_marker_keeps_an_old_directory() {
        let root = TempDir::new().unwrap();
        let base = root.path().join("lib").join("stream");
        let d = base.join("live");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(ALIVE_MARKER), b"").unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(25 * 3600);
        std::fs::File::open(&d).unwrap().set_modified(old).unwrap();
        stream_dir(root.path(), "lib").unwrap();
        assert!(d.exists());
    }

    #[test]
    #[cfg(unix)]
    fn stream_dir_removes_only_stale_siblings() {
        let root = TempDir::new().unwrap();
        let base = root.path().join("lib").join("stream");
        std::fs::create_dir_all(&base).unwrap();
        let ago = |h: u64| std::time::SystemTime::now() - Duration::from_secs(h * 3600);
        for (name, h) in [("old", 25), ("recent", 1)] {
            let d = base.join(name);
            std::fs::create_dir(&d).unwrap();
            std::fs::write(d.join("x"), b"x").unwrap();
            std::fs::File::open(&d)
                .unwrap()
                .set_modified(ago(h))
                .unwrap();
        }
        let mine = stream_dir(root.path(), "lib").unwrap();
        assert!(mine.is_dir());
        assert_eq!(mine.parent().unwrap(), base);
        assert!(!base.join("old").exists());
        assert!(base.join("recent").exists());
        let tag = mine.file_name().unwrap().to_str().unwrap();
        let (pid, hex) = tag.split_once('-').unwrap();
        assert_eq!(pid, std::process::id().to_string());
        assert_eq!(hex.len(), 8);
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn stream_dir_rejects_an_unsafe_library_id() {
        let outer = TempDir::new().unwrap();
        let root = outer.path().join("cache");
        std::fs::create_dir(&root).unwrap();
        let victim = outer.path().join("victim");
        std::fs::create_dir_all(victim.join("stream").join("old")).unwrap();
        let absolute = outer.path().join("abs").to_str().unwrap().to_owned();
        for id in [
            "..",
            "../x",
            "a/b",
            "a\\b",
            "",
            ".hidden",
            absolute.as_str(),
        ] {
            let err = stream_dir(&root, id).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{id:?}");
        }
        assert!(victim.join("stream").join("old").exists());
        assert!(!outer.path().join("abs").exists());
        assert!(!outer.path().join("x").exists());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }
}
