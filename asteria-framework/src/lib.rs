//! Shared framework for the asteria applications.

pub mod error;
pub mod fsutil;
pub mod hash;
pub mod ids;
pub mod journal;
pub mod library;
pub mod meta;
pub mod paths;
pub mod sidecar;

pub use error::{Error, Result};
pub use ids::{LibraryId, UserId};
pub use library::{Library, LibraryConfig, User, Users};
pub use sidecar::{RatingEntry, Sidecar};
