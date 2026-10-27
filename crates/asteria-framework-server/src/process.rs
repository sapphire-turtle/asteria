//! Rendering with `vipsthumbnail` (image thumbnails) and `ffmpeg` (video thumbnails and
//! offline audio).
//!
//! Renders are limited to `max_jobs` at a time, bounded by a timeout (the child is killed),
//! and shared: concurrent requests for the same `(hash, preset)` wait for one render.
//! At most `max_jobs * 32` renders are queued or running; beyond that a request gets
//! [`ProcessError::Busy`]. A queued render whose requests have all gone away is dropped
//! right away, freeing its queue place without waiting for a slot.
//!
//! Offline-audio encodes run in a pool of their own (one at a time, 32 queued), so a long
//! encode never holds up a thumbnail.

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
/// Offline-audio encodes that run at once.
const AUDIO_JOBS: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Preset {
    Thumb,
    Preview,
    /// Ogg Opus for listening offline; made from audio by ffmpeg.
    OfflineAudio,
}

impl Preset {
    /// The preset a URL segment names.
    pub fn parse(name: &str) -> Option<Preset> {
        match name {
            "thumb" => Some(Preset::Thumb),
            "preview" => Some(Preset::Preview),
            "offline-audio" => Some(Preset::OfflineAudio),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Preset::Thumb => "thumb",
            Preset::Preview => "preview",
            Preset::OfflineAudio => "offline-audio",
        }
    }

    /// Longest side in pixels; 0 for a preset that is not an image.
    pub fn size(self) -> u32 {
        match self {
            Preset::Thumb => 256,
            Preset::Preview => 1920,
            Preset::OfflineAudio => 0,
        }
    }

    /// File extension of the rendered file.
    pub fn ext(self) -> &'static str {
        match self {
            Preset::Thumb | Preset::Preview => "webp",
            Preset::OfflineAudio => "ogg",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            Preset::Thumb | Preset::Preview => "image/webp",
            Preset::OfflineAudio => "audio/ogg",
        }
    }

    /// How long one render may take: encoding a whole track takes far longer than scaling
    /// an image, so audio gets ten minutes whatever `default` is.
    pub fn timeout(self, default: Duration) -> Duration {
        match self {
            Preset::OfflineAudio => Duration::from_secs(600),
            _ => default,
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
    /// An offline-audio encode ran out of time. It is not recorded as unprocessable: a long
    /// file on a busy or slow machine may well finish another time.
    #[error("the render timed out")]
    TimedOut,
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
/// What a render reads.
enum Source {
    /// A file; `render` checked that the path is absolute.
    File(PathBuf),
    /// Image bytes, written to a staged file only if a render actually runs.
    Bytes { bytes: Vec<u8>, ext: String },
}

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
    /// Slots for thumbnails and previews.
    permits: Semaphore,
    /// Slots for offline-audio encodes, kept apart so thumbnails never wait for audio.
    audio_permits: Semaphore,
    queue_limit: usize,
    timeout: Duration,
    audio_timeout: Duration,
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
                audio_permits: Semaphore::new(AUDIO_JOBS),
                queue_limit: jobs * QUEUE_PER_JOB,
                timeout,
                audio_timeout: Preset::OfflineAudio.timeout(timeout),
                vips: vipsthumbnail.into(),
                ffmpeg: ffmpeg.into(),
                inflight: Mutex::new(HashMap::new()),
                on_unprocessable: OnceLock::new(),
                started: AtomicUsize::new(0),
                tmp_counter: AtomicUsize::new(0),
            }),
        }
    }

    /// Replaces the time an offline-audio encode may take (by default
    /// [`Preset::timeout`] of [`Preset::OfflineAudio`]).
    pub fn with_audio_timeout(mut self, timeout: Duration) -> Processor {
        Arc::get_mut(&mut self.inner)
            .expect("a new processor is not shared yet")
            .audio_timeout = timeout;
        self
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

    /// How many thumbnail and preview renders may be queued or running at once. Offline
    /// audio has a queue of its own.
    pub fn queue_limit(&self) -> usize {
        self.inner.queue_limit
    }

    /// How many renders are queued or running.
    pub fn renders_queued(&self) -> usize {
        self.inner.inflight.lock().unwrap().len()
    }

    /// How many renders actually ran a tool (not counting cache hits or shared waits).
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
                .join(format!("{hex}.{}", preset.ext())),
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
        if !src.is_absolute() {
            return Err(ProcessError::Io("source path is not absolute".into()));
        }
        self.render_source(Source::File(src.to_path_buf()), kind, hash, preset)
            .await
    }

    /// Like [`Processor::render`] for an image given as bytes (for example cover art
    /// embedded in a track). `key` is `blake3:<hex>`, names the result and is what
    /// single-flight and the render queue count; `ext` is the extension the decoder needs.
    pub async fn render_image_bytes(
        &self,
        bytes: &[u8],
        ext: &str,
        key: &str,
        preset: Preset,
    ) -> Result<PathBuf, ProcessError> {
        let ext: String = ext
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(8)
            .collect();
        let src = Source::Bytes {
            bytes: bytes.to_vec(),
            ext: if ext.is_empty() { "bin".into() } else { ext },
        };
        self.render_source(src, MediaKind::Image, key, preset).await
    }

    async fn render_source(
        &self,
        src: Source,
        kind: MediaKind,
        hash: &str,
        preset: Preset,
    ) -> Result<PathBuf, ProcessError> {
        if let Some(e) = &self.inner.config_error {
            return Err(ProcessError::Io(e.clone()));
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
                    let audio = is_audio(preset);
                    let limit = if audio {
                        AUDIO_JOBS * QUEUE_PER_JOB
                    } else {
                        self.inner.queue_limit
                    };
                    if map.keys().filter(|k| is_audio(k.1) == audio).count() >= limit {
                        return Err(ProcessError::Busy);
                    }
                    let (tx, rx) = watch::channel(None);
                    let tx = Arc::new(tx);
                    map.insert(key.clone(), tx.clone());
                    tokio::spawn(render_task(self.inner.clone(), key, tx, src, kind, out));
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
    src: Source,
    kind: MediaKind,
    out: PathBuf,
) {
    let _unregister = Unregister {
        inner: inner.clone(),
        key: key.clone(),
        tx: tx.clone(),
    };
    // Wait for a slot, but give up as soon as nobody waits for the result, so an abandoned
    // render does not hold its queue place until a slot frees up.
    let permits = if is_audio(key.1) {
        &inner.audio_permits
    } else {
        &inner.permits
    };
    let permit = loop {
        tokio::select! {
            biased;
            permit = permits.acquire() => match permit {
                Ok(p) => break p,
                Err(e) => {
                    tx.send_replace(Some(Err(ProcessError::Io(e.to_string()))));
                    return;
                }
            },
            () = tx.closed() => {
                if abandon_if_unwatched(&inner, &key, &tx) {
                    return;
                }
                // A request joined after the last one left; keep waiting for a slot.
                tokio::task::yield_now().await;
            }
        }
    };
    if abandon_if_unwatched(&inner, &key, &tx) {
        return;
    }
    let result = inner.render_once(src, kind, &out, key.1).await;
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
    // `send_replace` stores the result even with no receiver: a request may subscribe after
    // the last one left but before `Unregister` removes the entry, and must still see it.
    tx.send_replace(Some(result));
}

/// Whether `preset` renders in the offline-audio pool.
fn is_audio(preset: Preset) -> bool {
    preset == Preset::OfflineAudio
}

/// Removes the render's entry and returns `true` if no request waits for it any more.
fn abandon_if_unwatched(inner: &Inner, key: &(String, Preset), tx: &Pending) -> bool {
    // Waiters subscribe under this lock, so none can join after the check.
    let mut map = inner.inflight.lock().unwrap();
    if tx.receiver_count() != 0 {
        return false;
    }
    if map.get(key).is_some_and(|t| Arc::ptr_eq(t, tx)) {
        map.remove(key);
    }
    tracing::debug!("dropping a queued render nobody waits for");
    true
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
        src: Source,
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
        // The tool picks the format from the extension, so the temporary name ends in the
        // preset's.
        let tmp = dir.join(format!(".tmp-{pid}-{n}.{}", preset.ext()));
        let mut cleanup = Cleanup(vec![tmp.clone()]);
        let result = match (kind, preset, &src) {
            (MediaKind::Image, Preset::Thumb | Preset::Preview, _) => {
                // libvips reads a trailing `[...]` of a file name as loader options, so it
                // gets the image under a name we chose.
                let ext = match &src {
                    Source::File(path) => safe_ext(path),
                    Source::Bytes { ext, .. } => ext.clone(),
                };
                let staged = dir.join(format!(".src-{pid}-{n}.{ext}"));
                cleanup.0.push(staged.clone());
                let put = match &src {
                    Source::File(path) => link_or_copy(path, &staged).await,
                    Source::Bytes { bytes, .. } => tokio::fs::write(&staged, bytes).await,
                };
                match put {
                    Ok(()) => self.run_vips(&staged, &tmp, preset).await,
                    Err(e) => Err(ProcessError::Io(format!("cannot stage the source: {e}"))),
                }
            }
            (MediaKind::Video, Preset::Thumb | Preset::Preview, Source::File(path)) => {
                self.run_ffmpeg(path, &tmp, preset).await
            }
            (MediaKind::Audio, Preset::OfflineAudio, Source::File(path)) => {
                self.run_ffmpeg_audio(path, &tmp).await
            }
            _ => Err(ProcessError::Unprocessable(
                "cannot render this kind of media with this preset".into(),
            )),
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
        run_tool(cmd, "vipsthumbnail", preset.timeout(self.timeout), tmp).await
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
        let timeout = preset.timeout(self.timeout);
        let deadline = Instant::now() + timeout;
        match run_tool(attempt("1"), "ffmpeg", timeout, tmp).await {
            Err(ProcessError::Unprocessable(reason)) if reason != "timeout" => {
                // Shorter than one second: seeking past the end yields no frame.
                let left = deadline.saturating_duration_since(Instant::now());
                run_tool(attempt("0"), "ffmpeg", left, tmp).await
            }
            other => other,
        }
    }

    /// Transcodes the first audio stream to Ogg Opus without tags or cover art.
    async fn run_ffmpeg_audio(&self, src: &Path, tmp: &Path) -> Result<(), ProcessError> {
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args(["-nostdin", "-v", "error", "-y"])
            // As for video: plain files in ordinary audio containers only.
            .args(["-protocol_whitelist", "file"])
            .args(["-format_whitelist", "flac,mp3,mov,mp4,m4a,ogg,wav,aac"])
            .arg("-i")
            .arg(src)
            .args(["-map", "0:a:0", "-map_metadata", "-1", "-vn"])
            .args(["-c:a", "libopus", "-b:a", "96k", "-vbr", "on", "-f", "ogg"])
            .arg(tmp);
        match run_tool(cmd, "ffmpeg", self.audio_timeout, tmp).await {
            Err(ProcessError::Unprocessable(reason)) if reason == "timeout" => {
                tracing::warn!("an offline-audio encode timed out; not recording it");
                Err(ProcessError::TimedOut)
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
        if name == "ffmpeg"
            && (stderr.contains("Unknown encoder 'libopus'")
                || stderr.contains("Encoder libopus not found"))
        {
            tracing::error!("this ffmpeg was built without libopus");
            return Err(ProcessError::ToolMissing("ffmpeg with libopus"));
        }
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
