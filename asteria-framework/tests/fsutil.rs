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

#[test]
fn existing_tmp_is_an_error_and_left_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.txt");
    fs::write(&p, b"orig").unwrap();
    let tmp = tmp_path_for(&p);
    fs::write(&tmp, b"theirs").unwrap();
    let err = atomic_write(&p, b"new").unwrap_err();
    assert!(matches!(&err, asteria_framework::Error::TmpExists(t) if *t == tmp));
    assert_eq!(fs::read(&p).unwrap(), b"orig");
    assert_eq!(fs::read(&tmp).unwrap(), b"theirs");
}

#[cfg(unix)]
#[test]
fn permissions_are_kept() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.txt");
    fs::write(&p, b"orig").unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();
    atomic_write(&p, b"new").unwrap();
    assert_eq!(
        fs::metadata(&p).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[cfg(unix)]
#[test]
fn symlink_survives_and_target_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    fs::create_dir(&real).unwrap();
    let target = real.join("a.txt");
    fs::write(&target, b"orig").unwrap();
    let link = dir.path().join("link.txt");
    std::os::unix::fs::symlink("real/a.txt", &link).unwrap();
    atomic_write(&link, b"new").unwrap();
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&target).unwrap(), b"new");
    assert!(!tmp_path_for(&target).exists() && !tmp_path_for(&link).exists());
}
