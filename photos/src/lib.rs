//! The photos client: the app ([`app`]), the library screen ([`browse`]) and its strings
//! ([`i18n`]). `main.rs` only calls [`desktop::run`], which opens the window.

pub mod app;
pub mod browse;
#[cfg(not(any(target_os = "ios", target_os = "android")))]
pub mod desktop;
pub mod i18n;

pub use app::PhotosApp;
