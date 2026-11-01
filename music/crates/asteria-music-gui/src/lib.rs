//! The asteria music client: the app ([`app`]), its connection ([`connection`]), the browse
//! screens ([`library`], [`cover_grid`], [`tracks`]), the player ([`player`]) and the
//! strings ([`i18n`]). It opens no window and serves nothing: `asteria-music-desktop` and
//! `asteria-music-mobile` start it.

pub mod app;
pub mod connection;
pub mod cover_grid;
pub mod i18n;
pub mod library;
pub(crate) mod now_playing;
pub mod player;
pub(crate) mod queue;
pub mod settings;
pub mod tracks;

pub use app::{AppPaths, MusicApp};

/// The app's name, for its directories (the same as the server's, `asteria-music`).
pub const APP_NAME: &str = "asteria-music";
