//! [`Saver`]: writes a value off the UI thread, the latest one only.

use std::sync::mpsc;
use std::thread::JoinHandle;

use corundum_framework::egui;

/// Writes the latest value handed to it on its own thread, and hands back what went wrong.
/// Dropping it waits for the last write, so a change made just before the window closes is
/// not lost.
pub struct Saver<T: Send + 'static> {
    tx: Option<mpsc::Sender<T>>,
    errors: mpsc::Receiver<String>,
    thread: Option<JoinHandle<()>>,
}

impl<T: Send + 'static> Saver<T> {
    /// Starts the thread `name`. `write` does one write; its error text goes to
    /// [`take_error`](Self::take_error) and must not hold the file's contents (a token).
    /// `ctx` is repainted when a write fails.
    pub fn spawn(
        name: &str,
        ctx: egui::Context,
        write: impl Fn(&T) -> Result<(), String> + Send + 'static,
    ) -> std::io::Result<Saver<T>> {
        let (tx, rx) = mpsc::channel::<T>();
        let (err_tx, errors) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                while let Ok(mut value) = rx.recv() {
                    // Only the newest value matters.
                    while let Ok(newer) = rx.try_recv() {
                        value = newer;
                    }
                    if let Err(e) = write(&value) {
                        let _ = err_tx.send(e);
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

    /// Queues `value` to be written.
    pub fn save(&self, value: T) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(value);
        }
    }

    /// The oldest write error not yet taken.
    pub fn take_error(&self) -> Option<String> {
        self.errors.try_recv().ok()
    }
}

impl<T: Send + 'static> Drop for Saver<T> {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(thread) = self.thread.take() {
            // A join on the UI thread at exit, accepted so the last change is kept.
            let _ = thread.join();
        }
    }
}
