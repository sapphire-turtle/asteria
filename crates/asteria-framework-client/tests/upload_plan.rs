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
    assert_eq!(plan.excluded, 2);
}

#[test]
fn uppercase_xmp_extension_is_a_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "S/a.JPG");
    touch(dir.path(), "S/a.JPG.XMP");
    touch(dir.path(), "S/b.JPG");
    touch(dir.path(), "S/b.XMP");
    let plan = plan_drop(&[dir.path().join("S")], media);
    assert_eq!(
        paths(&plan),
        vec![
            // The server reads only a lowercase `.xmp`, so that is what is sent.
            vec!["S/a.JPG", "S/a.JPG.xmp"],
            vec!["S/b.JPG", "S/b.JPG.xmp"]
        ]
    );
    assert_eq!(
        plan.groups[0].files[1].local,
        dir.path().join("S/a.JPG.XMP")
    );
    assert_eq!(plan.excluded, 0);
}

#[test]
fn dotted_names_group_by_the_name_without_its_extension() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "D/Screenshot 2026-10-26 at 10.15.30.png");
    touch(dir.path(), "D/Screenshot 2026-10-26 at 10.42.07.png");
    touch(dir.path(), "D/2024.05.01 party.jpg");
    touch(dir.path(), "D/2024.05.02 beach.jpg");
    touch(dir.path(), "D/a.b.HEIC");
    touch(dir.path(), "D/a.b.MOV");
    touch(dir.path(), "D/a.b.HEIC.xmp");
    let plan = plan_drop(&[dir.path().join("D")], media);
    assert_eq!(
        paths(&plan),
        vec![
            vec!["D/2024.05.01 party.jpg"],
            vec!["D/2024.05.02 beach.jpg"],
            vec!["D/Screenshot 2026-10-26 at 10.15.30.png"],
            vec!["D/Screenshot 2026-10-26 at 10.42.07.png"],
            vec!["D/a.b.HEIC", "D/a.b.HEIC.xmp", "D/a.b.MOV"],
        ]
    );
    assert_eq!(plan.excluded, 0);
    assert!(plan.warnings.is_empty());
}

#[test]
fn lightroom_sidecar_of_a_dotted_name() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "D/IMG.1.jpg");
    touch(dir.path(), "D/IMG.1.xmp");
    touch(dir.path(), "D/IMG.2.jpg");
    let plan = plan_drop(&[dir.path().join("D")], media);
    assert_eq!(
        paths(&plan),
        vec![vec!["D/IMG.1.jpg", "D/IMG.1.jpg.xmp"], vec!["D/IMG.2.jpg"]]
    );
    assert_eq!(
        plan.groups[0].files[1].local,
        dir.path().join("D/IMG.1.xmp")
    );
    assert_eq!(plan.excluded, 0);
}

#[test]
fn a_sidecar_names_one_media_exactly() {
    let dir = tempfile::tempdir().unwrap();
    // `IMG.xmp` is no sidecar of `IMG.1.jpg` (its name without the extension is `IMG.1`).
    touch(dir.path(), "D/IMG.1.jpg");
    touch(dir.path(), "D/IMG.xmp");
    let plan = plan_drop(&[dir.path().join("D")], media);
    assert_eq!(paths(&plan), vec![vec!["D/IMG.1.jpg"]]);
    assert_eq!(plan.excluded, 1);
}

#[test]
fn media_sidecar_wins_over_stem_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "S/b.JPG");
    touch(dir.path(), "S/b.xmp");
    touch(dir.path(), "S/b.JPG.xmp");
    let plan = plan_drop(&[dir.path().join("S")], media);
    assert_eq!(paths(&plan), vec![vec!["S/b.JPG", "S/b.JPG.xmp"]]);
    assert_eq!(
        plan.groups[0].files[1].local,
        dir.path().join("S/b.JPG.xmp")
    );
    assert_eq!(plan.excluded, 1);
}

#[test]
fn a_file_reached_twice_is_planned_once() {
    let dir = tempfile::tempdir().unwrap();
    let a = touch(dir.path(), "D/a.jpg");
    let plan = plan_drop(&[a.clone(), a.clone()], media);
    assert_eq!(paths(&plan), vec![vec!["a.jpg"]]);
    let plan = plan_drop(&[dir.path().join("D"), a], media);
    assert_eq!(paths(&plan), vec![vec!["D/a.jpg"]]);
}

#[test]
fn accept_filters_kinds() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "D/a.jpg");
    touch(dir.path(), "D/b.mp3");
    let plan = plan_drop(&[dir.path().join("D")], |k| k == MediaKind::Image);
    assert_eq!(paths(&plan), vec![vec!["D/a.jpg"]]);
    assert_eq!(plan.excluded, 1);
}

#[cfg(unix)]
#[test]
fn names_the_server_would_reject_are_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "D/12:30.jpg");
    touch(dir.path(), "D/CON.jpg");
    touch(dir.path(), "D/ok.jpg");
    let plan = plan_drop(&[dir.path().join("D")], media);
    assert_eq!(paths(&plan), vec![vec!["D/ok.jpg"]]);
    let mut bad: Vec<_> = plan
        .warnings
        .iter()
        .map(|w| match w {
            PlanWarning::Unreadable { path, .. } => path.clone(),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    bad.sort();
    assert_eq!(
        bad,
        vec![dir.path().join("D/12:30.jpg"), dir.path().join("D/CON.jpg")]
    );
}
