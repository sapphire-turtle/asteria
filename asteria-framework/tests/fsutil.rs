use asteria_framework::fsutil::{atomic_write, cleanup_tmp_files, tmp_path_for};
use std::fs;
use std::path::Path;

#[test]
fn atomic_write_replaces_content_and_leaves_no_tmp() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("京都旅行");
    fs::create_dir_all(&sub).unwrap();
    let p = sub.join("a.txt");
    atomic_write(&p, b"first").unwrap();
    atomic_write(&p, b"second").unwrap();
    assert_eq!(fs::read(&p).unwrap(), b"second");
    assert!(!tmp_path_for(&p).exists());
    let names: Vec<_> = fs::read_dir(&sub).unwrap().collect();
    assert_eq!(names.len(), 1);
}

#[test]
fn tmp_path_is_hidden_sibling() {
    assert_eq!(
        tmp_path_for(Path::new("x/京都/a.jpg.xmp")),
        Path::new("x/京都/.a.jpg.xmp.asteria-tmp")
    );
}

#[test]
fn atomic_write_into_missing_dir_fails_without_litter() {
    let dir = tempfile::tempdir().unwrap();
    assert!(atomic_write(&dir.path().join("nope/a.txt"), b"x").is_err());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn cleanup_removes_only_asteria_tmp_files() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("deep/er");
    fs::create_dir_all(&sub).unwrap();
    fs::write(dir.path().join(".a.jpg.asteria-tmp"), b"x").unwrap();
    fs::write(dir.path().join("a.jpg"), b"x").unwrap();
    fs::write(dir.path().join("b.asteria-tmp"), b"x").unwrap();
    fs::write(sub.join(".b.xmp.asteria-tmp"), b"x").unwrap();
    assert_eq!(cleanup_tmp_files(dir.path()).unwrap(), 2);
    assert!(dir.path().join("a.jpg").exists());
    assert!(dir.path().join("b.asteria-tmp").exists());
    assert!(!dir.path().join(".a.jpg.asteria-tmp").exists());
    assert!(!sub.join(".b.xmp.asteria-tmp").exists());
}
