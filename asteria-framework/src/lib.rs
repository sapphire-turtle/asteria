//! Shared framework for the asteria applications.

pub mod error;
pub mod hash;
pub mod ids;
pub mod library;
pub mod paths;

pub use error::{Error, Result};
pub use ids::{LibraryId, UserId};
pub use library::{Library, LibraryConfig, User, Users};
