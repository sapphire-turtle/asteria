//! The server's journal: the directory it lives in, how it is made ready, and the corundum
//! `Journal` that runs every change to the library.

pub mod map;
mod prepare;

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use asteria_common_core::Library;
use corundum_framework::journal::{
    BatchId, BatchRecords, Clock, Journal, JournalConfig, JournalError, JsonlStore, Registry,
    UndoPolicy,
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

/// How many of an origin's newest batches can be undone.
const UNDO_BATCHES: usize = 1000;

/// How often `wait_up_to` looks at the batch.
const POLL: Duration = Duration::from_millis(50);

/// Prepares `<state>/journal`, opens the JsonlStore and starts the Journal with every
/// operation registered (`register_all`), the 30-day policy and `clock` (None = system).
pub fn open_journal(ctx: JournalCtx, clock: Option<Clock>) -> Result<ServerJournal, ServerError> {
    let dir = ctx
        .library
        .state_dir()
        .ok_or(asteria_common_core::Error::NoStateDir)?
        .join("journal");
    let now: Clock = clock.unwrap_or_else(|| Arc::new(system_now_ms));
    prepare(&dir, now(), UNDO_WINDOW)?;
    let store = JsonlStore::open(&dir)?;
    let mut registry = Registry::new();
    register_all(&mut registry, ctx.ext.as_ref());
    let config = JournalConfig {
        policy: UndoPolicy {
            max_batches: UNDO_BATCHES,
            max_age: UNDO_WINDOW,
        },
        now,
        ..JournalConfig::default()
    };
    Ok(Journal::new(registry, ctx, store, config))
}

/// Registers every operation the server runs; each task adds its own.
pub fn register_all(_reg: &mut Registry<JournalCtx>, _ext: &dyn AppExtension) {}

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
