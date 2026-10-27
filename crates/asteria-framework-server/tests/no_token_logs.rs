//! Own binary: the subscriber is global, so logs from blocking and writer threads are seen too.

mod common;

use std::fs;
use std::io::Write;
use std::sync::{Arc, Mutex};

use common::*;
use serde_json::json;

#[derive(Clone)]
struct Buf(Arc<Mutex<Vec<u8>>>);
impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
    type Writer = Buf;
    fn make_writer(&'a self) -> Buf {
        self.clone()
    }
}

#[tokio::test]
async fn token_not_logged() {
    let e = env();
    let buf = Buf(Arc::new(Mutex::new(Vec::new())));
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_max_level(tracing::Level::TRACE)
            .finish(),
    )
    .unwrap();
    let bad = "definitely-wrong-token-123";
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"server.info"}"#;
    post_raw(&e.app, Some(&format!("Bearer {bad}")), body).await;
    post_raw(&e.app, Some(&format!("Bearer {}", e.owner_token)), body).await;
    let t = &e.owner_token;
    call(&e.app, t, "media.get", json!({"id": "missing"})).await;
    // Paths that log: a write on an item whose file is gone, and a refused write.
    let id = item_id(&e, "a.jpg");
    fs::remove_file(e.root.join("a.jpg")).unwrap();
    call(&e.app, t, "media.update", json!({"id": id, "rating": 3})).await;
    let id = item_id(&e, "sub/b.jpg");
    let tmp = asteria_framework::fsutil::tmp_path_for(&e.root.join("sub/b.jpg.xmp"));
    fs::write(tmp, b"x").unwrap();
    call(&e.app, t, "media.update", json!({"id": id, "rating": 3})).await;
    let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(!log.is_empty(), "the subscriber captured nothing");
    assert!(log.contains("busy"), "refused write was not logged: {log}");
    assert!(!log.contains(bad));
    assert!(!log.contains(t.as_str()));
    assert!(!log.contains(&asteria_framework_server::token_hash(t)));
}
