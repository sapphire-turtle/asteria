//! API keys: every key acts as the library's owner, keys added or retired while the server
//! runs take effect, and an older config's `[[token]]` entries become keys.

mod common;

use std::fs;
use std::time::Duration;

use asteria_framework::Library;
use asteria_framework::journal::OpKind;
use asteria_framework_server::keys::{KeyVerifier, Verdict};
use asteria_framework_server::{
    ServerConfig, default_config_path, keys_dir_for, load_and_migrate, open_keys,
};
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
    let mut keys = open_keys(APP, &e.lib.config().id).unwrap();
    let (_, token) = keys.add("later", None, None).unwrap();
    assert!(token.expose().starts_with("asteria-photos-"));
    let (status, _, _) = info(&e, Some(&format!("Bearer {}", token.expose()))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_retired_key_stops_working() {
    let e = env_ttl(Duration::ZERO);
    let bearer = format!("Bearer {}", e.other_token);
    assert_eq!(info(&e, Some(&bearer)).await.0, StatusCode::OK);
    open_keys(APP, &e.lib.config().id)
        .unwrap()
        .retire("other")
        .unwrap();
    assert_eq!(info(&e, Some(&bearer)).await.0, StatusCode::UNAUTHORIZED);
    // The other key is untouched.
    let owner = format!("Bearer {}", e.owner_token);
    assert_eq!(info(&e, Some(&owner)).await.0, StatusCode::OK);
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
    let (lib, path) = old_config(dir.path(), &["first-token", "second-token"]);
    let _data = DataDir::of(&lib.config().id);
    let cfg = load_and_migrate(APP, &path).unwrap();
    assert_eq!(cfg.library, dir.path().join("lib"));
    assert_eq!(cfg.listen, "127.0.0.1:7810".parse().unwrap());

    let keys = open_keys(APP, &lib.config().id).unwrap();
    let names: Vec<&str> = keys.list().iter().map(|k| k.name.as_str()).collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.contains(&"token-1") && names.contains(&"token-2"));
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("token"), "{text}");
    assert_eq!(load_and_migrate(APP, &path).unwrap(), cfg);
    assert_eq!(ServerConfig::load(&path).unwrap(), cfg);

    let verifier = KeyVerifier::new(&keys_dir_for(APP, &lib.config().id).unwrap());
    for t in ["first-token", "second-token"] {
        assert!(matches!(verifier.verify(t), Verdict::Accepted(_)), "{t}");
    }
    assert_eq!(verifier.verify("third-token"), Verdict::Refused);
}

#[test]
fn a_failed_import_leaves_the_config_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (lib, path) = old_config(dir.path(), &["first-token"]);
    let data = DataDir::of(&lib.config().id);
    fs::create_dir_all(&data.0).unwrap();
    // A file where the keys' directory would go.
    fs::write(data.0.join("keys"), b"").unwrap();
    let before = fs::read(&path).unwrap();
    assert!(load_and_migrate(APP, &path).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn a_bad_old_hash_imports_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (lib, path) = old_config(dir.path(), &["first-token"]);
    let _data = DataDir::of(&lib.config().id);
    let text = fs::read_to_string(&path).unwrap()
        + &format!(
            "\n[[token]]\nuser = \"{}\"\nhash = \"blake3:zz\"\n",
            lib.owner()
        );
    fs::write(&path, &text).unwrap();
    let err = load_and_migrate(APP, &path).unwrap_err().to_string();
    assert!(err.contains("hash"), "{err}");
    assert!(!err.contains("zz"), "{err}");
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
    assert!(open_keys(APP, &lib.config().id).unwrap().list().is_empty());
}

#[test]
fn the_config_defaults_to_the_apps_config_dir() {
    let app = corundum_framework::platform::dirs::AppDirs::new("asteria-photos").unwrap();
    assert_eq!(
        default_config_path("asteria-photos").unwrap(),
        app.config().join("server.toml")
    );
}
