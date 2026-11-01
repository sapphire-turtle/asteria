//! The desktop's first-run pages (spec §5.8): where the library is; setting up a server for a
//! folder on this computer; keeping it running in the background; the external tools.
//!
//! corundum's flow has no way for a page to finish it early, so the answer to the first
//! page is shared with the rest: unless it is "This computer", each later page moves on as
//! soon as it is drawn (with an empty title), and the flow finishes without showing them.

use std::cell::Cell;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;

use asteria_framework_client::profiles::{Profiles, Token};
use asteria_framework_server::setup_local_at;
use corundum_framework::egui;
use corundum_framework::gui::first_run::{FirstRunPage, PageAction, ServicePage};
use corundum_framework::gui::{platform, store, tr};

use crate::{LocalPaths, LocalServerSpec, tools_ui};

/// The answer to "Where is your library?".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Choice {
    Unanswered,
    /// In a folder here, served by a local server.
    ThisComputer,
    /// On a server elsewhere: the app's own servers page registers it.
    Other,
    Later,
}

type Shared = Rc<Cell<Choice>>;

/// Asks the person for a folder, titled with the argument.
type Picker = Box<dyn FnMut(&str) -> Option<PathBuf>>;

/// The first-run pages for `spec`'s app, in order: where the library is, setting up the
/// local server, the background service, the external tools. A server set up here is added
/// to `servers` (the app's `servers.toml`) as "This computer" and chosen. Choosing another
/// server or "Later" on the first page finishes the flow there.
///
/// The server's config is at the default path, and its keys in the data directory.
pub fn first_run_pages(
    spec: &'static LocalServerSpec,
    servers: PathBuf,
) -> Vec<Box<dyn FirstRunPage>> {
    let paths = LocalPaths::default_for(spec.app).map_err(|e| e.to_string());
    pages(spec, servers, paths)
}

/// [`first_run_pages`] with the config and the keys at `paths`.
pub fn first_run_pages_with(
    spec: &'static LocalServerSpec,
    servers: PathBuf,
    paths: LocalPaths,
) -> Vec<Box<dyn FirstRunPage>> {
    pages(spec, servers, Ok(paths))
}

fn pages(
    spec: &'static LocalServerSpec,
    servers: PathBuf,
    paths: Result<LocalPaths, String>,
) -> Vec<Box<dyn FirstRunPage>> {
    let choice: Shared = Rc::new(Cell::new(Choice::Unanswered));
    let only_here = |page: Box<dyn FirstRunPage>| -> Box<dyn FirstRunPage> {
        Box::new(OnlyHere {
            page,
            choice: choice.clone(),
        })
    };
    // The service page's words are fixed when it is made, before any context exists:
    // they are looked up when it is first drawn.
    let service = Box::new(Service { spec, page: None });
    vec![
        Box::new(WherePage {
            choice: choice.clone(),
        }),
        only_here(Box::new(SetupPage::new(spec, servers, paths))),
        only_here(service),
        only_here(Box::new(ToolsPage { spec })),
    ]
}

/// "Where is your library?": a button for each answer, which moves on.
struct WherePage {
    choice: Shared,
}

impl FirstRunPage for WherePage {
    fn title(&self, ctx: &egui::Context) -> String {
        tr!(ctx, "asteria-desktop-where-title")
    }

    fn ui(&mut self, ui: &mut egui::Ui) -> PageAction {
        let mut action = PageAction::Stay;
        let answers = [
            (
                Choice::ThisComputer,
                "asteria-desktop-this-computer",
                "asteria-desktop-where-this-computer-note",
            ),
            (
                Choice::Other,
                "asteria-desktop-where-other",
                "asteria-desktop-where-other-note",
            ),
            (
                Choice::Later,
                "asteria-desktop-where-later",
                "asteria-desktop-where-later-note",
            ),
        ];
        for (choice, button, note) in answers {
            let chosen = self.choice.get() == choice;
            if ui
                .add(egui::Button::new(tr!(ui, button)).selected(chosen))
                .clicked()
            {
                self.choice.set(choice);
                action = PageAction::Next;
            }
            ui.label(egui::RichText::new(tr!(ui, note)).weak());
            ui.add_space(12.0);
        }
        action
    }

    fn can_continue(&self) -> bool {
        self.choice.get() != Choice::Unanswered
    }
}

/// A page shown only when the library is on this computer; otherwise it moves on at once.
struct OnlyHere {
    page: Box<dyn FirstRunPage>,
    choice: Shared,
}

impl OnlyHere {
    fn skipped(&self) -> bool {
        self.choice.get() != Choice::ThisComputer
    }
}

impl FirstRunPage for OnlyHere {
    fn title(&self, ctx: &egui::Context) -> String {
        if self.skipped() {
            String::new()
        } else {
            self.page.title(ctx)
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui) -> PageAction {
        if self.skipped() {
            ui.ctx().request_repaint();
            return PageAction::Next;
        }
        self.page.ui(ui)
    }

    fn can_continue(&self) -> bool {
        self.skipped() || self.page.can_continue()
    }
}

/// corundum's [`ServicePage`] for the app's service, made when first drawn, with the words
/// in the language then.
struct Service {
    spec: &'static LocalServerSpec,
    page: Option<ServicePage>,
}

impl Service {
    fn page(&mut self, ctx: &egui::Context) -> &mut ServicePage {
        self.page.get_or_insert_with(|| {
            ServicePage::new(
                vec![self.spec.service],
                tr!(ctx, "asteria-desktop-service-question"),
                tr!(ctx, "asteria-desktop-service-reason"),
            )
        })
    }
}

impl FirstRunPage for Service {
    fn title(&self, ctx: &egui::Context) -> String {
        match &self.page {
            Some(page) => page.title(ctx),
            None => tr!(ctx, "asteria-desktop-service-question"),
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui) -> PageAction {
        let ctx = ui.ctx().clone();
        self.page(&ctx).ui(ui)
    }

    fn can_continue(&self) -> bool {
        self.page.as_ref().is_some_and(|p| p.can_continue())
    }
}

/// The external tools; and, when the service was declined, that the server is not running
/// and how to start it.
struct ToolsPage {
    spec: &'static LocalServerSpec,
}

impl FirstRunPage for ToolsPage {
    fn title(&self, ctx: &egui::Context) -> String {
        tr!(ctx, "asteria-desktop-tools-title")
    }

    fn ui(&mut self, ui: &mut egui::Ui) -> PageAction {
        if store::state(ui.ctx()).services_asked == Some(false) {
            not_running(ui, self.spec.app);
            ui.add_space(12.0);
        }
        tools_ui(ui);
        PageAction::Stay
    }
}

/// That the server is not running, and the command that starts it.
fn not_running(ui: &mut egui::Ui, app: &str) {
    ui.colored_label(
        ui.visuals().warn_fg_color,
        tr!(ui, "asteria-desktop-not-running"),
    );
    ui.code(format!("{app} serve"));
}

/// The OS user's name, the library's owner: `USER` (`USERNAME` on Windows), else "me".
fn owner() -> String {
    ["USER", "USERNAME"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .map(|name| name.trim().to_owned())
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| "me".to_owned())
}

/// Where the setup page is.
enum Setup {
    /// No folder chosen yet.
    Idle,
    /// Setting up `library` on a thread.
    Running {
        library: PathBuf,
        /// The address the app reaches the server at, or why it could not be set up.
        job: mpsc::Receiver<Result<String, String>>,
    },
    /// `library` is served, and the app connects to it at `url`.
    Done { library: PathBuf, url: String },
    /// Setting up `library` failed, for the reason given.
    Failed { library: PathBuf, error: String },
}

/// Asks for a folder, sets the local server up for it, and registers it as "This computer".
struct SetupPage {
    spec: &'static LocalServerSpec,
    servers: PathBuf,
    paths: Result<LocalPaths, String>,
    /// Asks the person for a folder: corundum's dialog, or a stand-in in the tests.
    pick: Picker,
    setup: Setup,
}

impl SetupPage {
    fn new(
        spec: &'static LocalServerSpec,
        servers: PathBuf,
        paths: Result<LocalPaths, String>,
    ) -> SetupPage {
        SetupPage {
            spec,
            servers,
            paths,
            pick: Box::new(platform::pick_folder),
            setup: Setup::Idle,
        }
    }

    /// Set up `library` on a thread.
    fn start(&mut self, ctx: &egui::Context, library: PathBuf) {
        let paths = match &self.paths {
            Ok(paths) => paths.clone(),
            Err(error) => {
                self.setup = Setup::Failed {
                    library,
                    error: error.clone(),
                };
                return;
            }
        };
        let (send, job) = mpsc::channel();
        let spec = self.spec;
        let servers = self.servers.clone();
        let name = tr!(ctx, "asteria-desktop-this-computer");
        let owner = owner();
        let folder = library.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("local server setup".into())
            .spawn(move || {
                let done = set_up(spec, &paths, &servers, &folder, &owner, &name);
                let _ = send.send(done);
                ctx.request_repaint();
            })
            .expect("a thread to set up the server");
        self.setup = Setup::Running { library, job };
    }

    /// Ask for a folder and set it up.
    fn choose(&mut self, ctx: &egui::Context) {
        let title = tr!(ctx, "asteria-desktop-setup-picker-title");
        if let Some(library) = (self.pick)(&title) {
            self.start(ctx, library);
        }
    }
}

/// Where a client on this computer reaches a server listening on `listen`: an unspecified
/// address (`0.0.0.0`, `[::]`) through the loopback of its family, any other as it is.
fn local_url(listen: SocketAddr) -> String {
    let ip = match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    // `SocketAddr` puts an IPv6 address in brackets.
    format!("http://{}", SocketAddr::new(ip, listen.port()))
}

/// Sets up the server for `library` and makes it the app's server: a profile named `name`
/// at the address the server listens on (an existing config's, else the app's default) with
/// a new key's token, chosen. A profile at that address already (from an earlier first run)
/// is given the new token rather than added again. The address, when done.
fn set_up(
    spec: &LocalServerSpec,
    paths: &LocalPaths,
    servers: &Path,
    library: &Path,
    owner: &str,
    name: &str,
) -> Result<String, String> {
    let server = (spec.server)();
    let local = setup_local_at(&server, &paths.config, library, owner, "desktop", |id| {
        paths.open_keys(spec.app, id)
    })
    .map_err(|e| e.to_string())?;
    let mut profiles = Profiles::load(servers).map_err(|e| e.to_string())?;
    let token = Token::new(local.token);
    let address = local_url(local.listen);
    let url = asteria_framework_client::profiles::parse_url(&address).map_err(|e| e.to_string())?;
    let id = match profiles.servers.iter().find(|s| s.url == url).map(|s| s.id) {
        Some(id) => {
            profiles
                .update(id, name, &address, token)
                .map_err(|e| e.to_string())?;
            id
        }
        None => profiles
            .add(name, &address, token)
            .map_err(|e| e.to_string())?,
    };
    profiles.set_active(Some(id)).map_err(|e| e.to_string())?;
    if let Some(dir) = servers.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    profiles.save(servers).map_err(|e| e.to_string())?;
    Ok(address)
}

impl FirstRunPage for SetupPage {
    fn title(&self, ctx: &egui::Context) -> String {
        tr!(ctx, "asteria-desktop-setup-title")
    }

    fn ui(&mut self, ui: &mut egui::Ui) -> PageAction {
        if let Setup::Running { library, job } = &self.setup {
            let answer = match job.try_recv() {
                Ok(answer) => Some(answer),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err(tr!(ui, "asteria-desktop-setup-stopped")))
                }
            };
            if let Some(answer) = answer {
                let library = library.clone();
                self.setup = match answer {
                    Ok(url) => Setup::Done { library, url },
                    Err(error) => Setup::Failed { library, error },
                };
            }
        }
        ui.label(tr!(ui, "asteria-desktop-setup-intro"));
        ui.add_space(12.0);
        let ctx = ui.ctx().clone();
        match &self.setup {
            Setup::Idle => {
                if ui.button(tr!(ui, "asteria-desktop-setup-choose")).clicked() {
                    self.choose(&ctx);
                }
            }
            Setup::Running { library, .. } => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(tr!(
                        ui,
                        "asteria-desktop-setup-running",
                        path = library.display().to_string()
                    ));
                });
            }
            Setup::Done { library, url } => {
                ui.label(tr!(
                    ui,
                    "asteria-desktop-setup-done",
                    path = library.display().to_string(),
                    url = url.as_str()
                ));
            }
            Setup::Failed { library, error } => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    tr!(ui, "asteria-desktop-setup-failed", error = error.as_str()),
                );
                ui.add_space(8.0);
                let library = library.clone();
                let (mut again, mut other) = (false, false);
                ui.horizontal(|ui| {
                    again = ui
                        .button(tr!(ui, "asteria-desktop-setup-try-again"))
                        .clicked();
                    other = ui
                        .button(tr!(ui, "asteria-desktop-setup-other-folder"))
                        .clicked();
                });
                if again {
                    self.start(&ctx, library);
                } else if other {
                    self.choose(&ctx);
                }
            }
        }
        PageAction::Stay
    }

    fn can_continue(&self) -> bool {
        matches!(self.setup, Setup::Done { .. })
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use asteria_framework_server::ServerConfig;
    use asteria_framework_server::keys::KeyStore;

    use super::*;
    use crate::support::{self, SPEC, click, context, frame, shows};

    struct Fixture {
        dir: tempfile::TempDir,
        page: SetupPage,
        ctx: egui::Context,
    }

    impl Fixture {
        /// The setup page over a temporary directory, picking `lib` there.
        fn new() -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            let paths = LocalPaths {
                config: dir.path().join("conf/server.toml"),
                keys: Some(dir.path().join("keys")),
            };
            let mut page = SetupPage::new(&SPEC, dir.path().join("app/servers.toml"), Ok(paths));
            let library = dir.path().join("lib");
            page.pick = Box::new(move |_| Some(library.clone()));
            Fixture {
                dir,
                page,
                ctx: context(),
            }
        }

        fn servers(&self) -> PathBuf {
            self.dir.path().join("app/servers.toml")
        }

        fn click(&mut self, label: &str) {
            let page = &mut self.page;
            click(&self.ctx, label, |ui| {
                page.ui(ui);
            });
        }

        /// Draws the page until the setup is no longer running; what the last frame showed.
        fn settle(&mut self) -> egui::FullOutput {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let page = &mut self.page;
                let output = frame(&self.ctx, Vec::new(), |ui| {
                    page.ui(ui);
                });
                if !matches!(self.page.setup, Setup::Running { .. }) {
                    return output;
                }
                assert!(Instant::now() < deadline, "the setup never finished");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    #[test]
    fn a_folder_is_set_up_and_chosen_as_this_computer() {
        let mut f = Fixture::new();
        assert!(!f.page.can_continue());
        f.click("Choose a folder…");
        let output = f.settle();
        assert!(
            matches!(f.page.setup, Setup::Done { .. }),
            "{:?}",
            support::labels(&output)
        );
        assert!(f.page.can_continue());
        assert!(shows(&output, "is ready"), "{:?}", support::labels(&output));

        let lib = f.dir.path().join("lib");
        assert!(lib.join(".asteria/library.toml").is_file());
        let cfg = ServerConfig::load(&f.dir.path().join("conf/server.toml")).unwrap();
        assert_eq!(cfg.library, lib.canonicalize().unwrap());

        let profiles = Profiles::load(&f.servers()).unwrap();
        let active = profiles.active().expect("a server in use");
        assert_eq!(profiles.servers.len(), 1);
        assert_eq!(active.name, "This computer");
        assert_eq!(active.url.as_str(), "http://127.0.0.1:7899/");
        let keys = KeyStore::open(&f.dir.path().join("keys")).unwrap();
        let key = keys
            .authenticate(active.token.expose())
            .expect("the token is a key's");
        assert_eq!(key.name, "desktop");
    }

    #[test]
    fn a_second_setup_gives_this_computer_a_new_token() {
        let mut first = Fixture::new();
        first.click("Choose a folder…");
        first.settle();
        let before = Profiles::load(&first.servers()).unwrap();

        let mut again = SetupPage::new(&SPEC, first.servers(), first.page.paths.clone());
        let library = first.dir.path().join("lib");
        again.pick = Box::new(move |_| Some(library.clone()));
        first.page = again;
        first.click("Choose a folder…");
        first.settle();

        let after = Profiles::load(&first.servers()).unwrap();
        assert_eq!(after.servers.len(), 1);
        assert_eq!(after.servers[0].id, before.servers[0].id);
        assert_ne!(after.servers[0].token, before.servers[0].token);
        let keys = KeyStore::open(&first.dir.path().join("keys")).unwrap();
        let key = keys.authenticate(after.servers[0].token.expose()).unwrap();
        assert_eq!(key.name, "desktop-2");
    }

    #[test]
    fn a_failure_says_why_and_can_be_tried_again() {
        let mut f = Fixture::new();
        // A config for another library is there already.
        let other = f.dir.path().join("other");
        std::fs::create_dir(&other).unwrap();
        asteria_framework::Library::init(&other, "me").unwrap();
        std::fs::create_dir_all(f.dir.path().join("conf")).unwrap();
        ServerConfig {
            library: other,
            listen: "127.0.0.1:7899".parse().unwrap(),
        }
        .save(&f.dir.path().join("conf/server.toml"))
        .unwrap();

        f.click("Choose a folder…");
        let output = f.settle();
        assert!(matches!(f.page.setup, Setup::Failed { .. }));
        assert!(!f.page.can_continue());
        assert!(shows(&output, "is already set up in"));
        assert!(!f.servers().exists());

        std::fs::remove_file(f.dir.path().join("conf/server.toml")).unwrap();
        f.click("Try again");
        f.settle();
        assert!(matches!(f.page.setup, Setup::Done { .. }));
        assert!(f.servers().exists());
    }

    #[test]
    fn a_config_on_another_port_is_registered_at_that_port() {
        let mut f = Fixture::new();
        let lib = f.dir.path().join("lib");
        std::fs::create_dir(&lib).unwrap();
        asteria_framework::Library::init(&lib, "me").unwrap();
        std::fs::create_dir_all(f.dir.path().join("conf")).unwrap();
        ServerConfig {
            library: lib.canonicalize().unwrap(),
            listen: "0.0.0.0:7123".parse().unwrap(),
        }
        .save(&f.dir.path().join("conf/server.toml"))
        .unwrap();

        f.click("Choose a folder…");
        let output = f.settle();
        assert!(matches!(f.page.setup, Setup::Done { .. }));
        assert!(shows(&output, "http://127.0.0.1:7123"));
        let profiles = Profiles::load(&f.servers()).unwrap();
        assert_eq!(profiles.servers.len(), 1);
        assert_eq!(
            profiles.active().unwrap().url.as_str(),
            "http://127.0.0.1:7123/"
        );
    }

    #[test]
    fn the_local_url_reaches_the_listen_address() {
        let url = |listen: &str| local_url(listen.parse().unwrap());
        assert_eq!(url("0.0.0.0:7810"), "http://127.0.0.1:7810");
        assert_eq!(url("[::]:7810"), "http://[::1]:7810");
        assert_eq!(url("192.168.1.5:7811"), "http://192.168.1.5:7811");
        assert_eq!(url("[fe80::1]:7811"), "http://[fe80::1]:7811");
        assert_eq!(url("127.0.0.1:7810"), "http://127.0.0.1:7810");
    }

    #[test]
    fn the_owner_is_never_empty() {
        assert!(!owner().is_empty());
    }
}
