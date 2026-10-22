//! Thumbnail rendering with `vipsthumbnail` (images) and `ffmpeg` (videos).
//!
//! Renders are limited to `max_jobs` at a time, bounded by a timeout (the child is killed),
//! and shared: concurrent requests for the same `(hash, preset)` wait for one render.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asteria_framework::MediaKind;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{OnceCell, Semaphore};

/// Environment variable naming the `vipsthumbnail` executable.
pub const VIPSTHUMBNAIL_ENV: &str = "ASTERIA_VIPSTHUMBNAIL";
/// Environment variable naming the `ffmpeg` executable.
pub const FFMPEG_ENV: &str = "ASTERIA_FFMPEG";

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
}

type Shared = Arc<OnceCell<Result<PathBuf, ProcessError>>>;

pub struct Processor {
    thumbs_dir: PathBuf,
    permits: Semaphore,
    timeout: Duration,
    vips: OsString,
    ffmpeg: OsString,
    inflight: Mutex<HashMap<(String, Preset), Shared>>,
    started: AtomicUsize,
    tmp_counter: AtomicUsize,
}

impl Processor {
    /// Tools come from `ASTERIA_VIPSTHUMBNAIL` / `ASTERIA_FFMPEG`, else from `PATH`.
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
        Processor {
            thumbs_dir,
            permits: Semaphore::new(max_jobs.max(1)),
            timeout,
            vips: vipsthumbnail.into(),
            ffmpeg: ffmpeg.into(),
            inflight: Mutex::new(HashMap::new()),
            started: AtomicUsize::new(0),
            tmp_counter: AtomicUsize::new(0),
        }
    }

    /// How many renders actually ran a tool (not counting cache hits or shared waits).
    pub fn renders_started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }

    /// Where the thumbnail for `hash` (`blake3:<hex>`) lives, or `None` if the hash is not
    /// of that form (it becomes part of a path, so it must be plain hex).
    pub fn thumb_path(&self, hash: &str, preset: Preset) -> Option<PathBuf> {
        let hex = hash.strip_prefix("blake3:")?;
        if hex.len() < 2 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Some(
            self.thumbs_dir
                .join(preset.name())
                .join(&hex[..2])
                .join(format!("{hex}.webp")),
        )
    }

    /// Returns the path of the rendered thumbnail, rendering it first if needed.
    /// `src` must be absolute.
    pub async fn render(
        &self,
        src: &Path,
        kind: MediaKind,
        hash: &str,
        preset: Preset,
    ) -> Result<PathBuf, ProcessError> {
        let out = self
            .thumb_path(hash, preset)
            .ok_or_else(|| ProcessError::Io("malformed content hash".into()))?;
        if tokio::fs::try_exists(&out).await.unwrap_or(false) {
            return Ok(out);
        }
        let key = (hash.to_string(), preset);
        let cell = self
            .inflight
            .lock()
            .unwrap()
            .entry(key.clone())
            .or_default()
            .clone();
        let result = cell
            .get_or_init(|| self.render_once(src, kind, &out, preset))
            .await
            .clone();
        let mut map = self.inflight.lock().unwrap();
        if map.get(&key).is_some_and(|c| Arc::ptr_eq(c, &cell)) {
            map.remove(&key);
        }
        result
    }

    async fn render_once(
        &self,
        src: &Path,
        kind: MediaKind,
        out: &Path,
        preset: Preset,
    ) -> Result<PathBuf, ProcessError> {
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|e| ProcessError::Io(e.to_string()))?;
        // A render that finished while this one waited for a permit.
        if tokio::fs::try_exists(out).await.unwrap_or(false) {
            return Ok(out.to_path_buf());
        }
        self.started.fetch_add(1, Ordering::SeqCst);
        let dir = out.parent().expect("thumb path has a parent");
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| ProcessError::Io(e.to_string()))?;
        // The tool picks the format from the extension, so the temporary name ends in `.webp`.
        let tmp = dir.join(format!(
            ".tmp-{}-{}.webp",
            std::process::id(),
            self.tmp_counter.fetch_add(1, Ordering::SeqCst)
        ));
        let result = match kind {
            MediaKind::Image => self.run_vips(src, &tmp, preset).await,
            MediaKind::Video => self.run_ffmpeg(src, &tmp, preset).await,
            _ => Err(ProcessError::Unprocessable("not an image or video".into())),
        };
        let result = match result {
            Ok(()) => tokio::fs::rename(&tmp, out)
                .await
                .map(|()| out.to_path_buf())
                .map_err(|e| ProcessError::Io(e.to_string())),
            Err(e) => Err(e),
        };
        if result.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        result
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
            cmd.args(["-nostdin", "-v", "error", "-ss", seek, "-i"])
                .arg(src)
                .args(["-frames:v", "1", "-vf"])
                .arg(&filter)
                .args(["-c:v", "libwebp", "-quality", "80", "-y"])
                .arg(tmp);
            cmd
        };
        match run_tool(attempt("1"), "ffmpeg", self.timeout, tmp).await {
            Err(ProcessError::Unprocessable(reason)) if reason != "timeout" => {
                // Shorter than one second: seeking past the end yields no frame.
                run_tool(attempt("0"), "ffmpeg", self.timeout, tmp).await
            }
            other => other,
        }
    }
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
                "external tool `{name}` was not found; set it up or point its ASTERIA_* variable at it"
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
