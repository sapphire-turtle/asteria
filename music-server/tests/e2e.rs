//! Runs the real binary: init, serve, RPC and media requests, shutdown.
//! Linux only: the server is stopped with an external `kill -INT`, and the cache and data
//! directories are isolated through `XDG_CACHE_HOME` and `XDG_DATA_HOME`, which other
//! platforms ignore.

#[cfg(not(target_os = "linux"))]
#[test]
fn e2e_skipped_on_this_platform() {
    eprintln!(
        "skipping the end-to-end test: it relies on XDG_CACHE_HOME cache isolation (Linux only)"
    );
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use asteria_music_api::{
        Album, AlbumGetParams, AlbumId, AlbumListing, ArtistListing, EntryStatus, ListParams,
        Playlist, PlaylistGetParams, PlaylistId, PlaylistListing,
    };
    use lofty::config::WriteOptions;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::prelude::*;
    use lofty::tag::{Tag, TagType};
    use reqwest::StatusCode;
    use reqwest::blocking::{Client, Response};
    use serde_json::{Value, json};

    const BIN: &str = env!("CARGO_BIN_EXE_asteria-music-server");
    /// A 1x1 PNG.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    struct Env {
        home: PathBuf,
        logs: PathBuf,
    }

    impl Env {
        fn command(&self) -> Command {
            let mut c = Command::new(BIN);
            c.env("HOME", &self.home)
                .env("XDG_CACHE_HOME", self.home.join("cache"))
                .env("XDG_DATA_HOME", self.home.join("data"))
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

        /// Calls `method` and decodes the result as the wire type `T`.
        fn typed<T: serde::de::DeserializeOwned>(&self, method: &str, params: Value) -> T {
            serde_json::from_value(self.call(method, params)).unwrap()
        }

        /// GET `/media/{id}/{what}` with a token.
        fn media(&self, id: &str, what: &str) -> reqwest::blocking::RequestBuilder {
            self.client
                .get(format!("{}/media/{id}/{what}", self.base))
                .bearer_auth(&self.token)
        }
    }

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../crates/asteria-framework/tests/fixtures")
            .join(name)
    }

    /// Copies a fixture to `dest` and writes tags into the copy with lofty.
    fn tagged_copy(fixture_name: &str, dest: &Path, tag_type: TagType, tags: &Tags) {
        std::fs::copy(fixture(fixture_name), dest).unwrap();
        let mut file = lofty::read_from_path(dest).unwrap();
        let mut tag = Tag::new(tag_type);
        tag.set_title(tags.title.to_string());
        tag.set_artist(tags.artist.to_string());
        tag.set_album(tags.album.to_string());
        tag.set_track(tags.track);
        if tags.picture {
            tag.push_picture(Picture::new_unchecked(
                PictureType::CoverFront,
                Some(MimeType::Png),
                None,
                PNG.to_vec(),
            ));
        }
        file.insert_tag(tag);
        file.save_to_path(dest, WriteOptions::default()).unwrap();
    }

    struct Tags {
        title: &'static str,
        artist: &'static str,
        album: &'static str,
        track: u32,
        picture: bool,
    }

    fn have(program: &str, args: &[&str]) -> Option<String> {
        let out = Command::new(program)
            .args(args)
            .stderr(Stdio::null())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn have_vips() -> bool {
        have("vipsthumbnail", &["--version"]).is_some()
    }

    fn have_opus() -> bool {
        have("ffmpeg", &["-hide_banner", "-encoders"]).is_some_and(|s| s.contains("libopus"))
    }

    /// Requests a derived media until it is ready. With the tool installed a 200 is required
    /// (a 503 while the content hash is computed is retried); without it a 503 is accepted.
    /// Returns the 200 response, if there was one.
    fn derived(api: &Api, id: &str, what: &str, tool: bool) -> Option<Response> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let resp = api.media(id, what).send().unwrap();
            match resp.status() {
                StatusCode::OK => return Some(resp),
                StatusCode::SERVICE_UNAVAILABLE if !tool => return None,
                StatusCode::SERVICE_UNAVAILABLE => {
                    assert!(Instant::now() < deadline, "{what} never became ready");
                    std::thread::sleep(Duration::from_millis(200));
                }
                other => panic!("{what}: {other}"),
            }
        }
    }

    #[test]
    fn music_server_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let env = Env {
            home: tmp.path().join("home"),
            logs: tmp.path().join("logs"),
        };
        std::fs::create_dir_all(&env.home).unwrap();
        std::fs::create_dir_all(&env.logs).unwrap();
        let lib = tmp.path().join("lib");
        std::fs::create_dir_all(lib.join("First Artist/Album One")).unwrap();
        std::fs::create_dir_all(lib.join("Second Artist/Album Two")).unwrap();

        // Album One: two FLACs, written so that file order and track order differ; one has an
        // embedded picture. Album Two: an MP3 whose cover is a file in its folder.
        let one = lib.join("First Artist/Album One");
        let flac = |name: &str, fixture_name: &str, title, track, picture| {
            let tags = Tags {
                title,
                artist: "First Artist",
                album: "Album One",
                track,
                picture,
            };
            tagged_copy(
                fixture_name,
                &one.join(name),
                TagType::VorbisComments,
                &tags,
            );
        };
        flac("a.flac", "sine440.flac", "Second Song", 2, true);
        flac("b.flac", "sine880.flac", "First Song", 1, false);
        let two = lib.join("Second Artist/Album Two");
        let tags = Tags {
            title: "Only Song",
            artist: "Second Artist",
            album: "Album Two",
            track: 1,
            picture: false,
        };
        tagged_copy("sine440.mp3", &two.join("song.mp3"), TagType::Id3v2, &tags);
        std::fs::write(two.join("cover.png"), PNG).unwrap();
        std::fs::write(
            lib.join("list.m3u8"),
            "#EXTM3U\n\
             First Artist/Album One/b.flac\n\
             Second Artist/Album Two/song.mp3\n\
             Gone/nothing.flac\n",
        )
        .unwrap();
        let config = tmp.path().join("conf/server.toml");

        let out = env
            .command()
            .args(["init", "--owner", "me", "--library"])
            .arg(&lib)
            .arg("--config")
            .arg(&config)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let token = String::from_utf8(out.stdout).unwrap().trim().to_owned();

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
        assert_eq!(api.call("server.info", json!({}))["app"], "asteria-music");
        api.wait_scanned(3);

        // albums
        let page = json!(ListParams {
            cursor: None,
            limit: None
        });
        let albums: AlbumListing = api.typed("album.list", page.clone());
        let titles: Vec<&str> = albums.albums.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, ["Album One", "Album Two"]);
        assert_eq!(albums.albums[0].track_count, 2);
        assert_eq!(albums.albums[1].track_count, 1);

        // the tracks of an album are in track order, not file order
        let one_id: AlbumId = albums.albums[0].id.clone();
        let album: Album = api.typed("album.get", json!(AlbumGetParams { id: one_id }));
        let order: Vec<&str> = album
            .tracks
            .iter()
            .map(|t| t.display_title.as_str())
            .collect();
        assert_eq!(order, ["First Song", "Second Song"]);
        assert_eq!(album.folder, "First Artist/Album One");
        assert!(album.tracks[1].tags.has_embedded_cover);

        // artists
        let artists: ArtistListing = api.typed("artist.list", page.clone());
        let names: Vec<&str> = artists.artists.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["First Artist", "Second Artist"]);

        // playlist: two found, one missing
        let lists: PlaylistListing = api.typed("playlist.list", page);
        assert_eq!(lists.playlists.len(), 1);
        let list: Playlist = api.typed(
            "playlist.get",
            json!(PlaylistGetParams {
                id: PlaylistId(lists.playlists[0].id.0.clone())
            }),
        );
        let statuses: Vec<EntryStatus> = list.entries.iter().map(|e| e.status).collect();
        assert_eq!(
            statuses,
            [EntryStatus::Found, EntryStatus::Found, EntryStatus::Missing]
        );
        assert_eq!(
            list.entries[0].track.as_ref().unwrap().display_title,
            "First Song"
        );
        assert!(list.entries[2].track.is_none());
        assert!(!list.truncated);

        // media: the cover of a track with an embedded picture, and of one with a folder image
        let embedded_id = album.tracks[1].item.id.0.to_string();
        let folder_id = {
            let a: Album = api.typed(
                "album.get",
                json!(AlbumGetParams {
                    id: albums.albums[1].id.clone()
                }),
            );
            a.tracks[0].item.id.0.to_string()
        };
        for id in [&embedded_id, &folder_id] {
            if let Some(resp) = derived(&api, id, "thumb", have_vips()) {
                assert_eq!(resp.headers()["content-type"], "image/webp");
                let body = resp.bytes().unwrap();
                assert!(body.starts_with(b"RIFF") && &body[8..12] == b"WEBP");
            }
        }
        if let Some(resp) = derived(&api, &folder_id, "offline-audio", have_opus()) {
            assert_eq!(resp.headers()["content-type"], "audio/ogg");
            assert!(resp.bytes().unwrap().starts_with(b"OggS"));
        }
        let resp = api
            .media(&folder_id, "original")
            .header("Range", "bytes=0-3")
            .send()
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(resp.bytes().unwrap().len(), 4);

        // The tags and the playlist are only read: the library holds what was put there, and
        // `.asteria` only its own two files.
        server.stop();
        let mut meta: Vec<_> = std::fs::read_dir(lib.join(".asteria"))
            .unwrap()
            .map(|d| d.unwrap().file_name().into_string().unwrap())
            .collect();
        meta.sort();
        assert_eq!(meta, ["library.toml", "users.toml"]);
    }
}
