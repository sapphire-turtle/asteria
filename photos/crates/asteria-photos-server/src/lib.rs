//! The Asteria Photos server as a library: [`spec`] is what `asteria-photos` (headless) and
//! `asteria-photos-desktop` hand to the shared command line (`init`, `token add`, `serve`).

use std::sync::Arc;

use asteria_framework::MediaKind;
use asteria_framework_server::AppExtension;
use asteria_framework_server::cli::AppSpec;
use asteria_photos_api::{APP, APP_API};

/// Photos shows images and videos.
struct PhotosExtension;

impl AppExtension for PhotosExtension {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Image | MediaKind::Video)
    }
}

/// The photos server, for `asteria_framework_server::cli::main`.
pub fn spec() -> AppSpec {
    AppSpec {
        app: APP,
        app_api: APP_API,
        about: "Asteria Photos server",
        default_listen: "127.0.0.1:7810",
        extension: Arc::new(PhotosExtension),
    }
}
