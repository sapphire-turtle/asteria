use std::path::PathBuf;

use asteria_framework::UserId;
use asteria_framework_server::{ServerConfig, TokenEntry, Tokens, new_token, token_hash};

fn user() -> UserId {
    "123abcd".parse().unwrap()
}

fn config_with(hash: String) -> ServerConfig {
    ServerConfig {
        library: PathBuf::from("/srv/lib"),
        listen: "127.0.0.1:7810".parse().unwrap(),
        token: vec![TokenEntry { user: user(), hash }],
    }
}

#[test]
fn config_roundtrip_and_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.toml");
    let cfg = config_with(token_hash("abc"));
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
fn token_verifies_only_matching() {
    let t = new_token();
    let cfg = config_with(token_hash(&t));
    let tokens = Tokens::from_config(&cfg);
    assert_eq!(tokens.verify(&t), Some(user()));
    assert_eq!(tokens.verify("x"), None);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.toml");
    cfg.save(&path).unwrap();
    assert!(!std::fs::read_to_string(&path).unwrap().contains(&t));
    assert!(token_hash(&t).starts_with("blake3:"));
}

#[test]
fn tokens_are_distinct_and_long() {
    let (a, b) = (new_token(), new_token());
    assert_ne!(a, b);
    assert_eq!(a.len(), 43);
}

#[test]
fn cache_dir_is_per_app_and_library() {
    let id = asteria_framework::LibraryId(uuid::Uuid::nil());
    let dir = asteria_framework_server::cache_dir_for("asteria-gallery", &id).unwrap();
    assert!(dir.ends_with("asteria-gallery/00000000-0000-0000-0000-000000000000"));
}
