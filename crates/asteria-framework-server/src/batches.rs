//! What this server run knows about the organize batches it started: which are running and
//! the results of those that ended, for `organize.status`. The journal remembers batches
//! for good but not their per-operation results, so these are kept here for a while.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use asteria_framework::journal::BatchId;
use asteria_framework_api::OpResult;

/// How long the results of an ended batch are kept.
pub(crate) const BOOK_KEEP_FOR: Duration = Duration::from_secs(60 * 60);
/// The most ended batches kept; older ones go first.
pub(crate) const BOOK_KEEP: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Slot {
    Running,
    Done(Vec<OpResult>),
    /// The batch ended with an error; the details went to the log.
    Failed,
}

struct Entry {
    slot: Slot,
    /// When the batch ended (a running one has none), and a number that orders ends.
    ended: Option<(Instant, u64)>,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<BatchId, Entry>,
    ends: u64,
}

pub(crate) struct BatchBook {
    inner: Mutex<Inner>,
    keep: usize,
    keep_for: Duration,
}

impl Default for BatchBook {
    fn default() -> Self {
        BatchBook::with_limits(BOOK_KEEP, BOOK_KEEP_FOR)
    }
}

impl BatchBook {
    pub(crate) fn with_limits(keep: usize, keep_for: Duration) -> BatchBook {
        BatchBook {
            inner: Mutex::default(),
            keep,
            keep_for,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn start(&self, id: BatchId) {
        self.lock().entries.insert(
            id,
            Entry {
                slot: Slot::Running,
                ended: None,
            },
        );
    }

    pub(crate) fn finish(&self, id: BatchId, results: Vec<OpResult>) {
        self.end(id, Slot::Done(results));
    }

    pub(crate) fn fail(&self, id: BatchId) {
        self.end(id, Slot::Failed);
    }

    pub(crate) fn get(&self, id: BatchId) -> Option<Slot> {
        let mut inner = self.lock();
        self.evict(&mut inner);
        inner.entries.get(&id).map(|e| e.slot.clone())
    }

    fn end(&self, id: BatchId, slot: Slot) {
        let mut inner = self.lock();
        inner.ends += 1;
        let ended = Some((Instant::now(), inner.ends));
        inner.entries.insert(id, Entry { slot, ended });
        self.evict(&mut inner);
    }

    /// Drops ended batches that are too old, then the oldest ones beyond `keep`.
    /// A running batch is never dropped.
    fn evict(&self, inner: &mut Inner) {
        let keep_for = self.keep_for;
        inner
            .entries
            .retain(|_, e| e.ended.is_none_or(|(at, _)| at.elapsed() < keep_for));
        let mut ended: Vec<(u64, BatchId)> = inner
            .entries
            .iter()
            .filter_map(|(id, e)| Some((e.ended?.1, *id)))
            .collect();
        if ended.len() > self.keep {
            ended.sort_unstable_by_key(|(n, _)| *n);
            for (_, id) in &ended[..ended.len() - self.keep] {
                inner.entries.remove(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grain_id::GrainId;

    fn id() -> BatchId {
        BatchId(GrainId::random())
    }

    #[test]
    fn running_is_never_evicted() {
        let book = BatchBook::with_limits(1, Duration::ZERO);
        let (a, b) = (id(), id());
        book.start(a);
        book.start(b);
        book.finish(b, vec![]);
        assert_eq!(book.get(a), Some(Slot::Running));
    }

    #[test]
    fn finished_beyond_keep_are_dropped_oldest_first() {
        let book = BatchBook::with_limits(2, Duration::from_secs(3600));
        let ids = [id(), id(), id()];
        for i in ids {
            book.start(i);
            book.finish(i, vec![]);
        }
        assert_eq!(book.get(ids[0]), None);
        assert_eq!(book.get(ids[1]), Some(Slot::Done(vec![])));
        assert_eq!(book.get(ids[2]), Some(Slot::Done(vec![])));
    }

    #[test]
    fn finished_older_than_keep_for_are_dropped() {
        let book = BatchBook::with_limits(64, Duration::ZERO);
        let (a, b) = (id(), id());
        book.start(a);
        book.finish(a, vec![]);
        book.start(b);
        book.fail(b);
        assert_eq!(book.get(a), None);
        assert_eq!(book.get(b), None);
    }

    #[test]
    fn a_failed_batch_is_remembered() {
        let book = BatchBook::default();
        let a = id();
        book.start(a);
        book.fail(a);
        assert_eq!(book.get(a), Some(Slot::Failed));
    }
}
