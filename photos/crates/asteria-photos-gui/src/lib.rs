//! The photos client: the app ([`app`]), the library screen ([`browse`]), the detail pane
//! ([`detail`]) and their strings ([`i18n`]). It opens no window and serves nothing:
//! `asteria-photos-desktop` and `asteria-photos-mobile` start it.

pub mod app;
pub mod browse;
pub mod connection;
pub mod detail;
pub mod i18n;
pub mod offline;
pub mod uploads;
mod viewer;

pub use app::{AppPaths, PhotosApp};

/// The app's name, for its directories (the same as the server's, `asteria-photos`).
pub const APP_NAME: &str = "asteria-photos";
