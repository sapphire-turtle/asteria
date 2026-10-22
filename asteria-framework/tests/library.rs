use std::path::{Path, PathBuf};

use asteria_framework::paths::{is_sidecar, media_path_for_sidecar, sidecar_path_for};
use asteria_framework::{Error, Library};

#[test]
fn init_then_open_roundtrips_config() {
    let dir = tempfile::tempdir().unwrap();
    let created = Library::init(dir.path(), "fluo10").unwrap();
    let opened = Library::open(dir.path()).unwrap();
    assert_eq!(created.config().id, opened.config().id);
    assert_eq!(created.owner(), opened.owner());
    assert_eq!(opened.config().format, 1);
    let owner = opened
        .users()
        .user
        .iter()
        .find(|u| u.id == opened.owner())
        .unwrap();
    assert_eq!(owner.name, "fluo10");
    assert!(dir.path().join(".asteria/library.toml").is_file());
    assert!(dir.path().join(".asteria/users.toml").is_file());
    assert_eq!(opened.root(), dir.path());
}

#[test]
fn open_non_library_fails() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        Library::open(dir.path()),
        Err(Error::NotALibrary(_))
    ));
}

#[test]
fn init_twice_fails() {
    let dir = tempfile::tempdir().unwrap();
    Library::init(dir.path(), "a").unwrap();
    assert!(matches!(
        Library::init(dir.path(), "a"),
        Err(Error::AlreadyInitialized(_))
    ));
}

#[test]
fn unsupported_format_fails() {
    let dir = tempfile::tempdir().unwrap();
    Library::init(dir.path(), "a").unwrap();
    let p = dir.path().join(".asteria/library.toml");
    let s = std::fs::read_to_string(&p).unwrap();
    std::fs::write(&p, s.replace("format = 1", "format = 2")).unwrap();
    assert!(matches!(
        Library::open(dir.path()),
        Err(Error::UnsupportedFormat(2))
    ));
}

#[test]
fn sidecar_path_keeps_extension_and_japanese() {
    assert_eq!(
        sidecar_path_for(Path::new("2025/京都旅行/IMG_0001.HEIC")),
        PathBuf::from("2025/京都旅行/IMG_0001.HEIC.xmp")
    );
    assert_eq!(
        media_path_for_sidecar(Path::new("2025/京都旅行/IMG_0001.HEIC.xmp")),
        Some(PathBuf::from("2025/京都旅行/IMG_0001.HEIC"))
    );
    assert!(is_sidecar(Path::new("a.jpg.xmp")) && !is_sidecar(Path::new("a.jpg")));
}

#[test]
fn failed_init_can_be_retried() {
    let dir = tempfile::tempdir().unwrap();
    // A previous init died after creating the directory but before writing library.toml.
    std::fs::create_dir_all(dir.path().join(".asteria")).unwrap();
    std::fs::write(dir.path().join(".asteria/users.toml"), "stale").unwrap();
    let lib = Library::init(dir.path(), "a").unwrap();
    assert_eq!(Library::open(dir.path()).unwrap().owner(), lib.owner());
}

#[test]
fn missing_users_file_names_the_path() {
    let dir = tempfile::tempdir().unwrap();
    Library::init(dir.path(), "a").unwrap();
    std::fs::remove_file(dir.path().join(".asteria/users.toml")).unwrap();
    match Library::open(dir.path()) {
        Err(Error::MissingFile(p)) => assert!(p.ends_with(".asteria/users.toml")),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn users_file_without_users_is_empty_list() {
    let dir = tempfile::tempdir().unwrap();
    Library::init(dir.path(), "a").unwrap();
    std::fs::write(dir.path().join(".asteria/users.toml"), "").unwrap();
    assert!(Library::open(dir.path()).unwrap().users().user.is_empty());
}
