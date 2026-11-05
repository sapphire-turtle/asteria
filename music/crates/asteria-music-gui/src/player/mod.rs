//! Playback: what asteria plays through corundum-framework-audio's engine — the queue
//! entry, the server and quality tracks are fetched with, and the opener that fetches them.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use asteria_common_client::profiles::Token;
use asteria_music_api::{MediaItem, MusicTrack};
use corundum_framework_audio::QueueItem;
use url::Url;

pub use corundum_framework_audio::{PlayerError, Status};

pub mod feed;
pub mod stream;
#[cfg(test)]
mod test_server;

pub use feed::HttpOpener;

/// What the window sends the player.
pub type Command = corundum_framework_audio::Command<QueueEntry>;
/// What the window shows of the player.
pub type PlayerState = corundum_framework_audio::PlayerState<QueueEntry>;
/// The playback engine on a thread of its own.
pub type Player = corundum_framework_audio::Player<QueueEntry>;

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

impl QueueItem for QueueEntry {
    fn title(&self) -> String {
        self.title.clone()
    }

    fn duration_ms(&self) -> Option<u64> {
        self.duration_ms
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

#[derive(Debug, Default)]
struct Settings {
    endpoint: Option<Endpoint>,
    quality: Quality,
}

/// The server and the quality the [`HttpOpener`] fetches with, shared between the app (which
/// sets them) and the opener on the player's thread (which reads them when a fetch starts).
/// Clones share one value.
#[derive(Clone, Debug, Default)]
pub struct OpenerSettings(Arc<Mutex<Settings>>);

impl OpenerSettings {
    /// No server, the original quality.
    pub fn new() -> OpenerSettings {
        OpenerSettings::default()
    }

    fn lock(&self) -> MutexGuard<'_, Settings> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// From the next fetch on. What was fetched from the old server is forgotten by
    /// [`Command::Reset`](corundum_framework_audio::Command::Reset), which the app sends next.
    pub fn set_endpoint(&self, endpoint: Option<Endpoint>) {
        self.lock().endpoint = endpoint;
    }

    /// From the next fetch on.
    pub fn set_quality(&self, quality: Quality) {
        self.lock().quality = quality;
    }

    pub fn endpoint(&self) -> Option<Endpoint> {
        self.lock().endpoint.clone()
    }

    pub fn quality(&self) -> Quality {
        self.lock().quality
    }
}

/// A queue entry for `id`, titled `id`, with the file `<id>.flac`.
#[cfg(test)]
pub(crate) fn entry(id: &str) -> QueueEntry {
    use asteria_music_api::{ItemId, MediaKind};
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use asteria_common_client::profiles::Token;
    use corundum_framework_audio::fakes::FakeOutputs;
    use tempfile::TempDir;

    use super::test_server::{Script, TestServer};
    use super::*;

    fn tone() -> Vec<u8> {
        std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.flac"))
            .unwrap()
    }

    fn parts(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
            .count()
    }

    fn wait_for(what: &str, within: Duration, mut ok: impl FnMut() -> bool) {
        let end = Instant::now() + within;
        while !ok() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn queue_entries_name_themselves_by_title_and_tag_length() {
        let mut e = entry("a");
        e.title = "Song".into();
        e.duration_ms = Some(61_000);
        assert_eq!(QueueItem::title(&e), "Song");
        assert_eq!(QueueItem::duration_ms(&e), Some(61_000));
    }

    #[test]
    fn opener_settings_are_shared_by_their_clones() {
        let settings = OpenerSettings::new();
        assert!(settings.endpoint().is_none());
        assert_eq!(settings.quality(), Quality::Original);
        let other = settings.clone();
        other.set_quality(Quality::Low);
        assert_eq!(settings.quality(), Quality::Low);
    }

    /// The server is changed while a track plays and the next is fetched: the app sets the
    /// settings' endpoint and sends `Reset`.
    #[test]
    fn changing_the_server_stops_and_forgets() {
        let server = TestServer::start(Script {
            body: tone(),
            ..Script::default()
        });
        let dir = TempDir::new().unwrap();
        let settings = OpenerSettings::new();
        settings.set_endpoint(Some(Endpoint {
            url: server.url().join("/").unwrap(),
            token: Token::new("secret"),
            library_id: "lib".into(),
            stream_dir: dir.path().to_owned(),
        }));
        let player = Player::spawn(
            Box::new(HttpOpener::new(settings.clone())),
            Box::new(FakeOutputs::new()),
            || {},
        )
        .unwrap();
        player.send(Command::Replace {
            items: vec![entry("a"), entry("b")],
            start: 0,
        });
        wait_for("playing", Duration::from_secs(10), || {
            player.state().status == Status::Playing
        });
        assert!(parts(dir.path()) >= 1);

        settings.set_endpoint(None);
        player.send(Command::Reset);
        wait_for("the reset", Duration::from_secs(10), || {
            player.state().queue.is_empty()
        });
        let s = player.state();
        assert_eq!(s.status, Status::Stopped);
        assert_eq!(s.current, None);
        assert_eq!(s.error, None);
        wait_for("the stream files to go", Duration::from_secs(2), || {
            parts(dir.path()) == 0
        });
    }
}
