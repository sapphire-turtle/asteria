//! Fakes of the device and the opener, for the engine's tests and the integration tests.
//!
//! The device plays at 1000 Hz in one channel, so a frame is a millisecond.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use asteria_music_api::{ItemId, MediaItem, MediaKind};

use super::engine::{
    AudioOutput, Feed, FeedEvent, Opener, OutputConfig, OutputFactory, TrackError, Wait,
};
use super::{Endpoint, Quality, QueueEntry};

pub const RATE: u32 = 1000;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A queue entry for `id`, titled `id`, with the file `<id>.flac`.
pub fn entry(id: &str) -> QueueEntry {
    QueueEntry {
        media: MediaItem {
            id: ItemId(id.into()),
            path: format!("{id}.flac"),
            kind: MediaKind::Audio,
            size: 0,
            modified: "2026-01-01T00:00:00Z".into(),
            rating: None,
            subjects: vec![],
            hierarchical_subjects: vec![],
            description: None,
            label: None,
            content_hash: None,
            file_hash: Some(format!("blake3:{id}")),
            read_only_reason: None,
        },
        title: id.into(),
        artist: String::new(),
        album: String::new(),
        duration_ms: None,
    }
}

#[derive(Default)]
struct OutState {
    played: u64,
    written: Vec<f32>,
    paused: bool,
    clears: u32,
    failed: Option<String>,
}

/// The test's hold on a [`FakeOutput`] the engine owns.
#[derive(Clone, Default)]
pub struct OutputHandle(Arc<Mutex<OutState>>);

impl OutputHandle {
    /// The device plays `frames` more of what was written.
    pub fn advance(&self, frames: u64) {
        lock(&self.0).played += frames;
    }
    /// Every sample written since the device opened.
    pub fn written(&self) -> Vec<f32> {
        lock(&self.0).written.clone()
    }
    pub fn written_frames(&self) -> u64 {
        lock(&self.0).written.len() as u64
    }
    pub fn played(&self) -> u64 {
        lock(&self.0).played
    }
    pub fn is_paused(&self) -> bool {
        lock(&self.0).paused
    }
    pub fn clears(&self) -> u32 {
        lock(&self.0).clears
    }
    /// The device is gone.
    pub fn fail(&self, why: &str) {
        lock(&self.0).failed = Some(why.into());
    }
}

pub struct FakeOutput(OutputHandle);

impl AudioOutput for FakeOutput {
    fn config(&self) -> OutputConfig {
        OutputConfig {
            sample_rate: RATE,
            channels: 1,
        }
    }
    fn writable(&self) -> usize {
        4096
    }
    fn write(&mut self, samples: &[f32]) {
        lock(&(self.0).0).written.extend_from_slice(samples);
    }
    fn played(&self) -> u64 {
        self.0.played()
    }
    fn set_paused(&mut self, paused: bool) {
        lock(&(self.0).0).paused = paused;
    }
    fn clear(&mut self) {
        lock(&(self.0).0).clears += 1;
    }
    fn failed(&self) -> Option<String> {
        lock(&(self.0).0).failed.clone()
    }
}

#[derive(Default)]
struct OutputsState {
    opened: Vec<OutputHandle>,
    refuse: u32,
}

/// An [`OutputFactory`] whose devices the test can reach. Clones share one state.
#[derive(Clone, Default)]
pub struct FakeOutputs(Arc<Mutex<OutputsState>>);

impl FakeOutputs {
    pub fn new() -> Self {
        Self::default()
    }
    /// The device opened last.
    pub fn current(&self) -> Option<OutputHandle> {
        lock(&self.0).opened.last().cloned()
    }
    pub fn open_count(&self) -> usize {
        lock(&self.0).opened.len()
    }
    /// The next `n` opens fail.
    pub fn refuse_next(&self, n: u32) {
        lock(&self.0).refuse = n;
    }
}

impl OutputFactory for FakeOutputs {
    fn open(&mut self) -> Result<Box<dyn AudioOutput>, String> {
        let mut s = lock(&self.0);
        if s.refuse > 0 {
            s.refuse -= 1;
            return Err("no device".into());
        }
        let handle = OutputHandle::default();
        s.opened.push(handle.clone());
        Ok(Box::new(FakeOutput(handle)))
    }
}

/// One step of a track's script.
#[derive(Clone, PartialEq, Debug)]
pub enum Step {
    /// This many frames of 1.0.
    Pcm(usize),
    Waiting(Wait),
    Fail(TrackError),
    End,
}

#[derive(Default)]
struct OpenerState {
    scripts: HashMap<String, VecDeque<Vec<Step>>>,
    durations: HashMap<String, u64>,
    opened: Vec<(String, u64)>,
    preloads: Vec<Option<String>>,
    /// "clear", "endpoint", "quality" in the order they came.
    events: Vec<String>,
    quality: Quality,
    has_endpoint: bool,
}

/// An [`Opener`] that plays scripts. Clones share one state.
///
/// A track with no script gets `[Pcm(100), End]`. Each `open` of an id takes the next
/// script queued for it, and falls back to that default once they are used up. After a
/// script's last step its feed waits (`Buffering`) forever.
#[derive(Clone, Default)]
pub struct FakeOpener(Arc<Mutex<OpenerState>>);

impl FakeOpener {
    pub fn new() -> Self {
        Self::default()
    }
    /// Queues a script for the next `open` of `id` that has none queued before it.
    pub fn script(&self, id: &str, steps: Vec<Step>) {
        lock(&self.0)
            .scripts
            .entry(id.into())
            .or_default()
            .push_back(steps);
    }
    pub fn set_duration(&self, id: &str, ms: u64) {
        lock(&self.0).durations.insert(id.into(), ms);
    }
    /// Every `open`, as (item id, at_ms).
    pub fn opened(&self) -> Vec<(String, u64)> {
        lock(&self.0).opened.clone()
    }
    pub fn preloads(&self) -> Vec<Option<String>> {
        lock(&self.0).preloads.clone()
    }
    pub fn events(&self) -> Vec<String> {
        lock(&self.0).events.clone()
    }
    pub fn quality(&self) -> Quality {
        lock(&self.0).quality
    }
    pub fn has_endpoint(&self) -> bool {
        lock(&self.0).has_endpoint
    }
}

struct FakeFeed {
    steps: VecDeque<Step>,
    duration: Option<u64>,
}

impl Feed for FakeFeed {
    fn poll(&mut self) -> FeedEvent {
        match self.steps.pop_front() {
            Some(Step::Pcm(n)) => FeedEvent::Pcm(vec![1.0; n]),
            Some(Step::Waiting(w)) => FeedEvent::Waiting(w),
            Some(Step::Fail(e)) => FeedEvent::Failed(e),
            Some(Step::End) => FeedEvent::End,
            None => FeedEvent::Waiting(Wait::Buffering),
        }
    }
    fn duration_ms(&self) -> Option<u64> {
        self.duration
    }
}

impl Opener for FakeOpener {
    fn open(&mut self, entry: &QueueEntry, at_ms: u64, _target: OutputConfig) -> Box<dyn Feed> {
        let mut s = lock(&self.0);
        let id = entry.media.id.0.clone();
        s.opened.push((id.clone(), at_ms));
        let steps = s
            .scripts
            .get_mut(&id)
            .and_then(|q| q.pop_front())
            .unwrap_or_else(|| vec![Step::Pcm(100), Step::End]);
        Box::new(FakeFeed {
            steps: steps.into(),
            duration: s.durations.get(&id).copied(),
        })
    }
    fn preload(&mut self, next: Option<&QueueEntry>) {
        lock(&self.0)
            .preloads
            .push(next.map(|e| e.media.id.0.clone()));
    }
    fn set_endpoint(&mut self, endpoint: Option<Endpoint>) {
        let mut s = lock(&self.0);
        s.events.push("endpoint".into());
        s.has_endpoint = endpoint.is_some();
    }
    fn set_quality(&mut self, quality: Quality) {
        let mut s = lock(&self.0);
        s.events.push("quality".into());
        s.quality = quality;
    }
    fn clear(&mut self) {
        lock(&self.0).events.push("clear".into());
    }
}
