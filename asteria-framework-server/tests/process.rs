use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use asteria_framework::MediaKind;
use asteria_framework_server::{Preset, ProcessError, Processor};

const HASH: &str = "blake3:abcdef0123456789";

fn red() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../asteria-framework/tests/fixtures/red.jpg")
        .canonicalize()
        .unwrap()
}

fn have(tool: &str) -> bool {
    let ok = std::process::Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("skipping: `{tool}` is not on PATH");
    }
    ok
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
