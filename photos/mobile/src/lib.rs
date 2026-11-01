//! The Asteria Photos phone app. A client only: it depends on `asteria-photos-gui` and nothing
//! of the server's. The iOS and Android entry points (Plan 11) will start [`PhotosApp`] here.

pub use asteria_photos_gui::{APP_NAME, AppPaths, PhotosApp};
