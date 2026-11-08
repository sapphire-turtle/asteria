//! The offline copy of a library on disk: what the user pinned, the answers the server gave for
//! those pins, and the media files they need. Layout under `<data>/offline/`, see [`store`].

mod known;
mod store;

pub use known::KnownLibraries;
pub use store::{Doc, MediaKey, OfflineStore, Pin, PinStatus, PinTarget, SyncMark};

/// The `schema` of a stored answer copy; a file with another value is dropped on read.
pub const DOC_SCHEMA: u32 = 1;
