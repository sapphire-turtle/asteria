//! XMP sidecar files (`<media>.<ext>.xmp`).

mod model;
pub(crate) mod ns;
mod read;
mod write;
pub(crate) mod xml;

pub use model::{RatingEntry, Sidecar};
