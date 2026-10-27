//! The Asteria Music server: `init`, `token add` and `serve`.

mod ext;
mod index;
mod playlist;
mod tags;

use std::sync::Arc;

use asteria_framework_server::cli::{self, AppSpec};
use asteria_music_api::{APP, APP_API};

use ext::MusicExtension;

fn main() {
    cli::main(AppSpec {
        app: APP,
        app_api: APP_API,
        about: "Asteria Music server",
        default_listen: "127.0.0.1:7811",
        extension: Arc::new(MusicExtension::new()),
    })
}
