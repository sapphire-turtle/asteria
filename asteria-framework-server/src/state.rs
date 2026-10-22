//! Shared state handed to every request handler.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use asteria_framework::Library;
use tokio::sync::mpsc;

use crate::auth::Tokens;
use crate::cache::Cache;
use crate::process::Processor;
use crate::scan::MediaFilter;
use crate::writer::Writer;

/// Everything the HTTP layer needs. Cheap to clone.
#[derive(Clone)]
pub struct AppState {
    library: Arc<Library>,
    cache: Arc<Cache>,
    writer: Writer,
    tokens: Arc<Tokens>,
    app: &'static str,
    app_api: u32,
    scanning: Arc<AtomicBool>,
    rescan: mpsc::Sender<()>,
    filter: Arc<dyn MediaFilter>,
    processor: Arc<Processor>,
}

impl AppState {
    /// `scanning` is set by whoever runs scans; `rescan` asks that task for another scan.
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
        filter: Arc<dyn MediaFilter>,
        processor: Arc<Processor>,
    ) -> AppState {
        AppState {
            library,
            cache,
            writer,
            tokens,
            app,
            app_api,
            scanning,
            rescan,
            filter,
            processor,
        }
    }

    pub fn library(&self) -> &Arc<Library> {
        &self.library
    }
    pub fn cache(&self) -> &Arc<Cache> {
        &self.cache
    }
    pub fn writer(&self) -> &Writer {
        &self.writer
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
    pub fn filter(&self) -> &Arc<dyn MediaFilter> {
        &self.filter
    }
    pub fn processor(&self) -> &Arc<Processor> {
        &self.processor
    }
}
