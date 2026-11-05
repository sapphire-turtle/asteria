use std::path::PathBuf;

use asteria_common_server::ServerConfig;

fn config() -> ServerConfig {
    ServerConfig {
        library: PathBuf::from("/srv/lib"),
        listen: "127.0.0.1:7810".parse().unwrap(),
    }
}

#[test]
fn config_roundtrip_and_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.toml");
    let cfg = config();
    cfg.save(&path).unwrap();
    assert_eq!(ServerConfig::load(&path).unwrap(), cfg);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        // overwrite keeps 0600, even if it was loosened
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        cfg.save(&path).unwrap();
        assert_eq!(mode(&path), 0o600);
    }
}

#[test]
fn the_config_holds_only_the_library_and_the_address() {
    let text = toml::to_string_pretty(&config()).unwrap();
    let keys: Vec<&str> = text
        .lines()
        .filter_map(|l| l.split_once(" = ").map(|(k, _)| k))
        .collect();
    assert_eq!(keys, ["library", "listen"], "{text}");
}

/// `load` alone does not import an older config's tokens; it says what will.
#[test]
fn load_refuses_old_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.toml");
    let hash = format!("blake3:{}", "0".repeat(64));
    std::fs::write(
        &path,
        format!(
            "library = \"/srv/lib\"\nlisten = \"127.0.0.1:7810\"\n\n[[token]]\nuser = \"123abcd\"\nhash = \"{hash}\"\n"
        ),
    )
    .unwrap();
    let err = ServerConfig::load(&path).unwrap_err().to_string();
    assert!(err.contains("[[token]]"), "{err}");
    assert!(!err.contains(&hash), "{err}");
}

#[test]
fn cache_dir_is_per_app_and_library() {
    let id = asteria_common_core::LibraryId(uuid::Uuid::nil());
    let dir = asteria_common_server::cache_dir_for("asteria-photos", &id).unwrap();
    assert!(dir.ends_with("00000000-0000-0000-0000-000000000000"));
    // Inside the app's own cache: `<app>\cache` on Windows, where the cache root is the data
    // root, so the cache never shares a directory with the server's state.
    let app = corundum_framework::platform::dirs::AppDirs::new("asteria-photos").unwrap();
    assert_eq!(
        dir,
        app.cache().join("00000000-0000-0000-0000-000000000000")
    );
}

#[test]
fn data_dir_is_per_app_and_library_and_apart_from_the_cache() {
    let id = asteria_common_core::LibraryId(uuid::Uuid::nil());
    let dir = asteria_common_server::data_dir_for("asteria-photos", &id).unwrap();
    assert!(dir.ends_with("asteria-photos/00000000-0000-0000-0000-000000000000"));
    let cache = asteria_common_server::cache_dir_for("asteria-photos", &id).unwrap();
    assert_ne!(dir, cache);
}
