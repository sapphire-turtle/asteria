//! Scan loop: scan, hash and relink once at start, then again on debounced file-system
//! events, `library.rescan` triggers and a periodic timer.
//!
//! Shutdown: cancel the [`CancellationToken`] in [`ScanLoop::shutdown`]. The loop then sets the
//! `hash_pending` cancel flag (so a long hashing pass stops promptly) and exits.
//!
//! Coalescing: the trigger channel is drained just before a pass starts, so any number of
//! triggers arriving while a pass runs cause exactly one more pass.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use asteria_framework::Library;
use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until};
use tokio_util::sync::CancellationToken;

use crate::cache::Cache;
use crate::scan::{MediaFilter, scan};
use crate::writer::Writer;

pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(500);
pub const DEFAULT_PERIOD: Duration = Duration::from_secs(15 * 60);

pub struct ScanLoop {
    pub library: Arc<Library>,
    pub cache: Arc<Cache>,
    pub filter: Arc<dyn MediaFilter>,
    pub writer: Writer,
    pub scanning: Arc<AtomicBool>,
    pub trigger: mpsc::Receiver<()>,
    /// Quiet time after the last file event before a pass starts.
    pub debounce: Duration,
    /// Interval between passes when nothing else triggers one.
    pub period: Duration,
    pub shutdown: CancellationToken,
    /// Completed passes, for tests; share a clone with the caller.
    pub passes: Arc<AtomicUsize>,
}

/// Runs the loop on the current tokio runtime until `shutdown` fires.
pub fn spawn_scan_loop(state: ScanLoop) -> JoinHandle<()> {
    tokio::spawn(run(state))
}

/// Whether a notify event can change what a scan sees.
fn relevant(root: &Path, event: &notify::Event) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    let meta = root.join(".asteria");
    event.paths.iter().any(|p| {
        if p.starts_with(&meta) {
            return false;
        }
        let name = p.file_name().map(|n| n.to_string_lossy());
        !name.is_some_and(|n| {
            n.ends_with(".asteria-tmp") || n.starts_with(".tmp-") || n.starts_with(".src-")
        })
    })
}

fn start_watcher(
    root: &Path,
) -> notify::Result<(notify::RecommendedWatcher, mpsc::UnboundedReceiver<()>)> {
    let (tx, rx) = mpsc::unbounded_channel();
    let owned = root.to_path_buf();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        match res {
            Ok(ev) if relevant(&owned, &ev) => {
                let _ = tx.send(());
            }
            Ok(_) => {}
            // e.g. event queue overflow: rescan to be safe
            Err(e) => {
                tracing::warn!(error = %e, "file watcher error");
                let _ = tx.send(());
            }
        }
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok((watcher, rx))
}

async fn run(mut s: ScanLoop) {
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let (cancel, token) = (cancel.clone(), s.shutdown.clone());
        tokio::spawn(async move {
            token.cancelled().await;
            cancel.store(true, Ordering::Relaxed);
        });
    }
    // Keep `_watcher` alive for the whole loop.
    let (_watcher, mut events) = match start_watcher(s.library.root()) {
        Ok((w, rx)) => (Some(w), Some(rx)),
        Err(e) => {
            tracing::warn!(error = %e, "cannot watch library; using periodic and triggered rescans only");
            (None, None)
        }
    };
    let mut trigger_open = true;

    pass(&s, &cancel).await;
    loop {
        if s.shutdown.is_cancelled() {
            return;
        }
        let mut debounce_until: Option<Instant> = None;
        let next_period = sleep(s.period);
        tokio::pin!(next_period);
        // Wait for a reason to scan; a file event only starts the debounce window.
        loop {
            let deadline = debounce_until;
            tokio::select! {
                _ = s.shutdown.cancelled() => return,
                t = s.trigger.recv(), if trigger_open => {
                    if t.is_none() { trigger_open = false; continue; }
                    break;
                }
                Some(()) = async {
                    match events.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    debounce_until = Some(Instant::now() + s.debounce);
                }
                _ = async {
                    match deadline {
                        Some(d) => sleep_until(d).await,
                        None => std::future::pending().await,
                    }
                } => break,
                _ = &mut next_period => break,
            }
        }
        // Everything queued so far is covered by the pass that starts now.
        while s.trigger.try_recv().is_ok() {}
        if let Some(rx) = events.as_mut() {
            while rx.try_recv().is_ok() {}
        }
        pass(&s, &cancel).await;
    }
}

async fn pass(s: &ScanLoop, cancel: &Arc<AtomicBool>) {
    s.scanning.store(true, Ordering::SeqCst);
    let (library, cache, filter, flag) = (
        s.library.clone(),
        s.cache.clone(),
        s.filter.clone(),
        cancel.clone(),
    );
    let res = tokio::task::spawn_blocking(move || {
        let report = scan(&library, &cache, filter.as_ref())?;
        let hashed = crate::hashing::hash_pending(&library, &cache, &flag)?;
        Ok::<_, crate::error::ServerError>((report, hashed))
    })
    .await;
    match res {
        Ok(Ok((report, hashed))) => {
            tracing::info!(
                added = report.added,
                updated = report.updated,
                removed = report.removed,
                hashed,
                "scan pass finished"
            );
            if !report.skipped.is_empty() {
                tracing::warn!(skipped = ?report.skipped, "scan skipped entries");
            }
        }
        Ok(Err(e)) => tracing::error!(error = %e, "scan pass failed"),
        Err(e) => tracing::error!(error = %e, "scan pass panicked"),
    }
    match s.writer.relink_orphans().await {
        Ok(r) => {
            tracing::info!(
                relinked = r.relinked.len(),
                ambiguous = r.ambiguous.len(),
                unmatched = r.unmatched.len(),
                conflicts = r.conflicts.len(),
                "relink finished"
            );
            if !r.ambiguous.is_empty() || !r.conflicts.is_empty() {
                tracing::warn!(ambiguous = ?r.ambiguous, conflicts = ?r.conflicts, "relink could not resolve some sidecars");
            }
        }
        Err(e) => tracing::error!(error = %e, "relink failed"),
    }
    s.scanning.store(false, Ordering::SeqCst);
    s.passes.fetch_add(1, Ordering::SeqCst);
}
