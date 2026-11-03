//! The desktop as a cargo-packager package (spec §5.9): [`start`] is the start-up order every
//! desktop `main` runs, and the updater it makes for the window feeds the
//! [About page](crate::AboutPage).
//!
//! Outside a package (`cargo run`, a `cargo install`ed desktop) the package's start finds
//! nothing and no updater is made; the About page then shows the release it was installed
//! from.

use std::ffi::OsString;
use std::sync::OnceLock;
use std::time::Duration;

use corundum_framework::package::{PackageSpec, Source, Updater, Version};
use corundum_framework::platform::console;
use corundum_framework::platform::instance::{self, Instance};

use crate::LocalServerSpec;

/// The repository the desktops' packages are released in, as `v<version>` releases.
const REPO: &str = "https://github.com/sapphire-turtle/asteria";

/// sapphire-turtle's minisign public key (key ID F4A73F39AA17C8EB, `cargo packager signer
/// generate`), which the release workflow signs the packages for: an update signed otherwise
/// is not installed. The same key as sapphire-framework's `gui::package::UPDATE_PUBKEY`.
pub const UPDATE_PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEY0QTczRjM5QUExN0M4RUIKUldUcnlCZXFPVCtuOUJ6Zld0MkIzaW5wUXlsdlBBSll2U3k1dEJtT0VBYXV6ZHFtYlVNcWF5aHEK";

/// How often a running window looks for an update.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// The updater [`start_updates`] made: unset outside a package and on a command-line start.
static UPDATER: OnceLock<Updater> = OnceLock::new();

/// What a start of the desktop's executable is, from its arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// `serve`: the server, as the background service starts it.
    Serve,
    /// Any other arguments: the command line (or one of the installer's hooks).
    Cli,
    /// No arguments: the window.
    Window,
}

/// What the start with `args` (without the program's name) is: the first argument `serve`
/// is the server, any other arguments the command line, none the window.
pub fn entry(args: &[OsString]) -> Entry {
    match args.first() {
        Some(first) if first == "serve" => Entry::Serve,
        Some(_) => Entry::Cli,
        None => Entry::Window,
    }
}

/// Start the desktop in the order of spec §5.9, from the process's arguments. It returns
/// only for the window, with its single-instance lock, which the caller keeps while the
/// window is open; the server and the command line run here and exit.
///
/// - `serve` runs the server at once. The service manager's process never goes through the
///   package's start, which starts again the services an update stopped: started from the
///   service itself, it would restart itself.
/// - Anything else goes through [`corundum_framework::package::start`] first, which handles
///   the Windows installer's hooks (`--corundum-package installed|uninstalling`) and exits
///   for them, before the arguments are read as a command line, and starts again the
///   services an update stopped.
/// - Other arguments are the command line, given the parent's console on Windows when the
///   package's console shim did not.
/// - No arguments open the window: a second start brings the first to the front and exits,
///   and only the start that holds the lock applies a downloaded update and looks for more.
pub fn start(local: &'static LocalServerSpec, package: &'static PackageSpec) -> Instance {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let entry = entry(&args);
    if entry == Entry::Serve {
        // A no-op under the service manager; a `cargo install`ed Windows build run from a
        // terminal prints there.
        console::attach_parent();
        asteria_framework_server::cli::main((local.server)());
    }
    // Outside a package (`cargo run`, a `cargo install`ed desktop) it finds nothing to do
    // and says `NotPackaged`.
    let start = corundum_framework::package::start(package);
    tracing::debug!("package start: {start:?}");
    if entry == Entry::Cli {
        console::attach_parent();
        asteria_framework_server::cli::main((local.server)());
    }
    let Some(instance) = instance::acquire_or_activate(local.app) else {
        tracing::info!("{}: running already; brought it to the front", local.app);
        std::process::exit(0);
    };
    start_updates(package);
    instance
}

/// The updater [`start`] made for the window, if it made one.
pub fn updater() -> Option<Updater> {
    UPDATER.get().cloned()
}

/// For the window in a package: apply an update downloaded at an earlier run (which restarts
/// the desktop), else look for one now and every day. Outside a package nothing is made.
fn start_updates(package: &'static PackageSpec) {
    let updater = match Updater::new(package, source()) {
        Ok(updater) => updater,
        // Outside a package there is nothing to update: not worth a warning.
        Err(corundum_framework::package::Error::NotPackaged) => {
            tracing::debug!("not in a package: no updater");
            return;
        }
        Err(e) => {
            tracing::warn!("no updater: {e}");
            return;
        }
    };
    if let Err(e) = updater.apply_pending_at_start() {
        tracing::warn!("the downloaded update could not be applied: {e}");
    }
    updater.run_in_background(CHECK_EVERY);
    let _ = UPDATER.set(updater);
}

/// Where the updates are: the repository's releases, and its pre-releases too for a
/// pre-release build, so a release candidate moves on to the next one.
fn source() -> Source {
    Source::Github {
        repo: REPO.to_owned(),
        prerelease: is_prerelease(env!("CARGO_PKG_VERSION")),
    }
}

/// Whether `version` is a pre-release (`0.1.0-rc.1`).
fn is_prerelease(version: &str) -> bool {
    Version::parse(version).is_ok_and(|v| !v.pre.is_empty())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    fn args(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn serve_starts_the_server_before_anything_else() {
        assert!(matches!(entry(&args(&["serve"])), Entry::Serve));
        assert!(matches!(
            entry(&args(&["serve", "--listen", "0.0.0.0:7810"])),
            Entry::Serve
        ));
    }

    #[test]
    fn an_install_hook_is_not_the_server() {
        // package::start handles it before the CLI ever sees it
        assert!(matches!(
            entry(&args(&["--corundum-package", "installed"])),
            Entry::Cli
        ));
        assert!(matches!(entry(&args(&["api-key", "list"])), Entry::Cli));
    }

    #[test]
    fn no_arguments_open_the_window() {
        assert!(matches!(entry(&[]), Entry::Window));
    }

    #[test]
    fn a_pre_release_reads_pre_releases() {
        assert!(is_prerelease("0.1.0-rc.1"));
        assert!(!is_prerelease("0.1.0"));
    }
}
