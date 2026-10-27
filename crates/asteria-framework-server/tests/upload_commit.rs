use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use asteria_framework::hash::{ContentHash, file_hash};
use asteria_framework::journal::OpKind;
use asteria_framework::{Library, MediaKind, UserId};
use asteria_framework_api::{UploadCommitParams, UploadCommitResult, UploadFile, UploadWarning};
use asteria_framework_server::upload::{UploadError, UploadLimits, UploadStore};
use asteria_framework_server::upload_commit::commit;
use asteria_framework_server::{Cache, MediaFilter, hash_pending, scan, spawn_writer};
use grain_id::GrainId;

struct Photos;
impl MediaFilter for Photos {
    fn accepts(&self, kind: MediaKind) -> bool {
        matches!(kind, MediaKind::Image | MediaKind::Video)
    }
}

struct Env {
    dir: tempfile::TempDir,
    root: PathBuf,
    lib: Arc<Library>,
    cache: Arc<Cache>,
    store: Arc<UploadStore>,
    me: UserId,
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../asteria-framework/tests/fixtures")
        .join(name)
}

fn env() -> Env {
    env_with_limits(UploadLimits::default())
}

fn env_with_limits(limits: UploadLimits) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lib");
    fs::create_dir(&root).unwrap();
    let lib = Arc::new(
        Library::init(&root, "owner")
            .unwrap()
            .with_state_dir(dir.path().join("state")),
    );
    let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
    let store = Arc::new(UploadStore::open_with_limits(&dir.path().join("state"), limits).unwrap());
    Env {
        dir,
        root,
        me: lib.owner(),
        lib,
        cache,
        store,
    }
}

impl Env {
    /// A finished session holding `bytes`, begun with their real file hash.
    fn upload(&self, bytes: &[u8]) -> String {
        let tmp = self.dir.path().join("hash-me");
        fs::write(&tmp, bytes).unwrap();
        self.upload_claiming(bytes, file_hash(&tmp).unwrap())
    }

    fn upload_claiming(&self, bytes: &[u8], claim: ContentHash) -> String {
        let s = self
            .store
            .begin(self.me, bytes.len() as u64, claim)
            .unwrap()
            .session;
        self.store.put_chunk(self.me, &s, 0, bytes).unwrap();
        s
    }

    fn commit(
        &self,
        files: &[(&str, &str)],
        allow_duplicate: bool,
    ) -> Result<UploadCommitResult, UploadError> {
        commit(
            &self.lib,
            &self.cache,
            &self.store,
            self.me,
            params(files, allow_duplicate),
        )
    }

    fn alive(&self, session: &str) -> bool {
        match self.store.status(self.me, session) {
            Ok(_) => true,
            Err(UploadError::NotFound) => false,
            Err(e) => panic!("{e}"),
        }
    }

    fn session_dir(&self, session: &str) -> PathBuf {
        self.dir.path().join("state/uploads").join(session)
    }

    /// Committed: only a tombstone is left, with nothing missing and no data.
    fn committed(&self, session: &str) -> bool {
        self.store
            .status(self.me, session)
            .is_ok_and(|s| s.missing.is_empty())
            && !self.session_dir(session).join("data").exists()
    }

    /// Every file under `Inbox/`, relative to it.
    fn inbox_files(&self) -> Vec<String> {
        let inbox = self.root.join("Inbox");
        let mut out: Vec<String> = walkdir::WalkDir::new(&inbox)
            .into_iter()
            .map(Result::unwrap)
            .filter(|d| d.file_type().is_file())
            .map(|d| {
                let rel = d.path().strip_prefix(&inbox).unwrap();
                rel.to_string_lossy().replace('\\', "/")
            })
            .collect();
        out.sort();
        out
    }

    fn inbox_is_empty(&self) -> bool {
        let inbox = self.root.join("Inbox");
        !inbox.exists() || fs::read_dir(inbox).unwrap().next().is_none()
    }
}

fn params(files: &[(&str, &str)], allow_duplicate: bool) -> UploadCommitParams {
    UploadCommitParams {
        files: files
            .iter()
            .map(|(session, path)| UploadFile {
                session: session.to_string(),
                path: path.to_string(),
            })
            .collect(),
        allow_duplicate,
    }
}

fn placed(r: UploadCommitResult) -> (Vec<(String, String)>, Vec<UploadWarning>) {
    match r {
        UploadCommitResult::Placed { files, warnings } => (
            files.into_iter().map(|f| (f.session, f.path)).collect(),
            warnings,
        ),
        other => panic!("expected Placed, got {other:?}"),
    }
}

/// `jpeg` with an APP1 (EXIF) segment inserted right after SOI: same payload, other bytes.
fn with_exif(jpeg: &[u8], payload: &[u8]) -> Vec<u8> {
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
    let len = (payload.len() + 2) as u16;
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    out.extend_from_slice(&jpeg[2..]);
    out
}

#[test]
fn places_a_group_under_inbox() {
    let e = env();
    let heic = e.upload(b"heic bytes");
    let mov = e.upload(b"mov bytes");
    let r = e
        .commit(
            &[(&heic, "a/IMG_0001.HEIC"), (&mov, "a/IMG_0001.MOV")],
            false,
        )
        .unwrap();
    let (files, warnings) = placed(r);
    assert_eq!(
        files,
        vec![
            (heic.clone(), "Inbox/a/IMG_0001.HEIC".to_string()),
            (mov.clone(), "Inbox/a/IMG_0001.MOV".to_string()),
        ]
    );
    assert!(warnings.is_empty());
    assert_eq!(
        fs::read(e.root.join("Inbox/a/IMG_0001.HEIC")).unwrap(),
        b"heic bytes"
    );
    assert_eq!(
        fs::read(e.root.join("Inbox/a/IMG_0001.MOV")).unwrap(),
        b"mov bytes"
    );
    assert!(e.committed(&heic) && e.committed(&mov));

    let entries: Vec<_> = e
        .lib
        .journal()
        .unwrap()
        .read_all()
        .unwrap()
        .into_iter()
        .filter(|j| j.kind == OpKind::Upload)
        .collect();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].before, serde_json::json!({ "session": heic }));
    assert_eq!(
        entries[0].after,
        serde_json::json!({ "path": "Inbox/a/IMG_0001.HEIC" })
    );
    assert_eq!(entries[1].before, serde_json::json!({ "session": mov }));
    assert_eq!(
        entries[1].after,
        serde_json::json!({ "path": "Inbox/a/IMG_0001.MOV" })
    );
}

#[test]
fn collision_numbers_the_whole_group() {
    let e = env();
    fs::create_dir(e.root.join("Inbox")).unwrap();
    fs::write(e.root.join("Inbox/IMG_0001.MOV"), b"older").unwrap();
    let heic = e.upload(b"heic bytes");
    let mov = e.upload(b"mov bytes");
    let xmp = e.upload(&fs::read(fixture("xmp/asteria_full.xmp")).unwrap());
    let r = e
        .commit(
            &[
                (&heic, "IMG_0001.HEIC"),
                (&mov, "IMG_0001.MOV"),
                (&xmp, "IMG_0001.HEIC.xmp"),
            ],
            false,
        )
        .unwrap();
    let (files, warnings) = placed(r);
    let paths: Vec<_> = files.iter().map(|(_, p)| p.as_str()).collect();
    assert_eq!(
        paths,
        [
            "Inbox/IMG_0001 (2).HEIC",
            "Inbox/IMG_0001 (2).MOV",
            "Inbox/IMG_0001 (2).HEIC.xmp"
        ]
    );
    assert!(warnings.is_empty());
    assert_eq!(
        fs::read(e.root.join("Inbox/IMG_0001.MOV")).unwrap(),
        b"older"
    );
    assert_eq!(
        fs::read(e.root.join("Inbox/IMG_0001 (2).MOV")).unwrap(),
        b"mov bytes"
    );
    assert!(!e.root.join("Inbox/IMG_0001.HEIC").exists());
}

#[test]
fn rejects_escaping_paths() {
    let e = env();
    let s = e.upload(b"jpeg bytes");
    for p in [
        "../x.jpg",
        "/x.jpg",
        "a/../../x.jpg",
        ".asteria/x.jpg",
        "a/.hidden/x.jpg",
        "a\\x.jpg",
        "C:x.jpg",
        "",
        "a//x.jpg",
        "x.jpg/",
        "a/./x.jpg",
        "a\0/x.jpg",
    ] {
        let r = e.commit(&[(&s, p)], false);
        assert!(matches!(r, Err(UploadError::Invalid(_))), "{p:?}: {r:?}");
        assert!(e.inbox_is_empty(), "{p:?}");
        assert!(e.alive(&s), "{p:?}");
    }
}

#[test]
fn group_in_two_folders_is_invalid() {
    let e = env();
    let a = e.upload(b"one");
    let b = e.upload(b"two");
    let r = e.commit(&[(&a, "a/IMG_0001.HEIC"), (&b, "b/IMG_0001.MOV")], false);
    assert!(matches!(r, Err(UploadError::Invalid(_))), "{r:?}");
    let r = e.commit(&[(&a, "IMG_0001.HEIC"), (&b, "a/IMG_0001.MOV")], false);
    assert!(matches!(r, Err(UploadError::Invalid(_))), "{r:?}");
    assert!(e.inbox_is_empty());
    assert!(e.alive(&a) && e.alive(&b));
}

#[test]
fn malformed_groups_are_invalid() {
    let e = env();
    let a = e.upload(b"one");
    let b = e.upload(b"two");
    let xmp = e.upload(&fs::read(fixture("xmp/asteria_full.xmp")).unwrap());
    let c = e.upload(&fs::read(fixture("xmp/asteria_full.xmp")).unwrap());
    let cases: [&[(&str, &str)]; 5] = [
        &[],
        &[(&a, "x.jpg"), (&b, "x.jpg")],
        &[(&a, "x.jpg"), (&a, "y.jpg")],
        // The sidecar names no media of the group.
        &[(&a, "x.jpg"), (&xmp, "y.jpg.xmp")],
        // Both sidecars would be placed as `x.jpg.xmp`.
        &[(&a, "x.jpg"), (&xmp, "x.jpg.xmp"), (&c, "x.jpg.XMP")],
    ];
    for files in cases {
        let r = e.commit(files, false);
        assert!(
            matches!(r, Err(UploadError::Invalid(_))),
            "{files:?}: {r:?}"
        );
    }
    assert!(e.inbox_is_empty());
    assert!(e.alive(&a) && e.alive(&b) && e.alive(&xmp) && e.alive(&c));
}

#[test]
fn hash_mismatch_discards_the_session() {
    let e = env();
    let s = e.upload_claiming(b"jpeg bytes", ContentHash::from_bytes([7; 32]));
    let r = e.commit(&[(&s, "x.jpg")], false);
    assert!(matches!(r, Err(UploadError::HashMismatch)), "{r:?}");
    assert!(matches!(
        e.store.status(e.me, &s),
        Err(UploadError::NotFound)
    ));
    assert!(e.inbox_is_empty());
}

#[test]
fn incomplete_or_foreign_sessions_are_refused() {
    let e = env();
    let half = e
        .store
        .begin(e.me, 10, ContentHash::from_bytes([7; 32]))
        .unwrap()
        .session;
    let r = e.commit(&[(&half, "x.jpg")], false);
    assert!(matches!(r, Err(UploadError::Incomplete)), "{r:?}");

    let mine = e.upload(b"jpeg bytes");
    let stranger = UserId(GrainId::random());
    let r = commit(
        &e.lib,
        &e.cache,
        &e.store,
        stranger,
        params(&[(&mine, "x.jpg")], false),
    );
    assert!(matches!(r, Err(UploadError::NotFound)), "{r:?}");
    assert!(e.alive(&mine));
    assert!(e.inbox_is_empty());
}

#[test]
fn duplicate_waits_for_permission() {
    let e = env();
    let red = fs::read(fixture("red.jpg")).unwrap();
    fs::write(e.root.join("a.jpg"), &red).unwrap();
    scan(&e.lib, &e.cache, &Photos).unwrap();
    hash_pending(&e.lib, &e.cache, &AtomicBool::new(false)).unwrap();

    let s = e.upload(&with_exif(&red, b"Exif\0\0other camera"));
    let r = e.commit(&[(&s, "red.jpg")], false).unwrap();
    assert_eq!(
        r,
        UploadCommitResult::Duplicate {
            existing: vec!["a.jpg".into()]
        }
    );
    assert!(e.alive(&s));
    assert!(e.inbox_is_empty());

    let (files, _) = placed(e.commit(&[(&s, "red.jpg")], true).unwrap());
    assert_eq!(files, vec![(s.clone(), "Inbox/red.jpg".to_string())]);
    assert!(e.root.join("Inbox/red.jpg").is_file());
    assert!(e.committed(&s));
}

#[test]
fn broken_xmp_is_dropped_with_warning() {
    let e = env();
    let jpg = e.upload(b"jpeg bytes");
    let xmp = e.upload(&fs::read(fixture("xmp/malformed.xmp")).unwrap());
    let (files, warnings) = placed(
        e.commit(&[(&jpg, "x.jpg"), (&xmp, "x.jpg.xmp")], false)
            .unwrap(),
    );
    assert_eq!(files, vec![(jpg.clone(), "Inbox/x.jpg".to_string())]);
    match warnings.as_slice() {
        [UploadWarning::SidecarDropped { session, reason }] => {
            assert_eq!(session, &xmp);
            assert!(!reason.is_empty());
        }
        other => panic!("{other:?}"),
    }
    assert!(e.root.join("Inbox/x.jpg").is_file());
    assert!(!e.root.join("Inbox/x.jpg.xmp").exists());
    assert!(e.committed(&xmp) && e.committed(&jpg));
}

#[test]
fn japanese_path_roundtrips() {
    let e = env();
    let s = e.upload(b"heic bytes");
    let (files, _) = placed(e.commit(&[(&s, "京都旅行/IMG_0001.HEIC")], false).unwrap());
    assert_eq!(files, vec![(s, "Inbox/京都旅行/IMG_0001.HEIC".to_string())]);
    assert_eq!(
        fs::read(e.root.join("Inbox/京都旅行/IMG_0001.HEIC")).unwrap(),
        b"heic bytes"
    );
}

#[tokio::test]
async fn writer_commits_through_the_queue() {
    let e = env();
    let s = e.upload(b"heic bytes");
    let (writer, _h) = spawn_writer(e.lib.clone(), e.cache.clone());
    let r = writer
        .commit_upload(
            e.store.clone(),
            e.me,
            params(&[(&s, "IMG_0001.HEIC")], false),
        )
        .await
        .unwrap();
    let (files, _) = placed(r);
    assert_eq!(files, vec![(s, "Inbox/IMG_0001.HEIC".to_string())]);
    assert!(e.root.join("Inbox/IMG_0001.HEIC").is_file());
}

#[test]
fn rejects_windows_hostile_paths() {
    let e = env();
    let s = e.upload(b"jpeg bytes");
    for p in [
        "CON",
        "con.jpg",
        "a/Nul.tar.gz",
        "AUX/x.jpg",
        "prn.jpg",
        "COM1.jpg",
        "lpt9/x.jpg",
        "x.jpg.",
        "x.jpg ",
        "a./x.jpg",
        "a /x.jpg",
        "x<1>.jpg",
        "x\".jpg",
        "x|y.jpg",
        "x?.jpg",
        "x*.jpg",
        "x\u{1}.jpg",
        "a\n/x.jpg",
        "x\u{7f}.jpg",
    ] {
        let r = e.commit(&[(&s, p)], false);
        assert!(matches!(r, Err(UploadError::Invalid(_))), "{p:?}: {r:?}");
    }
    assert!(e.inbox_is_empty());
    // Names that merely contain a reserved word are fine.
    let (files, _) = placed(e.commit(&[(&s, "CONCERT/COM10 x.jpg")], false).unwrap());
    assert_eq!(files[0].1, "Inbox/CONCERT/COM10 x.jpg");
}

#[test]
fn a_stray_sidecar_is_not_adopted() {
    let e = env();
    fs::create_dir(e.root.join("Inbox")).unwrap();
    fs::write(e.root.join("Inbox/IMG_0001.HEIC.xmp"), b"someone else's").unwrap();
    let s = e.upload(b"heic bytes");
    let (files, _) = placed(e.commit(&[(&s, "IMG_0001.HEIC")], false).unwrap());
    assert_eq!(files[0].1, "Inbox/IMG_0001 (2).HEIC");
    assert!(!e.root.join("Inbox/IMG_0001.HEIC").exists());
}

#[test]
fn collision_of_a_dotted_name_numbers_before_the_extension() {
    let e = env();
    fs::create_dir(e.root.join("Inbox")).unwrap();
    fs::write(e.root.join("Inbox/a.b.HEIC"), b"older").unwrap();
    let heic = e.upload(b"heic bytes");
    let xmp = e.upload(&fs::read(fixture("xmp/asteria_full.xmp")).unwrap());
    let (files, _) = placed(
        e.commit(&[(&heic, "a.b.HEIC"), (&xmp, "a.b.HEIC.xmp")], false)
            .unwrap(),
    );
    let paths: Vec<_> = files.iter().map(|(_, p)| p.as_str()).collect();
    assert_eq!(paths, ["Inbox/a.b (2).HEIC", "Inbox/a.b (2).HEIC.xmp"]);
    assert!(e.root.join("Inbox/a.b (2).HEIC.xmp").is_file());
}

#[test]
fn an_uppercase_sidecar_is_placed_as_lowercase_xmp() {
    let e = env();
    let jpg = e.upload(b"jpeg bytes");
    let xmp = e.upload(&fs::read(fixture("xmp/asteria_full.xmp")).unwrap());
    let (files, warnings) = placed(
        e.commit(&[(&jpg, "x.jpg"), (&xmp, "x.jpg.XMP")], false)
            .unwrap(),
    );
    assert!(warnings.is_empty());
    assert_eq!(
        files,
        vec![
            (jpg, "Inbox/x.jpg".to_string()),
            (xmp, "Inbox/x.jpg.xmp".to_string()),
        ]
    );
    let names: Vec<_> = fs::read_dir(e.root.join("Inbox"))
        .unwrap()
        .map(|d| d.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(names.contains(&"x.jpg.xmp".to_string()), "{names:?}");
    assert!(!names.contains(&"x.jpg.XMP".to_string()), "{names:?}");
}

#[test]
fn a_repeated_commit_returns_the_first_result() {
    let e = env();
    let heic = e.upload(b"heic bytes");
    let mov = e.upload(b"mov bytes");
    let xmp = e.upload(&fs::read(fixture("xmp/malformed.xmp")).unwrap());
    let files = [
        (heic.as_str(), "a/IMG_0001.HEIC"),
        (mov.as_str(), "a/IMG_0001.MOV"),
        (xmp.as_str(), "a/IMG_0001.HEIC.xmp"),
    ];
    let first = e.commit(&files, true).unwrap();
    // The answer was lost; the client asks again, in any order, with or without permission.
    let mut again = files;
    again.reverse();
    assert_eq!(e.commit(&again, false).unwrap(), first);
    assert_eq!(e.commit(&files, true).unwrap(), first);
    assert_eq!(e.inbox_files(), ["a/IMG_0001.HEIC", "a/IMG_0001.MOV"]);
    let journaled = e
        .lib
        .journal()
        .unwrap()
        .read_all()
        .unwrap()
        .into_iter()
        .filter(|j| j.kind == OpKind::Upload)
        .count();
    assert_eq!(journaled, 2);
}

/// A crash while the tombstones were written leaves some of the group's sessions without
/// one. The commit happened (tombstones come after placing), so the repeated commit gets its
/// answer and the missing tombstones are written.
#[test]
fn a_commit_with_some_tombstones_missing_is_answered_and_completed() {
    let e = env();
    let heic = e.upload(b"heic bytes");
    let mov = e.upload(b"mov bytes");
    let files = [
        (heic.as_str(), "IMG_0001.HEIC"),
        (mov.as_str(), "IMG_0001.MOV"),
    ];
    let first = e.commit(&files, false).unwrap();
    fs::remove_file(e.session_dir(&mov).join("committed.json")).unwrap();
    // Its data may still be there (a staged commit drops it just before the tombstones).
    fs::write(e.session_dir(&mov).join("data"), b"mov bytes").unwrap();

    assert_eq!(e.commit(&files, false).unwrap(), first);
    assert!(e.committed(&heic) && e.committed(&mov));
    assert_eq!(e.inbox_files(), ["IMG_0001.HEIC", "IMG_0001.MOV"]);
    // Only one of them in the commit's set of sessions is not enough.
    assert!(matches!(
        e.commit(&files[..1], false),
        Err(UploadError::Invalid(_))
    ));
}

#[test]
fn status_of_a_committed_session_has_nothing_missing() {
    let e = env();
    let s = e.upload(b"jpeg bytes");
    placed(e.commit(&[(&s, "x.jpg")], false).unwrap());
    let status = e.store.status(e.me, &s).unwrap();
    assert_eq!(status.size, 10);
    assert!(status.missing.is_empty());
    // Still the owner's alone, and closed to more chunks.
    assert!(matches!(
        e.store.status(UserId(GrainId::random()), &s),
        Err(UploadError::NotFound)
    ));
    assert!(matches!(
        e.store.put_chunk(e.me, &s, 0, b"jpeg bytes"),
        Err(UploadError::Invalid(_))
    ));
    let r = commit(
        &e.lib,
        &e.cache,
        &e.store,
        UserId(GrainId::random()),
        params(&[(&s, "x.jpg")], false),
    );
    assert!(matches!(r, Err(UploadError::NotFound)), "{r:?}");
}

#[test]
fn a_commit_mixing_committed_and_other_sessions_is_invalid() {
    let e = env();
    let a = e.upload(b"one");
    let b = e.upload(b"two");
    placed(e.commit(&[(&a, "x.jpg"), (&b, "x.png")], false).unwrap());
    let c = e.upload(b"three");
    let cases: [&[(&str, &str)]; 2] = [
        // Part of the committed group.
        &[(&a, "x.jpg")],
        // The committed group plus a new session.
        &[(&a, "x.jpg"), (&b, "x.png"), (&c, "x.gif")],
    ];
    for files in cases {
        let r = e.commit(files, false);
        assert!(
            matches!(r, Err(UploadError::Invalid(_))),
            "{files:?}: {r:?}"
        );
    }
    assert!(e.alive(&c) && !e.committed(&c));
    assert_eq!(e.inbox_files(), ["x.jpg", "x.png"]);
}

#[test]
fn tombstones_take_no_quota_and_expire() {
    let e = env_with_limits(UploadLimits {
        max_pending_bytes: 10,
        max_sessions: 1,
    });
    let s = e.upload(b"jpeg bytes");
    placed(e.commit(&[(&s, "x.jpg")], false).unwrap());
    // The tombstone does not count: a full-size session fits beside it.
    let next = e.upload(b"0123456789");

    let now = SystemTime::now();
    assert_eq!(e.store.purge_expired(now), 0);
    assert!(e.committed(&s));
    let week = Duration::from_secs(7 * 24 * 60 * 60);
    let later = now + week + Duration::from_secs(60);
    // `next` is as old as the tombstone, so both go.
    assert_eq!(e.store.purge_expired(later), 2);
    assert!(!e.alive(&s) && !e.alive(&next));
}
