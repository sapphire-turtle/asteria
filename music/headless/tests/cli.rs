//! The headless binary runs the server's command line.

use std::path::Path;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_asteria-music");

/// Runs the binary with its directories in `home`.
fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("APPDATA", home.join("roaming"))
        .env("LOCALAPPDATA", home.join("local"))
        .output()
        .unwrap()
}

#[test]
fn help_lists_the_server_commands() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_asteria-music"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    for command in ["init", "api-key", "serve"] {
        assert!(text.contains(command), "{command} missing from:\n{text}");
    }
}

/// With no `--config`, `init` writes the app's default config (corundum's `dirs` reads the
/// environment on unix only).
#[cfg(unix)]
#[test]
fn init_without_config_uses_the_default_place() {
    let home = tempfile::tempdir().unwrap();
    let library = home.path().join("library");
    let out = run(
        home.path(),
        &[
            "init",
            "--owner",
            "me",
            "--library",
            library.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "{out:?}");
    // corundum's `dirs`: `$XDG_CONFIG_HOME` on Linux, `~/Library/Application Support` on macOS.
    let base = if cfg!(target_os = "macos") {
        home.path().join("Library/Application Support")
    } else {
        home.path().join("config")
    };
    let config = base.join("asteria-music/server.toml");
    assert!(config.is_file(), "{config:?}");
}

#[test]
fn api_key_add_prints_only_the_token_and_list_shows_the_keys() {
    let home = tempfile::tempdir().unwrap();
    let library = home.path().join("library");
    let config = home.path().join("server.toml");
    let config = config.to_str().unwrap();
    let out = run(
        home.path(),
        &[
            "init",
            "--owner",
            "me",
            "--library",
            library.to_str().unwrap(),
            "--config",
            config,
        ],
    );
    assert!(out.status.success(), "{out:?}");
    let out = run(
        home.path(),
        &[
            "api-key",
            "add",
            "laptop",
            "--profile",
            "owner",
            "--config",
            config,
        ],
    );
    assert!(out.status.success(), "{out:?}");
    let token = String::from_utf8(out.stdout).unwrap();
    assert_eq!(token.lines().count(), 1, "{token:?}");
    assert!(token.starts_with("asteria-music-"), "{token:?}");
    let out = run(home.path(), &["api-key", "list", "--config", config]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    for name in ["owner", "laptop"] {
        assert!(text.contains(name), "{name} missing from:\n{text}");
    }
    assert!(!text.contains(token.trim()));
}
