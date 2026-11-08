//! The offline copy of a library on disk: what the user pinned, the answers the server gave for
//! those pins, and the media files they need. Layout under `<data>/offline/`, see [`store`].
//! [`OfflineHandle`] keeps it in step with the server on a thread of its own, see [`sync`].

mod catalog;
mod known;
mod store;
mod sync;

use std::time::Duration;

pub use catalog::{Docs, Fetch, OfflineCatalog};
pub use known::KnownLibraries;
pub use store::{Doc, MediaKey, OfflineStore, Pin, PinStatus, PinTarget, SyncMark};
pub use sync::{OfflineHandle, PinInfo, SyncPhase, SyncProgress};

/// The `schema` of a stored answer copy; a file with another value is dropped on read.
pub const DOC_SCHEMA: u32 = 1;
/// The space the offline media may take unless the user chose another limit (8 GiB).
pub const DEFAULT_LIMIT_BYTES: u64 = 8 << 30;
/// How many media files a sync downloads at the same time.
pub const DOWNLOADS_AT_ONCE: usize = 2;
/// How long one `offline-audio` download may take: the server may transcode first.
pub const AUDIO_TIMEOUT: Duration = Duration::from_secs(600);
/// How many times a sync retries a request the server answered with 503; after that it gives
/// up on it for this sync (without counting it as unsavable).
pub const RETRY_ATTEMPTS: u32 = 20;
/// A temporary file this old is the leftover of a crashed write, not one in progress.
pub const STALE_TMP: Duration = Duration::from_secs(24 * 3600);
