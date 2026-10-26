//! [`Saver`]: writes `servers.toml` off the UI thread.

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::JoinHandle;

use asteria_framework_client::profiles::Profiles;
use corundum_framework::egui;

/// Writes `servers.toml` on its own thread, the latest state only, and hands back what
/// went wrong. Dropping it waits for the last write, so a change made just before the
/// window closes is not lost.
pub(crate) struct Saver {
    tx: Option<mpsc::Sender<Profiles>>,
    errors: mpsc::Receiver<String>,
    thread: Option<JoinHandle<()>>,
}

impl Saver {
    pub(crate) fn spawn(path: PathBuf, ctx: egui::Context) -> std::io::Result<Saver> {
        let (tx, rx) = mpsc::channel::<Profiles>();
        let (err_tx, errors) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("photos-servers-save".into())
            .spawn(move || {
                while let Ok(mut profiles) = rx.recv() {
                    // Only the newest state matters.
                    while let Ok(newer) = rx.try_recv() {
                        profiles = newer;
                    }
                    // The error never holds the file's contents, so no token.
                    if let Err(e) = profiles.save(&path) {
                        tracing::warn!(path = %path.display(), error = %e, "saving servers.toml failed");
                        let _ = err_tx.send(e.to_string());
                        ctx.request_repaint();
                    }
                }
            })?;
        Ok(Saver {
            tx: Some(tx),
            errors,
            thread: Some(thread),
        })
    }

    /// Queues `profiles` to be written.
    pub(crate) fn save(&self, profiles: &Profiles) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(profiles.clone());
        }
    }

    /// The writes that failed since the last call, as their errors' texts.
    pub(crate) fn errors(&self) -> Vec<String> {
        self.errors.try_iter().collect()
    }
}

impl Drop for Saver {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(thread) = self.thread.take() {
            // Ruling R11: a join on the UI thread at exit, accepted so the last switch is kept.
            let _ = thread.join();
        }
    }
}
