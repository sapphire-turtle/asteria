//! The playback engine: a synchronous state machine between the queue, the decoder's
//! feed and the sound device. The player thread (Task 7) calls `handle` and `pump`.

use std::time::Duration;

use asteria_music_api::ItemId;

use super::{
    Command, Endpoint, MAX_SKIPS, PlayerError, PlayerState, Quality, QueueEntry, RESTART_AFTER_MS,
    Status,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OutputConfig {
    pub sample_rate: u32,
    pub channels: u16,
}

pub trait AudioOutput: Send {
    fn config(&self) -> OutputConfig;
    /// Free room in the device's buffer, in frames.
    fn writable(&self) -> usize;
    /// Interleaved samples in `config().channels`, at most `writable()` frames.
    fn write(&mut self, samples: &[f32]);
    /// Frames the device has played since it was opened.
    fn played(&self) -> u64;
    fn set_paused(&mut self, paused: bool);
    /// Drops what is buffered and not yet played.
    fn clear(&mut self);
    /// Set once the device is gone (unplugged, say).
    fn failed(&self) -> Option<String>;
}

pub trait OutputFactory: Send {
    fn open(&mut self) -> Result<Box<dyn AudioOutput>, String>;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wait {
    Buffering,
    Preparing,
}

#[derive(Clone, PartialEq, Debug)]
pub enum TrackError {
    Unplayable(String),
    Network(String),
    Unauthorized,
}

#[derive(Clone, PartialEq, Debug)]
pub enum FeedEvent {
    Pcm(Vec<f32>),
    Waiting(Wait),
    End,
    Failed(TrackError),
}

/// One track being decoded, already in the output's format. Never blocks.
pub trait Feed: Send {
    fn poll(&mut self) -> FeedEvent;
    /// The decoder's length, once known.
    fn duration_ms(&self) -> Option<u64>;
}

pub trait Opener: Send {
    /// Opens `entry` from `at_ms`, converting to `target`. A seek is a new `open` of the
    /// same entry: the opener keeps what it fetched.
    fn open(&mut self, entry: &QueueEntry, at_ms: u64, target: OutputConfig) -> Box<dyn Feed>;
    /// The track after the current one, to fetch once the current one is fetched.
    fn preload(&mut self, next: Option<&QueueEntry>);
    fn set_endpoint(&mut self, endpoint: Option<Endpoint>);
    fn set_quality(&mut self, quality: Quality);
    /// Forgets every track: stops fetching and deletes the temporary files.
    fn clear(&mut self);
}

/// How soon `pump` wants to be called again while there is something to follow.
const TICK: Duration = Duration::from_millis(10);

pub struct Engine {
    opener: Box<dyn Opener>,
    outputs: Box<dyn OutputFactory>,
    output: Option<Box<dyn AudioOutput>>,
    feed: Option<Box<dyn Feed>>,
    state: PlayerState,
    /// Tracks in a row that could not be played.
    skips: u32,
    /// Where the current feed was opened, in ms.
    base_ms: u64,
    /// `output.played()` when it was opened, and the frames written since.
    played_base: u64,
    written: u64,
    /// PCM the device had no room for yet.
    pending: Vec<f32>,
    /// The feed is at its end; the device is playing out what was written.
    ended: bool,
    /// The device was reopened and has not played since: another loss pauses.
    reopened: bool,
    /// The decoder's length of the current track, once it is known.
    known_duration: Option<u64>,
    /// The id the opener was last told to preload.
    preloaded: Option<ItemId>,
}

impl Engine {
    pub fn new(opener: Box<dyn Opener>, outputs: Box<dyn OutputFactory>) -> Engine {
        Engine {
            opener,
            outputs,
            output: None,
            feed: None,
            state: PlayerState {
                queue: Vec::new(),
                current: None,
                status: Status::Stopped,
                position_ms: 0,
                duration_ms: None,
                volume: 1.0,
                error: None,
            },
            skips: 0,
            base_ms: 0,
            played_base: 0,
            written: 0,
            pending: Vec::new(),
            ended: false,
            reopened: false,
            known_duration: None,
            preloaded: None,
        }
    }

    pub fn state(&self) -> &PlayerState {
        &self.state
    }

    pub fn handle(&mut self, command: Command) {
        match command {
            Command::Replace { entries, start } => self.replace(entries, start),
            Command::PlayNext(entries) => {
                let at = self.state.current.map_or(0, |c| c + 1);
                self.insert(at, entries);
            }
            Command::Append(entries) => {
                let at = self.state.queue.len();
                self.insert(at, entries);
            }
            Command::Remove(i) => self.remove(i),
            Command::Move { from, to } => self.move_entry(from, to),
            Command::Jump(i) => {
                if i < self.state.queue.len() {
                    self.skips = 0;
                    self.open_track(i, false);
                }
            }
            Command::Play => self.play(),
            Command::Pause => self.pause(),
            Command::Toggle => match self.state.status {
                Status::Stopped | Status::Paused => self.play(),
                _ => self.pause(),
            },
            Command::Seek(ms) => self.seek(ms),
            Command::Next => self.next(),
            Command::Previous => self.previous(),
            Command::SetVolume(v) => {
                if !v.is_nan() {
                    self.state.volume = v.clamp(0.0, 1.0);
                }
            }
            Command::Stop => {
                self.skips = 0;
                self.halt();
            }
            Command::SetEndpoint(endpoint) => {
                self.skips = 0;
                self.state.queue.clear();
                self.state.current = None;
                self.known_duration = None;
                self.halt();
                self.opener.clear();
                self.preloaded = None;
                self.opener.set_endpoint(endpoint);
            }
            Command::SetQuality(q) => self.opener.set_quality(q),
        }
        self.sync_preload();
        self.refresh();
    }

    /// Moves PCM from the feed to the output and follows the track's end. Returns how soon
    /// it wants to be called again: ~10 ms while playing or waiting, `None` when idle.
    pub fn pump(&mut self) -> Option<Duration> {
        if !self.is_active() {
            return None;
        }
        if let Some(why) = self.output.as_ref().and_then(|o| o.failed()) {
            self.lose_device(why);
        } else if self.ended {
            self.follow_end();
        } else {
            self.feed_output();
        }
        self.sync_preload();
        self.refresh();
        self.is_active().then_some(TICK)
    }

    fn is_active(&self) -> bool {
        matches!(
            self.state.status,
            Status::Playing | Status::Buffering | Status::Preparing
        )
    }

    // The feed, the device and the position.

    fn feed_output(&mut self) {
        if !self.pending.is_empty() {
            self.write_pending();
            return;
        }
        let (Some(feed), Some(output)) = (self.feed.as_mut(), self.output.as_ref()) else {
            return;
        };
        if output.writable() == 0 {
            return;
        }
        match feed.poll() {
            FeedEvent::Pcm(samples) => {
                if samples.is_empty() {
                    return;
                }
                self.skips = 0;
                self.reopened = false;
                self.state.status = Status::Playing;
                let volume = self.state.volume;
                self.pending = samples.into_iter().map(|s| s * volume).collect();
                self.write_pending();
            }
            FeedEvent::Waiting(Wait::Buffering) => self.state.status = Status::Buffering,
            FeedEvent::Waiting(Wait::Preparing) => self.state.status = Status::Preparing,
            FeedEvent::End => self.ended = true,
            FeedEvent::Failed(TrackError::Unplayable(reason)) => self.unplayable(reason),
            FeedEvent::Failed(TrackError::Network(e)) => self.pause_with(PlayerError::Network(e)),
            FeedEvent::Failed(TrackError::Unauthorized) => {
                self.pause_with(PlayerError::Unauthorized)
            }
        }
    }

    fn write_pending(&mut self) {
        let Some(output) = self.output.as_mut() else {
            return;
        };
        let channels = usize::from(output.config().channels.max(1));
        let room = output.writable() * channels;
        let n = room.min(self.pending.len());
        if n == 0 {
            return;
        }
        output.write(&self.pending[..n]);
        self.written += (n / channels) as u64;
        self.pending.drain(..n);
    }

    /// The feed is done: once the device has played what it was given, the next track.
    fn follow_end(&mut self) {
        if !self.pending.is_empty() {
            self.write_pending();
            return;
        }
        let Some(output) = self.output.as_ref() else {
            return;
        };
        if output.played() < self.played_base + self.written {
            return;
        }
        self.advance();
    }

    /// The track is over or unplayable: the next one, or a stop at the last one's head.
    fn advance(&mut self) {
        match self.state.current {
            Some(c) if c + 1 < self.state.queue.len() => self.open_track(c + 1, false),
            _ => self.halt(),
        }
    }

    fn unplayable(&mut self, reason: String) {
        self.skips += 1;
        let title = self
            .state
            .current
            .and_then(|c| self.state.queue.get(c))
            .map(|e| e.title.clone())
            .unwrap_or_default();
        if self.skips >= MAX_SKIPS {
            self.halt();
            self.state.error = Some(PlayerError::TooManyFailures);
        } else {
            self.advance();
            self.state.error = Some(PlayerError::Unplayable { title, reason });
        }
    }

    /// Pauses where the track is, to be reopened from there by `Play`.
    fn pause_with(&mut self, error: PlayerError) {
        let at = self.position();
        self.drop_feed();
        if let Some(output) = self.output.as_mut() {
            output.clear();
            output.set_paused(true);
        }
        self.base_ms = at;
        self.state.status = Status::Paused;
        self.state.error = Some(error);
    }

    /// The device is gone: reopen it once and carry on from the same place.
    fn lose_device(&mut self, why: String) {
        tracing::warn!("the audio device failed: {why}");
        if self.reopened {
            self.pause_with(PlayerError::Output(why));
            self.output = None;
            return;
        }
        let at = self.position();
        self.output = None;
        self.reopened = true;
        self.start(at, false);
    }

    /// Where the current track is, in ms.
    fn position(&self) -> u64 {
        match (&self.feed, &self.output) {
            (Some(_), Some(output)) => {
                let rate = u64::from(output.config().sample_rate.max(1));
                let frames = output.played().saturating_sub(self.played_base);
                self.base_ms + frames * 1000 / rate
            }
            _ => self.base_ms,
        }
    }

    fn refresh(&mut self) {
        if let Some(d) = self.feed.as_ref().and_then(|f| f.duration_ms()) {
            self.known_duration = Some(d);
        }
        self.state.duration_ms = self.known_duration.or_else(|| {
            self.state
                .current
                .and_then(|c| self.state.queue.get(c))
                .and_then(|e| e.duration_ms)
        });
        self.state.position_ms = self.position();
    }

    fn sync_preload(&mut self) {
        let next = self.state.current.and_then(|c| self.state.queue.get(c + 1));
        let id = next.map(|e| e.media.id.clone());
        if id != self.preloaded {
            self.opener.preload(next);
            self.preloaded = id;
        }
    }

    // Opening and stopping tracks.

    fn drop_feed(&mut self) {
        self.feed = None;
        self.ended = false;
        self.pending.clear();
    }

    /// Makes `index` the current track and plays it from its head.
    fn open_track(&mut self, index: usize, paused: bool) {
        self.state.current = Some(index);
        self.known_duration = None;
        self.state.error = None;
        self.start(0, paused);
    }

    /// Opens the current track at `at_ms`; `paused` leaves the device silent.
    fn start(&mut self, at_ms: u64, paused: bool) {
        self.drop_feed();
        self.base_ms = at_ms;
        let Some(entry) = self
            .state
            .current
            .and_then(|c| self.state.queue.get(c))
            .cloned()
        else {
            self.state.status = Status::Stopped;
            return;
        };
        if self.output.is_none() {
            match self.outputs.open() {
                Ok(output) => self.output = Some(output),
                Err(e) => {
                    self.state.status = Status::Paused;
                    self.state.error = Some(PlayerError::Output(e));
                    return;
                }
            }
        }
        let Some(output) = self.output.as_mut() else {
            return;
        };
        output.clear();
        output.set_paused(paused);
        self.played_base = output.played();
        self.written = 0;
        self.feed = Some(self.opener.open(&entry, at_ms, output.config()));
        self.state.status = if paused {
            Status::Paused
        } else {
            Status::Playing
        };
    }

    /// Stops at the head of the current track. The queue stays.
    fn halt(&mut self) {
        self.drop_feed();
        if let Some(output) = self.output.as_mut() {
            output.clear();
            output.set_paused(true);
        }
        self.base_ms = 0;
        self.state.status = Status::Stopped;
    }

    // Commands.

    fn play(&mut self) {
        self.skips = 0;
        self.state.error = None;
        if self.state.current.is_none() {
            return;
        }
        match self.state.status {
            Status::Playing | Status::Buffering | Status::Preparing => {}
            Status::Paused if self.feed.is_some() => {
                if let Some(output) = self.output.as_mut() {
                    output.set_paused(false);
                }
                self.state.status = Status::Playing;
            }
            Status::Paused | Status::Stopped => {
                let at = self.base_ms;
                self.start(at, false);
            }
        }
    }

    fn pause(&mut self) {
        if !self.is_active() {
            return;
        }
        if let Some(output) = self.output.as_mut() {
            output.set_paused(true);
        }
        self.state.status = Status::Paused;
    }

    fn seek(&mut self, ms: u64) {
        if self.state.current.is_none() {
            return;
        }
        self.refresh();
        let ms = self.state.duration_ms.map_or(ms, |d| ms.min(d));
        if self.state.status == Status::Stopped {
            self.base_ms = ms;
        } else {
            let paused = self.state.status == Status::Paused;
            self.start(ms, paused);
        }
    }

    fn next(&mut self) {
        let Some(c) = self.state.current else {
            return;
        };
        self.skips = 0;
        if c + 1 < self.state.queue.len() {
            self.go_to(c + 1);
        } else if self.state.status == Status::Stopped {
            self.base_ms = 0;
        } else {
            self.halt();
        }
    }

    fn previous(&mut self) {
        let Some(c) = self.state.current else {
            return;
        };
        self.skips = 0;
        if self.position() > RESTART_AFTER_MS || c == 0 {
            if self.state.status == Status::Stopped {
                self.base_ms = 0;
            } else {
                let paused = self.state.status == Status::Paused;
                self.start(0, paused);
            }
        } else {
            self.go_to(c - 1);
        }
    }

    /// Makes `index` current, keeping the play state: a stopped player stays stopped.
    fn go_to(&mut self, index: usize) {
        match self.state.status {
            Status::Stopped => {
                self.state.current = Some(index);
                self.known_duration = None;
                self.base_ms = 0;
            }
            status => self.open_track(index, status == Status::Paused),
        }
    }

    fn replace(&mut self, entries: Vec<QueueEntry>, start: usize) {
        self.skips = 0;
        self.state.queue = entries;
        if self.state.queue.is_empty() {
            self.state.current = None;
            self.known_duration = None;
            self.halt();
            return;
        }
        let start = if start < self.state.queue.len() {
            start
        } else {
            0
        };
        self.open_track(start, false);
    }

    fn insert(&mut self, at: usize, entries: Vec<QueueEntry>) {
        if entries.is_empty() {
            return;
        }
        let was_empty = self.state.queue.is_empty();
        let at = at.min(self.state.queue.len());
        self.state.queue.splice(at..at, entries);
        if was_empty {
            // Something to play: `Play` starts it.
            self.state.current = Some(0);
            self.known_duration = None;
            self.base_ms = 0;
        }
    }

    fn remove(&mut self, i: usize) {
        if i >= self.state.queue.len() {
            return;
        }
        self.state.queue.remove(i);
        let Some(c) = self.state.current else {
            return;
        };
        if i < c {
            self.state.current = Some(c - 1);
        } else if i == c {
            let status = self.state.status;
            if self.state.queue.is_empty() {
                self.state.current = None;
                self.known_duration = None;
                self.halt();
            } else if c < self.state.queue.len() {
                // The next track slid into the removed one's place.
                match status {
                    Status::Stopped => self.go_to(c),
                    _ => self.open_track(c, status == Status::Paused),
                }
            } else {
                self.state.current = Some(c - 1);
                self.known_duration = None;
                self.halt();
            }
        }
    }

    fn move_entry(&mut self, from: usize, to: usize) {
        let len = self.state.queue.len();
        if from >= len || len == 0 {
            return;
        }
        let to = to.min(len - 1);
        let entry = self.state.queue.remove(from);
        self.state.queue.insert(to, entry);
        if let Some(c) = self.state.current {
            self.state.current = Some(if from == c {
                to
            } else if from < c && to >= c {
                c - 1
            } else if from > c && to <= c {
                c + 1
            } else {
                c
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::fakes::{FakeOpener, FakeOutputs, OutputHandle, Step, entry};

    struct Rig {
        engine: Engine,
        opener: FakeOpener,
        outputs: FakeOutputs,
    }

    impl Rig {
        fn new() -> Rig {
            let opener = FakeOpener::new();
            let outputs = FakeOutputs::new();
            let engine = Engine::new(Box::new(opener.clone()), Box::new(outputs.clone()));
            Rig {
                engine,
                opener,
                outputs,
            }
        }
        fn replace(&mut self, ids: &[&str], start: usize) {
            self.engine.handle(Command::Replace {
                entries: ids.iter().map(|i| entry(i)).collect(),
                start,
            });
        }
        fn pump(&mut self, n: usize) {
            for _ in 0..n {
                self.engine.pump();
            }
        }
        fn out(&self) -> OutputHandle {
            self.outputs.current().expect("the device is open")
        }
        fn opened(&self) -> Vec<(String, u64)> {
            self.opener.opened()
        }
        fn current(&self) -> Option<usize> {
            self.engine.state().current
        }
        fn status(&self) -> Status {
            self.engine.state().status
        }
    }

    fn o(id: &str, at: u64) -> (String, u64) {
        (id.to_string(), at)
    }

    #[test]
    fn replace_plays_from_start() {
        let mut r = Rig::new();
        r.replace(&["a", "b", "c"], 1);
        assert_eq!(r.current(), Some(1));
        assert_eq!(r.status(), Status::Playing);
        assert_eq!(r.opened(), vec![o("b", 0)]);
        assert_eq!(r.engine.state().queue.len(), 3);
        // An out-of-range start plays the first.
        r.replace(&["a", "b"], 9);
        assert_eq!(r.current(), Some(0));
        assert_eq!(r.opened().last(), Some(&o("a", 0)));
        // An empty queue is a stop.
        r.replace(&[], 0);
        assert_eq!(r.status(), Status::Stopped);
        assert_eq!(r.current(), None);
        assert!(r.engine.state().queue.is_empty());
    }

    #[test]
    fn end_of_track_moves_on_after_the_output_drains() {
        let mut r = Rig::new();
        r.opener.script("b", vec![Step::Pcm(50), Step::End]);
        r.replace(&["a", "b", "c"], 1);
        r.pump(1); // 50 frames written
        r.pump(1); // End
        assert_eq!(r.out().written_frames(), 50);
        r.out().advance(20);
        r.pump(3);
        assert_eq!(
            r.opened(),
            vec![o("b", 0)],
            "still playing what was written"
        );
        assert_eq!(r.current(), Some(1));
        assert_eq!(r.engine.state().position_ms, 20);
        r.out().advance(30);
        r.pump(1);
        assert_eq!(r.opened(), vec![o("b", 0), o("c", 0)]);
        assert_eq!(r.current(), Some(2));
        assert_eq!(r.status(), Status::Playing);
        assert_eq!(r.engine.state().position_ms, 0);
    }

    #[test]
    fn end_of_queue_stops_at_the_head_of_the_last_track() {
        let mut r = Rig::new();
        r.opener.script("b", vec![Step::Pcm(10), Step::End]);
        r.replace(&["a", "b"], 1);
        r.pump(2);
        r.out().advance(10);
        r.pump(1);
        assert_eq!(r.status(), Status::Stopped);
        assert_eq!(r.current(), Some(1));
        assert_eq!(r.engine.state().position_ms, 0);
        assert_eq!(r.engine.pump(), None);
        // Play starts the last track again.
        r.engine.handle(Command::Play);
        assert_eq!(r.opened().last(), Some(&o("b", 0)));
        assert_eq!(r.status(), Status::Playing);
    }

    #[test]
    fn previous_restarts_after_three_seconds() {
        let mut r = Rig::new();
        r.opener.script("b", vec![Step::Pcm(4000)]);
        r.opener.script("b", vec![Step::Pcm(4000)]);
        r.replace(&["a", "b"], 1);
        r.pump(1);
        r.out().advance(3001);
        r.engine.handle(Command::Previous);
        assert_eq!(r.opened(), vec![o("b", 0), o("b", 0)]);
        assert_eq!(r.current(), Some(1));
        r.pump(1);
        r.out().advance(2999);
        r.pump(1);
        assert_eq!(r.engine.state().position_ms, 2999);
        r.engine.handle(Command::Previous);
        assert_eq!(r.opened().last(), Some(&o("a", 0)));
        assert_eq!(r.current(), Some(0));
        // On the first track it goes to its head.
        r.out().advance(10);
        r.engine.handle(Command::Previous);
        assert_eq!(r.opened().last(), Some(&o("a", 0)));
        assert_eq!(r.current(), Some(0));
        assert_eq!(r.engine.state().position_ms, 0);
    }

    fn bad() -> Vec<Step> {
        vec![Step::Fail(TrackError::Unplayable("no".into()))]
    }

    #[test]
    fn unplayable_tracks_are_skipped_and_three_in_a_row_stop() {
        let mut r = Rig::new();
        for id in ["t0", "t1", "t3", "t4", "t5"] {
            r.opener.script(id, bad());
        }
        r.replace(&["t0", "t1", "t2", "t3", "t4", "t5", "t6"], 0);
        r.pump(2); // t0 fails, t1 fails
        assert_eq!(r.current(), Some(2));
        assert_eq!(
            r.engine.state().error,
            Some(PlayerError::Unplayable {
                title: "t1".into(),
                reason: "no".into()
            })
        );
        r.pump(1); // t2 Pcm: skips reset
        r.pump(1); // t2 End
        r.out().advance(100);
        r.pump(1); // t3 opens
        assert_eq!(r.current(), Some(3));
        r.pump(3); // t3, t4, t5 fail
        assert_eq!(r.status(), Status::Stopped);
        assert_eq!(r.engine.state().error, Some(PlayerError::TooManyFailures));
        assert_eq!(r.current(), Some(5));
        assert!(!r.opened().iter().any(|(id, _)| id == "t6"));
    }

    #[test]
    fn network_error_pauses_and_play_resumes_at_the_same_position() {
        let mut r = Rig::new();
        r.opener.script(
            "a",
            vec![
                Step::Pcm(100),
                Step::Fail(TrackError::Network("down".into())),
            ],
        );
        r.replace(&["a", "b"], 0);
        r.pump(1);
        r.out().advance(40);
        r.pump(1);
        assert_eq!(r.status(), Status::Paused);
        assert_eq!(
            r.engine.state().error,
            Some(PlayerError::Network("down".into()))
        );
        assert_eq!(r.engine.state().position_ms, 40);
        assert_eq!(r.engine.pump(), None);
        r.engine.handle(Command::Play);
        assert_eq!(r.opened(), vec![o("a", 0), o("a", 40)]);
        assert_eq!(r.status(), Status::Playing);
        assert_eq!(r.engine.state().error, None);
        assert_eq!(r.engine.state().position_ms, 40);
        // An unauthorized token pauses the same way.
        r.opener
            .script("a", vec![Step::Fail(TrackError::Unauthorized)]);
        r.engine.handle(Command::Seek(70));
        r.pump(1);
        assert_eq!(r.status(), Status::Paused);
        assert_eq!(r.engine.state().error, Some(PlayerError::Unauthorized));
        assert_eq!(r.engine.state().position_ms, 70);
    }

    #[test]
    fn removing_and_moving_keep_current_on_the_playing_track() {
        let mut r = Rig::new();
        let ids = |r: &Rig| -> Vec<String> {
            r.engine
                .state()
                .queue
                .iter()
                .map(|e| e.media.id.0.clone())
                .collect()
        };
        r.replace(&["a", "b", "c", "d", "e"], 2);
        // Remove before: the playing track is not reopened.
        r.engine.handle(Command::Remove(0));
        assert_eq!(r.current(), Some(1));
        assert_eq!(ids(&r), ["b", "c", "d", "e"]);
        assert_eq!(r.opened(), vec![o("c", 0)]);
        // Remove after: nothing changes.
        r.engine.handle(Command::Remove(3));
        assert_eq!(r.current(), Some(1));
        assert_eq!(r.opened().len(), 1);
        // Move the playing track: current follows.
        r.engine.handle(Command::Move { from: 1, to: 2 });
        assert_eq!(ids(&r), ["b", "d", "c"]);
        assert_eq!(r.current(), Some(2));
        // Move another across it, both ways.
        r.engine.handle(Command::Move { from: 0, to: 2 });
        assert_eq!(ids(&r), ["d", "c", "b"]);
        assert_eq!(r.current(), Some(1));
        r.engine.handle(Command::Move { from: 2, to: 0 });
        assert_eq!(ids(&r), ["b", "d", "c"]);
        assert_eq!(r.current(), Some(2));
        assert_eq!(r.opened().len(), 1, "moves never reopen the track");
        // Remove the current: the next one plays, in the same state.
        r.engine.handle(Command::Move { from: 2, to: 1 });
        assert_eq!(ids(&r), ["b", "c", "d"]);
        assert_eq!(r.current(), Some(1));
        r.engine.handle(Command::Pause);
        r.engine.handle(Command::Remove(1));
        assert_eq!(ids(&r), ["b", "d"]);
        assert_eq!(r.current(), Some(1));
        assert_eq!(r.opened().last(), Some(&o("d", 0)));
        assert_eq!(r.status(), Status::Paused);
        // Remove the current when it is the last: stopped, at the head of the new last.
        r.engine.handle(Command::Remove(1));
        assert_eq!(r.status(), Status::Stopped);
        assert_eq!(ids(&r), ["b"]);
        assert_eq!(r.current(), Some(0));
        // Replace swaps the queue and plays.
        r.replace(&["x", "y"], 0);
        assert_eq!(r.current(), Some(0));
        assert_eq!(r.status(), Status::Playing);
        assert_eq!(ids(&r), ["x", "y"]);
        r.engine.handle(Command::Remove(0));
        r.engine.handle(Command::Remove(0));
        assert_eq!(r.current(), None);
        assert_eq!(r.status(), Status::Stopped);
    }

    #[test]
    fn seek_past_the_end_is_clamped() {
        let mut r = Rig::new();
        r.opener.set_duration("a", 5000);
        r.replace(&["a"], 0);
        r.pump(1);
        assert_eq!(r.engine.state().duration_ms, Some(5000));
        r.engine.handle(Command::Seek(9000));
        assert_eq!(r.opened().last(), Some(&o("a", 5000)));
        assert_eq!(r.engine.state().position_ms, 5000);
        // Without a known length, the seek goes as asked.
        r.replace(&["z"], 0);
        r.engine.handle(Command::Seek(9000));
        assert_eq!(r.opened().last(), Some(&o("z", 9000)));
        // While stopped, only the position moves.
        r.engine.handle(Command::Stop);
        let n = r.opened().len();
        r.engine.handle(Command::Seek(1234));
        assert_eq!(r.opened().len(), n);
        assert_eq!(r.engine.state().position_ms, 1234);
        assert_eq!(r.status(), Status::Stopped);
        r.engine.handle(Command::Play);
        assert_eq!(r.opened().last(), Some(&o("z", 1234)));
    }

    #[test]
    fn volume_scales_the_samples() {
        let mut r = Rig::new();
        r.opener.script("a", vec![Step::Pcm(3), Step::Pcm(3)]);
        r.replace(&["a"], 0);
        r.engine.handle(Command::SetVolume(0.5));
        r.pump(1);
        assert_eq!(r.out().written(), vec![0.5; 3]);
        r.engine.handle(Command::SetVolume(7.0));
        assert_eq!(r.engine.state().volume, 1.0);
        r.pump(1);
        assert_eq!(r.out().written()[3..], [1.0; 3]);
        r.engine.handle(Command::SetVolume(-1.0));
        assert_eq!(r.engine.state().volume, 0.0);
    }

    #[test]
    fn a_lost_device_is_reopened_once_then_pauses() {
        let mut r = Rig::new();
        r.opener.script("a", vec![Step::Pcm(500)]);
        r.replace(&["a"], 0);
        r.pump(1);
        r.out().advance(200);
        r.out().fail("unplugged");
        r.pump(1);
        assert_eq!(r.outputs.open_count(), 2);
        assert_eq!(r.opened(), vec![o("a", 0), o("a", 200)]);
        assert_eq!(r.status(), Status::Playing);
        assert_eq!(r.engine.state().position_ms, 200);
        // The new device is lost at once as well.
        r.out().fail("unplugged again");
        r.pump(1);
        assert_eq!(r.outputs.open_count(), 2);
        assert_eq!(r.status(), Status::Paused);
        assert_eq!(
            r.engine.state().error,
            Some(PlayerError::Output("unplugged again".into()))
        );
        // A device that cannot be opened pauses too.
        let mut r = Rig::new();
        r.replace(&["a"], 0);
        r.pump(1);
        r.outputs.refuse_next(1);
        r.out().fail("gone");
        r.pump(1);
        assert_eq!(r.status(), Status::Paused);
        assert!(matches!(
            r.engine.state().error,
            Some(PlayerError::Output(_))
        ));
        r.engine.handle(Command::Play);
        assert_eq!(r.status(), Status::Playing);
    }

    #[test]
    fn a_device_that_will_not_open_pauses_the_first_play() {
        let mut r = Rig::new();
        r.outputs.refuse_next(1);
        r.replace(&["a"], 0);
        assert_eq!(r.status(), Status::Paused);
        assert_eq!(
            r.engine.state().error,
            Some(PlayerError::Output("no device".into()))
        );
        assert!(r.opened().is_empty());
        r.engine.handle(Command::Play);
        assert_eq!(r.status(), Status::Playing);
        assert_eq!(r.engine.state().error, None);
        assert_eq!(r.opened(), vec![o("a", 0)]);
    }

    #[test]
    fn set_endpoint_stops_clears_and_forgets() {
        let mut r = Rig::new();
        r.replace(&["a", "b"], 0);
        r.pump(1);
        r.engine.handle(Command::SetEndpoint(None));
        let s = r.engine.state();
        assert!(s.queue.is_empty());
        assert_eq!(s.current, None);
        assert_eq!(s.status, Status::Stopped);
        assert_eq!(s.position_ms, 0);
        assert_eq!(r.opener.events(), ["clear", "endpoint"]);
        assert_eq!(r.engine.pump(), None);
        r.engine.handle(Command::SetQuality(Quality::Low));
        assert_eq!(r.opener.quality(), Quality::Low);
        assert_eq!(r.opened().len(), 1, "a quality change opens nothing");
    }

    #[test]
    fn preload_follows_the_current_track() {
        let mut r = Rig::new();
        r.replace(&["a", "b", "c"], 0);
        assert_eq!(r.opener.preloads(), [Some("b".to_string())]);
        r.engine.handle(Command::Next);
        assert_eq!(r.opener.preloads().last(), Some(&Some("c".to_string())));
        r.engine.handle(Command::Next);
        assert_eq!(r.opener.preloads().last(), Some(&None));
        r.engine.handle(Command::Append(vec![entry("d")]));
        assert_eq!(r.opener.preloads().last(), Some(&Some("d".to_string())));
        r.engine.handle(Command::PlayNext(vec![entry("e")]));
        assert_eq!(r.opener.preloads().last(), Some(&Some("e".to_string())));
        let n = r.opener.preloads().len();
        r.engine.handle(Command::SetVolume(0.3));
        r.pump(2);
        assert_eq!(r.opener.preloads().len(), n, "no repeats");
    }

    #[test]
    fn buffering_and_preparing_show_in_the_status() {
        let mut r = Rig::new();
        r.opener.script(
            "a",
            vec![
                Step::Waiting(Wait::Preparing),
                Step::Waiting(Wait::Buffering),
                Step::Pcm(10),
            ],
        );
        r.replace(&["a"], 0);
        r.pump(1);
        assert_eq!(r.status(), Status::Preparing);
        r.pump(1);
        assert_eq!(r.status(), Status::Buffering);
        assert!(r.engine.pump().is_some());
        assert_eq!(r.status(), Status::Playing);
        r.engine.handle(Command::Pause);
        assert_eq!(r.status(), Status::Paused);
        assert_eq!(r.engine.pump(), None);
        r.engine.handle(Command::Toggle);
        assert_eq!(r.status(), Status::Playing);
    }
}
