use std::fs;
use std::path::{Path, PathBuf};

use asteria_framework::MediaKind;
use asteria_framework_client::upload::{DropPlan, PlanWarning, plan_drop};

fn touch(root: &Path, rel: &str) -> PathBuf {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, b"x").unwrap();
    p
}

fn media(kind: MediaKind) -> bool {
    matches!(kind, MediaKind::Image | MediaKind::Video)
}

fn paths(plan: &DropPlan) -> Vec<Vec<&str>> {
    plan.groups
        .iter()
        .map(|g| g.files.iter().map(|f| f.inbox_path.as_str()).collect())
        .collect()
}

#[test]
fn folder_keeps_its_structure() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "Trip/day1/a.jpg");
    touch(dir.path(), "Trip/b.png");
    let plan = plan_drop(&[dir.path().join("Trip")], media);
    assert_eq!(
        paths(&plan),
        vec![vec!["Trip/b.png"], vec!["Trip/day1/a.jpg"]]
    );
    assert_eq!(
        plan.groups[1].files[0].local,
        dir.path().join("Trip/day1/a.jpg")
    );
    assert_eq!(plan.excluded, 0);
    assert!(plan.warnings.is_empty());
}

#[test]
fn live_photo_pair_is_one_group() {
    let dir = tempfile::tempdir().unwrap();
    let a = touch(dir.path(), "IMG_1.HEIC");
    let b = touch(dir.path(), "IMG_1.MOV");
    touch(dir.path(), "IMG_2.JPG");
    let plan = plan_drop(&[b, a, dir.path().join("IMG_2.JPG")], media);
    assert_eq!(
        paths(&plan),
        vec![vec!["IMG_1.HEIC", "IMG_1.MOV"], vec!["IMG_2.JPG"]]
    );
}

#[test]
fn darktable_and_lightroom_sidecars() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "S/a.HEIC");
    touch(dir.path(), "S/a.HEIC.xmp");
    touch(dir.path(), "S/b.JPG");
    touch(dir.path(), "S/b.xmp");
    touch(dir.path(), "S/c.CR2");
    touch(dir.path(), "S/c.JPG");
    touch(dir.path(), "S/c.xmp");
    touch(dir.path(), "S/orphan.xmp");
    let plan = plan_drop(&[dir.path().join("S")], media);
    assert_eq!(
        paths(&plan),
        vec![
            vec!["S/a.HEIC", "S/a.HEIC.xmp"],
            vec!["S/b.JPG", "S/b.JPG.xmp"],
            vec!["S/c.CR2", "S/c.JPG"],
        ]
    );
    assert_eq!(plan.groups[1].files[1].local, dir.path().join("S/b.xmp"));
    assert_eq!(plan.excluded, 1);
    assert_eq!(
        plan.warnings,
        vec![PlanWarning::AmbiguousSidecar {
            path: dir.path().join("S/c.xmp")
        }]
    );
}

#[test]
fn non_media_and_hidden_are_excluded() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "D/.DS_Store");
    touch(dir.path(), "D/Thumbs.db");
    touch(dir.path(), "D/notes.txt");
    touch(dir.path(), "D/a.jpg");
    let plan = plan_drop(&[dir.path().join("D")], media);
    assert_eq!(paths(&plan), vec![vec!["D/a.jpg"]]);
    assert_eq!(plan.excluded, 3);
}

#[test]
fn hidden_directories_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "D/.cache/a.jpg");
    touch(dir.path(), "D/b.jpg");
    let plan = plan_drop(&[dir.path().join("D")], media);
    assert_eq!(paths(&plan), vec![vec!["D/b.jpg"]]);
    assert_eq!(plan.excluded, 1);
}

#[test]
fn same_name_from_two_places_stays_two_groups() {
    let dir = tempfile::tempdir().unwrap();
    let a = touch(dir.path(), "one/a.jpg");
    let b = touch(dir.path(), "two/a.jpg");
    let plan = plan_drop(&[a, b], media);
    assert_eq!(paths(&plan), vec![vec!["a.jpg"], vec!["a.jpg"]]);
}

#[test]
fn japanese_names_pass_through() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "旅行/写真1.jpg");
    let plan = plan_drop(&[dir.path().join("旅行")], media);
    assert_eq!(paths(&plan), vec![vec!["旅行/写真1.jpg"]]);
}

#[cfg(unix)]
#[test]
fn symlinks_are_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let real = touch(dir.path(), "elsewhere/real.jpg");
    touch(dir.path(), "D/a.jpg");
    std::os::unix::fs::symlink(&real, dir.path().join("D/link.jpg")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("elsewhere"), dir.path().join("D/dirlink")).unwrap();
    let plan = plan_drop(&[dir.path().join("D")], media);
    assert_eq!(paths(&plan), vec![vec!["D/a.jpg"]]);
}
