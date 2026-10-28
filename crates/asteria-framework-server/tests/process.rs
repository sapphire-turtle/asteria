mod common;

use std::path::{Path, PathBuf};
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

#[cfg(unix)]
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

/// Makes a one-second sine tone at `out` (the format follows the extension).
fn tone(out: &Path, extra: &[&str]) {
    let st = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-f", "lavfi", "-i"])
        .arg("sine=frequency=440:duration=1")
        .args(extra)
        .arg("-y")
        .arg(out)
        .status()
        .unwrap();
    assert!(st.success());
}

fn has_libopus() -> bool {
    std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("libopus"))
}

#[tokio::test]
async fn offline_audio_renders_ogg_opus() {
    if !have("ffmpeg") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let flac = dir.path().join("sine440.flac");
    tone(&flac, &[]);
    let m4a = dir.path().join("sine440.m4a");
    tone(&m4a, &["-c:a", "aac"]);
    let p = Processor::new(dir.path().join("thumbs"), 1, Duration::from_secs(30));
    let r = p
        .render(&flac, MediaKind::Audio, HASH, Preset::OfflineAudio)
        .await;
    if !has_libopus() {
        assert_eq!(r, Err(ProcessError::ToolMissing("ffmpeg with libopus")));
        return;
    }
    let out = r.unwrap();
    assert_eq!(
        out,
        dir.path()
            .join("thumbs/offline-audio/ab/abcdef0123456789.ogg")
    );
    let b = std::fs::read(&out).unwrap();
    assert_eq!(&b[..4], b"OggS");
    assert!(b.windows(8).any(|w| w == b"OpusHead"));
    // The mov-family demuxer is on the whitelist too.
    let out = p
        .render(&m4a, MediaKind::Audio, "blake3:ee99", Preset::OfflineAudio)
        .await
        .unwrap();
    assert_eq!(&std::fs::read(out).unwrap()[..4], b"OggS");
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
async fn offline_audio_refuses_concat_scripts() {
    if !have("ffmpeg") || !has_libopus() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real.wav");
    tone(&real, &[]);
    let evil = dir.path().join("evil.mp3");
    std::fs::write(&evil, "ffconcat version 1.0\nfile 'real.wav'\n").unwrap();
    let p = Processor::new(dir.path().join("thumbs"), 1, Duration::from_secs(30));
    let r = p
        .render(&evil, MediaKind::Audio, HASH, Preset::OfflineAudio)
        .await;
    assert!(matches!(r, Err(ProcessError::Unprocessable(_))), "{r:?}");
    p.render(&real, MediaKind::Audio, "blake3:ee99", Preset::OfflineAudio)
        .await
        .unwrap();
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

#[cfg(unix)]
#[tokio::test]
async fn thumbnails_do_not_wait_for_offline_audio() {
    let dir = tempfile::tempdir().unwrap();
    let slow = script(dir.path(), "ffmpeg.sh", "exec sleep 30");
    let p = std::sync::Arc::new(Processor::with_tools(
        dir.path().join("thumbs"),
        1,
        Duration::from_secs(60),
        quick_vips(dir.path()),
        &slow,
    ));
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

#[cfg(unix)]
#[tokio::test]
async fn an_offline_audio_timeout_is_retryable_and_not_recorded() {
    use std::sync::{Arc, Mutex};
    let dir = tempfile::tempdir().unwrap();
    let hang = script(dir.path(), "ffmpeg.sh", "exec sleep 10");
    let p = Processor::with_tools(
        dir.path().join("thumbs"),
        1,
        Duration::from_secs(60),
        "vipsthumbnail",
        &hang,
    )
    .with_audio_timeout(Duration::from_millis(500));
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let r2 = recorded.clone();
    p.set_unprocessable_hook(move |h, _, reason| {
        r2.lock().unwrap().push((h.to_string(), reason.to_string()))
    });
    let r = p
        .render(&red(), MediaKind::Audio, HASH, Preset::OfflineAudio)
        .await;
    assert_eq!(r, Err(ProcessError::TimedOut));
    assert!(recorded.lock().unwrap().is_empty());
    // A decode failure is still recorded.
    let fail = script(dir.path(), "fail.sh", "echo 'invalid data' >&2\nexit 1");
    let p = Processor::with_tools(
        dir.path().join("thumbs"),
        1,
        Duration::from_secs(60),
        "vipsthumbnail",
        &fail,
    );
    let r2 = recorded.clone();
    p.set_unprocessable_hook(move |h, _, reason| {
        r2.lock().unwrap().push((h.to_string(), reason.to_string()))
    });
    let r = p
        .render(&red(), MediaKind::Audio, HASH, Preset::OfflineAudio)
        .await;
    assert!(matches!(r, Err(ProcessError::Unprocessable(_))), "{r:?}");
    assert_eq!(recorded.lock().unwrap().len(), 1);
}
