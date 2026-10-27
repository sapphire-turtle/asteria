use asteria_framework::fsutil::{atomic_write, cleanup_tmp_files, tmp_path_for};
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Backdates `path`'s modification time by `hours`.
fn age(path: &Path, hours: u64) {
    let f = fs::File::options().write(true).open(path).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(hours * 3600))
        .unwrap();
}

/// Writes a temp file that a run long gone left behind.
fn stale(path: &Path) {
    fs::write(path, b"x").unwrap();
    age(path, 25);
}

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
fn tmp_path_is_hidden_sibling_with_this_process_tag() {
    let tmp = tmp_path_for(Path::new("x/京都/a.jpg.xmp"));
    assert_eq!(tmp.parent(), Some(Path::new("x/京都")));
    let name = tmp.file_name().unwrap().to_str().unwrap();
    let tag = name
        .strip_prefix(".a.jpg.xmp.")
        .and_then(|r| r.strip_suffix(".asteria-tmp"))
        .unwrap_or_else(|| panic!("{name}"));
    assert!(!tag.is_empty() && !tag.contains('.'), "{name}");
    assert_eq!(tmp_path_for(Path::new("x/京都/a.jpg.xmp")), tmp);
}

#[test]
fn leftover_tmp_of_another_run_does_not_block_a_write() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.txt");
    fs::write(&p, b"orig").unwrap();
    let old_form = dir.path().join(".a.txt.asteria-tmp");
    let other_tag = dir.path().join(".a.txt.0123abcd.asteria-tmp");
    fs::write(&old_form, b"crash").unwrap();
    fs::write(&other_tag, b"crash").unwrap();
    atomic_write(&p, b"new").unwrap();
    assert_eq!(fs::read(&p).unwrap(), b"new");
    assert_eq!(fs::read(&old_form).unwrap(), b"crash");
    assert_eq!(fs::read(&other_tag).unwrap(), b"crash");
}

#[test]
fn atomic_write_into_missing_dir_fails_without_litter() {
    let dir = tempfile::tempdir().unwrap();
    assert!(atomic_write(&dir.path().join("nope/a.txt"), b"x").is_err());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn cleanup_removes_only_stale_asteria_tmp_files() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("deep/er");
    fs::create_dir_all(&sub).unwrap();
    stale(&dir.path().join(".a.jpg.asteria-tmp"));
    stale(&dir.path().join("a.jpg"));
    stale(&dir.path().join("b.asteria-tmp"));
    stale(&sub.join(".b.xmp.asteria-tmp"));
    stale(&sub.join(".c.xmp.5f3e9a01.asteria-tmp"));
    assert_eq!(cleanup_tmp_files(dir.path()).unwrap(), 3);
    assert!(dir.path().join("a.jpg").exists());
    assert!(dir.path().join("b.asteria-tmp").exists());
    assert!(!dir.path().join(".a.jpg.asteria-tmp").exists());
    assert!(!sub.join(".b.xmp.asteria-tmp").exists());
    assert!(!sub.join(".c.xmp.5f3e9a01.asteria-tmp").exists());
}

/// Another instance sharing the workspace may be writing it right now.
#[test]
fn cleanup_keeps_a_fresh_tmp_file() {
    let dir = tempfile::tempdir().unwrap();
    let fresh = dir.path().join(".a.jpg.xmp.5f3e9a01.asteria-tmp");
    fs::write(&fresh, b"x").unwrap();
    let day_old = dir.path().join(".b.jpg.xmp.5f3e9a01.asteria-tmp");
    fs::write(&day_old, b"x").unwrap();
    age(&day_old, 23);
    assert_eq!(cleanup_tmp_files(dir.path()).unwrap(), 0);
    assert!(fresh.exists() && day_old.exists());
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

#[cfg(unix)]
#[test]
fn cleanup_skips_unreadable_subdir() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).unwrap();
    stale(&locked.join(".hidden.asteria-tmp"));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let other = dir.path().join("other");
    fs::create_dir(&other).unwrap();
    stale(&other.join(".x.asteria-tmp"));
    let readable = fs::read_dir(&locked).is_ok(); // root ignores modes
    let result = cleanup_tmp_files(dir.path());
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    let expected = if readable { 2 } else { 1 };
    assert_eq!(result.unwrap(), expected);
    assert!(!other.join(".x.asteria-tmp").exists());
}

#[test]
fn cleanup_unreadable_root_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    assert!(cleanup_tmp_files(&dir.path().join("missing")).is_err());
}
