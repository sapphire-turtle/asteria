//! Playback: the queue, the state the window shows, and the engine that follows them.

use std::path::PathBuf;

use asteria_framework_client::profiles::Token;
use asteria_music_api::{MediaItem, MusicTrack};
use url::Url;

pub mod decode;
pub mod engine;
#[cfg(any(test, feature = "test-support"))]
pub mod fakes;
pub mod feed;
pub mod resample;
pub mod stream;
#[cfg(test)]
mod test_server;

/// A track gets this many ms of play before "previous" restarts it rather than going back.
pub const RESTART_AFTER_MS: u64 = 3_000;
/// Tracks in a row that cannot be played before playback stops.
pub const MAX_SKIPS: u32 = 3;

/// One queued track, with what the window shows about it.
#[derive(Clone, PartialEq, Debug)]
pub struct QueueEntry {
    /// Its id, its path (the extension is the decoder's hint) and its `file_hash`.
    pub media: MediaItem,
    pub title: String,
    /// The artists joined with ", "; empty when there are none.
    pub artist: String,
    /// The album tag, or empty.
    pub album: String,
    pub duration_ms: Option<u64>,
}

impl QueueEntry {
    pub fn from_track(t: &MusicTrack) -> QueueEntry {
        QueueEntry {
            media: t.item.clone(),
            title: t.display_title.clone(),
            artist: t.tags.artists.join(", "),
            album: t.tags.album.clone().unwrap_or_default(),
            duration_ms: t.tags.duration_ms,
        }
    }
}

/// Which rendition of a track is streamed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Quality {
    #[default]
    Original,
    Low,
}

impl Quality {
    /// The server's name for the rendition.
    pub fn preset(self) -> &'static str {
        match self {
            Quality::Original => "original",
            Quality::Low => "offline-audio",
        }
    }
}

/// The server tracks are streamed from.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub url: Url,
    pub token: Token,
    pub library_id: String,
    /// Where this process keeps the tracks it streams.
    pub stream_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub enum Command {
    Replace {
        entries: Vec<QueueEntry>,
        start: usize,
    },
    PlayNext(Vec<QueueEntry>),
    Append(Vec<QueueEntry>),
    Remove(usize),
    Move {
        from: usize,
        to: usize,
    },
    Jump(usize),
    Play,
    Pause,
    Toggle,
    /// To this many ms into the track.
    Seek(u64),
    Next,
    Previous,
    /// Clamped to 0.0..=1.0.
    SetVolume(f32),
    Stop,
    SetEndpoint(Option<Endpoint>),
    SetQuality(Quality),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Stopped,
    Playing,
    Paused,
    Buffering,
    Preparing,
}

#[derive(Clone, PartialEq, Debug)]
pub enum PlayerError {
    /// The track could not be played; it was skipped.
    Unplayable { title: String, reason: String },
    /// The server could not be reached, or a fetch stalled; playback paused.
    Network(String),
    /// The server refused the token; playback paused.
    Unauthorized,
    /// The audio device failed and could not be reopened; playback paused.
    Output(String),
    /// `MAX_SKIPS` tracks in a row could not be played; playback stopped.
    TooManyFailures,
}

#[derive(Clone, PartialEq, Debug)]
pub struct PlayerState {
    pub queue: Vec<QueueEntry>,
    pub current: Option<usize>,
    pub status: Status,
    pub position_ms: u64,
    /// The track's length: the decoder's, else the tag's.
    pub duration_ms: Option<u64>,
    pub volume: f32,
    pub error: Option<PlayerError>,
}
