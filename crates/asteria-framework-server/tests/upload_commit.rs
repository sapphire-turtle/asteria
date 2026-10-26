use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use asteria_framework::hash::{ContentHash, file_hash};
use asteria_framework::journal::{Journal, OpKind};
use asteria_framework::{Library, MediaKind, UserId};
use asteria_framework_api::{UploadCommitParams, UploadCommitResult, UploadFile, UploadWarning};
use asteria_framework_server::upload::{UploadError, UploadStore};
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
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("lib");
    fs::create_dir(&root).unwrap();
    let lib = Arc::new(Library::init(&root, "owner").unwrap());
    let cache = Arc::new(Cache::open(&dir.path().join("cache"), &lib.config().id).unwrap());
    let store = Arc::new(UploadStore::open(&root).unwrap());
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
    assert!(!e.alive(&heic) && !e.alive(&mov));

    let entries: Vec<_> = Journal::new(&e.lib)
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
    let cases: [&[(&str, &str)]; 4] = [
        &[],
        &[(&a, "x.jpg"), (&b, "x.jpg")],
        &[(&a, "x.jpg"), (&a, "y.jpg")],
        // The sidecar names no media of the group.
        &[(&a, "x.jpg"), (&xmp, "y.jpg.xmp")],
    ];
    for files in cases {
        let r = e.commit(files, false);
        assert!(
            matches!(r, Err(UploadError::Invalid(_))),
            "{files:?}: {r:?}"
        );
    }
    assert!(e.inbox_is_empty());
    assert!(e.alive(&a) && e.alive(&b) && e.alive(&xmp));
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
    assert!(!e.alive(&s));
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
    assert!(!e.alive(&xmp) && !e.alive(&jpg));
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
