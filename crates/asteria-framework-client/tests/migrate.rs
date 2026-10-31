//! Off Windows the move from Roaming to Local does nothing: the app starts and finds its
//! settings where they were. Its own test binary, since it points the platform's directories
//! at a temporary home before anything else runs.

#![cfg(not(windows))]

#[test]
fn migration_is_a_no_op_off_windows() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: this test binary has only this test, so no other thread reads the environment.
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
        std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
        std::env::set_var("XDG_CACHE_HOME", home.path().join("cache"));
    }
    asteria_framework_client::migrate_dirs("asteria-test").unwrap();
    let left: Vec<_> = walk(home.path());
    assert!(left.is_empty(), "the migration created {left:?}");
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        }
        out.push(p);
    }
    out
}
