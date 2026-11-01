//! API keys: an owner key acts as the library's owner and another profile is forbidden,
//! keys added or retired while the server runs take effect, and an older config's
//! `[[token]]` entries become keys.

mod common;

use std::fs;
use std::time::Duration;

use asteria_framework::Library;
use asteria_framework::journal::OpKind;
use asteria_framework_server::keys::{KeyVerifier, Verdict};
use asteria_framework_server::{ServerConfig, default_config_path, load_and_migrate_in};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::*;
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

const INFO: &str = r#"{"jsonrpc":"2.0","id":1,"method":"server.info"}"#;

/// POSTs `INFO` with `authorization`; the status, the `WWW-Authenticate` header and the body.
async fn info(e: &Env, authorization: Option<&str>) -> (StatusCode, Option<String>, Vec<u8>) {
    let mut req = Request::post("/rpc");
    if let Some(a) = authorization {
        req = req.header(header::AUTHORIZATION, a);
    }
    let resp = e
        .app
        .clone()
        .oneshot(req.body(Body::from(INFO)).unwrap())
        .await
        .unwrap();
    let (parts, body) = resp.into_parts();
    let challenge = parts
        .headers
        .get(header::WWW_AUTHENTICATE)
        .map(|v| v.to_str().unwrap().to_owned());
    let body = body.collect().await.unwrap().to_bytes().to_vec();
    (parts.status, challenge, body)
}

#[tokio::test]
async fn a_new_key_works_without_a_restart() {
    let e = env();
    // The verifier has read the store once already.
    call(&e.app, &e.owner_token, "server.info", json!({})).await;
    let mut keys = key_store(&e.keys_dir);
    let (_, token) = keys
        .add(
            "later",
            None,
            asteria_framework_server::keys::ProfileId::new("owner").unwrap(),
        )
        .unwrap();
    assert!(token.expose().starts_with("asteria-photos-"));
    let (status, _, _) = info(&e, Some(&format!("Bearer {}", token.expose()))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_retired_key_stops_working() {
    let e = env_ttl(Duration::ZERO);
    let bearer = format!("Bearer {}", e.other_token);
    assert_eq!(info(&e, Some(&bearer)).await.0, StatusCode::OK);
    key_store(&e.keys_dir).retire("other").unwrap();
    assert_eq!(info(&e, Some(&bearer)).await.0, StatusCode::UNAUTHORIZED);
    // The other key is untouched.
    let owner = format!("Bearer {}", e.owner_token);
    assert_eq!(info(&e, Some(&owner)).await.0, StatusCode::OK);
}

#[tokio::test]
async fn a_key_of_another_profile_is_forbidden() {
    let e = env();
    let mut keys = key_store(&e.keys_dir);
    let reader = asteria_framework_server::keys::ProfileId::new("reader").unwrap();
    let (_, token) = keys.add("pad", None, reader).unwrap();
    let (status, _, body) = info(&e, Some(&format!("Bearer {}", token.expose()))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.is_empty());
}

#[tokio::test]
async fn refused_has_no_body_and_names_bearer() {
    let e = env();
    let wrong = format!("Bearer {}x", e.owner_token);
    for auth in [None, Some(wrong.as_str()), Some("Basic Zm9vOmJhcg==")] {
        let (status, challenge, body) = info(&e, auth).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{auth:?}");
        assert_eq!(challenge.as_deref(), Some("Bearer"), "{auth:?}");
        assert!(body.is_empty(), "{auth:?}");
    }
    // The scheme's case does not matter.
    let lower = format!("bearer {}", e.owner_token);
    assert_eq!(info(&e, Some(&lower)).await.0, StatusCode::OK);
}

#[tokio::test]
async fn every_key_acts_as_the_owner() {
    let e = env();
    let id = item_id(&e, "a.jpg");
    call(
        &e.app,
        &e.other_token,
        "media.update",
        json!({"id": id, "rating": 4}),
    )
    .await;
    let got = call(&e.app, &e.owner_token, "media.get", json!({"id": id})).await;
    assert_eq!(got["result"]["rating"], 4);
    let write = e
        .lib
        .journal()
        .unwrap()
        .read_all()
        .unwrap()
        .into_iter()
        .rev()
        .find(|l| l.kind == OpKind::WriteSidecar)
        .unwrap();
    assert_eq!(
        serde_json::to_value(write.actor).unwrap(),
        json!({"user": e.lib.owner().to_string()})
    );
}

/// A library and a config file of the older form, with a `[[token]]` per token in `tokens`.
fn old_config(dir: &std::path::Path, tokens: &[&str]) -> (Library, std::path::PathBuf) {
    let root = dir.join("lib");
    fs::create_dir(&root).unwrap();
    let lib = Library::init(&root, "owner").unwrap();
    let mut text = format!(
        "library = {:?}\nlisten = \"127.0.0.1:7810\"\n",
        root.to_str().unwrap()
    );
    for t in tokens {
        text.push_str(&format!(
            "\n[[token]]\nuser = \"{}\"\nhash = \"blake3:{}\"\n",
            lib.owner(),
            blake3::hash(t.as_bytes()).to_hex()
        ));
    }
    let path = dir.join("server.toml");
    fs::write(&path, text).unwrap();
    (lib, path)
}

#[test]
fn old_tokens_are_imported_and_dropped_from_the_config() {
    let dir = tempfile::tempdir().unwrap();
    let (_, path) = old_config(dir.path(), &["first-token", "second-token"]);
    let keys_dir = dir.path().join("keys");
    let cfg = load_and_migrate_in(&keys_dir, APP, &path).unwrap();
    assert_eq!(cfg.library, dir.path().join("lib"));
    assert_eq!(cfg.listen, "127.0.0.1:7810".parse().unwrap());

    let keys = key_store(&keys_dir);
    let names: Vec<&str> = keys.list().iter().map(|k| k.name.as_str()).collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.contains(&"token-1") && names.contains(&"token-2"));
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("token"), "{text}");
    assert_eq!(load_and_migrate_in(&keys_dir, APP, &path).unwrap(), cfg);
    assert_eq!(key_store(&keys_dir).list().len(), 2);
    assert_eq!(ServerConfig::load(&path).unwrap(), cfg);

    let verifier = KeyVerifier::new(&keys_dir);
    for t in ["first-token", "second-token"] {
        assert!(matches!(verifier.verify(t), Verdict::Accepted(_)), "{t}");
    }
    assert_eq!(verifier.verify("third-token"), Verdict::Refused);
}

/// An imported key takes the first `token-<n>` no unretired key has, so a name never
/// names two keys.
#[test]
fn imported_keys_take_free_names() {
    let dir = tempfile::tempdir().unwrap();
    let (_, path) = old_config(dir.path(), &["first-token", "second-token"]);
    let keys_dir = dir.path().join("keys");
    let mut keys = key_store(&keys_dir);
    keys.add(
        "token-1",
        None,
        asteria_framework_server::keys::ProfileId::new("owner").unwrap(),
    )
    .unwrap();
    keys.add(
        "token-2",
        None,
        asteria_framework_server::keys::ProfileId::new("owner").unwrap(),
    )
    .unwrap();
    keys.retire("token-2").unwrap();
    load_and_migrate_in(&keys_dir, APP, &path).unwrap();
    let keys = key_store(&keys_dir);
    let live: Vec<&str> = keys
        .list()
        .iter()
        .filter(|k| !k.is_retired())
        .map(|k| k.name.as_str())
        .collect();
    assert_eq!(live.len(), 3, "{live:?}");
    for name in ["token-1", "token-2", "token-3"] {
        assert!(live.contains(&name), "{name} missing from {live:?}");
    }
    assert_eq!(keys.authenticate("first-token").unwrap().name, "token-2");
    assert_eq!(keys.authenticate("second-token").unwrap().name, "token-3");
}

#[test]
fn a_failed_import_leaves_the_config_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (_, path) = old_config(dir.path(), &["first-token"]);
    // A file where the keys' directory would go.
    let keys_dir = dir.path().join("keys");
    fs::write(&keys_dir, b"").unwrap();
    let before = fs::read(&path).unwrap();
    assert!(load_and_migrate_in(&keys_dir, APP, &path).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn a_bad_old_hash_imports_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (lib, path) = old_config(dir.path(), &["first-token"]);
    let keys_dir = dir.path().join("keys");
    let text = fs::read_to_string(&path).unwrap()
        + &format!(
            "\n[[token]]\nuser = \"{}\"\nhash = \"blake3:zz\"\n",
            lib.owner()
        );
    fs::write(&path, &text).unwrap();
    let err = load_and_migrate_in(&keys_dir, APP, &path)
        .unwrap_err()
        .to_string();
    assert!(err.contains("hash"), "{err}");
    assert!(!err.contains("zz"), "{err}");
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
    assert!(!keys_dir.exists() || key_store(&keys_dir).list().is_empty());
}

#[test]
fn the_config_defaults_to_the_apps_config_dir() {
    let app = corundum_framework::platform::dirs::AppDirs::new("asteria-photos").unwrap();
    assert_eq!(
        default_config_path("asteria-photos").unwrap(),
        app.config().join("server.toml")
    );
}

/// A config nobody may rewrite (owned by root, a read-only mount): the tokens are imported
/// and the server goes on with the config as it is.
#[cfg(unix)]
#[test]
fn a_read_only_old_config_is_imported_and_left_alone() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let conf = dir.path().join("conf");
    fs::create_dir(&conf).unwrap();
    let (_, path) = old_config(&conf, &["first-token"]);
    let lib = conf.join("lib");
    let keys_dir = dir.path().join("keys");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&conf, fs::Permissions::from_mode(0o555)).unwrap();
    struct Writable<'a>(&'a std::path::Path);
    impl Drop for Writable<'_> {
        fn drop(&mut self) {
            let _ = fs::set_permissions(self.0, fs::Permissions::from_mode(0o755));
        }
    }
    let _restore = Writable(&conf);
    // Root writes anyway: nothing to test then.
    if fs::write(conf.join("probe"), b"").is_ok() {
        eprintln!("skipped: running as root");
        return;
    }
    let before = fs::read(&path).unwrap();

    let cfg = load_and_migrate_in(&keys_dir, APP, &path).unwrap();
    assert_eq!(cfg.library, lib);
    assert_eq!(fs::read(&path).unwrap(), before);
    let verifier = KeyVerifier::new(&keys_dir);
    assert!(matches!(
        verifier.verify("first-token"),
        Verdict::Accepted(_)
    ));
    // A second run imports nothing more.
    load_and_migrate_in(&keys_dir, APP, &path).unwrap();
    assert_eq!(key_store(&keys_dir).list().len(), 1);
}

#[test]
fn a_missing_config_is_named_in_the_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nothing.toml");
    let err = load_and_migrate_in(&dir.path().join("keys"), APP, &path)
        .unwrap_err()
        .to_string();
    assert!(err.contains(&path.display().to_string()), "{err}");
    let err = ServerConfig::load(&path).unwrap_err().to_string();
    assert!(err.contains(&path.display().to_string()), "{err}");
}
