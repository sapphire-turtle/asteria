//! What the photos and music desktop apps share about the server on this computer (spec
//! §5.8): the [first-run pages](first_run_pages) that set it up, the "Local server"
//! [settings page](LocalServerPage), its [background service](service_spec), and
//! [what external tools it has](tools_ui); and as a Velopack package (spec §5.9), the
//! [start-up order](start) and the [About page](AboutPage).
//!
//! Each desktop keeps one [`LocalServerSpec`] in a `static` and hands it to these. The
//! server is always the resident `asteria-<app> serve`; the window only connects to it.

mod about;
mod first_run;
mod i18n;
mod local_page;
mod package;
mod paths;
mod service;
mod tools;

pub use about::AboutPage;
pub use first_run::{first_run_pages, first_run_pages_with};
pub use i18n::{RESOURCES, register};
pub use local_page::{LocalServerPage, PageState, request_setup, take_setup_request};
pub use package::{Entry, entry, start, updater};
pub use paths::LocalPaths;
pub use service::service_spec;
pub use tools::tools_ui;

use asteria_framework_server::cli::AppSpec;
use corundum_framework::service::ServiceSpec;

// The integration tests' helpers, which name the crate from outside.
#[cfg(test)]
extern crate self as asteria_framework_desktop;
#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod support;

/// A desktop app's local server: what the first run and the settings page need to know.
pub struct LocalServerSpec {
    /// The app's name (`asteria-photos`): names its config, data and service.
    pub app: &'static str,
    /// The server's spec, as its command line has it.
    pub server: fn() -> AppSpec,
    /// The background service that runs `serve`.
    pub service: fn() -> ServiceSpec,
    /// Where the window reaches the server on this computer with the default listen address
    /// (`http://127.0.0.1:7810`). The first run registers the address the server actually
    /// listens on, which is this unless an existing config says otherwise.
    pub url: &'static str,
}
