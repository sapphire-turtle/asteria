//! Runs the real binary: init, serve, RPC and media requests, restart.
//! Linux only: the server is stopped with an external `kill -INT`, and the cache is isolated
//! through `XDG_CACHE_HOME`, which other platforms ignore.

#[cfg(not(target_os = "linux"))]
#[test]
fn e2e_skipped_on_this_platform() {
    eprintln!(
        "skipping the end-to-end test: it relies on XDG_CACHE_HOME cache isolation (Linux only)"
    );
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use reqwest::StatusCode;
    use reqwest::blocking::Client;
    use serde_json::{Value, json};

    const BIN: &str = env!("CARGO_BIN_EXE_asteria-photos-server");
    /// A 1x1 PNG.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    const VIDEO_LIKE: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

    struct Env {
        home: PathBuf,
        logs: PathBuf,
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

    /// Kills the server when dropped, so a failing test leaves nothing running. Its output goes to
    /// files, which are printed if the test is failing.
    struct Server {
        child: Child,
        stdout: PathBuf,
        stderr: PathBuf,
    }

    impl Server {
        fn start(env: &Env, config: &Path, listen: &str) -> Server {
            let n = std::fs::read_dir(&env.logs).unwrap().count();
            let stdout = env.logs.join(format!("server-{n}.out"));
            let stderr = env.logs.join(format!("server-{n}.err"));
            let child = env
                .command()
                .args(["serve", "--config"])
                .arg(config)
                .args(["--listen", listen])
                .stdout(std::fs::File::create(&stdout).unwrap())
                .stderr(std::fs::File::create(&stderr).unwrap())
                .spawn()
                .unwrap();
            Server {
                child,
                stdout,
                stderr,
            }
        }

        fn logs(&self) -> String {
            let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
            format!(
                "--- server stdout ---\n{}--- server stderr ---\n{}",
                read(&self.stdout),
                read(&self.stderr)
            )
        }

        /// Ctrl-C, then waits for a clean exit.
        fn stop(mut self) {
            let status = Command::new("kill")
                .args(["-INT", &self.child.id().to_string()])
                .status()
                .unwrap();
            assert!(status.success());
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    assert!(
                        status.success(),
                        "server exited with {status}\n{}",
                        self.logs()
                    );
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "server did not stop\n{}",
                    self.logs()
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            if std::thread::panicking() {
                eprintln!("{}", self.logs());
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
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
        /// Ready once `/rpc` answers 401 to a request without a token; fails fast if the server
        /// exited.
        fn wait_ready(&self, server: &mut Server) {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                if let Some(status) = server.child.try_wait().unwrap() {
                    panic!("server exited early with {status}\n{}", server.logs());
                }
                let up = self
                    .client
                    .post(format!("{}/rpc", self.base))
                    .send()
                    .is_ok_and(|r| r.status() == StatusCode::UNAUTHORIZED);
                if up {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "server did not come up\n{}",
                    server.logs()
                );
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

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn init_serve_rate_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let env = Env {
            home: tmp.path().join("home"),
            logs: tmp.path().join("logs"),
        };
        std::fs::create_dir_all(&env.home).unwrap();
        std::fs::create_dir_all(&env.logs).unwrap();
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
        assert_eq!(mode(&config), 0o600);

        // A refused init leaves the config untouched; --force replaces it (with a new token).
        let before = std::fs::read(&config).unwrap();
        assert!(!init(false).status.success());
        assert_eq!(std::fs::read(&config).unwrap(), before);
        let out = init(true);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let forced = String::from_utf8(out.stdout).unwrap().trim().to_owned();
        assert_ne!(forced, token);
        assert_ne!(std::fs::read(&config).unwrap(), before);
        assert_eq!(mode(&config), 0o600);
        let token = forced;

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

        let mut server = Server::start(&env, &config, &listen);
        api.wait_ready(&mut server);

        // 1. handshake
        let info = api.call("server.info", json!({}));
        assert_eq!(info["app"], "asteria-photos");

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
        if !have_vips() {
            eprintln!("skipping the thumbnail step: vipsthumbnail is not installed");
        } else {
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

        // 8. a second server on the same library fails fast and touches nothing: an in-flight
        // temporary file of the first one survives.
        let inflight = lib.join("photos/.a.png.xmp.asteria-tmp");
        std::fs::write(&inflight, "being written").unwrap();
        let other = format!("127.0.0.1:{}", free_port());
        let mut second = Server::start(&env, &config, &other);
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = second.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "a second server kept running\n{}",
                second.logs()
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(!status.success());
        assert!(
            second
                .logs()
                .contains("another asteria server is serving this library"),
            "{}",
            second.logs()
        );
        drop(second);
        assert!(inflight.exists(), "the first server's tmp file was removed");
        assert_eq!(api.call("server.info", json!({}))["app"], "asteria-photos");
        std::fs::remove_file(&inflight).unwrap();

        // 9. the rating is on disk in the sidecar, and survives a restart with the cache deleted
        // (a real scan rebuilds the index from the library).
        let sidecar = asteria_framework::paths::sidecar_path_for(&lib.join("photos/a.png"));
        let rating = |path: &Path| {
            asteria_framework::Sidecar::read(path)
                .unwrap()
                .and_then(|s| s.rating)
                .map(|r| r.value)
        };
        server.stop();
        assert_eq!(rating(&sidecar), Some(4));
        std::fs::remove_dir_all(env.home.join("cache")).unwrap();
        let mut server = Server::start(&env, &config, &listen);
        api.wait_ready(&mut server);
        api.wait_scanned(2);
        assert_eq!(api.list()[0]["rating"], 4);
        assert_eq!(rating(&sidecar), Some(4));

        // 10. a client that stops reading a large original does not hold up shutdown for
        // longer than the grace period (10 s).
        let big = std::fs::File::create(lib.join("photos/c.mp4")).unwrap();
        big.set_len(256 << 20).unwrap();
        drop(big);
        api.wait_scanned(3);
        let big_id = api.list()[2]["id"].as_str().unwrap().to_owned();
        let mut stalled = std::net::TcpStream::connect(&listen).unwrap();
        write!(
            stalled,
            "GET /media/{big_id}/original HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {}\r\n\r\n",
            api.token
        )
        .unwrap();
        let mut head = [0u8; 1024];
        stalled.read_exact(&mut head).unwrap();
        assert!(head.starts_with(b"HTTP/1.1 200"));
        let start = Instant::now();
        server.stop();
        assert!(
            start.elapsed() < Duration::from_secs(18),
            "{:?}",
            start.elapsed()
        );
        drop(stalled);
        assert_eq!(rating(&sidecar), Some(4));
    }
}
