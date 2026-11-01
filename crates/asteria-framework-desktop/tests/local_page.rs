//! The "Local server" settings page, read from a config in a temporary directory.

mod support;

use asteria_framework_client::settings::ExtraSettingsPage;
use asteria_framework_desktop::{LocalPaths, LocalServerPage, PageState, take_setup_request};
use asteria_framework_server::ServerConfig;
use corundum_framework::gui::first_run::FirstRun;
use support::{SPEC, click, context, frame, shows};

fn paths(dir: &tempfile::TempDir) -> LocalPaths {
    LocalPaths {
        config: dir.path().join("conf/server.toml"),
        keys: Some(dir.path().join("keys")),
    }
}

#[test]
fn without_a_config_it_is_not_set_up() {
    let dir = tempfile::tempdir().unwrap();
    let mut page = LocalServerPage::with_paths(&SPEC, paths(&dir));
    assert_eq!(page.state(), &PageState::NotSetUp);
    let ctx = context();
    assert_eq!(page.label(&ctx), "Local server");
    let output = frame(&ctx, Vec::new(), |ui| page.ui(ui));
    assert!(shows(&output, "No local server is set up yet."));
    assert!(shows(&output, "Set up a local server"));
    assert!(!dir.path().join("conf").exists());
}

#[test]
fn a_config_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let library = dir.path().join("lib");
    std::fs::create_dir(&library).unwrap();
    asteria_framework::Library::init(&library, "me").unwrap();
    let paths = paths(&dir);
    std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
    let listen = "127.0.0.1:7899".parse().unwrap();
    ServerConfig {
        library: library.clone(),
        listen,
    }
    .save(&paths.config)
    .unwrap();
    let page = LocalServerPage::with_paths(&SPEC, paths);
    assert_eq!(page.state(), &PageState::Ready { library, listen });
}

/// An older config's `[[token]]` entries are a server command's to import; the page shows
/// the library and the address all the same, and leaves the file alone.
#[test]
fn a_config_with_old_tokens_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let library = dir.path().join("lib");
    let paths = paths(&dir);
    std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
    let text = format!(
        "library = {:?}\nlisten = \"127.0.0.1:7899\"\n\n[[token]]\nuser = \"{}\"\nhash = \"blake3:{}\"\n",
        library.to_str().unwrap(),
        "0192f3a4-5b6c-7d8e-9f01-23456789abcd",
        "00".repeat(32)
    );
    std::fs::write(&paths.config, &text).unwrap();
    let config = paths.config.clone();
    let page = LocalServerPage::with_paths(&SPEC, paths);
    assert_eq!(
        page.state(),
        &PageState::Ready {
            library,
            listen: "127.0.0.1:7899".parse().unwrap()
        }
    );
    assert_eq!(std::fs::read_to_string(config).unwrap(), text);
}

#[test]
fn an_unreadable_config_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let paths = paths(&dir);
    std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
    std::fs::write(&paths.config, "not toml [").unwrap();
    let page = LocalServerPage::with_paths(&SPEC, paths);
    assert!(matches!(page.state(), PageState::Unreadable(_)));
}

#[test]
fn it_is_a_settings_page_for_the_apps() {
    let dir = tempfile::tempdir().unwrap();
    let mut page: Box<dyn ExtraSettingsPage> =
        Box::new(LocalServerPage::with_paths(&SPEC, paths(&dir)));
    let ctx = context();
    assert_eq!(page.label(&ctx), "Local server");
    let output = frame(&ctx, Vec::new(), |ui| page.ui(ui));
    assert!(shows(&output, "No local server is set up yet."));
}

#[test]
fn setting_up_asks_the_desktop_for_the_first_run_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut page = LocalServerPage::with_paths(&SPEC, paths(&dir));
    let ctx = context();
    assert!(!take_setup_request(&ctx));
    click(&ctx, "Set up a local server", |ui| page.ui(ui));
    assert!(take_setup_request(&ctx), "the click asks for the first run");
    assert!(!take_setup_request(&ctx), "the request is taken once");
    assert!(!FirstRun::done(&ctx));
}
