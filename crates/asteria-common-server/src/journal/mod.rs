//! The server's journal: the directory it lives in, how it is made ready, and the corundum
//! `Journal` that runs every change to the library.

pub mod files;
pub mod map;
mod prepare;
pub mod sidecar;

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use asteria_common_core::Library;
use corundum_framework::journal::{
    BatchId, BatchRecords, Clock, Journal, JournalConfig, JournalError, JournalStore, JsonlStore,
    Registry, UndoPolicy,
};

pub use prepare::{Prepared, UNDO_WINDOW, prepare};

use crate::cache::Cache;
use crate::error::ServerError;
use crate::ext::AppExtension;

/// What every operation runs against.
pub struct JournalCtx {
    pub library: Arc<Library>,
    pub cache: Arc<Cache>,
    pub ext: Arc<dyn AppExtension>,
}

pub type ServerJournal = Journal<JournalCtx>;

/// How often `wait_up_to` (and an upload commit settling its batch) looks at the batch.
pub(crate) const POLL: Duration = Duration::from_millis(50);

/// Prepares `<state>/journal`, opens the JsonlStore and starts the Journal with every
/// operation registered (`register_all`), the 30-day policy and `clock` (None = system).
pub fn open_journal(ctx: JournalCtx, clock: Option<Clock>) -> Result<ServerJournal, ServerError> {
    open_journal_over(ctx, clock, |store| store)
}

/// [`open_journal`] over the store `wrap` makes of the JsonlStore (tests put one in the way).
pub(crate) fn open_journal_over<S: JournalStore + 'static>(
    ctx: JournalCtx,
    clock: Option<Clock>,
    wrap: impl FnOnce(JsonlStore) -> S,
) -> Result<ServerJournal, ServerError> {
    let dir = ctx
        .library
        .state_dir()
        .ok_or(asteria_common_core::Error::NoStateDir)?
        .join("journal");
    let now: Clock = clock.unwrap_or_else(|| Arc::new(system_now_ms));
    prepare(&dir, now(), UNDO_WINDOW)?;
    let store = wrap(JsonlStore::open(&dir)?);
    let mut registry = Registry::new();
    register_all(&mut registry, ctx.ext.as_ref());
    let config = JournalConfig {
        policy: undo_policy(),
        now,
        ..JournalConfig::default()
    };
    Ok(Journal::new(registry, ctx, store, config))
}

/// How far back undo reaches: the 30-day window, with no limit on the count.
pub(crate) fn undo_policy() -> UndoPolicy {
    UndoPolicy {
        max_batches: usize::MAX,
        max_age: UNDO_WINDOW,
    }
}

/// Registers every operation the server runs.
pub fn register_all(reg: &mut Registry<JournalCtx>, _ext: &dyn AppExtension) {
    files::register(reg);
    sidecar::register(reg);
}

/// The label of a batch given none, by the kind of its first operation and how many
/// operations of that kind it has; `None` for a kind with no label of its own.
pub fn auto_label(kind: &str, n: usize) -> Option<String> {
    Some(match kind {
        "move" | "trash" => format!("Move {n} files"),
        "rate" => format!("Rate {n} items"),
        "write_sidecar" => "Edit metadata".into(),
        "upload" => format!("Upload {n} files"),
        "relink_sidecar" => format!("Relink {n} sidecars"),
        _ => return None,
    })
}

fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Polls `status` every 50 ms; `Ok(None)` when the batch has not ended within `wait`.
pub async fn wait_up_to<C: Send + Sync + 'static>(
    j: &Journal<C>,
    id: BatchId,
    wait: Duration,
) -> Result<Option<BatchRecords>, JournalError> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let records = j.status(id)?;
        if records.summary.complete {
            return Ok(Some(records));
        }
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        tokio::time::sleep(POLL.min(left)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unnamed_batches_are_labelled_by_their_first_kind() {
        assert_eq!(auto_label("move", 3).as_deref(), Some("Move 3 files"));
        assert_eq!(auto_label("trash", 1).as_deref(), Some("Move 1 files"));
        assert_eq!(auto_label("rate", 2).as_deref(), Some("Rate 2 items"));
        assert_eq!(
            auto_label("write_sidecar", 1).as_deref(),
            Some("Edit metadata")
        );
        assert_eq!(auto_label("upload", 4).as_deref(), Some("Upload 4 files"));
        assert_eq!(
            auto_label("relink_sidecar", 5).as_deref(),
            Some("Relink 5 sidecars")
        );
        assert_eq!(auto_label("organize_tidy", 1), None);
    }

    #[test]
    fn the_undo_policy_is_the_window_with_no_count_limit() {
        let p = undo_policy();
        assert_eq!(p.max_batches, usize::MAX);
        assert_eq!(p.max_age, UNDO_WINDOW);
    }
}
