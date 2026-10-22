mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
    let start = Instant::now();
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
    let target = dir.path().join("secret.txt");
    std::fs::write(&target, "x").unwrap();
    let evil = dir.path().join("evil.mkv");
    std::fs::write(
        &evil,
        format!("ffconcat version 1.0\nfile '{}'\n", target.display()),
    )
    .unwrap();
    let p = Processor::new(dir.path().join("thumbs"), 1, Duration::from_secs(30));
    let r = p.render(&evil, MediaKind::Video, HASH, Preset::Thumb).await;
    assert!(matches!(r, Err(ProcessError::Unprocessable(_))), "{r:?}");
    assert!(leftovers(&dir.path().join("thumbs")).is_empty());
}
