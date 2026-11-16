//! Process-wide ownership of a queue file: one owner at a time, so two sessions never write one file.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, LazyLock, Mutex};
use std::time::Duration;

/// The queue files owned by an engine of this process.
static OWNED: LazyLock<(Mutex<HashSet<PathBuf>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(HashSet::new()), Condvar::new()));

/// Ownership of one queue file; released when dropped.
pub(crate) struct Claim(PathBuf);

impl Drop for Claim {
    fn drop(&mut self) {
        let (owned, released) = &*OWNED;
        owned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
        released.notify_all();
    }
}

/// Takes ownership of the queue file `path`, waiting while another engine holds it. `None`
/// if `give_up` turns true first.
pub(crate) fn claim(path: &Path, give_up: impl Fn() -> bool) -> Option<Claim> {
    let (owned, released) = &*OWNED;
    let mut set = owned.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if give_up() {
            return None;
        }
        if set.insert(path.to_owned()) {
            return Some(Claim(path.to_owned()));
        }
        set = released
            .wait_timeout(set, Duration::from_millis(50))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}
