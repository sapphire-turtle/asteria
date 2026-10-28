# Plan 6b: music のデスクトップクライアントと再生エンジン 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `asteria-music` を起動すると、music-server に接続して、アルバム、アーティスト、プレイリストを閲覧できる。曲をクリックするとその一覧がキューになり、原本（設定で低音質版）を Range で少しずつ取りながら再生する。

**Architecture:**
- framework-client に、アプリ固有の RPC を送る `Request::Call` と、ID だけでサムネイルを取る `ImageLoader::request_id` を足す。photos の中にある `servers.toml` の保存スレッドとステータスバーのサーバー切り替えを framework-client に移し、photos と music で共有する。
- `music/`（`asteria-music`）は photos と同じ形の lib＋bin。再生エンジンは `music/src/player/` に置き、egui に依存させない。
  - `engine`：キューと状態を持つ同期的な中核。`Opener`（曲を開く）と `OutputFactory`（音を出す）をトレイトで受け取るので、テストでは偽物に差し替えて、スレッドも時刻も使わずに確かめられる。
  - `stream`：1 MiB ずつ Range で一時ファイルに取る。読む側（デコーダー）は届くまで待つ。
  - `decode`：symphonia（FLAC、MP3、AAC/ALAC、Vorbis）と、Ogg Opus 用の ogg＋opus-pure。
  - `resample`：rubato でサンプルレートを、自前でチャンネル数を出力に合わせる。
  - `feed`：1曲ごとのデコードのスレッド。`stream`→`decode`→`resample` をつなぎ、PCM をチャンネルで渡す。`HttpOpener` が `Opener` の本物。
  - `output`：cpal の出力（`OutputFactory` の本物）。
  - `Player`：エンジンを専用のスレッドで回し、UI とはコマンドのチャンネルと共有した状態でやり取りする。
- UI は photos と同じく corundum の Shell に載せる。レールはアルバム、アーティスト、プレイリスト、再生キュー、足元に設定。再生バーは各画面の下に置く。

**Tech Stack:** Rust 1.99.0、eframe/egui（corundum-framework 0.2.0 の再エクスポート）、reqwest 0.13（blocking）、symphonia 0.6、ogg 0.9、opus-pure 0.2、rubato 5、cpal 0.18、rtrb、axum（テストのみ）。

**Spec:** `docs/superpowers/specs/2026-10-22-asteria-design.md`（特に §5.1、§5.4、§5.7、§7）

## Global Constraints

- ツールチェインは `1.99.0`、edition 2024。CI は clippy `-D warnings`、`cargo fmt --check`、`cargo deny check` を通し、Linux（x86_64、arm64）、macOS、Windows でテストする。手元では `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings` も通すこと（`#[cfg(unix)]` のテストだけで使う import が Windows で未使用になる失敗が PR #16 で出た）。
- サーバーと `*-api` クレートは変えない。`asteria-music-api` は 2.3.0 のまま。
- OS ごとの API（objc2、JNI、Windows API、D-Bus）を直接使うコードを asteria に書かない。メディアキーと OS の「再生中」の表示は入れない（corundum-framework#50 を待つ）。cpal は使ってよい。
- 新しい依存：`symphonia = { version = "0.6.1", default-features = false, features = ["flac", "mp3", "aac", "alac", "isomp4", "ogg", "vorbis"] }`、`ogg = "0.9.2"`、`opus-pure = { version = "0.2.2", default-features = false }`、`rubato = "5.0.1"`、`cpal = "0.18.2"`（既定の features）、`rtrb = "0.3"`。どれも MIT、Apache-2.0、BSD-3-Clause、MPL-2.0 のいずれかで、`deny.toml` の `allow` で通る。`deny.toml` に足す例外は `asteria-music` の GPL だけ。
- CI の `linux-packages` に `libasound2-dev`（cpal の ALSA）を足す。
- トークンはログにも画面にも出さない。`Authorization` ヘッダーにだけ載せ、`HeaderValue::set_sensitive(true)` にする。
- `asteria-framework-client` は eframe に依存しない（egui だけ）。iOS 向けの `cargo check -p asteria-framework-client --target aarch64-apple-ios` を壊さない。
- 画面の文字列は Fluent（`music/i18n/{en,ja}/music.ftl`）に置き、ID は `asteria-music-<area>-<what>`。framework-client に移す文字列は `asteria-client-<area>-<what>`。
- 定数（`music/src/player/` のもの）
  - `CHUNK_BYTES = 1 << 20`（1回の Range の大きさ）
  - `CHUNK_TIMEOUT = 30 s`（1回の要求の上限）
  - `PREPARE_TIMEOUT = 600 s`（`offline-audio` の最初の要求の上限）
  - `RESTART_AFTER_MS = 3_000`（「前の曲」で頭に戻る境目）
  - `MAX_SKIPS = 3`（続けて再生できない曲がこの数になったら止める）
  - `STALE_STREAM_DIR = 24 h`（起動時に消す、残った一時ディレクトリの古さ）
- 一覧のページの大きさは 200（`PAGE_SIZE`。サーバーの既定と同じ）。

## Review Focus

1. **再生中の操作の取り違え**：曲の途中でキューの前の曲を消す、いまの曲を消す、いまの曲を別の位置へ動かす、キューを置き換える、を続けても、`current` はいま鳴っている曲を指し続ける（消したときは次の曲へ進む）。音と表示がずれない。→ Task 3 でテストする
2. **届かない、遅い、途中で切れるサーバー**：最初の要求が 503 を2回返す、途中の要求が切れる、30 秒応答しない、のどれでもアプリは固まらない。503 は待って続き、切れたら一時停止してエラーを出し、再生で同じ位置から再開する。→ Task 4、Task 6 でテストする
3. **シークの行き先**：まだ届いていない位置へ飛ぶ、もう届いた位置へ戻る、曲の長さを超えた位置を指定する、を行っても、読み出しは正しいバイトを返し、位置は曲の中に収まる。→ Task 4、Task 5 でテストする
4. **サーバーの切り替えと終了の後始末**：再生中に別のサーバーへ切り替えると、音が止まり、キューが空になり、前のサーバーへの要求は続かず、一時ファイルが消える。アプリを閉じると一時ディレクトリが消える。→ Task 6、Task 9 でテストする
5. **再生できない曲が並んだキュー**：404、422、デコードできないファイルが続くと、3 曲目で止まり、それまでの曲は飛ばされる。間に再生できる曲があれば数え直す。プレイリストの「見つからない」エントリはキューに入らない。→ Task 3、Task 8 でテストする

---

### Task 1: framework-client にアプリ固有の RPC と ID でのサムネイルを足す

**Files:**
- Modify: `crates/asteria-framework-client/src/session.rs`、`crates/asteria-framework-client/src/images.rs`
- Test: `crates/asteria-framework-client/tests/session.rs`、`crates/asteria-framework-client/tests/images.rs`

**Interfaces:**
- Produces（`session.rs`）:
  - `Request::Call { method: String, params: serde_json::Value }`：`method` を `params` で呼ぶ。結果は `Response::Value(serde_json::Value)`。ほかの `Request` と同じく、ハンドシェイクの前に送ったものはハンドシェイクを待ち、失敗したセッションではそのエラーで返る。
  - アプリは `serde_json::from_value` で自分の型に直す。直せなければ `ClientError::Protocol` として扱う（アプリ側の責任。framework-client は値をそのまま返す）。
- Produces（`images.rs`）:
  - `ImageLoader::request_id(&mut self, id: &ItemId, preset: Preset) -> ImageState`：`file_hash` を知らない画像（アルバムの `cover_item`）を取る。`request` と同じエントリの表（キーは `(ItemId, Preset)`）を使い、`file_hash` は `None` として扱う。ディスクのキャッシュ（`MediaCache`）には `file_hash` がないので読み書きしない。同じ ID を `request`（`file_hash` あり）でも頼んだ場合は、`file_hash` の違いで取り直す、という今の規則のままでよい。

- [ ] **Step 1: 失敗するテストを書く**

`tests/session.rs` に足す（既存のスタブ `start_stub` を使う。スタブが知らないメソッドには `{"echo": <params>}` を返すように広げる）:

```rust
#[test]
fn call_passes_any_method_and_returns_its_value() {
    // album.list with {"limit": 5} comes back as the stub's echo.
    // assert_eq!(reply.result, Ok(Response::Value(json!({"echo": {"limit": 5}}))));
}

#[test]
fn call_before_the_handshake_fails_with_it() {
    // A stub whose server.info names another app: the Call made right after open
    // replies Err(ClientError::Handshake(_)), and the stub never sees the method.
}
```

`tests/images.rs` に足す:

```rust
#[test]
fn request_id_fetches_the_thumb_of_an_id() {
    // A stub serving GET /media/c1/thumb with a 2x2 PNG: request_id(&ItemId("c1"), Thumb)
    // becomes Ready within DEADLINE; the stub saw exactly one GET for it.
}

#[test]
fn request_id_does_not_touch_the_disk_cache() {
    // With a MediaCache root: after Ready, the library's cache directory holds no file.
}
```

- [ ] **Step 2: テストが失敗することを確かめる**

Run: `cargo test -p asteria-framework-client --test session --test images`
Expected: `Request::Call` と `request_id` がないのでコンパイルエラー。

- [ ] **Step 3: 実装する**

`run` の `match request` に `Request::Call { method, params } => Response::Value(rpc.call(&method, &params)?)` を足す。`request_id` は `request` の中身を `(id, file_hash, preset)` を取る内部関数に分けて共有する。ディスクのキャッシュを読む・書く箇所は、`file_hash` が `None` なら飛ばす（今のコードがそうなっていればそのまま）。

- [ ] **Step 4: テストが通ることを確かめる**

Run: `cargo test -p asteria-framework-client`
Expected: すべて PASS。

- [ ] **Step 5: コミット**

```bash
git add crates/asteria-framework-client
git commit -m "feat(client): app-specific RPC calls and thumbnails by item id"
```

---

### Task 2: 保存のスレッドとサーバーの切り替えを framework-client に移す

photos の `saver.rs` とステータスバーの `switcher_ui` は、music でもそのまま要る。framework-client に移し、photos はそれを使う。photos の動きは変えない。

**Files:**
- Create: `crates/asteria-framework-client/src/saver.rs`、`crates/asteria-framework-client/src/widgets/server_switcher.rs`
- Modify: `crates/asteria-framework-client/src/{lib.rs,widgets/mod.rs}`、`crates/asteria-framework-client/i18n/{en,ja}/*.ftl`、`photos/src/{app.rs,lib.rs}`、`photos/i18n/{en,ja}/photos.ftl`
- Delete: `photos/src/saver.rs`
- Test: `crates/asteria-framework-client/tests/saver.rs`（新規）、photos の既存のテスト

**Interfaces:**
- Produces（`saver.rs`）:

```rust
/// Writes the latest value handed to it on its own thread, and hands back what went wrong.
/// Dropping it waits for the last write.
pub struct Saver<T: Send + 'static> { /* tx, errors, thread */ }
impl<T: Send + 'static> Saver<T> {
    pub fn spawn(
        name: &str,                       // the thread's name
        ctx: egui::Context,               // repainted when a write fails
        write: impl Fn(&T) -> Result<(), String> + Send + 'static,
    ) -> std::io::Result<Saver<T>>;
    pub fn save(&self, value: T);
    pub fn take_error(&self) -> Option<String>;
}
```

  - 書く関数は呼び出し側が渡す。photos は `move |p: &Profiles| p.save(&path).map_err(|e| e.to_string())`。エラーの文言にファイルの中身（トークン）を入れない、という今の約束は呼び出し側の関数が守る。
- Produces（`widgets/server_switcher.rs`）:

```rust
pub struct ServerSwitcher<'a> {
    pub profiles: &'a Profiles,
    pub session: Option<&'a Session>,
    pub compact: bool,
}
pub enum SwitcherAction { Switch(ProfileId), Manage }
impl ServerSwitcher<'_> {
    pub fn show(self, ui: &mut egui::Ui) -> Option<SwitcherAction>;
}
```

  - 中身は photos の `switcher_ui` をそのまま移す。文字列は `asteria-photos-status-{none,connected,connecting,failed,degraded,manage,hover}` を `asteria-client-status-*` に移し、photos の ftl から消す。英語と日本語の文言は変えない。

- [ ] **Step 1: 失敗するテストを書く**

`tests/saver.rs`:

```rust
#[test]
fn only_the_latest_value_is_written_and_drop_waits_for_it() {
    // save(1), save(2), save(3) in a row, then drop: the write function saw 3 last,
    // and was called at most 3 times.
}

#[test]
fn a_failed_write_is_handed_back() {
    // A write that returns Err("disk full"): take_error() becomes Some("disk full")
    // within DEADLINE, and None after it was taken.
}
```

- [ ] **Step 2: テストが失敗することを確かめる**

Run: `cargo test -p asteria-framework-client --test saver`
Expected: `saver` モジュールがないのでコンパイルエラー。

- [ ] **Step 3: 実装し、photos を移行する**

`photos/src/saver.rs` を消し、`PhotosApp` は `Saver<Profiles>` を使う。`switcher_ui` は `ServerSwitcher` を呼び、返った `SwitcherAction` で今と同じく `switch_to` か `open_servers_settings` を行う。

- [ ] **Step 4: テストが通ることを確かめる**

Run: `cargo test -p asteria-framework-client -p asteria-photos`
Expected: すべて PASS（photos の `switch.rs`、`i18n.rs` も今のまま通る）。

- [ ] **Step 5: コミット**

```bash
git add crates/asteria-framework-client photos
git commit -m "refactor(client): share the settings saver and the server switcher"
```

---

### Task 3: 再生エンジンの中核（`player::engine`）

**Files:**
- Create: `music/Cargo.toml`、`music/src/{lib.rs,main.rs}`、`music/src/player/{mod.rs,engine.rs,fakes.rs}`
- Modify: `Cargo.toml`（workspace の `members` に `music`、`[workspace.dependencies]` に Global Constraints の新しい依存）、`deny.toml`（`{ name = "asteria-music", allow = ["GPL-3.0-or-later"] }`）、`.github/workflows/ci.yml`（`libasound2-dev`）
- Test: `music/src/player/engine.rs` の `#[cfg(test)] mod tests`

偽物（`FakeOutput`、`FakeOpener`）は `player/fakes.rs` に置き、`#[cfg(any(test, feature = "test-support"))] pub mod fakes;` で公開する。Task 7 と Task 8 の結合テストは、dev-dependencies で自分自身を `features = ["test-support"]` で読んで使う。

この Task では `main.rs` は空の `fn main() {}` でよい（Task 9 で窓を開く）。`lib.rs` は `pub mod player;` だけ。依存は、この Task で使う `asteria-music-api`、`asteria-framework-client`（`Token`）、`url`、`tracing` だけを足し、ほかは使う Task で足す。

**Interfaces:**
- Produces（`player/mod.rs`）:

```rust
pub struct QueueEntry {
    pub media: MediaItem,          // id, path (its extension is the decoder's hint), file_hash
    pub title: String,             // MusicTrack::display_title
    pub artist: String,            // tags.artists joined with ", "; "" when none
    pub album: String,             // tags.album, or ""
    pub duration_ms: Option<u64>,  // tags.duration_ms
}
impl QueueEntry { pub fn from_track(t: &MusicTrack) -> QueueEntry; }

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Quality { #[default] Original, Low }
impl Quality { pub fn preset(self) -> &'static str; } // "original" | "offline-audio"

pub struct Endpoint {
    pub url: Url,
    pub token: Token,
    pub library_id: String,
    /// Where this process keeps the tracks it streams (Task 6 makes it).
    pub stream_dir: PathBuf,
}

pub enum Command {
    Replace { entries: Vec<QueueEntry>, start: usize },
    PlayNext(Vec<QueueEntry>),
    Append(Vec<QueueEntry>),
    Remove(usize),
    Move { from: usize, to: usize },
    Jump(usize),
    Play, Pause, Toggle,
    Seek(u64),            // ms
    Next, Previous,
    SetVolume(f32),       // clamped to 0.0..=1.0
    Stop,
    SetEndpoint(Option<Endpoint>),
    SetQuality(Quality),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status { Stopped, Playing, Paused, Buffering, Preparing }

#[derive(Clone, PartialEq, Debug)]
pub enum PlayerError {
    /// The track could not be played; it was skipped.
    Unplayable { title: String, reason: String },
    /// The server could not be reached, or a fetch stalled; playback paused.
    Network(String),
    /// The server refused the token; playback paused.
    Unauthorized,
    /// The audio device failed and could not be reopened; playback paused.
    Output(String),
    /// MAX_SKIPS tracks in a row could not be played; playback stopped.
    TooManyFailures,
}

#[derive(Clone, PartialEq, Debug)]
pub struct PlayerState {
    pub queue: Vec<QueueEntry>,
    pub current: Option<usize>,
    pub status: Status,
    pub position_ms: u64,
    /// The track's length: the decoder's, else the tag's.
    pub duration_ms: Option<u64>,
    pub volume: f32,
    pub error: Option<PlayerError>,
}
```

- Produces（`player/engine.rs`）: 本物と偽物の境目になるトレイトと、同期的な `Engine`。

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OutputConfig { pub sample_rate: u32, pub channels: u16 }

pub trait AudioOutput: Send {
    fn config(&self) -> OutputConfig;
    /// Free room in the device's buffer, in frames.
    fn writable(&self) -> usize;
    /// Interleaved samples in `config().channels`, at most `writable()` frames.
    fn write(&mut self, samples: &[f32]);
    /// Frames the device has played since it was opened.
    fn played(&self) -> u64;
    fn set_paused(&mut self, paused: bool);
    /// Drops what is buffered and not yet played.
    fn clear(&mut self);
    /// Set once the device is gone (unplugged, say).
    fn failed(&self) -> Option<String>;
}
pub trait OutputFactory: Send {
    fn open(&mut self) -> Result<Box<dyn AudioOutput>, String>;
}

pub enum Wait { Buffering, Preparing }
pub enum TrackError { Unplayable(String), Network(String), Unauthorized }
pub enum FeedEvent { Pcm(Vec<f32>), Waiting(Wait), End, Failed(TrackError) }

/// One track being decoded, already in the output's format. Never blocks.
pub trait Feed: Send {
    fn poll(&mut self) -> FeedEvent;
    /// The decoder's length, once known.
    fn duration_ms(&self) -> Option<u64>;
}
pub trait Opener: Send {
    /// Opens `entry` from `at_ms`, converting to `target`. A seek is a new `open` of the
    /// same entry: the opener keeps what it fetched.
    fn open(&mut self, entry: &QueueEntry, at_ms: u64, target: OutputConfig) -> Box<dyn Feed>;
    /// The track after the current one, to fetch once the current one is fetched.
    fn preload(&mut self, next: Option<&QueueEntry>);
    fn set_endpoint(&mut self, endpoint: Option<Endpoint>);
    fn set_quality(&mut self, quality: Quality);
    /// Forgets every track: stops fetching and deletes the temporary files.
    fn clear(&mut self);
}

pub struct Engine { /* opener, outputs, output, feed, state, skips, base position */ }
impl Engine {
    pub fn new(opener: Box<dyn Opener>, outputs: Box<dyn OutputFactory>) -> Engine;
    pub fn handle(&mut self, command: Command);
    /// Moves PCM from the feed to the output and follows the track's end. Returns how soon
    /// it wants to be called again: ~10 ms while playing or waiting, `None` when idle.
    pub fn pump(&mut self) -> Option<Duration>;
    pub fn state(&self) -> &PlayerState;
}
```

**規則（spec §5.4 から）**
- 出力は最初の再生で開く。開けなければ `PlayerError::Output` で `Paused`。
- 位置は「いまの曲を開いた位置（ms）＋開いてから出力が鳴らしたフレーム数 / サンプルレート」。曲を開き直す（シーク、次の曲）たびに `output.clear()` し、基準を取り直す。
- 音量は `write` の前にサンプルに掛ける。
- フィードが `End` を返したら、出力が書いた分を鳴らし終えるのを待ってから次の曲を開く（ギャップレスは入れない）。キューの最後なら、最後の曲の頭（位置 0）で `Stopped`。
- `Waiting(Buffering)` → `Status::Buffering`、`Waiting(Preparing)` → `Status::Preparing`。データが来たら `Playing` に戻る。
- `Failed(Unplayable)`：`error = Unplayable`、`skips += 1`、次の曲へ。`skips == MAX_SKIPS` なら `TooManyFailures` で `Stopped`。曲が1つでも `Pcm` を出したら `skips = 0`。
- `Failed(Network)`/`Failed(Unauthorized)`：その位置で `Paused`、エラーを出す。`Play` で同じ位置から開き直す。
- 出力の `failed()`：一度だけ開き直し、いまの位置から曲を開き直して続ける。開けなければ `Output` で `Paused`。
- `Previous`：位置が `RESTART_AFTER_MS` より後なら頭へ、そうでなければ前の曲（最初の曲なら頭へ）。
- `Remove(i)`：`i < current` なら `current -= 1`。`i == current` なら次の曲を同じ再生状態で開く（最後だったら `Stopped`）。
- `Move { from, to }`：`current` は動いた曲を追う。
- `Replace`：前のキューを捨て、`start` の曲を開いて再生する（`start` が範囲外なら 0）。空なら `Stop` と同じ。
- `Seek(ms)`：曲の長さを超えたら長さに丸める（長さが分からなければそのまま）。`Stopped` ではいまの曲の位置だけを変える。
- `Stop`：`Stopped`、キューは残す。
- `SetEndpoint(_)`：止め、キューを空にし、`opener.clear()` してから `opener.set_endpoint(..)`。
- `SetQuality(q)`：`opener.set_quality(q)` だけ。次に開く曲から効く。
- いまの曲が変わるたびに `opener.preload(キューの次の曲)`。
- コマンドで `error` は消えない。次の曲を開いた、または `Play` を受けたときに消す。

- [ ] **Step 1: 偽物とテストを書く**

`fakes.rs` に `FakeOutput`（`played` はテストが `advance(frames)` で進める。`writable` は固定の 4096 フレーム、`write` されたサンプルを記録）と `FakeOpener`（曲ごとに台本を持つ：`Pcm` を何個出すか、途中で `Waiting`/`Failed` を返すか。`open` の呼び出しを `(item id, at_ms)` で記録）を置く。サンプルレート 1000 Hz・1 チャンネルにして、1 フレーム＝1 ms で数える。

```rust
#[test] fn replace_plays_from_start() {}
// Replace([a,b,c], start=1) → current Some(1), Playing; opener.opened == [("b",0)]
#[test] fn end_of_track_moves_on_after_the_output_drains() {}
// b ends: next open waits until played covers what was written, then opens c at 0
#[test] fn end_of_queue_stops_at_the_head_of_the_last_track() {}
#[test] fn previous_restarts_after_three_seconds() {}
// at 3001 ms → reopens same at 0; at 2999 ms → opens the one before
#[test] fn unplayable_tracks_are_skipped_and_three_in_a_row_stop() {}
// [bad, bad, ok, bad, bad, bad, ok]: plays ok, then stops with TooManyFailures before the last ok
#[test] fn network_error_pauses_and_play_resumes_at_the_same_position() {}
#[test] fn removing_and_moving_keep_current_on_the_playing_track() {}
// Review Focus 1: remove before, remove current, move current, replace
#[test] fn seek_past_the_end_is_clamped() {}
#[test] fn volume_scales_the_samples() {}
#[test] fn a_lost_device_is_reopened_once_then_pauses() {}
#[test] fn set_endpoint_stops_clears_and_forgets() {}
// queue empty, Stopped, opener.clear() called before set_endpoint
#[test] fn preload_follows_the_current_track() {}
#[test] fn buffering_and_preparing_show_in_the_status() {}
```

- [ ] **Step 2: テストが失敗することを確かめる**

Run: `cargo test -p asteria-music player::engine`
Expected: `Engine` がないのでコンパイルエラー。

- [ ] **Step 3: `Engine` を実装する**

- [ ] **Step 4: テストが通ることを確かめる**

Run: `cargo test -p asteria-music player::engine && cargo deny check`
Expected: すべて PASS、deny は ok。

- [ ] **Step 5: コミット**

```bash
git add Cargo.toml Cargo.lock deny.toml .github/workflows/ci.yml music
git commit -m "feat(music): the playback engine's queue and state"
```

---

### Task 4: Range で取りながら読む（`player::stream`）

**Files:**
- Create: `music/src/player/stream.rs`
- Modify: `music/Cargo.toml`（`reqwest`、`bytes`。dev に `axum`、`tokio`、`tempfile`）
- Test: `music/src/player/stream.rs` の `#[cfg(test)] mod tests`、テスト用のサーバーは `music/src/player/test_server.rs`（`#[cfg(test)]`。Task 6 でも使う）

**Interfaces:**
- Produces:

```rust
pub enum StreamStatus { Waiting(Wait), Fetching, Complete, Failed(TrackError) }

pub struct StreamRequest {
    /// `<base>/media/<id>/<preset>`.
    pub url: Url,
    pub auth: HeaderValue,          // "Bearer <token>", sensitive
    /// The temporary file; created here, removed when the Stream is dropped.
    pub path: PathBuf,
    /// The first request's timeout: PREPARE_TIMEOUT for offline-audio, else CHUNK_TIMEOUT.
    pub first_timeout: Duration,
    /// Fetch nothing until this one is complete (or dropped): the preloaded next track.
    pub after: Option<Arc<Stream>>,
}

pub struct Stream { /* file, total length, fetched chunks, wanted position, status, cancel */ }
impl Stream {
    /// Starts the fetch thread.
    pub fn start(req: StreamRequest) -> std::io::Result<Arc<Stream>>;
    pub fn reader(self: &Arc<Self>) -> StreamReader;
    pub fn status(&self) -> StreamStatus;
    pub fn len(&self) -> Option<u64>;
    /// Stops the fetch and wakes every reader, which then fail.
    pub fn cancel(&self);
}

/// Read + Seek over the stream. A read of bytes not yet fetched waits for them, and moves
/// the fetch there. Implements symphonia's `MediaSource` (seekable, `byte_len` = `len()`).
pub struct StreamReader { /* Arc<Stream>, pos */ }
```

**規則**
- 取得のスレッドは、読む側が最後に求めた位置を含むチャンクから順に、まだ取っていないチャンクを `Range: bytes=<s>-<e>`（`CHUNK_BYTES` ごと）で頼む。そこから最後まで取ったら、先頭から抜けているチャンクを取る。全部そろったら `Complete`。
- 1回目の応答の `Content-Range: bytes s-e/total` で全体の長さを知る。`206` 以外（`200` で全体を返したなど）は、`s == 0` かつ本文が全体ならそのまま受け付け、そうでなければ `Unplayable("the server does not support ranges")`。
- `503`：`Retry-After`（なければ `DEFAULT_RETRY_AFTER`、長すぎれば `MAX_RETRY_AFTER`。framework-client の `rpc` の定数）を待って同じ要求をし直す。待つ間は `Waiting(Preparing)`（最初の要求）か `Waiting(Buffering)`。
- `404`/`422`：`Failed(Unplayable(..))`。`401`：`Failed(Unauthorized)`。接続できない、タイムアウト、途中で切れた：`Failed(Network(..))`。文言にトークンを入れない。
- `Failed` の後、読む側は待たずに `io::Error` を返す。`cancel` でも同じ。
- 読む側が待つ間の状態は、最初のチャンクが届くまで `Waiting(Preparing)`（`first_timeout` が `PREPARE_TIMEOUT` のとき）、それ以外は `Waiting(Buffering)`。
- `Stream` を落とすと（最後の `Arc`）、スレッドを止め、一時ファイルを消す。

- [ ] **Step 1: テスト用のサーバーと、失敗するテストを書く**

`test_server.rs`：axum でバイト列を Range 対応で返す。テストごとに、要求の記録（`Range` ヘッダー）、最初の n 回は 503、k 番目の要求で接続を切る、応答を止める、を指定できる。

```rust
#[test] fn reads_back_the_whole_body_in_order() {}        // 3.5 MiB of pseudo-random bytes
#[test] fn a_seek_ahead_fetches_from_there_first() {}     // seek to 3 MiB before anything: the 2nd request starts at 3 MiB
#[test] fn a_seek_back_does_not_refetch() {}
#[test] fn retry_after_is_waited_for() {}                 // two 503 with Retry-After: 1, then served
#[test] fn a_dropped_connection_fails_the_reader() {}     // Failed(Network), read returns Err
#[test] fn a_stalled_chunk_fails_after_the_timeout() {}   // CHUNK_TIMEOUT overridden to 300 ms in tests
#[test] fn not_found_is_unplayable_and_unauthorized_is_kept() {}
#[test] fn the_preloaded_stream_waits_for_the_one_before() {}
#[test] fn dropping_the_stream_removes_its_file() {}
#[test] fn the_token_is_not_in_any_error_text() {}
```

タイムアウトはテストで短くできるように、`StreamRequest` とは別に `pub(crate)` のテスト用の値（`#[cfg(test)]` の上書き、または `Stream::start_with(req, Timeouts)`）を用意する。

- [ ] **Step 2: テストが失敗することを確かめる**

Run: `cargo test -p asteria-music player::stream`
Expected: コンパイルエラー。

- [ ] **Step 3: 実装する**

- [ ] **Step 4: テストが通ることを確かめる**

Run: `cargo test -p asteria-music player::stream`
Expected: すべて PASS。

- [ ] **Step 5: コミット**

```bash
git add Cargo.lock music
git commit -m "feat(music): fetch a track in Range chunks while it is read"
```

---

### Task 5: デコードと変換（`player::decode`、`player::resample`）

**Files:**
- Create: `music/src/player/{decode.rs,resample.rs}`、`music/tests/fixtures/{generate.sh,tone.flac,tone.mp3,tone.m4a,tone-alac.m4a,tone.ogg,tone.opus,tone-mono-22k.flac,broken.flac}`
- Modify: `music/Cargo.toml`（`symphonia`、`ogg`、`opus-pure`、`rubato`）、`.gitattributes`（`music/tests/fixtures/** -text`）
- Test: `music/tests/decode.rs`

**Interfaces:**
- Produces（`decode.rs`）:

```rust
pub trait TrackDecoder: Send {
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    fn duration_ms(&self) -> Option<u64>;
    /// The next block of interleaved samples; `None` at the end.
    fn next_block(&mut self) -> Result<Option<Vec<f32>>, DecodeError>;
    /// Seeks to `ms` (clamped to the length); returns where it landed.
    fn seek(&mut self, ms: u64) -> Result<u64, DecodeError>;
}
pub enum DecodeError { Unsupported(String), Corrupt(String), Io(std::io::Error) }

/// Opens Ogg Opus (an `OggS` page whose first packet starts `OpusHead`) with ogg + opus-pure,
/// anything else with symphonia, using `ext` (the file's extension) as its hint.
pub fn open(source: Box<dyn MediaSource>, ext: Option<&str>) -> Result<Box<dyn TrackDecoder>, DecodeError>;
```

- Produces（`resample.rs`）:

```rust
/// Converts interleaved blocks from (rate, channels) to an OutputConfig: channels first
/// (mono is copied to both; more than two keeps the first two), then the rate with rubato.
pub struct Converter { /* ... */ }
impl Converter {
    pub fn new(from_rate: u32, from_channels: u16, to: OutputConfig) -> Result<Converter, String>;
    pub fn process(&mut self, block: &[f32]) -> Vec<f32>;
    /// What is left in the resampler at the end of the track.
    pub fn flush(&mut self) -> Vec<f32>;
}
```

  - レートが同じならリサンプラーを通さない。
- Opus：プリスキップを捨て、最後のページのグラニュールで長さを切る（Plan 3 の試作 `spikes/ios/opus-probe` と同じ。コミット 3bcfff9 の `opus-probe/src/lib.rs` に参考の実装がある）。シークはグラニュールで探し、80 ms 前から復号して捨てる。チャンネルのマッピングファミリーは 0 だけ対応し、それ以外は `Unsupported`。

- [ ] **Step 1: テスト用のファイルを作る**

`generate.sh`（`crates/asteria-framework/tests/fixtures/generate.sh` と同じ書き方。`-map_metadata -1 -fflags +bitexact`）で、2 秒・440 Hz・ステレオ 44.1 kHz の正弦波から `tone.{flac,mp3,m4a(AAC),ogg(Vorbis)}`、`tone-alac.m4a`、`tone.opus`（libopus 96k、48 kHz）、1 秒・モノラル 22.05 kHz の `tone-mono-22k.flac` を作る。`broken.flac` は `tone.flac` の先頭 200 バイトだけ。合計 200 KB 以下に収める。

- [ ] **Step 2: 失敗するテストを書く**

```rust
#[test] fn each_format_decodes_to_its_length_rate_and_channels() {}
// for every tone.*: rate 44100 (opus 48000), 2 ch, decoded frames within 2 % of 2 s,
// peak amplitude between 0.2 and 1.0 (it is the tone, not silence)
#[test] fn seek_lands_near_the_asked_position() {}   // seek(1500) → within 100 ms, then ~0.5 s remains
#[test] fn seek_past_the_end_is_clamped() {}
#[test] fn a_truncated_file_is_an_error_not_a_panic() {}
#[test] fn mono_22k_converts_to_stereo_48k() {}       // 1 s → 48000 ± 1 % frames, both channels equal
#[test] fn same_rate_passes_through() {}
```

- [ ] **Step 3: テストが失敗することを確かめる**

Run: `cargo test -p asteria-music --test decode`
Expected: コンパイルエラー。

- [ ] **Step 4: 実装する**

- [ ] **Step 5: テストが通ることを確かめる**

Run: `cargo test -p asteria-music --test decode && cargo deny check`
Expected: すべて PASS、deny は ok。

- [ ] **Step 6: コミット**

```bash
git add .gitattributes Cargo.lock music
git commit -m "feat(music): decode the library's formats and Ogg Opus, convert to the device"
```

---

### Task 6: 1曲のデコードのスレッドと `HttpOpener`（`player::feed`）

**Files:**
- Create: `music/src/player/feed.rs`
- Test: `music/src/player/feed.rs` の `#[cfg(test)] mod tests`（Task 4 の `test_server` でテスト用のファイルを返す）

**Interfaces:**
- Consumes: Task 3 の `Opener`、`Feed`、`FeedEvent`。Task 4 の `Stream`。Task 5 の `decode::open`、`Converter`。
- Produces:

```rust
/// The real Opener: fetches from the endpoint's server into `stream_dir`.
pub struct HttpOpener { /* endpoint, quality, streams: current and preloaded */ }
impl HttpOpener { pub fn new() -> HttpOpener; }
impl Opener for HttpOpener { /* ... */ }

/// Makes `<cache root>/<library id>/stream/<process tag>/` and removes, beside it, the
/// directories last modified more than STALE_STREAM_DIR ago. The process tag is the pid and
/// 8 random hex digits. Returns the directory; the caller removes it at exit (Task 9).
pub fn stream_dir(cache_root: &Path, library_id: &str) -> std::io::Result<PathBuf>;
```

**規則**
- `open` は、同じ（ID、品質）の `Stream` を持っていればそれを使い、なければ始める。いまの曲と先読みの曲以外の `Stream` は落とす（一時ファイルも消える）。
- 1曲につきスレッドを1つ立て、`decode::open(StreamReader)` → `seek(at_ms)`（0 なら呼ばない）→ `next_block` → `Converter::process` を、容量 32 の `sync_channel` に送る。最後に `flush` の分を送ってから `End`。
- `Feed::poll` は `try_recv`。空なら `Stream::status()` を見て `Waiting(Preparing|Buffering)` を返す。
- デコードのエラーは、`Stream` が `Failed(e)` ならそれを、そうでなければ `Unplayable(<デコーダーの理由>)` を返す。
- `Feed` を落としたらスレッドを止める（`StreamReader` の待ちも `cancel` で起こす。ただし `Stream` 自体は opener が持っているので一時ファイルは残る）。
- `preload(next)` は、先読みの `Stream` を `after: Some(いまの曲の Stream)` で始める。
- `set_endpoint(None)`、`clear()`：すべての `Stream` を落とす。
- URL は `<base>/media/<id>/original` か `<base>/media/<id>/offline-audio`。`offline-audio` の最初の要求だけ `PREPARE_TIMEOUT`。
- エンドポイントがないのに `open` されたら `Failed(Network("not connected"))` を返すフィード。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn a_track_from_the_server_comes_out_as_pcm_then_end() {}  // tone.flac, target 48 kHz/2 ch
#[test] fn opening_again_at_a_position_reuses_the_fetched_file() {} // server sees no second request for chunk 0
#[test] fn low_quality_asks_for_offline_audio() {}                 // path ends with /offline-audio
#[test] fn a_404_is_unplayable() {}
#[test] fn a_stalled_server_is_a_network_failure() {}
#[test] fn preload_waits_for_the_current_track() {}
#[test] fn clear_removes_the_temporary_files() {}
#[test] fn stream_dir_removes_only_stale_siblings() {}  // a sibling with mtime 25 h ago goes, 1 h ago stays
```

- [ ] **Step 2: テストが失敗することを確かめる**

Run: `cargo test -p asteria-music player::feed`
Expected: コンパイルエラー。

- [ ] **Step 3: 実装する**

- [ ] **Step 4: テストが通ることを確かめる**

Run: `cargo test -p asteria-music player::feed`
Expected: すべて PASS。

- [ ] **Step 5: コミット**

```bash
git add music
git commit -m "feat(music): decode a fetched track on its own thread"
```

---

### Task 7: 音の出力と `Player` のスレッド（`player::output`、`player::Player`）

**Files:**
- Create: `music/src/player/{output.rs,thread.rs}`
- Modify: `music/Cargo.toml`（`cpal`、`rtrb`、`corundum-framework`（`egui` の `Context` だけ使う））
- Test: `music/src/player/thread.rs` の `#[cfg(test)] mod tests`、`output.rs` の `#[cfg(test)] mod tests`（デバイスを使わない部分だけ）

**Interfaces:**
- Produces（`output.rs`）:

```rust
/// cpal's default output device, at its default config, as f32. Samples go through an rtrb
/// ring of about 200 ms; the callback writes silence when it runs dry or while paused.
pub struct CpalOutputs;
impl OutputFactory for CpalOutputs { /* ... */ }
```

  - 出力の形式が f32 以外（i16、u16 など）のデバイスでも、変換して書く。
  - `played()` はコールバックが実際に渡したフレーム数（無音を除く）。
  - ストリームのエラーのコールバックで `failed` を立てる。
  - リングの読み書きと変換は、cpal から切り離した関数にしてテストする（デバイスは CI にない）。
- Produces（`thread.rs`、`player/mod.rs` から `pub use`）:

```rust
pub struct Player { /* tx, state: Arc<Mutex<PlayerState>>, thread */ }
impl Player {
    /// Runs `Engine::new(opener, outputs)` on a thread named "asteria-music-player".
    /// `ctx` is asked to repaint when the state changes (the position at most every 250 ms).
    pub fn spawn(opener: Box<dyn Opener>, outputs: Box<dyn OutputFactory>, ctx: egui::Context)
        -> std::io::Result<Player>;
    pub fn send(&self, command: Command);
    pub fn state(&self) -> PlayerState;   // a clone
}
impl Drop for Player { /* sends a stop, joins the thread */ }
```

  - スレッドは `recv_timeout(pump が返した時間)`、`None` なら `recv()` で待つ。コマンドを処理したら `pump` を呼ぶ。

- [ ] **Step 1: 失敗するテストを書く**

```rust
// thread.rs, with Task 3's `player::fakes`
#[test] fn commands_reach_the_engine_and_the_state_comes_back() {}
#[test] fn dropping_the_player_joins_its_thread() {}
// output.rs
#[test] fn the_ring_counts_only_real_frames_as_played() {}
#[test] fn i16_devices_get_converted_samples() {}
```

- [ ] **Step 2: テストが失敗することを確かめる**

Run: `cargo test -p asteria-music player::thread player::output`
Expected: コンパイルエラー。

- [ ] **Step 3: 実装する**

- [ ] **Step 4: テストが通ることを確かめる**

Run: `cargo test -p asteria-music && cargo clippy -p asteria-music --all-targets --target x86_64-pc-windows-gnu -- -D warnings`
Expected: すべて PASS。Windows 向けの clippy も通る。

- [ ] **Step 5: コミット**

```bash
git add Cargo.lock music
git commit -m "feat(music): play through cpal on the player's thread"
```

---

### Task 8: 閲覧の画面（アルバム、アーティスト、プレイリスト）

**Files:**
- Create: `music/src/{app.rs,connection.rs,library.rs,cover_grid.rs,tracks.rs,i18n.rs}`、`music/i18n/{en,ja}/music.ftl`
- Test: `music/tests/support/mod.rs`、`music/tests/library.rs`

**Interfaces:**
- Consumes: Task 1 の `Request::Call`、`ImageLoader::request_id`。Task 2 の `Saver`、`ServerSwitcher`。Task 3 の `Command`、`QueueEntry`。
- Produces:
  - `app.rs`：`MusicApp`。photos の `PhotosApp` と同じ形（`AppPaths { servers, playback, cache }`、`new(paths, ctx, player: Player)`、`show(ui)`、`switch_to`、`connection()`、`profiles()`、`active_profile()`、`current_screen()`）。`Screen::{Albums, Artists, Playlists, Queue, Settings}`。この Task では Queue と Settings の画面は空でよい（Task 9）。サーバーの設定ページと切り替えは Task 2 の部品を使う。
  - `connection.rs`：`Connection { session, library: LibraryState }`。`asteria_music_api::client_info()` で開く。photos と同じく `library.info` を `SCANNING_POLL`/`IDLE_POLL` で聞き直し、`take_refetch_listing` で一覧を取り直す。
  - `library.rs`：`LibraryState`。3つの一覧（`album.list`、`artist.list`、`playlist.list`。`PAGE_SIZE` ずつ、最後に近づいたら次のページ）と、画面ごとに開いているもの（`Album`、`Artist`→その中の `Album`、`Playlist`）。返事は `RequestId` で突き合わせ、待っていない返事は捨てる（photos の `BrowseState` と同じ）。`Response::Value` を直せなければ `ClientError::Protocol` として表示する。
    - `pub fn play_from(&self, list: TrackList, index: usize) -> Option<Command>`：一覧の曲でキューを置き換えるコマンドを作る。`TrackList::{Album, Playlist}`。プレイリストは `Found`/`Moved` の曲だけを入れ、`index`（エントリの位置）を入れた曲の位置に直す。`index` が `Missing` なら `None`。
    - `pub fn play_next(&self, list, index) -> Option<Command>`、`pub fn append(&self, list, index) -> Option<Command>`：右クリックのメニュー。
  - `cover_grid.rs`：アルバムのグリッド。セルはジャケット（`cover_item` を `request_id(.., Thumb)`。なければ `icons::music()`）、アルバム名、アルバムアーティスト。クリックとダブルクリックで開く。`ThumbGrid` の列の計算（`ThumbGrid::columns_for`）と最後に描いた位置（ページ送り用）は同じ考え方にする。
  - `tracks.rs`：曲の表（ディスク、番号、曲名、アルバムアーティストと違うときだけアーティスト、長さ `m:ss`）。プレイリストでは `Missing` を薄く、元のパスを添えて、`Moved` に印を付ける。行のクリックで `play_from`、右クリックのメニューで `play_next`/`append`。返したコマンドは `MusicApp` が `player.send` する。
  - レールのアイコン：アルバム `icons::music()`、アーティスト `music/assets/icons/mic-vocal.svg`（Lucide、ISC。corundum に人のアイコンがないので、framework-client の `lock.svg` と同じく同梱し、`egui::include_image!` で読む。corundum に足すイシューは PR のメモに書く）、プレイリスト `icons::file_text()`、キュー `icons::list_music()`、設定 `icons::settings()`。

- [ ] **Step 1: テスト用の支えと、失敗するテストを書く**

`tests/support/mod.rs`：photos の `tests/support/mod.rs` と同じく、framework-client の `tests/common` を `#[path]` で共有し、`server.info`（app `asteria-music`、app_api 2）、`library.info`、`album.list`/`album.get`/`artist.list`/`artist.get`/`playlist.list`/`playlist.get` を JSON で返すスタブを作る。`Player` は Task 3 の `asteria_music::player::fakes` で作る。

```rust
#[test] fn albums_load_and_the_next_page_follows() {}        // 250 albums: two album.list calls
#[test] fn opening_an_album_shows_its_tracks() {}
#[test] fn clicking_a_track_replaces_the_queue_from_it() {}   // via play_from(Album, 2): Replace{start 2}
#[test] fn playlist_missing_entries_are_left_out_of_the_queue() {} // [found, missing, moved, found], index 2 → start 1 of 3
#[test] fn play_next_and_append_build_their_commands() {}
#[test] fn an_artist_shows_own_and_appears_on_albums() {}
#[test] fn a_reply_after_switching_servers_is_ignored() {}
#[test] fn a_value_that_does_not_parse_is_a_protocol_error() {}
#[test] fn every_frame_draws_without_panicking_on_each_screen() {} // run frames on each Screen, compact too
```

- [ ] **Step 2: テストが失敗することを確かめる**

Run: `cargo test -p asteria-music --test library`
Expected: コンパイルエラー。

- [ ] **Step 3: 実装する**

- [ ] **Step 4: テストが通ることを確かめる**

Run: `cargo test -p asteria-music`
Expected: すべて PASS。

- [ ] **Step 5: コミット**

```bash
git add music
git commit -m "feat(music): browse albums, artists and playlists"
```

---

### Task 9: 再生バー、再生キュー、設定、窓を開く

**Files:**
- Create: `music/src/{now_playing.rs,queue.rs,settings.rs,desktop.rs}`
- Modify: `music/src/{app.rs,main.rs,lib.rs}`、`music/i18n/{en,ja}/music.ftl`
- Test: `music/tests/playback.rs`、`music/tests/i18n.rs`

**Interfaces:**
- Produces:
  - `settings.rs`：

```rust
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaybackSettings { pub low_quality: bool, pub volume: f32 }  // default: false, 1.0
impl PlaybackSettings {
    /// A missing or unreadable file gives the defaults (logged); volume is clamped.
    pub fn load(path: &Path) -> PlaybackSettings;
    pub fn save(&self, path: &Path) -> std::io::Result<()>;   // temp file + rename
}
```

    - 起動時に読んで `SetQuality`、`SetVolume` を送る。変えたら `Saver<PlaybackSettings>` で書く。設定画面に「再生」のページを足し、「低音質で再生する」のスイッチと、その説明（次の曲から効く、回線が細いときはオフラインのキャッシュを使う想定）を置く。
  - `now_playing.rs`：再生バー。`ShellApp::screen_ui` の最初に `egui::Panel::bottom` で描く（設定の画面にも出す）。幅が広いとき：ジャケット（いまの曲の `media` を `images().request(.., Thumb)`）、曲名、アーティスト、前／再生・一時停止／次、シークバー（ドラッグを離したときに `Seek`）と `m:ss / m:ss`、音量（`icons::volume_2()`/`volume_x()` と横のスライダー）、`Buffering` と `Preparing` の表示、エラー（`PlayerError` を文言にしたもの）。狭いとき：ジャケット、曲名、再生・一時停止の1行。キューが空なら描かない。
  - `queue.rs`：キューの画面。行は曲名、アーティスト、長さ。いまの曲を強調。行のクリックで `Jump`、`↑`/`↓` で `Move`、`icons::x()` で `Remove`。
  - `app.rs` のつなぎ込み：
    - セッションが `Ready` になったら `SetEndpoint(Some(Endpoint { url, token, library_id, stream_dir }))`。`stream_dir` は `feed::stream_dir(paths.cache, library_id)`（キャッシュのディレクトリがない、または作れなければ `std::env::temp_dir()/asteria-music` の下）。
    - サーバーを切り替えた（接続を落とした）ら `SetEndpoint(None)`。
    - `MusicApp` を落とすときに、作った `stream_dir` を消す。
  - `desktop.rs`：photos の `desktop.rs` と同じ形。`APP_NAME = "asteria-music"`。`paths(dirs)` は `servers.toml`、`playback.toml`（設定ディレクトリ）、キャッシュのディレクトリ。`run()` は `Player::spawn(Box::new(HttpOpener::new()), Box::new(CpalOutputs), ctx)` を作って `MusicApp::new` に渡す。
  - `main.rs`：photos の `main.rs` と同じ。

- [ ] **Step 1: 失敗するテストを書く**

```rust
// tests/playback.rs (fake player parts, stub server)
#[test] fn ready_session_hands_the_player_its_endpoint() {}   // the fake opener saw set_endpoint(Some) with the library id
#[test] fn switching_servers_stops_and_clears_the_player() {} // Review Focus 4
#[test] fn the_bar_buttons_send_their_commands() {}           // via MusicApp methods the bar calls: toggle, next, previous, seek
#[test] fn queue_rows_jump_move_and_remove() {}
#[test] fn low_quality_and_volume_are_saved_and_restored() {} // a new MusicApp on the same paths sends SetQuality(Low), SetVolume(0.4)
#[test] fn an_unreadable_playback_toml_gives_the_defaults() {}
#[test] fn closing_the_app_removes_its_stream_directory() {}
// tests/i18n.rs
#[test] fn every_key_used_exists_in_english_and_japanese() {} // as photos' tests/i18n.rs
```

- [ ] **Step 2: テストが失敗することを確かめる**

Run: `cargo test -p asteria-music --test playback --test i18n`
Expected: コンパイルエラー。

- [ ] **Step 3: 実装する**

- [ ] **Step 4: テストが通ることを確かめる**

Run: `cargo test -p asteria-music`
Expected: すべて PASS。

- [ ] **Step 5: コミット**

```bash
git add music
git commit -m "feat(music): the playback bar, the queue, playback settings and the window"
```

---

### Task 10: README と全体の確認

**Files:**
- Modify: `README.md`、`README.ja.md`

- [ ] **Step 1: README に music のクライアントを足す**

起動のしかた（`cargo run -p asteria-music`）、サーバーの登録、Linux では ALSA（`libasound2-dev`）が要ること、「低音質で再生する」の意味、まだないもの（フォルダ表示 #20、評価 #19、シャッフルとリピート #17、ギャップレス #18、メディアキー corundum-framework#50）を、photos の節と同じ書き方で足す。

- [ ] **Step 2: 全体を確かめる**

Run:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings
cargo fmt --all -- --check
cargo deny check
```

Expected: すべて通る。

- [ ] **Step 3: 手で確かめること（PR のチェックリストに入れる）**

実際の music-server とデスクトップで：アルバムからの再生、シーク（未取得の位置と取得済みの位置）、次の曲への移行、ハイレゾ FLAC、低音質での再生（初回の「準備中」）、再生中のサーバー停止と再開、サーバーの切り替え、イヤホンの抜き差し、閉じたあとに一時ディレクトリが残らないこと。

- [ ] **Step 4: コミット**

```bash
git add README.md README.ja.md
git commit -m "docs: the music desktop client"
```
