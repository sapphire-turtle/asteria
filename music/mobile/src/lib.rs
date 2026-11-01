//! The Asteria Music phone app. A client only: it depends on `asteria-music-gui` and nothing
//! of the server's. The iOS and Android entry points (Plan 11) will start [`MusicApp`] here.

pub use asteria_music_gui::{APP_NAME, AppPaths, MusicApp};
