use asteria_framework_client::profiles::{Profiles, Token};

fn token(s: &str) -> Token {
    Token::new(s)
}

#[test]
fn roundtrip_and_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("servers.toml");
    let mut p = Profiles::default();
    let a = p
        .add("自宅", "http://192.168.1.10:7810", token("t-a"))
        .unwrap();
    let b = p
        .add("work", "https://example.com/x", token("t-b"))
        .unwrap();
    p.set_active(Some(b)).unwrap();
    p.save(&path).unwrap();
    let loaded = Profiles::load(&path).unwrap();
    assert_eq!(loaded, p);
    assert_eq!(loaded.active().unwrap().id, b);
    assert_eq!(loaded.servers.len(), 2);
    assert_eq!(loaded.servers[0].id, a);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // Saving again over an existing file keeps 0o600 and leaves no temp file.
    p.save(&path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn missing_file_is_empty_and_dangling_active_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("servers.toml");
    assert_eq!(Profiles::load(&path).unwrap(), Profiles::default());
    std::fs::write(&path, "active = \"zzzzzzz\"\n").unwrap();
    assert!(Profiles::load(&path).unwrap().active().is_none());
}

#[test]
fn removing_active_clears_it() {
    let mut p = Profiles::default();
    let a = p.add("a", "http://h:1", token("x")).unwrap();
    let b = p.add("b", "http://h:2", token("y")).unwrap();
    p.set_active(Some(a)).unwrap();
    p.remove(b);
    assert_eq!(p.active().unwrap().id, a);
    p.remove(a);
    assert!(p.active().is_none());
    assert!(p.set_active(Some(a)).is_err());
}

#[test]
fn unreadable_file_is_set_aside() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("servers.toml");
    std::fs::write(&path, [0xff, 0xfe, 0x00, 0x81]).unwrap();
    let p = Profiles::load(&path).unwrap();
    assert!(p.servers.is_empty() && p.active().is_none());
    assert!(!path.exists());
    let bad: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("servers.toml.bad-"))
        .collect();
    assert_eq!(bad.len(), 1);
    assert_eq!(
        std::fs::read(dir.path().join(&bad[0])).unwrap(),
        [0xff, 0xfe, 0x00, 0x81]
    );
}

#[test]
fn token_is_not_in_debug() {
    let mut p = Profiles::default();
    let id = p.add("a", "http://h:1", token("sekrit-value")).unwrap();
    let profile = p.servers.iter().find(|s| s.id == id).unwrap();
    assert!(!format!("{profile:?}").contains("sekrit-value"));
    assert!(!format!("{p:?}").contains("sekrit-value"));
    assert!(!format!("{profile:#?}").contains("sekrit-value"));
    assert_eq!(format!("{:?}", token("sekrit-value")), "Token(***)");
}

#[test]
fn url_must_be_http_or_https() {
    let mut p = Profiles::default();
    for bad in ["ftp://h/x", "/relative", "h:7810", "file:///etc/passwd", ""] {
        assert!(p.add("a", bad, token("t")).is_err(), "{bad}");
    }
    assert!(p.servers.is_empty());
    let id = p.add("a", "https://h", token("t")).unwrap();
    assert!(p.update(id, "a", "ftp://h", token("t")).is_err());
    assert_eq!(p.servers[0].url.as_str(), "https://h/");
}
