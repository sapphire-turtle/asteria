mod common;

use std::sync::Arc;
use std::time::Duration;

use asteria_framework_server::Processor;
use axum::http::StatusCode;
use common::*;

fn webp(b: &[u8]) -> bool {
    b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP"
}

#[tokio::test]
async fn media_routes_require_token() {
    let e = env();
    let id = item_id(&e, "a.jpg");
    for what in ["original", "thumb"] {
        let (s, _, _) = get(&e.app, None, &format!("/media/{id}/{what}"), None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn original_supports_range() {
    let e = env();
    let id = item_id(&e, "a.jpg");
    let full = std::fs::read(e.root.join("a.jpg")).unwrap();
    let (s, h, b) = get(
        &e.app,
        Some(&e.owner_token),
        &format!("/media/{id}/original"),
        Some("bytes=0-9"),
    )
    .await;
    assert_eq!(s, StatusCode::PARTIAL_CONTENT);
    assert_eq!(b, full[..10]);
    assert_eq!(
        h["content-range"].to_str().unwrap(),
        format!("bytes 0-9/{}", full.len())
    );
    assert_eq!(h["content-type"], "image/jpeg");
    let (s, _, b) = get(
        &e.app,
        Some(&e.owner_token),
        &format!("/media/{id}/original"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b, full);
}

#[tokio::test]
async fn original_of_deleted_file_is_404() {
    let e = env();
    let id = item_id(&e, "a.jpg");
    std::fs::remove_file(e.root.join("a.jpg")).unwrap();
    let (s, _, _) = get(
        &e.app,
        Some(&e.owner_token),
        &format!("/media/{id}/original"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = get(
        &e.app,
        Some(&e.owner_token),
        "/media/no-such-id/original",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    hash_all(&e);
    let (s, _, _) = get(
        &e.app,
        Some(&e.owner_token),
        &format!("/media/{id}/thumb"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unknown_preset_is_404() {
    let e = env();
    hash_all(&e);
    let id = item_id(&e, "a.jpg");
    for what in ["huge", "Thumb", "", "thumb.webp"] {
        let (s, _, _) = get(
            &e.app,
            Some(&e.owner_token),
            &format!("/media/{id}/{what}"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{what:?}");
    }
}

#[tokio::test]
async fn thumb_before_hash_is_503_with_retry_after() {
    let e = env();
    let id = item_id(&e, "a.jpg");
    let (s, h, _) = get(
        &e.app,
        Some(&e.owner_token),
        &format!("/media/{id}/thumb"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(h["retry-after"], "2");
}

#[tokio::test]
async fn original_of_changed_file_is_still_served() {
    let e = env();
    let id = item_id(&e, "a.jpg");
    std::fs::write(e.root.join("a.jpg"), b"changed after the scan").unwrap();
    let (s, h, b) = get(
        &e.app,
        Some(&e.owner_token),
        &format!("/media/{id}/original"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b, b"changed after the scan");
    assert_eq!(h["x-content-type-options"], "nosniff");
}

#[tokio::test]
async fn thumb_of_changed_file_is_503() {
    let mut e = env();
    hash_all(&e);
    let id = item_id(&e, "a.jpg");
    // Same record, different file on disk: the stored hash describes older content.
    std::fs::write(e.root.join("a.jpg"), b"changed after the scan").unwrap();
    let (s, h, _) = get(
        &e.app,
        Some(&e.owner_token),
        &format!("/media/{id}/thumb"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(h["retry-after"], "2");
    assert!(e._rescan_rx.try_recv().is_ok(), "a rescan was requested");
}

#[tokio::test]
async fn thumb_is_webp_and_cached() {
    if !have_tool("vipsthumbnail") {
        return;
    }
    let e = env();
    hash_all(&e);
    let id = item_id(&e, "a.jpg");
    let uri = format!("/media/{id}/thumb");
    let (s, h, b) = get(&e.app, Some(&e.owner_token), &uri, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["content-type"], "image/webp");
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert!(webp(&b));
    assert_eq!(e.processor.renders_started(), 1);

    let hash = e
        .cache
        .get_by_id(&asteria_framework_api::ItemId(id.clone()))
        .unwrap()
        .unwrap()
        .hash
        .unwrap()
        .hash;
    let path = e
        .processor
        .thumb_path(&hash, asteria_framework_server::Preset::Thumb)
        .unwrap();
    assert!(path.starts_with(e._dir.path().join("cache/thumbs/thumb")));
    let m1 = std::fs::metadata(&path).unwrap().modified().unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let (s, _, b2) = get(&e.app, Some(&e.owner_token), &uri, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b, b2);
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), m1);
    assert_eq!(e.processor.renders_started(), 1);
}

#[tokio::test]
async fn corrupt_image_is_422_and_not_retried() {
    if !have_tool("vipsthumbnail") {
        return;
    }
    let e = env();
    let good = std::fs::read(e.root.join("a.jpg")).unwrap();
    // A JPEG start-of-image marker followed by garbage: no decoder accepts it.
    let _ = good;
    std::fs::write(
        e.root.join("bad.jpg"),
        [0xFF, 0xD8, 0xFF, 0x00, 1, 2, 3, 4, 5, 6],
    )
    .unwrap();
    asteria_framework_server::scan(&e.lib, &e.cache, &Gallery).unwrap();
    hash_all(&e);
    let id = item_id(&e, "bad.jpg");
    let uri = format!("/media/{id}/thumb");
    let (s, _, _) = get(&e.app, Some(&e.owner_token), &uri, None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(e.processor.renders_started(), 1);
    let (s, _, _) = get(&e.app, Some(&e.owner_token), &uri, None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(e.processor.renders_started(), 1);
}

#[tokio::test]
async fn missing_tool_is_503_and_not_marked_unprocessable() {
    let e = env_with(|t| {
        Processor::with_tools(t, 2, Duration::from_secs(5), "/nonexistent/vips", "ffmpeg")
    });
    hash_all(&e);
    let id = item_id(&e, "a.jpg");
    let uri = format!("/media/{id}/thumb");
    let (s, h, _) = get(&e.app, Some(&e.owner_token), &uri, None).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(h["retry-after"], "2");
    let (s, _, _) = get(&e.app, Some(&e.owner_token), &uri, None).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(e.processor.renders_started(), 2);
}

#[tokio::test]
async fn concurrent_requests_render_once() {
    if !have_tool("vipsthumbnail") {
        return;
    }
    let e = Arc::new(env());
    hash_all(&e);
    let id = item_id(&e, "a.jpg");
    let tasks: Vec<_> = (0..10)
        .map(|_| {
            let (e, id) = (e.clone(), id.clone());
            tokio::spawn(async move {
                get(
                    &e.app,
                    Some(&e.owner_token),
                    &format!("/media/{id}/thumb"),
                    None,
                )
                .await
            })
        })
        .collect();
    for t in tasks {
        let (s, _, b) = t.await.unwrap();
        assert_eq!(s, StatusCode::OK);
        assert!(webp(&b));
    }
    assert_eq!(e.processor.renders_started(), 1);
}

#[tokio::test]
async fn video_thumb() {
    if !have_tool("ffmpeg") {
        return;
    }
    let e = env();
    let out = std::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=10:duration=2",
            "-y",
        ])
        .arg(e.root.join("v.mp4"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    asteria_framework_server::scan(&e.lib, &e.cache, &Gallery).unwrap();
    hash_all(&e);
    let id = item_id(&e, "v.mp4");
    let (s, h, b) = get(
        &e.app,
        Some(&e.owner_token),
        &format!("/media/{id}/thumb"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["content-type"], "image/webp");
    assert!(webp(&b));
}

#[cfg(unix)]
fn script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(unix)]
#[tokio::test]
async fn failure_after_the_request_left_is_still_marked_unprocessable() {
    let tools = tempfile::tempdir().unwrap();
    let fail = script(
        tools.path(),
        "slowfail.sh",
        "sleep 1\necho 'bad data' >&2\nexit 1",
    );
    let e = env_with(|t| Processor::with_tools(t, 1, Duration::from_secs(30), &fail, "ffmpeg"));
    hash_all(&e);
    let id = item_id(&e, "a.jpg");
    let uri = format!("/media/{id}/thumb");
    let early = tokio::time::timeout(
        Duration::from_millis(200),
        get(&e.app, Some(&e.owner_token), &uri, None),
    )
    .await;
    assert!(early.is_err(), "the request was dropped mid-render");
    let hash = e
        .cache
        .get_by_path("a.jpg")
        .unwrap()
        .unwrap()
        .hash
        .unwrap()
        .hash;
    let mut marked = None;
    for _ in 0..100 {
        marked = e.cache.unprocessable(&hash, "thumb").unwrap();
        if marked.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(marked.is_some(), "the failure was recorded");
    let (s, _, _) = get(&e.app, Some(&e.owner_token), &uri, None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(e.processor.renders_started(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn full_render_queue_is_503_with_retry_after() {
    let tools = tempfile::tempdir().unwrap();
    let hang = script(tools.path(), "hang.sh", "exec sleep 30");
    let e = env_with(|t| Processor::with_tools(t, 1, Duration::from_secs(60), &hang, "ffmpeg"));
    hash_all(&e);
    let limit = e.processor.queue_limit();
    let src = e.root.join("sub/b.jpg");
    let mut waiters = Vec::new();
    for n in 0..limit {
        let (p, src) = (e.processor.clone(), src.clone());
        waiters.push(tokio::spawn(async move {
            p.render(
                &src,
                asteria_framework::MediaKind::Image,
                &format!("blake3:{n:016x}"),
                asteria_framework_server::Preset::Thumb,
            )
            .await
        }));
    }
    for _ in 0..100 {
        if e.processor.renders_queued() == limit {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(e.processor.renders_queued(), limit);
    let id = item_id(&e, "a.jpg");
    let (s, h, _) = get(
        &e.app,
        Some(&e.owner_token),
        &format!("/media/{id}/thumb"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(h["retry-after"], "2");
    for w in waiters {
        w.abort();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn file_swapped_for_an_escaping_symlink_is_404() {
    let e = env();
    hash_all(&e);
    let outside = e._dir.path().join("outside.jpg");
    std::fs::copy(e.root.join("a.jpg"), &outside).unwrap();
    let id_a = item_id(&e, "a.jpg");
    let id_b = item_id(&e, "sub/b.jpg");
    // No rescan: the records still describe the regular files.
    std::fs::remove_file(e.root.join("a.jpg")).unwrap();
    std::os::unix::fs::symlink(&outside, e.root.join("a.jpg")).unwrap();
    std::fs::remove_file(e.root.join("sub/b.jpg")).unwrap();
    std::os::unix::fs::symlink(
        e.root.join(".asteria/library.toml"),
        e.root.join("sub/b.jpg"),
    )
    .unwrap();
    for id in [&id_a, &id_b] {
        for what in ["original", "thumb"] {
            let (s, _, b) = get(
                &e.app,
                Some(&e.owner_token),
                &format!("/media/{id}/{what}"),
                None,
            )
            .await;
            assert_eq!(s, StatusCode::NOT_FOUND, "{id} {what}");
            assert!(b.is_empty());
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_inside_the_library_is_served() {
    let e = env();
    std::fs::remove_file(e.root.join("sub/b.jpg")).unwrap();
    std::os::unix::fs::symlink(e.root.join("a.jpg"), e.root.join("sub/b.jpg")).unwrap();
    let id = item_id(&e, "sub/b.jpg");
    let (s, _, b) = get(
        &e.app,
        Some(&e.owner_token),
        &format!("/media/{id}/original"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b, std::fs::read(e.root.join("a.jpg")).unwrap());
}
