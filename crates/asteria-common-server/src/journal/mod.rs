//! The server's journal: the directory it lives in and how it is made ready.

mod prepare;

pub use prepare::{Prepared, UNDO_WINDOW, prepare};
