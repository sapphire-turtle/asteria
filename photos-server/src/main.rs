//! The Asteria Photos server: `init`, `token add` and `serve`.

use std::sync::Arc;

use asteria_framework::MediaKind;
use asteria_framework_server::AppExtension;
use asteria_framework_server::cli::{self, AppSpec};
use asteria_photos_api::{APP, APP_API};

/// Photos shows images and videos.
struct PhotosExtension;

impl AppExtension for PhotosExtension {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Image | MediaKind::Video)
    }
}

fn main() {
    cli::main(AppSpec {
        app: APP,
        app_api: APP_API,
        about: "Asteria Photos server",
        default_listen: "127.0.0.1:7810",
        extension: Arc::new(PhotosExtension),
    })
}
