//! Shared state handed to every request handler.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use asteria_framework::Library;
use tokio::sync::mpsc;

use crate::auth::Tokens;
use crate::cache::Cache;
use crate::ext::AppExtension;
use crate::process::Processor;
use crate::upload::UploadStore;
use crate::writer::Writer;

/// Everything the HTTP layer needs. Cheap to clone.
#[derive(Clone)]
pub struct AppState {
    library: Arc<Library>,
    /// The library root with symlinks resolved, computed once.
    canonical_root: Arc<PathBuf>,
    cache: Arc<Cache>,
    writer: Writer,
    tokens: Arc<Tokens>,
    app: &'static str,
    app_api: u32,
    scanning: Arc<AtomicBool>,
    rescan: mpsc::Sender<()>,
    extension: Arc<dyn AppExtension>,
    processor: Arc<Processor>,
    uploads: Arc<UploadStore>,
}

impl AppState {
    /// `scanning` is set by whoever runs scans; `rescan` asks that task for another scan.
    /// Also makes `processor` record unprocessable renders in `cache`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        library: Arc<Library>,
        cache: Arc<Cache>,
        writer: Writer,
        tokens: Arc<Tokens>,
        app: &'static str,
        app_api: u32,
        scanning: Arc<AtomicBool>,
        rescan: mpsc::Sender<()>,
        extension: Arc<dyn AppExtension>,
        processor: Arc<Processor>,
        uploads: Arc<UploadStore>,
    ) -> AppState {
        let marks = cache.clone();
        processor.set_unprocessable_hook(move |hash, preset, reason| {
            if let Err(e) = marks.mark_unprocessable(hash, preset.name(), reason) {
                tracing::error!("could not record an unprocessable thumbnail: {e}");
            }
        });
        let canonical_root = library.root().canonicalize().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "cannot resolve the library root; using it as given");
            library.root().to_path_buf()
        });
        AppState {
            canonical_root: Arc::new(canonical_root),
            library,
            cache,
            writer,
            tokens,
            app,
            app_api,
            scanning,
            rescan,
            extension,
            processor,
            uploads,
        }
    }

    pub fn library(&self) -> &Arc<Library> {
        &self.library
    }
    /// The library root with symlinks resolved.
    pub fn canonical_root(&self) -> &std::path::Path {
        &self.canonical_root
    }
    pub fn cache(&self) -> &Arc<Cache> {
        &self.cache
    }
    pub fn writer(&self) -> &Writer {
        &self.writer
    }
    /// Sets how long `organize.apply` and `organize.undo` wait for a batch before they
    /// answer `running`. For tests.
    #[doc(hidden)]
    pub fn set_organize_wait(&self, wait: std::time::Duration) {
        self.writer.set_organize_wait(wait);
    }
    pub fn tokens(&self) -> &Tokens {
        &self.tokens
    }
    pub fn app(&self) -> &'static str {
        self.app
    }
    pub fn app_api(&self) -> u32 {
        self.app_api
    }
    pub fn scanning(&self) -> &Arc<AtomicBool> {
        &self.scanning
    }
    pub fn rescan(&self) -> &mpsc::Sender<()> {
        &self.rescan
    }
    pub fn extension(&self) -> &Arc<dyn AppExtension> {
        &self.extension
    }
    pub fn processor(&self) -> &Arc<Processor> {
        &self.processor
    }
    pub fn uploads(&self) -> &Arc<UploadStore> {
        &self.uploads
    }
}
