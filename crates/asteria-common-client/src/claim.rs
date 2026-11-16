//! Process-wide ownership of a queue file: one owner at a time, so two sessions never write one file.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

#[derive(Default)]
struct Owners {
    /// The queue files owned by an engine of this process.
    owned: HashSet<PathBuf>,
    /// Per file, the [`Ticket`]s waiting for it, in the order they were taken.
    waiting: HashMap<PathBuf, BTreeSet<u64>>,
    next_ticket: u64,
}

static OWNERS: LazyLock<(Mutex<Owners>, Condvar)> =
    LazyLock::new(|| (Mutex::new(Owners::default()), Condvar::new()));

fn owners() -> MutexGuard<'static, Owners> {
    OWNERS.0.lock().unwrap_or_else(|e| e.into_inner())
}

/// Ownership of one queue file; released when dropped.
pub(crate) struct Claim(PathBuf);

impl Drop for Claim {
    fn drop(&mut self) {
        owners().owned.remove(&self.0);
        OWNERS.1.notify_all();
    }
}

/// Takes ownership of the queue file `path`, waiting while another engine holds it. `None`
/// if `give_up` turns true first. `give_up` runs with the claims locked: it must not claim.
pub(crate) fn claim(path: &Path, give_up: impl Fn() -> bool) -> Option<Claim> {
    wait_until(path, give_up, |_| true)
}

/// A place in line for a queue file: [`Ticket::claim`] gets the file only once every ticket
/// for it taken earlier has claimed it and let go, or been dropped. Taking one never waits,
/// so a queue takes it as it starts, and the queue started before it keeps its turn however
/// late that queue's thread runs.
pub(crate) struct Ticket {
    path: PathBuf,
    n: u64,
}

impl Ticket {
    pub(crate) fn new(path: &Path) -> Ticket {
        let mut o = owners();
        let n = o.next_ticket;
        o.next_ticket += 1;
        o.waiting.entry(path.to_owned()).or_default().insert(n);
        Ticket {
            path: path.to_owned(),
            n,
        }
    }

    /// [`claim`], in turn. The ticket leaves the line either way.
    pub(crate) fn claim(self, give_up: impl Fn() -> bool) -> Option<Claim> {
        wait_until(&self.path, give_up, |o| {
            o.waiting
                .get(&self.path)
                .and_then(|line| line.first())
                .is_none_or(|&first| first == self.n)
        })
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut o = owners();
        if let Some(line) = o.waiting.get_mut(&self.path) {
            line.remove(&self.n);
            if line.is_empty() {
                o.waiting.remove(&self.path);
            }
        }
        drop(o);
        OWNERS.1.notify_all();
    }
}

/// Claims `path` once nobody owns it and `turn` says so; `None` if `give_up` turns true first.
fn wait_until(
    path: &Path,
    give_up: impl Fn() -> bool,
    turn: impl Fn(&Owners) -> bool,
) -> Option<Claim> {
    let mut o = owners();
    loop {
        if give_up() {
            return None;
        }
        if turn(&o) && o.owned.insert(path.to_owned()) {
            return Some(Claim(path.to_owned()));
        }
        o = OWNERS
            .1
            .wait_timeout(o, Duration::from_millis(50))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tickets_claim_in_the_order_they_were_taken() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.jsonl");
        let first = Ticket::new(&path);
        let second = Ticket::new(&path);
        // The second gives up rather than claim ahead of the first.
        let tries = std::cell::Cell::new(0);
        assert!(
            second
                .claim(|| {
                    tries.set(tries.get() + 1);
                    tries.get() > 3
                })
                .is_none()
        );
        let third = Ticket::new(&path);
        let held = first.claim(|| false).unwrap();
        drop(held);
        // The first left the line and the second dropped out: the third's turn.
        assert!(third.claim(|| false).is_some());
    }
}
