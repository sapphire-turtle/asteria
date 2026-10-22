//! Runs the real binary: init, serve, RPC and media requests, restart.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use reqwest::blocking::Client;
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_asteria-gallery-server");
/// A 1x1 PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];
const VIDEO_LIKE: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

struct Env {
    home: PathBuf,
}

impl Env {
    fn command(&self) -> Command {
        let mut c = Command::new(BIN);
        c.env("HOME", &self.home)
            .env("XDG_CACHE_HOME", self.home.join("cache"))
            .env("RUST_LOG", "info");
        c
    }
}

/// Kills the server when dropped, so a failing test leaves nothing running.
struct Server(Child);

impl Server {
    fn start(env: &Env, config: &Path, listen: &str) -> Server {
        let child = env
            .command()
            .args(["serve", "--config"])
            .arg(config)
            .args(["--listen", listen])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Server(child)
    }

    /// Ctrl-C, then waits for a clean exit.
    fn stop(mut self) {
        let status = Command::new("kill")
            .args(["-INT", &self.0.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "server exited with {status}");
                return;
            }
            assert!(Instant::now() < deadline, "server did not stop");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

struct Api {
    client: Client,
    base: String,
    token: String,
}

impl Api {
    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while self.client.get(&self.base).send().is_err() {
            assert!(Instant::now() < deadline, "server did not come up");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn call(&self, method: &str, params: Value) -> Value {
        let resp: Value = self
            .client
            .post(format!("{}/rpc", self.base))
            .bearer_auth(&self.token)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            .send()
            .unwrap()
            .json()
            .unwrap();
        assert!(resp.get("error").is_none(), "{method}: {resp}");
        resp["result"].clone()
    }

    fn wait_scanned(&self, items: u64) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let info = self.call("library.info", json!({}));
            if info["item_count"] == items && info["scanning"] == false {
                return;
            }
            assert!(Instant::now() < deadline, "scan did not finish: {info}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn list(&self) -> Vec<Value> {
        let r = self.call("folder.list", json!({"path": "photos"}));
        r["items"].as_array().unwrap().clone()
    }
}

fn have_vips() -> bool {
    Command::new("vipsthumbnail")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

#[test]
fn init_serve_rate_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let env = Env {
        home: tmp.path().join("home"),
    };
    std::fs::create_dir_all(&env.home).unwrap();
    let lib = tmp.path().join("lib");
    std::fs::create_dir_all(lib.join("photos")).unwrap();
    std::fs::write(lib.join("photos/a.png"), PNG).unwrap();
    std::fs::write(lib.join("photos/b.mp4"), VIDEO_LIKE).unwrap();
    std::fs::write(lib.join("photos/notes.txt"), "not media").unwrap();
    let config = tmp.path().join("conf/server.toml");

    // init: only the token goes to stdout; a second init is refused without --force.
    let init = |force: bool| {
        let mut c = env.command();
        c.args(["init", "--owner", "me", "--library"])
            .arg(&lib)
            .arg("--config")
            .arg(&config);
        if force {
            c.arg("--force");
        }
        c.output().unwrap()
    };
    let out = init(false);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let token = String::from_utf8(out.stdout).unwrap();
    assert_eq!(token.lines().count(), 1, "stdout: {token:?}");
    let token = token.trim().to_owned();
    assert!(!std::fs::read_to_string(&config).unwrap().contains(&token));
    assert!(!init(false).status.success());

    // token add: the owner gets a second working token; unknown users are refused.
    let token_add = |user: &str| {
        env.command()
            .args(["token", "add", "--config"])
            .arg(&config)
            .args(["--user", user])
            .output()
            .unwrap()
    };
    let users = std::fs::read_to_string(lib.join(".asteria/users.toml")).unwrap();
    let owner_id = users
        .lines()
        .find_map(|l| l.strip_prefix("id = "))
        .unwrap()
        .trim_matches('"')
        .to_owned();
    let out = token_add(&owner_id);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let second = String::from_utf8(out.stdout).unwrap();
    assert_eq!(second.lines().count(), 1, "stdout: {second:?}");
    assert_ne!(second.trim(), token);
    let stranger = asteria_framework::UserId(grain_id::GrainId::random()).to_string();
    assert!(!token_add(&stranger).status.success());

    let listen = format!("127.0.0.1:{}", free_port());
    let api = Api {
        client: Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap(),
        base: format!("http://{listen}"),
        token,
    };

    let server = Server::start(&env, &config, &listen);
    api.wait_ready();

    // 1. handshake
    let info = api.call("server.info", json!({}));
    assert_eq!(info["app"], "asteria-gallery");

    // 2. scan
    api.wait_scanned(2);

    // 3. folder.list
    let items = api.list();
    let names: Vec<&str> = items.iter().map(|i| i["path"].as_str().unwrap()).collect();
    assert_eq!(names, ["photos/a.png", "photos/b.mp4"]);
    let png_id = items[0]["id"].as_str().unwrap().to_owned();
    let video_id = items[1]["id"].as_str().unwrap().to_owned();

    // 4. media.update
    let updated = api.call("media.update", json!({"id": png_id, "rating": 4}));
    assert_eq!(updated["rating"], 4);

    // 5. original, with a Range
    let url = format!("{}/media/{video_id}/original", api.base);
    let resp = api
        .client
        .get(&url)
        .bearer_auth(&api.token)
        .header("Range", "bytes=2-5")
        .send()
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(resp.bytes().unwrap().as_ref(), b"2345");

    // 6. thumbnail (the content hash is computed in the background)
    if have_vips() {
        let url = format!("{}/media/{png_id}/thumb", api.base);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let resp = api.client.get(&url).bearer_auth(&api.token).send().unwrap();
            match resp.status() {
                StatusCode::OK => {
                    assert_eq!(resp.headers()["content-type"], "image/webp");
                    break;
                }
                StatusCode::SERVICE_UNAVAILABLE => {
                    assert!(Instant::now() < deadline, "thumbnail never became ready");
                    std::thread::sleep(Duration::from_millis(200));
                }
                other => panic!("thumbnail: {other}"),
            }
        }
    }

    // 7. 401
    let resp = api.client.post(format!("{}/rpc", api.base)).send().unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = api
        .client
        .post(format!("{}/rpc", api.base))
        .bearer_auth("wrong")
        .send()
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // 8. the rating survives a restart (the sidecar is the source of truth)
    server.stop();
    let server = Server::start(&env, &config, &listen);
    api.wait_ready();
    api.wait_scanned(2);
    assert_eq!(api.list()[0]["rating"], 4);
    server.stop();
}
