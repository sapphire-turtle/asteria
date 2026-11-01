//! The "Local server" settings page, read from a config in a temporary directory.

mod support;

use asteria_framework_desktop::{LocalPaths, LocalServerPage, PageState};
use asteria_framework_server::ServerConfig;
use support::{SPEC, context, frame, shows};

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

#[test]
fn an_unreadable_config_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let paths = paths(&dir);
    std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
    std::fs::write(&paths.config, "not toml [").unwrap();
    let page = LocalServerPage::with_paths(&SPEC, paths);
    assert!(matches!(page.state(), PageState::Unreadable(_)));
}
