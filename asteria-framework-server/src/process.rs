//! Thumbnail rendering with `vipsthumbnail` (images) and `ffmpeg` (videos).
//!
//! Renders are limited to `max_jobs` at a time, bounded by a timeout (the child is killed),
//! and shared: concurrent requests for the same `(hash, preset)` wait for one render.
//! At most `max_jobs * 32` renders are queued or running; beyond that a request gets
//! [`ProcessError::Busy`]. A queued render whose requests have all gone away is dropped
//! before it starts its tool.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use asteria_framework::MediaKind;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{Semaphore, watch};

/// Environment variable naming the `vipsthumbnail` executable.
pub const VIPSTHUMBNAIL_ENV: &str = "ASTERIA_VIPSTHUMBNAIL";
/// Environment variable naming the `ffmpeg` executable.
pub const FFMPEG_ENV: &str = "ASTERIA_FFMPEG";

/// Queued or running renders allowed per render slot.
const QUEUE_PER_JOB: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Preset {
    Thumb,
    Preview,
}

impl Preset {
    /// The preset a URL segment names.
    pub fn parse(name: &str) -> Option<Preset> {
        match name {
            "thumb" => Some(Preset::Thumb),
            "preview" => Some(Preset::Preview),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Preset::Thumb => "thumb",
            Preset::Preview => "preview",
        }
    }

    /// Longest side in pixels.
    pub fn size(self) -> u32 {
        match self {
            Preset::Thumb => 256,
            Preset::Preview => 1920,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProcessError {
    /// This file cannot be rendered (bad data, timeout); retrying will not help.
    #[error("unprocessable: {0}")]
    Unprocessable(String),
    /// The external tool is not installed.
    #[error("{0} is not installed")]
    ToolMissing(&'static str),
    #[error("i/o error: {0}")]
    Io(String),
    /// Too many renders are queued; try again shortly.
    #[error("too many renders are queued")]
    Busy,
}

type Outcome = Result<PathBuf, ProcessError>;
/// The sending side of one render's result; waiters `subscribe` to it, so its
/// `receiver_count` is the number of requests still waiting.
type Pending = Arc<watch::Sender<Option<Outcome>>>;
/// Called with `(hash, preset, reason)` when a render fails as unprocessable.
type Hook = Box<dyn Fn(&str, Preset, &str) + Send + Sync>;

/// Renders thumbnails. Cheap to share behind an `Arc`; renders run detached from the
/// requests that asked for them, so a dropped request neither kills nor leaks a render.
pub struct Processor {
    inner: Arc<Inner>,
}

struct Inner {
    thumbs_dir: PathBuf,
    /// Set when the thumbs directory cannot be used; every render then fails with it.
    config_error: Option<String>,
    permits: Semaphore,
    queue_limit: usize,
    timeout: Duration,
    vips: OsString,
    ffmpeg: OsString,
    /// Renders queued or running, by key. Its length is the queue size.
    inflight: Mutex<HashMap<(String, Preset), Pending>>,
    on_unprocessable: OnceLock<Hook>,
    started: AtomicUsize,
    tmp_counter: AtomicUsize,
}

/// Deletes the listed files when dropped, on every exit path.
struct Cleanup(Vec<PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Removes a single-flight entry when dropped, even if the render task panicked. Only its
/// own entry: a later render for the same key may have replaced it.
struct Unregister {
    inner: Arc<Inner>,
    key: (String, Preset),
    tx: Pending,
}

impl Drop for Unregister {
    fn drop(&mut self) {
        let mut map = self.inner.inflight.lock().unwrap();
        if map
            .get(&self.key)
            .is_some_and(|tx| Arc::ptr_eq(tx, &self.tx))
        {
            map.remove(&self.key);
        }
    }
}

impl Processor {
    /// Tools come from `ASTERIA_VIPSTHUMBNAIL` / `ASTERIA_FFMPEG`, else from `PATH`.
    /// `thumbs_dir` must be absolute and must not contain `%` (vipsthumbnail expands it in
    /// output names); otherwise every render fails with an `Io` error.
    pub fn new(thumbs_dir: PathBuf, max_jobs: usize, timeout: Duration) -> Processor {
        let tool = |var: &str, default: &str| {
            std::env::var_os(var).unwrap_or_else(|| OsString::from(default))
        };
        Processor::with_tools(
            thumbs_dir,
            max_jobs,
            timeout,
            tool(VIPSTHUMBNAIL_ENV, "vipsthumbnail"),
            tool(FFMPEG_ENV, "ffmpeg"),
        )
    }

    pub fn with_tools(
        thumbs_dir: PathBuf,
        max_jobs: usize,
        timeout: Duration,
        vipsthumbnail: impl Into<OsString>,
        ffmpeg: impl Into<OsString>,
    ) -> Processor {
        let config_error = if !thumbs_dir.is_absolute() {
            Some("thumbs directory is not absolute".to_string())
        } else if thumbs_dir.to_string_lossy().contains('%') {
            Some("thumbs directory contains '%'".to_string())
        } else {
            None
        };
        let jobs = max_jobs.max(1);
        Processor {
            inner: Arc::new(Inner {
                thumbs_dir,
                config_error,
                permits: Semaphore::new(jobs),
                queue_limit: jobs * QUEUE_PER_JOB,
                timeout,
                vips: vipsthumbnail.into(),
                ffmpeg: ffmpeg.into(),
                inflight: Mutex::new(HashMap::new()),
                on_unprocessable: OnceLock::new(),
                started: AtomicUsize::new(0),
                tmp_counter: AtomicUsize::new(0),
            }),
        }
    }

    /// Removes stale `.tmp-*` and `.src-*` files that an earlier run left under the thumbs
    /// directory. Call once at startup, before serving. Returns how many were removed.
    pub fn sweep_tmp(&self) -> usize {
        let mut removed = 0;
        for entry in walkdir::WalkDir::new(&self.inner.thumbs_dir)
            .into_iter()
            .flatten()
        {
            let name = entry.file_name().to_string_lossy();
            if (name.starts_with(".tmp-") || name.starts_with(".src-"))
                && !entry.file_type().is_dir()
                && std::fs::remove_file(entry.path()).is_ok()
            {
                removed += 1;
            }
        }
        removed
    }

    /// How many renders actually ran a tool (not counting cache hits or shared waits).
    /// Sets what records an unprocessable result. It runs in the render task (on a blocking
    /// thread), before the result reaches any waiter, so it runs even when every request has
    /// gone away. Only the first call has an effect.
    pub fn set_unprocessable_hook(
        &self,
        hook: impl Fn(&str, Preset, &str) + Send + Sync + 'static,
    ) {
        if self.inner.on_unprocessable.set(Box::new(hook)).is_err() {
            tracing::warn!("unprocessable hook already set; ignoring another");
        }
    }

    /// How many renders may be queued or running at once.
    pub fn queue_limit(&self) -> usize {
        self.inner.queue_limit
    }

    /// How many renders are queued or running.
    pub fn renders_queued(&self) -> usize {
        self.inner.inflight.lock().unwrap().len()
    }

    pub fn renders_started(&self) -> usize {
        self.inner.started.load(Ordering::SeqCst)
    }

    /// Where the thumbnail for `hash` (`blake3:<hex>`) lives, or `None` if the hash is not
    /// of that form (it becomes part of a path, so it must be plain hex).
    pub fn thumb_path(&self, hash: &str, preset: Preset) -> Option<PathBuf> {
        let hex = hash.strip_prefix("blake3:")?;
        if hex.len() < 2 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Some(
            self.inner
                .thumbs_dir
                .join(preset.name())
                .join(&hex[..2])
                .join(format!("{hex}.webp")),
        )
    }

    /// Returns the path of the rendered thumbnail, rendering it first if needed.
    /// `src` must be absolute. A render that has started keeps running if this future is
    /// dropped; one still waiting for a slot is dropped once nobody waits for it.
    pub async fn render(
        &self,
        src: &Path,
        kind: MediaKind,
        hash: &str,
        preset: Preset,
    ) -> Result<PathBuf, ProcessError> {
        if let Some(e) = &self.inner.config_error {
            return Err(ProcessError::Io(e.clone()));
        }
        if !src.is_absolute() {
            return Err(ProcessError::Io("source path is not absolute".into()));
        }
        let out = self
            .thumb_path(hash, preset)
            .ok_or_else(|| ProcessError::Io("malformed content hash".into()))?;
        if exists(&out).await? {
            return Ok(out);
        }
        let key = (hash.to_string(), preset);
        let mut pending = {
            let mut map = self.inner.inflight.lock().unwrap();
            match map.get(&key) {
                Some(tx) => tx.subscribe(),
                None => {
                    if map.len() >= self.inner.queue_limit {
                        return Err(ProcessError::Busy);
                    }
                    let (tx, rx) = watch::channel(None);
                    let tx = Arc::new(tx);
                    map.insert(key.clone(), tx.clone());
                    tokio::spawn(render_task(
                        self.inner.clone(),
                        key,
                        tx,
                        src.to_path_buf(),
                        kind,
                        out,
                    ));
                    rx
                }
            }
        };
        let done = pending
            .wait_for(|v| v.is_some())
            .await
            .map_err(|_| ProcessError::Io("render task ended without a result".into()))?;
        done.clone().unwrap_or_else(|| {
            Err(ProcessError::Io(
                "render task ended without a result".into(),
            ))
        })
    }
}

/// One detached render: waits for a slot, gives up if nobody waits any more, renders,
/// records an unprocessable result, then hands the result to the waiters.
async fn render_task(
    inner: Arc<Inner>,
    key: (String, Preset),
    tx: Pending,
    src: PathBuf,
    kind: MediaKind,
    out: PathBuf,
) {
    let _unregister = Unregister {
        inner: inner.clone(),
        key: key.clone(),
        tx: tx.clone(),
    };
    let permit = match inner.permits.acquire().await {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(Some(Err(ProcessError::Io(e.to_string()))));
            return;
        }
    };
    {
        // Waiters subscribe under this lock, so none can join after the check.
        let mut map = inner.inflight.lock().unwrap();
        if tx.receiver_count() == 0 {
            if map.get(&key).is_some_and(|t| Arc::ptr_eq(t, &tx)) {
                map.remove(&key);
            }
            tracing::debug!("dropping a queued render nobody waits for");
            return;
        }
    }
    let result = inner.render_once(&src, kind, &out, key.1).await;
    drop(permit);
    if let Err(ProcessError::Unprocessable(reason)) = &result
        && inner.on_unprocessable.get().is_some()
    {
        let (inner, hash, preset, reason) = (inner.clone(), key.0.clone(), key.1, reason.clone());
        let recorded = tokio::task::spawn_blocking(move || {
            if let Some(hook) = inner.on_unprocessable.get() {
                hook(&hash, preset, &reason);
            }
        })
        .await;
        if recorded.is_err() {
            tracing::error!("recording an unprocessable thumbnail panicked");
        }
    }
    // Nobody may be listening any more; that is fine.
    let _ = tx.send(Some(result));
}

/// `try_exists`, with an error being an `Io` failure (logged) instead of "absent".
async fn exists(path: &Path) -> Result<bool, ProcessError> {
    tokio::fs::try_exists(path).await.map_err(|e| {
        tracing::error!("cannot check for a thumbnail: {e}");
        ProcessError::Io(format!("cannot check for a thumbnail: {e}"))
    })
}

/// A file extension that is safe to put in a file name: a few ASCII alphanumerics.
fn safe_ext(src: &Path) -> String {
    let ext: String = src
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    if ext.is_empty() { "bin".into() } else { ext }
}

#[cfg(unix)]
async fn link_or_copy(src: &Path, dest: &Path) -> std::io::Result<()> {
    tokio::fs::symlink(src, dest).await
}

#[cfg(not(unix))]
async fn link_or_copy(src: &Path, dest: &Path) -> std::io::Result<()> {
    tokio::fs::copy(src, dest).await.map(|_| ())
}

impl Inner {
    async fn render_once(
        &self,
        src: &Path,
        kind: MediaKind,
        out: &Path,
        preset: Preset,
    ) -> Result<PathBuf, ProcessError> {
        // The caller holds a permit. A render that finished while this one waited for it.
        if exists(out).await? {
            return Ok(out.to_path_buf());
        }
        self.started.fetch_add(1, Ordering::SeqCst);
        let dir = out.parent().expect("thumb path has a parent");
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| ProcessError::Io(e.to_string()))?;
        let n = self.tmp_counter.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        // The tool picks the format from the extension, so the temporary name ends in `.webp`.
        let tmp = dir.join(format!(".tmp-{pid}-{n}.webp"));
        let mut cleanup = Cleanup(vec![tmp.clone()]);
        let result = match kind {
            MediaKind::Image => {
                // libvips reads a trailing `[...]` of a file name as loader options, so it
                // gets the image under a name we chose.
                let link = dir.join(format!(".src-{pid}-{n}.{}", safe_ext(src)));
                cleanup.0.push(link.clone());
                match link_or_copy(src, &link).await {
                    Ok(()) => self.run_vips(&link, &tmp, preset).await,
                    Err(e) => Err(ProcessError::Io(format!("cannot stage the source: {e}"))),
                }
            }
            MediaKind::Video => self.run_ffmpeg(src, &tmp, preset).await,
            _ => Err(ProcessError::Unprocessable("not an image or video".into())),
        };
        match result {
            Ok(()) => tokio::fs::rename(&tmp, out)
                .await
                .map(|()| out.to_path_buf())
                .map_err(|e| ProcessError::Io(e.to_string())),
            Err(e) => Err(e),
        }
    }

    async fn run_vips(&self, src: &Path, tmp: &Path, preset: Preset) -> Result<(), ProcessError> {
        let n = preset.size();
        let mut dest = tmp.as_os_str().to_owned();
        dest.push("[Q=80,strip]");
        let mut cmd = Command::new(&self.vips);
        // `NxN>` only shrinks, like the `min()` in the ffmpeg filter.
        cmd.arg(src)
            .arg("--size")
            .arg(format!("{n}x{n}>"))
            .arg("--export-profile")
            .arg("srgb")
            .arg("-o")
            .arg(dest);
        run_tool(cmd, "vipsthumbnail", self.timeout, tmp).await
    }

    async fn run_ffmpeg(&self, src: &Path, tmp: &Path, preset: Preset) -> Result<(), ProcessError> {
        let n = preset.size();
        let filter =
            format!("scale=w='min({n},iw)':h='min({n},ih)':force_original_aspect_ratio=decrease");
        let attempt = |seek: &'static str| {
            let mut cmd = Command::new(&self.ffmpeg);
            cmd.args(["-nostdin", "-v", "error", "-ss", seek])
                // Only plain files and ordinary container formats: no playlists, concat
                // scripts or network protocols that a crafted file could abuse.
                .args(["-protocol_whitelist", "file"])
                .args([
                    "-format_whitelist",
                    "mov,mp4,m4a,3gp,3g2,mj2,matroska,webm,avi",
                ])
                .arg("-i")
                .arg(src)
                .args(["-frames:v", "1", "-vf"])
                .arg(&filter)
                .args(["-c:v", "libwebp", "-quality", "80", "-y"])
                .arg(tmp);
            cmd
        };
        // Both attempts share one timeout budget.
        let deadline = Instant::now() + self.timeout;
        match run_tool(attempt("1"), "ffmpeg", self.timeout, tmp).await {
            Err(ProcessError::Unprocessable(reason)) if reason != "timeout" => {
                // Shorter than one second: seeking past the end yields no frame.
                let left = deadline.saturating_duration_since(Instant::now());
                run_tool(attempt("0"), "ffmpeg", left, tmp).await
            }
            other => other,
        }
    }
}

/// Whether a tool's error output says it could not write (or read) files: the machine's
/// problem, not the media file's.
fn is_io_failure(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    [
        "unable to open for write",
        "unable to write",
        "error opening output",
        "permission denied",
        "no space left",
        "read-only file system",
        "disk quota",
    ]
    .iter()
    .any(|m| s.contains(m))
}

/// Runs `cmd` to completion within `timeout`. Success means exit status 0 and a non-empty
/// file at `output`; ffmpeg exits 0 with an empty file when asked to seek past the end.
async fn run_tool(
    mut cmd: Command,
    name: &'static str,
    timeout: Duration,
    output: &Path,
) -> Result<(), ProcessError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::error!(
                "external tool `{name}` was not found; install it or point its ASTERIA_* variable at it"
            );
            return Err(ProcessError::ToolMissing(name));
        }
        Err(e) => return Err(ProcessError::Io(format!("{name}: {e}"))),
    };
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let reader = tokio::spawn(async move {
        // Keep the first 4 KiB, and drain the rest so the child never blocks on a full pipe.
        let mut kept = Vec::new();
        let mut buf = [0u8; 1024];
        while let Ok(n) = stderr.read(&mut buf).await {
            if n == 0 {
                break;
            }
            if kept.len() < 4096 {
                kept.extend_from_slice(&buf[..n]);
            }
        }
        String::from_utf8_lossy(&kept).into_owned()
    });
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            reader.abort();
            return Err(ProcessError::Io(format!("{name}: {e}")));
        }
        Err(_) => {
            let _ = child.kill().await;
            reader.abort();
            return Err(ProcessError::Unprocessable("timeout".into()));
        }
    };
    let stderr = reader.await.unwrap_or_default();
    if !status.success() {
        let detail = stderr
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.contains("VIPS-WARNING"))
            .unwrap_or("no error output");
        if is_io_failure(&stderr) {
            return Err(ProcessError::Io(format!(
                "{name} failed ({status}): {detail}"
            )));
        }
        return Err(ProcessError::Unprocessable(format!(
            "{name} failed ({status}): {detail}"
        )));
    }
    match tokio::fs::metadata(output).await {
        Ok(m) if m.len() > 0 => Ok(()),
        _ => Err(ProcessError::Unprocessable(format!(
            "{name} produced no output"
        ))),
    }
}
