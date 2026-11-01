//! The Asteria Music server as a library: [`spec`] is what `asteria-music` (headless) and
//! `asteria-music-desktop` hand to the shared command line (`init`, `api-key`, `serve`).

mod cover;
mod ext;
mod index;
mod playlist;
mod rpc;
mod tags;

use std::sync::Arc;

use asteria_framework_server::cli::AppSpec;
use asteria_music_api::{APP, APP_API};

use ext::MusicExtension;

/// The music server, for `asteria_framework_server::cli::main`.
pub fn spec() -> AppSpec {
    AppSpec {
        app: APP,
        app_api: APP_API,
        about: "Asteria Music server",
        default_listen: "127.0.0.1:7811",
        extension: Arc::new(MusicExtension::new()),
    }
}
