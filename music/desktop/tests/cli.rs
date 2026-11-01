//! With arguments, the desktop binary is the server's command line, the same as headless; it
//! opens no window.

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_asteria-music-desktop");

/// Runs the binary with its directories in `home`, and gives up after 60 s: a window opened
/// by mistake would otherwise never return.
fn run(home: &Path, args: &[&str]) -> Output {
    let mut child = Command::new(BIN)
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("APPDATA", home.join("roaming"))
        .env("LOCALAPPDATA", home.join("local"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > end {
            let _ = child.kill();
            panic!("{args:?} did not return: did it open a window?");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn help_runs_the_server_cli() {
    let home = tempfile::tempdir().unwrap();
    let out = run(home.path(), &["--help"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    for command in ["init", "api-key", "serve"] {
        assert!(text.contains(command), "{command} missing from:\n{text}");
    }
}

#[test]
fn init_works_from_the_desktop_binary() {
    let home = tempfile::tempdir().unwrap();
    let library = home.path().join("library");
    std::fs::create_dir(&library).unwrap();
    let config = home.path().join("server.toml");
    let out = run(
        home.path(),
        &[
            "init",
            "--owner",
            "me",
            "--library",
            library.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(config.is_file());
    assert!(library.join(".asteria/library.toml").is_file());
    // The owner's token, and nothing else, goes to stdout.
    assert!(!String::from_utf8(out.stdout).unwrap().trim().is_empty());
}
