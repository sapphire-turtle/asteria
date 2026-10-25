//! `MediaCache` on temporary directories.

use std::fs::{self, File};
use std::time::{Duration, SystemTime};

use asteria_framework_client::media_cache::{DEFAULT_LIMIT_BYTES, MediaCache, TRIM_EVERY_PUTS};

fn hash(c: char) -> String {
    format!("blake3:{}", c.to_string().repeat(64))
}

fn open(root: &std::path::Path, library: &str) -> MediaCache {
    MediaCache::open(root, library).expect("valid library id")
}

fn age(path: &std::path::Path, secs_ago: u64) {
    let when = SystemTime::now() - Duration::from_secs(secs_ago);
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(when)
        .unwrap();
}

#[test]
fn same_library_shares_cache() {
    let root = tempfile::tempdir().unwrap();
    // Two profiles pointing at one library open two caches over the same directory.
    let a = open(root.path(), "lib-1");
    let b = open(root.path(), "lib-1");
    let h = hash('a');
    let stored = a.put(Some(&h), "thumb", b"webp").unwrap().unwrap();
    assert_eq!(
        stored,
        root.path()
            .join("lib-1/temp/thumb/aa")
            .join(format!("{}.webp", "a".repeat(64)))
    );
    let got = b.get(Some(&h), "thumb").unwrap();
    assert_eq!(fs::read(got).unwrap(), b"webp");
    assert_eq!(DEFAULT_LIMIT_BYTES, 512 * 1024 * 1024);
}

#[test]
fn different_libraries_do_not_mix() {
    let root = tempfile::tempdir().unwrap();
    let a = open(root.path(), "lib-1");
    let b = open(root.path(), "lib-2");
    let h = hash('b');
    a.put(Some(&h), "thumb", b"x").unwrap();
    assert!(b.get(Some(&h), "thumb").is_none());
    // The preset is part of the key too.
    assert!(a.get(Some(&h), "preview").is_none());
}

#[test]
fn null_hash_is_not_cached() {
    let root = tempfile::tempdir().unwrap();
    let c = open(root.path(), "lib-1");
    assert_eq!(c.put(None, "thumb", b"x").unwrap(), None);
    assert!(c.get(None, "thumb").is_none());
    assert!(!root.path().join("lib-1").exists());
}

#[test]
fn trim_removes_oldest_first() {
    let root = tempfile::tempdir().unwrap();
    let mut c = open(root.path(), "lib-1");
    let (old, mid, new) = (hash('1'), hash('2'), hash('3'));
    for h in [&old, &mid, &new] {
        c.put(Some(h), "thumb", &[0u8; 10]).unwrap();
    }
    age(&c.get(Some(&old), "thumb").unwrap(), 300);
    age(&c.get(Some(&mid), "thumb").unwrap(), 200);
    age(&c.get(Some(&new), "thumb").unwrap(), 100);
    // A hit makes the oldest the newest.
    let _ = c.get(Some(&old), "thumb").unwrap();
    c.limit_bytes = 20;
    c.trim();
    assert!(c.get(Some(&mid), "thumb").is_none(), "mid was the oldest");
    assert!(c.get(Some(&old), "thumb").is_some());
    assert!(c.get(Some(&new), "thumb").is_some());
    c.limit_bytes = 0;
    c.trim();
    assert!(c.get(Some(&old), "thumb").is_none());
    assert!(c.get(Some(&new), "thumb").is_none());
}

#[test]
fn partial_write_is_not_visible() {
    let root = tempfile::tempdir().unwrap();
    let mut c = open(root.path(), "lib-1");
    let h = hash('c');
    let dir = root.path().join("lib-1/temp/thumb/cc");
    fs::create_dir_all(&dir).unwrap();
    // What a crashed put leaves behind.
    let leftover = dir.join(format!(".{}.1-0.asteria-tmp", "c".repeat(64)));
    fs::write(&leftover, b"half").unwrap();
    assert!(c.get(Some(&h), "thumb").is_none());
    // Trim neither counts it nor deletes a fresh one, but removes a stale one.
    c.limit_bytes = 0;
    c.trim();
    assert!(leftover.exists());
    age(&leftover, 7 * 24 * 3600);
    c.trim();
    assert!(!leftover.exists());
}

#[test]
fn unsafe_names_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    for lib in ["", ".", "..", "../x", "a/b", "a\\b", ".hidden", "a\0b"] {
        assert!(MediaCache::open(root.path(), lib).is_none(), "{lib:?}");
    }
    let c = open(root.path(), "lib-1");
    for preset in ["", "..", "a/b", ".x", "a\\b"] {
        assert!(c.put(Some(&hash('d')), preset, b"x").is_err(), "{preset:?}");
        assert!(c.get(Some(&hash('d')), preset).is_none());
    }
    for bad in [
        "blake3:../../etc/passwd",
        "blake3:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "sha256:aaaa",
        "blake3:abc",
        "",
    ] {
        assert!(c.put(Some(bad), "thumb", b"x").is_err(), "{bad:?}");
        assert!(c.get(Some(bad), "thumb").is_none());
    }
}

#[test]
fn put_trims_every_hundred_writes() {
    let root = tempfile::tempdir().unwrap();
    let mut c = open(root.path(), "lib-1");
    c.limit_bytes = 0;
    for i in 0..TRIM_EVERY_PUTS - 1 {
        let h = format!("blake3:{i:064x}");
        c.put(Some(&h), "thumb", b"x").unwrap();
    }
    let first = format!("blake3:{:064x}", 0);
    assert!(c.get(Some(&first), "thumb").is_some(), "no trim yet");
    c.put(Some(&hash('f')), "thumb", b"x").unwrap();
    assert!(
        c.get(Some(&first), "thumb").is_none(),
        "the 100th put trimmed"
    );
}
