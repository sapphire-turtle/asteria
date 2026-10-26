use std::fs;
use std::time::{Duration, SystemTime};

use asteria_framework::UserId;
use asteria_framework::hash::ContentHash;
use asteria_framework_api::{UPLOAD_CHUNK_SIZE, UPLOAD_MAX_FILE};
use asteria_framework_server::upload::{UploadError, UploadLimits, UploadStore};
use grain_id::GrainId;

const CHUNK: u64 = UPLOAD_CHUNK_SIZE;
const WEEK: Duration = Duration::from_secs(7 * 24 * 60 * 60);

fn user() -> UserId {
    UserId(GrainId::random())
}

fn hash() -> ContentHash {
    ContentHash::from_bytes([7; 32])
}

fn store() -> (tempfile::TempDir, UploadStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = UploadStore::open(dir.path()).unwrap();
    (dir, store)
}

fn pattern(len: u64) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn slice(data: &[u8], index: u64) -> &[u8] {
    let start = (index * CHUNK) as usize;
    &data[start..data.len().min(start + CHUNK as usize)]
}

#[test]
fn chunks_in_any_order_complete_the_file() {
    let (_dir, store) = store();
    let me = user();
    let size = 2 * CHUNK + 5;
    let data = pattern(size);
    let begin = store.begin(me, size, hash()).unwrap();
    assert_eq!(begin.chunk_size, CHUNK);

    assert!(matches!(
        store.completed(me, &begin.session),
        Err(UploadError::Incomplete)
    ));
    for index in [2, 0, 1] {
        store
            .put_chunk(me, &begin.session, index, slice(&data, index))
            .unwrap();
    }
    let done = store.completed(me, &begin.session).unwrap();
    assert_eq!(done.size, size);
    assert_eq!(done.file_hash, hash());
    assert_eq!(fs::read(&done.data_path).unwrap(), data);

    store.remove(&begin.session);
    assert!(matches!(
        store.status(me, &begin.session),
        Err(UploadError::NotFound)
    ));
}

#[test]
fn resending_a_chunk_is_idempotent() {
    let (_dir, store) = store();
    let me = user();
    let data = pattern(CHUNK + 3);
    let s = store.begin(me, data.len() as u64, hash()).unwrap().session;
    store.put_chunk(me, &s, 0, slice(&data, 0)).unwrap();
    store.put_chunk(me, &s, 0, slice(&data, 0)).unwrap();
    assert_eq!(store.status(me, &s).unwrap().missing, vec![1]);
    store.put_chunk(me, &s, 1, slice(&data, 1)).unwrap();
    store.put_chunk(me, &s, 1, slice(&data, 1)).unwrap();
    let done = store.completed(me, &s).unwrap();
    assert_eq!(fs::read(done.data_path).unwrap(), data);
}

#[test]
fn wrong_chunk_length_is_invalid() {
    let (_dir, store) = store();
    let me = user();
    let size = CHUNK + 10;
    let s = store.begin(me, size, hash()).unwrap().session;
    let full = vec![1u8; CHUNK as usize];
    // A non-final chunk that is short.
    assert!(matches!(
        store.put_chunk(me, &s, 0, &full[..10]),
        Err(UploadError::Invalid(_))
    ));
    // The final chunk too long, and too short.
    assert!(matches!(
        store.put_chunk(me, &s, 1, &full[..11]),
        Err(UploadError::Invalid(_))
    ));
    assert!(matches!(
        store.put_chunk(me, &s, 1, &full[..9]),
        Err(UploadError::Invalid(_))
    ));
    // Out of range.
    assert!(matches!(
        store.put_chunk(me, &s, 2, &full[..10]),
        Err(UploadError::Invalid(_))
    ));
    assert_eq!(store.status(me, &s).unwrap().missing, vec![0, 1]);
}

#[test]
fn status_lists_missing_chunks() {
    let (_dir, store) = store();
    let me = user();
    let size = 3 * CHUNK;
    let s = store.begin(me, size, hash()).unwrap().session;
    let st = store.status(me, &s).unwrap();
    assert_eq!((st.size, st.missing), (size, vec![0, 1, 2]));
    store
        .put_chunk(me, &s, 1, &vec![0u8; CHUNK as usize])
        .unwrap();
    assert_eq!(store.status(me, &s).unwrap().missing, vec![0, 2]);
}

#[test]
fn survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let me = user();
    let store = UploadStore::open(dir.path()).unwrap();
    let s = store.begin(me, 2 * CHUNK, hash()).unwrap().session;
    store
        .put_chunk(me, &s, 0, &vec![9u8; CHUNK as usize])
        .unwrap();
    let before = store.status(me, &s).unwrap();
    drop(store);

    let store = UploadStore::open(dir.path()).unwrap();
    assert_eq!(store.status(me, &s).unwrap(), before);
    store
        .put_chunk(me, &s, 1, &vec![9u8; CHUNK as usize])
        .unwrap();
    assert!(store.completed(me, &s).is_ok());
}

#[test]
fn other_user_gets_not_found() {
    let (_dir, store) = store();
    let (me, other) = (user(), user());
    let s = store.begin(me, 10, hash()).unwrap().session;
    assert!(matches!(
        store.status(other, &s),
        Err(UploadError::NotFound)
    ));
    assert!(matches!(
        store.put_chunk(other, &s, 0, &[0; 10]),
        Err(UploadError::NotFound)
    ));
    assert!(matches!(store.abort(other, &s), Err(UploadError::NotFound)));
    assert!(matches!(
        store.completed(other, &s),
        Err(UploadError::NotFound)
    ));
    // Not a session id at all.
    assert!(matches!(
        store.status(me, "../x"),
        Err(UploadError::NotFound)
    ));
    // The owner is unaffected and can abort.
    assert!(store.status(me, &s).is_ok());
    store.abort(me, &s).unwrap();
    assert!(matches!(store.status(me, &s), Err(UploadError::NotFound)));
}

#[test]
fn begin_rejects_bad_sizes() {
    let (_dir, store) = store();
    let me = user();
    assert!(matches!(
        store.begin(me, 0, hash()),
        Err(UploadError::Invalid(_))
    ));
    assert!(matches!(
        store.begin(me, UPLOAD_MAX_FILE + 1, hash()),
        Err(UploadError::Invalid(_))
    ));
}

#[test]
fn quota_by_bytes_and_count() {
    let dir = tempfile::tempdir().unwrap();
    let store = UploadStore::open_with_limits(
        dir.path(),
        UploadLimits {
            max_pending_bytes: 100,
            max_sessions: 3,
        },
    )
    .unwrap();
    let (me, other) = (user(), user());

    let a = store.begin(me, 60, hash()).unwrap().session;
    assert!(matches!(
        store.begin(me, 41, hash()),
        Err(UploadError::Quota(_))
    ));
    store.begin(me, 40, hash()).unwrap();
    // Another user has their own budget.
    store.begin(other, 100, hash()).unwrap();
    // Freeing space makes room again.
    store.abort(me, &a).unwrap();
    store.begin(me, 60, hash()).unwrap();

    let dir2 = tempfile::tempdir().unwrap();
    let store2 = UploadStore::open_with_limits(
        dir2.path(),
        UploadLimits {
            max_pending_bytes: 1000,
            max_sessions: 3,
        },
    )
    .unwrap();
    for _ in 0..3 {
        store2.begin(me, 1, hash()).unwrap();
    }
    assert!(matches!(
        store2.begin(me, 1, hash()),
        Err(UploadError::Quota(_))
    ));
}

#[test]
fn purge_removes_only_stale_sessions() {
    let (dir, store) = store();
    let me = user();
    let old = store.begin(me, 10, hash()).unwrap().session;
    let fresh = store.begin(me, 10, hash()).unwrap().session;

    let uploads = dir.path().join(".asteria").join("uploads");
    // Junk: not a session id, and a session dir without a readable session.json.
    fs::create_dir(uploads.join("junk")).unwrap();
    let broken = GrainId::random().to_string();
    fs::create_dir(uploads.join(&broken)).unwrap();

    let now = SystemTime::now();
    // Nothing is stale yet, except the junk.
    assert_eq!(store.purge_expired(now), 2);
    assert!(store.status(me, &old).is_ok());

    // `old` last got a chunk 7 days and a bit ago; `fresh` just now.
    let stamp = |s: &str, t: SystemTime| {
        let f = fs::OpenOptions::new()
            .write(true)
            .open(uploads.join(s).join("received"))
            .unwrap();
        f.set_modified(t).unwrap();
    };
    stamp(&old, now - WEEK - Duration::from_secs(1));
    stamp(&fresh, now);
    assert_eq!(store.purge_expired(now), 1);
    assert!(matches!(store.status(me, &old), Err(UploadError::NotFound)));
    assert!(store.status(me, &fresh).is_ok());
    assert!(!uploads.join("junk").exists());
    assert!(!uploads.join(&broken).exists());
}
