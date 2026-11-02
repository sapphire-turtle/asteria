mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use asteria_framework::MediaKind;
use asteria_framework_server::{Preset, ProcessError, Processor};
use common::have_tool as have;

const HASH: &str = "blake3:abcdef0123456789";

fn red() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../asteria-framework/tests/fixtures/red.jpg")
        .canonicalize()
        .unwrap()
}

#[test]
fn thumb_path_layout_and_hash_validation() {
    let p = Processor::with_tools("/t".into(), 1, Duration::from_secs(1), "v", "f");
    assert_eq!(
        p.thumb_path(HASH, Preset::Preview).unwrap(),
        Path::new("/t/preview/ab/abcdef0123456789.webp")
    );
    assert!(p.thumb_path("blake3:../../x", Preset::Thumb).is_none());
    assert!(p.thumb_path("sha1:abcd", Preset::Thumb).is_none());
}

#[tokio::test]
async fn renders_image_and_reuses_file() {
    if !have("vipsthumbnail") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let p = Processor::new(dir.path().to_path_buf(), 2, Duration::from_secs(30));
    let out = p
        .render(&red(), MediaKind::Image, HASH, Preset::Thumb)
        .await
        .unwrap();
    assert_eq!(out, dir.path().join("thumb/ab/abcdef0123456789.webp"));
    assert!(out.exists());
    p.render(&red(), MediaKind::Image, HASH, Preset::Thumb)
        .await
        .unwrap();
    assert_eq!(p.renders_started(), 1);
    // No temporary files are left behind.
    let names: Vec<_> = std::fs::read_dir(out.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
}

#[tokio::test]
async fn missing_tool_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let p = Processor::with_tools(
        dir.path().to_path_buf(),
        1,
        Duration::from_secs(5),
        "/nonexistent/vipsthumbnail",
        "/nonexistent/ffmpeg",
    );
    let r = p
        .render(&red(), MediaKind::Image, HASH, Preset::Thumb)
        .await;
    assert_eq!(r, Err(ProcessError::ToolMissing("vipsthumbnail")));
    let r = p
        .render(&red(), MediaKind::Video, HASH, Preset::Thumb)
        .await;
    assert_eq!(r, Err(ProcessError::ToolMissing("ffmpeg")));
}

#[cfg(unix)]
#[tokio::test]
async fn hung_tool_times_out() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("hang.sh");
    std::fs::write(&script, "#!/bin/sh\nexec sleep 10\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let p = Processor::with_tools(
        dir.path().join("thumbs"),
        1,
        Duration::from_secs(1),
        &script,
        "ffmpeg",
    );
    let start = std::time::Instant::now();
    let r = p
        .render(&red(), MediaKind::Image, HASH, Preset::Thumb)
        .await;
    assert_eq!(r, Err(ProcessError::Unprocessable("timeout".into())));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn short_video_falls_back_to_first_frame() {
    if !have("ffmpeg") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("short.mp4");
    let st = std::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x64:rate=10:duration=0.5",
            "-y",
        ])
        .arg(&video)
        .status()
        .unwrap();
    assert!(st.success());
    let p = Processor::new(dir.path().join("thumbs"), 1, Duration::from_secs(30));
    let out = p
        .render(&video, MediaKind::Video, HASH, Preset::Thumb)
        .await
        .unwrap();
    assert!(std::fs::metadata(out).unwrap().len() > 0);
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../asteria-framework/tests/fixtures")
        .join(name)
        .canonicalize()
        .unwrap()
}

fn leftovers(dir: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy();
            n.starts_with(".tmp-") || n.starts_with(".src-")
        })
        .map(|e| e.into_path())
        .collect()
}

#[cfg(unix)]
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(unix)]
#[tokio::test]
async fn bracketed_file_name_is_not_read_as_loader_options() {
    if !have("vipsthumbnail") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("x.jpg");
    let tricky = dir.path().join("x.jpg[shrink=8]");
    std::fs::copy(fixture("red.jpg"), &plain).unwrap();
    std::fs::copy(fixture("blue.jpg"), &tricky).unwrap();
    let p = Processor::new(dir.path().join("thumbs"), 2, Duration::from_secs(30));
    let a = p
        .render(&plain, MediaKind::Image, "blake3:aa11", Preset::Thumb)
        .await
        .unwrap();
    let b = p
        .render(&tricky, MediaKind::Image, "blake3:bb22", Preset::Thumb)
        .await
        .unwrap();
    assert_ne!(std::fs::read(a).unwrap(), std::fs::read(b).unwrap());
    assert!(leftovers(&dir.path().join("thumbs")).is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn abandoned_queued_render_frees_its_place_without_a_slot() {
    let dir = tempfile::tempdir().unwrap();
    let hang = script(dir.path(), "hang.sh", "exec sleep 30");
    let p = std::sync::Arc::new(Processor::with_tools(
        dir.path().join("thumbs"),
        1,
        Duration::from_secs(60),
        &hang,
        "ffmpeg",
    ));
    let spawn = |hash: &'static str| {
        let p = p.clone();
        tokio::spawn(async move {
            p.render(&red(), MediaKind::Image, hash, Preset::Thumb)
                .await
        })
    };
    let running = spawn("blake3:aaaa");
    for _ in 0..100 {
        if p.renders_started() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        p.renders_started(),
        1,
        "the first render holds the only slot"
    );
    let queued = spawn("blake3:bbbb");
    for _ in 0..100 {
        if p.renders_queued() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(p.renders_queued(), 2);
    queued.abort();
    for _ in 0..100 {
        if p.renders_queued() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(p.renders_queued(), 1, "the abandoned render left the queue");
    assert_eq!(p.renders_started(), 1);
    running.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn dropped_request_does_not_kill_or_leak_a_render() {
    let dir = tempfile::tempdir().unwrap();
    // Writes a fake WebP to the `-o` argument (minus its `[...]` options) after a delay.
    let fake = script(
        dir.path(),
        "slow.sh",
        "sleep 1\nout=${7%\\[*}\nprintf 'RIFF0000WEBP' > \"$out\"",
    );
    let thumbs = dir.path().join("thumbs");
    let p = Processor::with_tools(thumbs.clone(), 1, Duration::from_secs(30), &fake, "ffmpeg");
    let src = red();
    let early = tokio::time::timeout(
        Duration::from_millis(200),
        p.render(&src, MediaKind::Image, HASH, Preset::Thumb),
    )
    .await;
    assert!(early.is_err(), "the request was dropped mid-render");
    let out = p.thumb_path(HASH, Preset::Thumb).unwrap();
    for _ in 0..100 {
        if out.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(out.exists(), "the render still completed");
    assert_eq!(p.renders_started(), 1);
    assert!(leftovers(&thumbs).is_empty(), "{:?}", leftovers(&thumbs));
}

#[cfg(unix)]
#[tokio::test]
async fn failure_and_timeout_leave_no_tmp_files() {
    let dir = tempfile::tempdir().unwrap();
    let thumbs = dir.path().join("thumbs");
    let fail = script(dir.path(), "fail.sh", "echo 'not an image' >&2\nexit 1");
    let p = Processor::with_tools(thumbs.clone(), 1, Duration::from_secs(5), &fail, "ffmpeg");
    let r = p
        .render(&red(), MediaKind::Image, HASH, Preset::Thumb)
        .await;
    assert!(matches!(r, Err(ProcessError::Unprocessable(_))), "{r:?}");
    assert!(leftovers(&thumbs).is_empty());

    let hang = script(dir.path(), "hang.sh", "exec sleep 10");
    let p = Processor::with_tools(
        thumbs.clone(),
        1,
        Duration::from_millis(300),
        &hang,
        "ffmpeg",
    );
    let r = p
        .render(&red(), MediaKind::Image, HASH, Preset::Preview)
        .await;
    assert_eq!(r, Err(ProcessError::Unprocessable("timeout".into())));
    assert!(leftovers(&thumbs).is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn write_failures_are_io_not_unprocessable() {
    let dir = tempfile::tempdir().unwrap();
    let fail = script(
        dir.path(),
        "full.sh",
        "echo 'unable to write to target: No space left on device' >&2\nexit 1",
    );
    let p = Processor::with_tools(
        dir.path().join("thumbs"),
        1,
        Duration::from_secs(5),
        &fail,
        "ffmpeg",
    );
    let r = p
        .render(&red(), MediaKind::Image, HASH, Preset::Thumb)
        .await;
    assert!(matches!(r, Err(ProcessError::Io(_))), "{r:?}");
}

#[tokio::test]
async fn relative_paths_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let p = Processor::new(dir.path().join("thumbs"), 1, Duration::from_secs(5));
    let r = p
        .render(Path::new("red.jpg"), MediaKind::Image, HASH, Preset::Thumb)
        .await;
    assert!(matches!(r, Err(ProcessError::Io(_))), "{r:?}");
    for bad in [PathBuf::from("thumbs"), dir.path().join("100%")] {
        let p = Processor::new(bad, 1, Duration::from_secs(5));
        let r = p
            .render(&red(), MediaKind::Image, HASH, Preset::Thumb)
            .await;
        assert!(matches!(r, Err(ProcessError::Io(_))), "{r:?}");
    }
}

#[test]
fn sweep_removes_only_stale_tmp_files() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("thumb/ab");
    std::fs::create_dir_all(&sub).unwrap();
    for n in [".tmp-1-0.webp", ".src-1-0.jpg", "abcd.webp"] {
        std::fs::write(sub.join(n), b"x").unwrap();
    }
    let p = Processor::new(dir.path().to_path_buf(), 1, Duration::from_secs(5));
    assert_eq!(p.sweep_tmp(), 2);
    assert!(sub.join("abcd.webp").exists());
    assert!(leftovers(dir.path()).is_empty());
}

#[tokio::test]
async fn ffmpeg_refuses_concat_scripts() {
    if !have("ffmpeg") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // A real, renderable video next to the script: without the whitelists ffmpeg would
    // follow the script to it and succeed. Intra-only MJPEG, because the concat demuxer
    // cannot seek (`-ss`) into an H.264 file with B-frames, which would fail regardless.
    let real = dir.path().join("real.mp4");
    let st = std::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x64:rate=10:duration=2",
            "-c:v",
            "mjpeg",
            "-y",
        ])
        .arg(&real)
        .status()
        .unwrap();
    assert!(st.success());
    let evil = dir.path().join("evil.mkv");
    std::fs::write(&evil, "ffconcat version 1.0\nfile 'real.mp4'\n").unwrap();
    let p = Processor::new(dir.path().join("thumbs"), 1, Duration::from_secs(30));
    let r = p.render(&evil, MediaKind::Video, HASH, Preset::Thumb).await;
    assert!(matches!(r, Err(ProcessError::Unprocessable(_))), "{r:?}");
    assert!(leftovers(&dir.path().join("thumbs")).is_empty());
    // The real video itself renders, so the failure above is the script being refused.
    p.render(&real, MediaKind::Video, "blake3:ee99", Preset::Thumb)
        .await
        .unwrap();
}

/// Hashes `blake3:<n as 16 hex digits>`, distinct per `n`.
#[cfg(unix)]
fn nth_hash(n: usize) -> String {
    format!("blake3:{n:016x}")
}

#[cfg(unix)]
#[tokio::test]
async fn failure_with_no_waiters_is_still_recorded() {
    use std::sync::{Arc, Mutex};
    let dir = tempfile::tempdir().unwrap();
    let fail = script(
        dir.path(),
        "slowfail.sh",
        "sleep 1\necho 'not an image' >&2\nexit 1",
    );
    let p = Processor::with_tools(
        dir.path().join("thumbs"),
        1,
        Duration::from_secs(30),
        &fail,
        "ffmpeg",
    );
    let marks: Arc<Mutex<Vec<(String, Preset, String)>>> = Arc::default();
    let m = marks.clone();
    p.set_unprocessable_hook(move |hash, preset, reason| {
        m.lock()
            .unwrap()
            .push((hash.to_string(), preset, reason.to_string()))
    });
    let early = tokio::time::timeout(
        Duration::from_millis(200),
        p.render(&red(), MediaKind::Image, HASH, Preset::Thumb),
    )
    .await;
    assert!(early.is_err(), "the request was dropped mid-render");
    for _ in 0..100 {
        if !marks.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let marks = marks.lock().unwrap();
    assert_eq!(marks.len(), 1, "{marks:?}");
    assert_eq!((marks[0].0.as_str(), marks[0].1), (HASH, Preset::Thumb));
    assert!(marks[0].2.contains("not an image"), "{:?}", marks[0].2);
}

#[cfg(unix)]
#[tokio::test]
async fn abandoned_queued_render_never_starts() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("invocations");
    // Logs each run, then writes a fake WebP to the `-o` argument after a delay.
    let fake = script(
        dir.path(),
        "slow.sh",
        &format!(
            "echo run >> '{}'\nsleep 1\nout=${{7%\\[*}}\nprintf 'RIFF0000WEBP' > \"$out\"",
            log.display()
        ),
    );
    let p = std::sync::Arc::new(Processor::with_tools(
        dir.path().join("thumbs"),
        1,
        Duration::from_secs(30),
        &fake,
        "ffmpeg",
    ));
    let src = red();
    // The first render holds the only permit.
    let first = {
        let (p, src) = (p.clone(), src.clone());
        tokio::spawn(async move {
            p.render(&src, MediaKind::Image, &nth_hash(1), Preset::Thumb)
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The second waits for the permit, and its only request goes away.
    let dropped = tokio::time::timeout(
        Duration::from_millis(200),
        p.render(&src, MediaKind::Image, &nth_hash(2), Preset::Thumb),
    )
    .await;
    assert!(dropped.is_err());
    first.await.unwrap().unwrap();
    // Give the abandoned render the chance to (wrongly) run.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(p.renders_started(), 1);
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
    assert!(!p.thumb_path(&nth_hash(2), Preset::Thumb).unwrap().exists());
    assert_eq!(p.renders_queued(), 0);
    // A later request for it renders normally.
    p.render(&src, MediaKind::Image, &nth_hash(2), Preset::Thumb)
        .await
        .unwrap();
    assert_eq!(p.renders_started(), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn render_queue_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let hang = script(dir.path(), "hang.sh", "exec sleep 30");
    let p = std::sync::Arc::new(Processor::with_tools(
        dir.path().join("thumbs"),
        1,
        Duration::from_secs(60),
        &hang,
        "ffmpeg",
    ));
    let limit = p.queue_limit();
    assert_eq!(limit, 32);
    let mut waiters = Vec::new();
    for n in 0..limit {
        let p = p.clone();
        waiters.push(tokio::spawn(async move {
            p.render(&red(), MediaKind::Image, &nth_hash(n), Preset::Thumb)
                .await
        }));
    }
    for _ in 0..100 {
        if p.renders_queued() == limit {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(p.renders_queued(), limit);
    let r = p
        .render(&red(), MediaKind::Image, &nth_hash(limit), Preset::Thumb)
        .await;
    assert_eq!(r, Err(ProcessError::Busy));
    // Joining a render that is already queued is still allowed.
    let joined = tokio::time::timeout(
        Duration::from_millis(100),
        p.render(&red(), MediaKind::Image, &nth_hash(3), Preset::Thumb),
    )
    .await;
    assert!(joined.is_err(), "it waits for the queued render");
    assert_eq!(p.renders_queued(), limit);
    for w in waiters {
        w.abort();
    }
}

#[test]
fn offline_audio_preset_parses_and_names() {
    let p = Preset::parse("offline-audio").unwrap();
    assert_eq!(p, Preset::OfflineAudio);
    assert_eq!(p.name(), "offline-audio");
    assert_eq!(p.ext(), "ogg");
    assert_eq!(p.mime(), "audio/ogg");
    assert_eq!(p.timeout(Duration::from_secs(5)), Duration::from_secs(600));
    assert_eq!(Preset::Thumb.ext(), "webp");
    assert_eq!(Preset::Preview.mime(), "image/webp");
    assert_eq!(
        Preset::Thumb.timeout(Duration::from_secs(5)),
        Duration::from_secs(5)
    );
    let proc = Processor::with_tools("/t".into(), 1, Duration::from_secs(1), "v", "f");
    assert_eq!(
        proc.thumb_path(HASH, p).unwrap(),
        Path::new("/t/offline-audio/ab/abcdef0123456789.ogg")
    );
}

/// A processor without any external tool: offline audio needs none.
fn toolless(thumbs: PathBuf) -> Processor {
    Processor::with_tools(
        thumbs,
        1,
        Duration::from_secs(30),
        "/nonexistent/vipsthumbnail",
        "/nonexistent/ffmpeg",
    )
}

/// A 16-bit PCM mono WAV of `secs` seconds of a 440 Hz tone at 44.1 kHz.
fn wav(out: &Path, secs: u32) {
    let frames = 44_100 * secs;
    let mut b = Vec::new();
    b.extend(b"RIFF");
    b.extend((36 + frames * 2).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(44_100u32.to_le_bytes());
    b.extend(88_200u32.to_le_bytes());
    b.extend(2u16.to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend((frames * 2).to_le_bytes());
    for i in 0..frames {
        let s = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 44_100.0).sin() * 16_000.0;
        b.extend((s as i16).to_le_bytes());
    }
    std::fs::write(out, b).unwrap();
}

fn is_ogg_opus(b: &[u8]) -> bool {
    b.len() > 36 && &b[..4] == b"OggS" && &b[28..36] == b"OpusHead"
}

#[tokio::test]
async fn offline_audio_renders_ogg_opus_without_any_tool() {
    let dir = tempfile::tempdir().unwrap();
    let p = toolless(dir.path().join("thumbs"));
    let out = p
        .render(
            &fixture("sine440.flac"),
            MediaKind::Audio,
            HASH,
            Preset::OfflineAudio,
        )
        .await
        .unwrap();
    assert_eq!(
        out,
        dir.path()
            .join("thumbs/offline-audio/ab/abcdef0123456789.ogg")
    );
    assert!(is_ogg_opus(&std::fs::read(&out).unwrap()));
    let out = p
        .render(
            &fixture("sine440.mp3"),
            MediaKind::Audio,
            "blake3:ee99",
            Preset::OfflineAudio,
        )
        .await
        .unwrap();
    assert!(is_ogg_opus(&std::fs::read(out).unwrap()));
    // Other media are not audio, and no temporary files remain.
    let r = p
        .render(
            &red(),
            MediaKind::Image,
            "blake3:ff00",
            Preset::OfflineAudio,
        )
        .await;
    assert!(matches!(r, Err(ProcessError::Unprocessable(_))), "{r:?}");
    assert!(leftovers(&dir.path().join("thumbs")).is_empty());
}

#[tokio::test]
async fn a_broken_audio_file_is_unprocessable_and_recorded() {
    use std::sync::{Arc, Mutex};
    let dir = tempfile::tempdir().unwrap();
    let broken = dir.path().join("x.flac");
    std::fs::write(
        &broken,
        (0..4000u32)
            .map(|n| (n * 37 % 251) as u8)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    // What ffmpeg once had to be kept from following.
    let concat = dir.path().join("evil.mp3");
    std::fs::write(&concat, "ffconcat version 1.0\nfile 'x.flac'\n").unwrap();
    let p = toolless(dir.path().join("thumbs"));
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let r2 = recorded.clone();
    p.set_unprocessable_hook(move |h, _, _| r2.lock().unwrap().push(h.to_string()));
    for (src, hash) in [(&broken, HASH), (&concat, "blake3:ee99")] {
        let r = p
            .render(src, MediaKind::Audio, hash, Preset::OfflineAudio)
            .await;
        assert!(matches!(r, Err(ProcessError::Unprocessable(_))), "{r:?}");
    }
    assert_eq!(*recorded.lock().unwrap(), [HASH, "blake3:ee99"]);
    assert!(leftovers(&dir.path().join("thumbs")).is_empty());
}

#[tokio::test]
async fn image_bytes_are_rendered_once_per_key() {
    if !have("vipsthumbnail") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let bytes = std::fs::read(red()).unwrap();
    let p = Processor::new(dir.path().join("thumbs"), 2, Duration::from_secs(30));
    let (a, b) = tokio::join!(
        p.render_image_bytes(&bytes, "jpg", HASH, Preset::Thumb),
        p.render_image_bytes(&bytes, "jpg", HASH, Preset::Thumb)
    );
    assert_eq!(a.unwrap(), b.unwrap());
    assert_eq!(p.renders_started(), 1);
    assert!(leftovers(&dir.path().join("thumbs")).is_empty());
}

/// A fake `vipsthumbnail` that writes a few bytes to the output named after `-o`.
#[cfg(unix)]
fn quick_vips(dir: &Path) -> PathBuf {
    script(
        dir,
        "vips.sh",
        "while [ \"$1\" != -o ]; do shift; done\nprintf x > \"${2%%\\[*}\"",
    )
}

/// An offline-audio encoder that holds its slot until it is cancelled, then says so in
/// `stopped`.
fn hanging_encoder(
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> impl Fn(&Path, &Path, &AtomicBool) -> Result<(), ProcessError> + Send + Sync + 'static {
    move |_, _, cancel| {
        while !cancel.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
        stopped.store(true, Ordering::SeqCst);
        Err(ProcessError::TimedOut)
    }
}

#[cfg(unix)]
#[tokio::test]
async fn thumbnails_do_not_wait_for_offline_audio() {
    let dir = tempfile::tempdir().unwrap();
    let stopped = std::sync::Arc::new(AtomicBool::new(false));
    let p = std::sync::Arc::new(
        Processor::with_tools(
            dir.path().join("thumbs"),
            1,
            Duration::from_secs(60),
            quick_vips(dir.path()),
            "/nonexistent/ffmpeg",
        )
        .with_audio_timeout(Duration::from_secs(5))
        .with_audio_encoder(hanging_encoder(stopped)),
    );
    let audio = {
        let p = p.clone();
        tokio::spawn(async move {
            p.render(&red(), MediaKind::Audio, HASH, Preset::OfflineAudio)
                .await
        })
    };
    for _ in 0..100 {
        if p.renders_started() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(p.renders_started(), 1, "the encode holds its permit");
    let thumb = tokio::time::timeout(
        Duration::from_secs(5),
        p.render(&red(), MediaKind::Image, "blake3:ee99", Preset::Thumb),
    )
    .await
    .expect("the thumbnail does not wait for the encode");
    assert!(thumb.unwrap().exists());
    // A second encode does wait: audio has a single slot.
    let second = tokio::time::timeout(
        Duration::from_millis(300),
        p.render(
            &red(),
            MediaKind::Audio,
            "blake3:ee98",
            Preset::OfflineAudio,
        ),
    )
    .await;
    assert!(second.is_err());
    assert_eq!(p.renders_started(), 2);
    audio.abort();
}

#[tokio::test]
async fn an_offline_audio_timeout_cancels_the_encode_and_is_not_recorded() {
    use std::sync::{Arc, Mutex};
    let dir = tempfile::tempdir().unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let p = toolless(dir.path().join("thumbs"))
        .with_audio_timeout(Duration::from_millis(200))
        .with_audio_encoder(hanging_encoder(stopped.clone()));
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let r2 = recorded.clone();
    p.set_unprocessable_hook(move |h, _, _| r2.lock().unwrap().push(h.to_string()));
    let r = p
        .render(&red(), MediaKind::Audio, HASH, Preset::OfflineAudio)
        .await;
    assert_eq!(r, Err(ProcessError::TimedOut));
    assert!(
        stopped.load(Ordering::SeqCst),
        "the encode was told to stop, and had stopped, before the result came"
    );
    assert!(recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_long_real_encode_times_out_and_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let long = dir.path().join("long.wav");
    wav(&long, 120);
    let thumbs = dir.path().join("thumbs");
    let p = toolless(thumbs.clone()).with_audio_timeout(Duration::from_millis(100));
    let start = std::time::Instant::now();
    let r = p
        .render(&long, MediaKind::Audio, HASH, Preset::OfflineAudio)
        .await;
    assert_eq!(r, Err(ProcessError::TimedOut));
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(leftovers(&thumbs).is_empty());
    assert!(!p.thumb_path(HASH, Preset::OfflineAudio).unwrap().exists());
}

#[tokio::test]
async fn a_panicking_encode_is_unprocessable() {
    let dir = tempfile::tempdir().unwrap();
    let p = toolless(dir.path().join("thumbs")).with_audio_encoder(|_, _, _| panic!("boom"));
    let r = p
        .render(&red(), MediaKind::Audio, HASH, Preset::OfflineAudio)
        .await;
    assert!(matches!(r, Err(ProcessError::Unprocessable(_))), "{r:?}");
}
