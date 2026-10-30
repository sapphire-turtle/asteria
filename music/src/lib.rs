//! The asteria music client: the app ([`app`]), its connection ([`connection`]), the browse
//! screens ([`library`], [`cover_grid`], [`tracks`]), the player ([`player`]) and the
//! strings ([`i18n`]). `main.rs` only calls [`desktop::run`].

pub mod app;
pub mod connection;
pub mod cover_grid;
pub mod desktop;
pub mod i18n;
pub mod library;
pub(crate) mod now_playing;
pub mod player;
pub(crate) mod queue;
pub mod settings;
pub mod tracks;
