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
use crate::ext::AppExtension;
use crate::scan::scan;
use crate::writer::Writer;

pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(500);
pub const DEFAULT_PERIOD: Duration = Duration::from_secs(15 * 60);

pub struct ScanLoop {
    pub library: Arc<Library>,
    pub cache: Arc<Cache>,
    pub extension: Arc<dyn AppExtension>,
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

/// Whether a notify event can change what a scan sees. `meta` is `<root>/.asteria`.
fn relevant(meta: &Path, event: &notify::Event) -> bool {
    // Queue overflow (`Flag::Rescan`, no paths) means events were lost.
    if event.need_rescan() {
        return true;
    }
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    if event.paths.is_empty() {
        return true;
    }
    event.paths.iter().any(|p| {
        if p.starts_with(meta) {
            return false;
        }
        !p.file_name()
            .is_some_and(|n| n.to_string_lossy().ends_with(".asteria-tmp"))
    })
}

/// The receiver holds at most one pending wake-up, so bursts coalesce.
fn start_watcher(root: &Path) -> notify::Result<(notify::RecommendedWatcher, mpsc::Receiver<()>)> {
    let (tx, rx) = mpsc::channel(1);
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let meta = root.join(".asteria");
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let wake = match res {
            Ok(ev) => relevant(&meta, &ev),
            Err(e) => {
                tracing::warn!(error = %e, "file watcher error");
                true
            }
        };
        if wake {
            let _ = tx.try_send(());
        }
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;
    Ok((watcher, rx))
}

struct AbortOnDrop(JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Clears `scanning` on drop, so an aborted loop never leaves it set.
struct ScanningGuard(Arc<AtomicBool>);
impl Drop for ScanningGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

async fn run(mut s: ScanLoop) {
    let _guard = ScanningGuard(s.scanning.clone());
    let cancel = Arc::new(AtomicBool::new(false));
    // Copies shutdown into the hash flag; aborted when the loop ends.
    let copier = {
        let (cancel, token) = (cancel.clone(), s.shutdown.clone());
        tokio::spawn(async move {
            token.cancelled().await;
            cancel.store(true, Ordering::Relaxed);
        })
    };
    let _copier = AbortOnDrop(copier);
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
            let _ = rx.try_recv();
        }
        pass(&s, &cancel).await;
    }
}

async fn pass(s: &ScanLoop, cancel: &Arc<AtomicBool>) {
    s.scanning.store(true, Ordering::SeqCst);
    let (library, cache, extension, flag) = (
        s.library.clone(),
        s.cache.clone(),
        s.extension.clone(),
        cancel.clone(),
    );
    let res = tokio::task::spawn_blocking(move || {
        let report = scan(&library, &cache, extension.as_ref())?;
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
    if s.shutdown.is_cancelled() {
        s.scanning.store(false, Ordering::SeqCst);
        return;
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

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, Flag};

    fn meta() -> std::path::PathBuf {
        "/lib/.asteria".into()
    }
    fn ev(kind: EventKind, path: &str) -> notify::Event {
        notify::Event::new(kind).add_path(path.into())
    }

    #[test]
    fn rescan_flag_is_relevant() {
        let e = notify::Event::new(EventKind::Other).set_flag(Flag::Rescan);
        assert!(e.paths.is_empty());
        assert!(relevant(&meta(), &e));
    }

    #[test]
    fn access_is_not_relevant() {
        let e = ev(EventKind::Access(AccessKind::Any), "/lib/a.jpg");
        assert!(!relevant(&meta(), &e));
    }

    #[test]
    fn meta_dir_and_tmp_files_are_not_relevant() {
        let c = EventKind::Create(CreateKind::File);
        assert!(!relevant(&meta(), &ev(c, "/lib/.asteria/journal/x.jsonl")));
        assert!(!relevant(&meta(), &ev(c, "/lib/.a.jpg.xmp.asteria-tmp")));
        assert!(!relevant(
            &meta(),
            &ev(c, "/lib/.a.jpg.xmp.5f3e9a01.asteria-tmp")
        ));
    }

    #[test]
    fn sidecar_and_media_are_relevant() {
        let c = EventKind::Create(CreateKind::File);
        assert!(relevant(&meta(), &ev(c, "/lib/a.jpg.xmp")));
        assert!(relevant(&meta(), &ev(c, "/lib/a.jpg")));
    }
}
