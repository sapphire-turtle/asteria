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
        .render(&heic(dir.path()), MediaKind::Image, HASH, Preset::Thumb)
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
        .render(&heic(dir.path()), MediaKind::Image, HASH, Preset::Thumb)
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
    let dir = tempfile::tempdir().unwrap();
    let seen = dir.path().join("seen");
    // Records the file name the tool is given, then writes a fake WebP to its output.
    let tool = script(
        dir.path(),
        "vips.sh",
        &format!(
            "echo \"$1\" >> '{}'\nwhile [ \"$1\" != -o ]; do shift; done\nprintf 'RIFF0000WEBP' > \"${{2%%\\[*}}\"",
            seen.display()
        ),
    );
    // A file the crate cannot read, so it goes to the tool, under a name with brackets.
    let tricky = dir.path().join("x.heic[shrink=8]");
    std::fs::write(&tricky, common::heic_bytes()).unwrap();
    let p = Processor::with_tools(
        dir.path().join("thumbs"),
        2,
        Duration::from_secs(30),
        &tool,
        "ffmpeg",
    );
    p.render(&tricky, MediaKind::Image, "blake3:bb22", Preset::Thumb)
        .await
        .unwrap();
    let given = std::fs::read_to_string(&seen).unwrap();
    assert_eq!(given.lines().count(), 1);
    assert!(!given.contains('['), "the tool was given {given:?}");
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
    let src = heic(dir.path());
    let spawn = |hash: &'static str| {
        let p = p.clone();
        let src = src.clone();
        tokio::spawn(async move { p.render(&src, MediaKind::Image, hash, Preset::Thumb).await })
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
    let src = heic(dir.path());
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
        .render(&heic(dir.path()), MediaKind::Image, HASH, Preset::Thumb)
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
        .render(&heic(dir.path()), MediaKind::Image, HASH, Preset::Preview)
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
        .render(&heic(dir.path()), MediaKind::Image, HASH, Preset::Thumb)
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
        p.render(&heic(dir.path()), MediaKind::Image, HASH, Preset::Thumb),
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
    let src = heic(dir.path());
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
    let src = heic(dir.path());
    let mut waiters = Vec::new();
    for n in 0..limit {
        let (p, src) = (p.clone(), src.clone());
        waiters.push(tokio::spawn(async move {
            p.render(&src, MediaKind::Image, &nth_hash(n), Preset::Thumb)
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
        .render(
            &heic(dir.path()),
            MediaKind::Image,
            &nth_hash(limit),
            Preset::Thumb,
        )
        .await;
    assert_eq!(r, Err(ProcessError::Busy));
    // Joining a render that is already queued is still allowed.
    let joined = tokio::time::timeout(
        Duration::from_millis(100),
        p.render(
            &heic(dir.path()),
            MediaKind::Image,
            &nth_hash(3),
            Preset::Thumb,
        ),
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
        p.render(
            &heic(dir.path()),
            MediaKind::Image,
            "blake3:ee99",
            Preset::Thumb,
        ),
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

// ---- thumbnails made in the server's own process ----

fn webp_size(bytes: &[u8]) -> (u32, u32) {
    let img =
        image::load_from_memory_with_format(bytes, image::ImageFormat::WebP).expect("a valid WebP");
    (img.width(), img.height())
}

fn encoded(width: u32, height: u32, format: image::ImageFormat) -> Vec<u8> {
    let img = image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x % 251) as u8, (y % 251) as u8, 90])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, format)
        .unwrap();
    out.into_inner()
}

/// A JPEG with an EXIF APP1 segment that sets the orientation.
fn jpeg_with_orientation(width: u32, height: u32, orientation: u16) -> Vec<u8> {
    let jpeg = encoded(width, height, image::ImageFormat::Jpeg);
    let mut tiff = b"MM\0*\0\0\0\x08".to_vec();
    tiff.extend_from_slice(&1u16.to_be_bytes()); // one entry
    tiff.extend_from_slice(&0x0112u16.to_be_bytes()); // Orientation
    tiff.extend_from_slice(&3u16.to_be_bytes()); // SHORT
    tiff.extend_from_slice(&1u32.to_be_bytes());
    tiff.extend_from_slice(&orientation.to_be_bytes());
    tiff.extend_from_slice(&[0, 0]);
    tiff.extend_from_slice(&0u32.to_be_bytes()); // no next IFD
    let mut app1 = b"Exif\0\0".to_vec();
    app1.extend_from_slice(&tiff);
    let mut out = jpeg[..2].to_vec(); // SOI
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&app1);
    out.extend_from_slice(&jpeg[2..]);
    out
}

/// A file the in-process decoder reports as unsupported (a HEIC), in `dir`.
fn heic(dir: &Path) -> PathBuf {
    common::write_heic(dir)
}

/// A fake image tool that counts its runs in `ran` and copies `webp` to the output.
#[cfg(unix)]
fn webp_tool(dir: &Path, webp: &Path, ran: &Path) -> PathBuf {
    script(
        dir,
        "webp-tool.sh",
        &format!(
            "echo x >> '{}'\nwhile [ \"$1\" != -o ]; do shift; done\ncp '{}' \"${{2%%\\[*}}\"",
            ran.display(),
            webp.display()
        ),
    )
}

/// A tool that records that it ran and fails.
#[cfg(unix)]
fn failing_tool(dir: &Path, ran: &Path) -> PathBuf {
    script(
        dir,
        "never.sh",
        &format!("echo x >> '{}'\nexit 1", ran.display()),
    )
}

fn runs(ran: &Path) -> usize {
    std::fs::read_to_string(ran).map_or(0, |s| s.lines().count())
}

#[cfg(unix)]
fn inprocess_processor(dir: &Path, tool: &Path) -> Processor {
    Processor::with_tools(
        dir.join("thumbs"),
        1,
        Duration::from_secs(30),
        tool,
        "ffmpeg",
    )
}

#[cfg(unix)]
#[tokio::test]
async fn a_jpeg_thumbnail_is_made_in_process() {
    let dir = tempfile::tempdir().unwrap();
    let ran = dir.path().join("ran");
    let tool = failing_tool(dir.path(), &ran);
    let src = dir.path().join("wide.jpg");
    std::fs::write(&src, encoded(1000, 500, image::ImageFormat::Jpeg)).unwrap();
    let p = inprocess_processor(dir.path(), &tool);
    let out = p
        .render(&src, MediaKind::Image, HASH, Preset::Thumb)
        .await
        .unwrap();
    assert_eq!(webp_size(&std::fs::read(out).unwrap()), (256, 128));
    assert_eq!(runs(&ran), 0, "the tool must not run");
    assert!(leftovers(&dir.path().join("thumbs")).is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn a_small_picture_is_not_enlarged() {
    let dir = tempfile::tempdir().unwrap();
    let ran = dir.path().join("ran");
    let tool = failing_tool(dir.path(), &ran);
    let src = dir.path().join("small.png");
    std::fs::write(&src, encoded(100, 50, image::ImageFormat::Png)).unwrap();
    let p = inprocess_processor(dir.path(), &tool);
    let out = p
        .render(&src, MediaKind::Image, HASH, Preset::Preview)
        .await
        .unwrap();
    assert_eq!(webp_size(&std::fs::read(out).unwrap()), (100, 50));
    assert_eq!(runs(&ran), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn the_exif_orientation_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let ran = dir.path().join("ran");
    let tool = failing_tool(dir.path(), &ran);
    let src = dir.path().join("turned.jpg");
    std::fs::write(&src, jpeg_with_orientation(400, 200, 6)).unwrap();
    let p = inprocess_processor(dir.path(), &tool);
    let out = p
        .render(&src, MediaKind::Image, HASH, Preset::Thumb)
        .await
        .unwrap();
    assert_eq!(webp_size(&std::fs::read(out).unwrap()), (128, 256));
    assert_eq!(runs(&ran), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn an_embedded_cover_needs_no_tool() {
    let dir = tempfile::tempdir().unwrap();
    let tool = dir.path().join("nonexistent-tool");
    let png = encoded(600, 600, image::ImageFormat::Png);
    let p = inprocess_processor(dir.path(), &tool);
    // The extension is wrong: the bytes name the format.
    let out = p
        .render_image_bytes(&png, "jpg", HASH, Preset::Thumb)
        .await
        .unwrap();
    assert_eq!(webp_size(&std::fs::read(out).unwrap()), (256, 256));
    assert!(leftovers(&dir.path().join("thumbs")).is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn an_image_the_crate_cannot_decode_goes_to_the_tool() {
    let dir = tempfile::tempdir().unwrap();
    let ran = dir.path().join("ran");
    let webp = dir.path().join("made.webp");
    std::fs::write(&webp, {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(40, 30));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::WebP).unwrap();
        out.into_inner()
    })
    .unwrap();
    let tool = webp_tool(dir.path(), &webp, &ran);
    let src = heic(dir.path());
    let p = inprocess_processor(dir.path(), &tool);
    let out = p
        .render(&src, MediaKind::Image, HASH, Preset::Thumb)
        .await
        .unwrap();
    assert_eq!(webp_size(&std::fs::read(out).unwrap()), (40, 30));
    assert_eq!(runs(&ran), 1);
    assert!(leftovers(&dir.path().join("thumbs")).is_empty());
}

/// The CRC-32 a PNG chunk carries.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A tiny PNG whose header claims `width`×`height`; its pixels are not there.
fn png_claiming(width: u32, height: u32) -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(
        &mut out,
        b"IDAT",
        &[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
    );
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(unix)]
#[tokio::test]
async fn a_picture_over_the_limits_goes_to_the_tool() {
    let dir = tempfile::tempdir().unwrap();
    let ran = dir.path().join("ran");
    let webp = dir.path().join("made.webp");
    std::fs::write(&webp, {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(20, 10));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::WebP).unwrap();
        out.into_inner()
    })
    .unwrap();
    let tool = webp_tool(dir.path(), &webp, &ran);
    let src = dir.path().join("huge.png");
    std::fs::write(&src, png_claiming(20_000, 20_000)).unwrap();
    let p = inprocess_processor(dir.path(), &tool);
    let out = p
        .render(&src, MediaKind::Image, HASH, Preset::Thumb)
        .await
        .unwrap();
    assert_eq!(webp_size(&std::fs::read(out).unwrap()), (20, 10));
    assert_eq!(runs(&ran), 1, "the tool made it, not the process");
}

#[cfg(unix)]
#[tokio::test]
async fn with_no_tool_an_undecodable_image_waits_for_one() {
    let dir = tempfile::tempdir().unwrap();
    let src = heic(dir.path());
    let p = inprocess_processor(dir.path(), &dir.path().join("nonexistent-tool"));
    let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = recorded.clone();
    p.set_unprocessable_hook(move |hash, _, reason| {
        sink.lock()
            .unwrap()
            .push((hash.to_string(), reason.to_string()));
    });
    let r = p.render(&src, MediaKind::Image, HASH, Preset::Thumb).await;
    assert!(
        matches!(r, Err(ProcessError::ToolMissing(_))),
        "expected ToolMissing, got {r:?}"
    );
    assert!(
        recorded.lock().unwrap().is_empty(),
        "a missing tool is not recorded as unprocessable"
    );
    assert!(leftovers(&dir.path().join("thumbs")).is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn a_missing_source_is_an_io_error() {
    let dir = tempfile::tempdir().unwrap();
    let ran = dir.path().join("ran");
    let tool = failing_tool(dir.path(), &ran);
    let p = inprocess_processor(dir.path(), &tool);
    let r = p
        .render(
            &dir.path().join("gone.jpg"),
            MediaKind::Image,
            HASH,
            Preset::Thumb,
        )
        .await;
    assert!(matches!(r, Err(ProcessError::Io(_))), "{r:?}");
    assert_eq!(
        runs(&ran),
        0,
        "an unreadable source does not go to the tool"
    );
}
