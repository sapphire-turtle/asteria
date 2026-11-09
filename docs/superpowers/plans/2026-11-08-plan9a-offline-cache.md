# Plan 9a: オフラインキャッシュ 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** music ではアルバム・アーティスト・プレイリストを、photos ではフォルダを「オフラインに保存」でき、サーバーに届かないときも（起動時から）保存したものを閲覧・再生できる。保存するのは低画質・低音質の版（`offline-image` 1024px、`offline-audio` 96 kbps）。

**Architecture:**
- `asteria-common-client` に `offline/` を足す。`store`（ディスクの配置）、`known`（プロファイル→最後のライブラリ）、`catalog`（アプリが実装する `OfflineCatalog` と、読み込んだ写しの索引 `Docs`）、`sync`（同期のスレッドとその窓口 `OfflineHandle`）、`page`（設定の「オフライン」ページ）。
- `Session` は、ハンドシェイクが届かず記録したライブラリがあれば `Offline` になり、要求を `OfflineCatalog::answer` で答える。`Gate::Offline` を足し、`ImageLoader` と同期もこれを見る。
- サーバーは `offline-image` のプリセットを足すだけ。API のバージョンは変えない。

**Tech Stack:** Rust（`rust-toolchain.toml` のまま）、egui（corundum-framework 0.11 の再エクスポート）、reqwest blocking、serde_json、blake3、corundum-framework-audio 0.11。

**Spec:** `docs/superpowers/specs/2026-10-22-asteria-design.md`（§5.10 が中心。§4.5、§5.1、§5.4、§7）

## Global Constraints

- CI は clippy `-D warnings`、`cargo fmt --check`、`cargo deny check`、Linux（x86_64・arm64）・macOS・Windows のテスト、`asteria-common-client` と `asteria-<app>-mobile` の `aarch64-apple-ios` 向け `cargo check`。手元でも `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings` を通す。
- `asteria-common-client` は eframe に依存しない（egui だけ）。新しい依存は `blake3`（ワークスペースの `blake3 = "1"`）だけ。
- テストは一時ディレクトリだけを使い、本物のユーザーのディレクトリに触れない。
- トークンをログにも画面にも出さない。
- 画面の文字列は Fluent。共通は `crates/asteria-common-client/i18n/{en,ja}/client.ftl` の `asteria-common-client-offline-<what>`、アプリは各 `*.ftl` の `asteria-<app>-offline-<what>`。en と ja を同時に足す。
- 置き場所はアプリのデータディレクトリ（`SessionDirs::data`）の `offline/`。`<cache_dir>` に置かない。
- 定数（`asteria_common_client::offline`）
  - `DEFAULT_LIMIT_BYTES = 8 << 30`、`LIMIT_CHOICES_GIB = [1, 2, 4, 8, 16, 32]`
  - `DOWNLOADS_AT_ONCE = 2`
  - `AUDIO_TIMEOUT = 600 s`（`offline-audio` の取得。ほかは `RpcClient::new` の既定）
  - `RETRY_ATTEMPTS = 20`（503 をやり直す回数の上限。超えたらその同期では諦め、「保存できない」にはしない）
  - `RECONNECT_EVERY = 30 s`
  - `STALE_TMP = 24 h`
  - `DOC_SCHEMA = 1`
  - 目安の大きさ：曲は `duration_ms * 12` バイト（96 kbps）、`offline-image` は 130_000、`thumb` は 20_000。長さが無い曲は 4 分とする。
- WebP の品質：`thumb`・`preview` は 80、`offline-image` は 70。`offline-image` は長辺 1024px。

## Review Focus

1. 同期の途中で回線が切れる（ノート PC のスリープ、地下鉄）：その同期は止まり、何も消さず、再接続で続きから取る。→ Task 3 の `a_sync_cut_off_midway_deletes_nothing`
2. 同じ曲が2つのピンにある（アルバムとプレイリスト）：片方を外しても、もう片方のメディアは残る。→ Task 3 の `unpinning_keeps_media_another_pin_uses`
3. オフラインで開いた後、つながったサーバーが別のライブラリを出している：`Failed(LibraryChanged)` になり、古い写しをそのサーバーのものとして使い続けない。→ Task 4 の `reconnecting_to_another_library_fails`
4. 容量の上限を今の使用量より下げる：保存済みは消さず、次のダウンロードで「容量が足りない」になる。→ Task 3 の `a_lower_limit_stops_downloads_but_keeps_files`
5. 同期中にサーバーを切り替える（セッションを捨てる）：同期のスレッドはすぐ止まり、以後そのサーバーへ要求しない。→ Task 4 の `dropping_the_session_stops_the_sync`

---

### Task 1: サーバーの `offline-image` プリセット

**Files:**
- Modify: `crates/asteria-common-server/src/process.rs`（`Preset`、`render` の分岐、vips の `[Q=80,strip]`、ffmpeg の `-quality 80`、`sips::finish(&bytes, 80.0)`）
- Modify: `crates/asteria-common-server/src/inprocess_image.rs`（`render` に品質を渡す）
- Modify: `crates/asteria-common-server/src/media_http.rs`（`thumb_file` の `debug_assert` と doc）
- Test: `crates/asteria-common-server/src/process.rs` の tests、`crates/asteria-common-server/tests/media_http.rs`

**Interfaces:**
- Produces: `Preset::OfflineImage`（名前 `"offline-image"`、`size() == 1024`、`ext() == "webp"`、`mime() == "image/webp"`）、`Preset::quality(self) -> f32`（Thumb・Preview 80.0、OfflineImage 70.0、OfflineAudio 0.0）、`inprocess_image::render(src: InSource, longest: u32, quality: f32)`。

- [ ] **Step 1: 失敗するテストを書く**（process.rs の tests）
  - `offline_image_parses_and_is_1024_webp`：`Preset::parse("offline-image") == Some(Preset::OfflineImage)`、`name()`・`size()`・`ext()` が上の値、`quality()` が 70.0、`Preset::Preview.quality()` が 80.0。
  - `offline_image_never_enlarges`：既存の Preview の縮小テストと同じ作りで、400x200 の JPEG を `Preset::OfflineImage` で render し、出力の WebP が 400x200 のまま。3000x2000 なら 1024x683。
  - tests/media_http.rs に `offline_image_of_a_photo_is_served`：既存の thumb の e2e と同じ作りで `GET /media/{id}/offline-image` が 200、`content-type: image/webp`、デコードした長辺が 1024 以下。
- [ ] **Step 2: 走らせて失敗を確かめる**
  Run: `cargo test -p asteria-common-server offline_image`
  Expected: FAIL（`OfflineImage` が無い）
- [ ] **Step 3: 実装する**
  `Preset` に `OfflineImage` を足し、`render` の `(MediaKind::Image | Video, Thumb | Preview, …)` の腕に `OfflineImage` を加える。品質はすべての経路（プロセス内、vips の `[Q=<q>,strip]`、ffmpeg の `-quality <q>`、sips の `finish`）で `preset.quality()` を使う。`inprocess_image::QUALITY` は消す。`thumb_file` の `debug_assert` に `OfflineImage` を加える（MCP は今のまま Thumb だけ）。
- [ ] **Step 4: 走らせて通るのを確かめる**
  Run: `cargo test -p asteria-common-server`
  Expected: PASS（既存のテストも含めて）
- [ ] **Step 5: コミット**
  `git commit -m "feat(server): the offline-image preset (1024px, WebP quality 70)"`

### Task 2: 写しの保存先（`offline::store` と `offline::known`）

**Files:**
- Create: `crates/asteria-common-client/src/offline/mod.rs`（定数と再エクスポート）、`offline/store.rs`、`offline/known.rs`
- Modify: `crates/asteria-common-client/src/lib.rs`（`pub mod offline;`）、`crates/asteria-common-client/Cargo.toml`（`blake3.workspace = true`）
- Test: `store.rs` と `known.rs` の `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `media_cache::safe_component`、`asteria_common_api::{ItemId, LibraryInfo}`、`profiles::ProfileId`。
- Produces（すべて `asteria_common_client::offline` から公開）:
  ```rust
  #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
  pub struct PinTarget { pub kind: String, pub id: String, pub name: String }
  #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
  pub enum PinStatus { Pending, Saved, Gone }
  #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
  pub struct Pin { pub target: PinTarget, pub added_unix: u64, pub status: PinStatus }
  #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
  pub struct Doc { pub kind: String, pub id: String, pub value: serde_json::Value }
  #[derive(Clone, Debug, PartialEq, Eq, Hash)]
  pub struct MediaKey { pub item: ItemId, pub file_hash: String, pub preset: String, pub estimate: u64 }
  #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
  pub struct SyncMark { pub epoch: String, pub revision: u64, pub pins_digest: String }

  pub struct OfflineStore { /* root: <data>/offline/<library id> */ }
  impl OfflineStore {
      pub fn open(data_root: &Path, library_id: &str) -> Option<OfflineStore>; // None: unsafe id
      pub fn media(&self, file_hash: &str, preset: &str) -> Option<PathBuf>;   // an existing regular file
      pub fn put_media(&self, file_hash: &str, preset: &str, bytes: &[u8]) -> io::Result<PathBuf>;
      pub fn media_bytes(&self) -> u64;
      pub fn read_docs(&self) -> Vec<Doc>;
      pub fn write_doc(&self, doc: &Doc) -> io::Result<()>;
      pub fn remove_doc(&self, kind: &str, id: &str) -> io::Result<()>;
      pub fn load_pins(&self) -> Vec<Pin>;
      pub fn save_pins(&self, pins: &[Pin]) -> io::Result<()>;
      pub fn load_mark(&self) -> SyncMark;
      pub fn save_mark(&self, mark: &SyncMark) -> io::Result<()>;
      /// Deletes media files not in `keep` ((file_hash, preset)); returns the bytes freed.
      pub fn collect_garbage(&self, keep: &HashSet<(String, String)>) -> io::Result<u64>;
      pub fn sweep_stale_tmp(&self);
      pub fn clear(&self) -> io::Result<()>;
  }
  pub struct KnownLibraries { /* <data>/offline/libraries.json */ }
  impl KnownLibraries {
      pub fn new(data_root: &Path) -> KnownLibraries;
      pub fn get(&self, profile: ProfileId) -> Option<LibraryInfo>;
      pub fn record(&self, profile: ProfileId, info: &LibraryInfo) -> io::Result<()>;
  }
  ```
- 配置（spec §5.10 の表どおり）：`pins.json`、`state.json`（`SyncMark`）、`docs/<kind>/<blake3(id) の16進>.json`（中身は `{"schema": DOC_SCHEMA, "kind", "id", "value"}`）、`media/<preset>/<hex[..2]>/<hex>.<ext>`（ext は `offline-audio` なら `ogg`、ほかは `webp`）。`kind` と `preset` は `safe_component`、ハッシュは `blake3:` と 64 桁の小文字 16 進でなければ `InvalidInput`。
- 書き込みは `media_cache` の `write_atomic` と同じく、隣の `.<名前>.<pid>-<n>.asteria-tmp` に書いてから名前を変える。`pins.json`・`state.json`・`libraries.json` は `sync_all` してから名前を変える（再生成できない利用者の選択を含むため）。
- `read_docs` は JSON として読めないファイルと `schema` が違うファイルを消して飛ばす。

- [ ] **Step 1: 失敗するテストを書く**
  - `media_round_trips_and_is_counted`：`put_media("blake3:"+"ab"*32, "offline-audio", b"ogg")` のパスが `media/offline-audio/ab/<hex>.ogg` で終わり、`media()` が同じパス、`media_bytes() == 3`。
  - `unsafe_names_are_refused`：`open(dir, "..")` が `None`、`put_media("md5:x", "thumb", …)` と `put_media(valid, "../x", …)` が `InvalidInput`、`write_doc(Doc{kind:"a/b",..})` が `InvalidInput`。
  - `docs_round_trip_and_bad_files_are_dropped`：`write_doc` した2つが `read_docs` で戻る。同じディレクトリに `{"schema":99,…}` と `not json` を置くと、`read_docs` はその2つを返さず、ファイルも消えている。
  - `doc_ids_never_become_paths`：`id` が `"../../etc"` の doc を書いても `docs/<kind>/` の外に何もできない。
  - `garbage_collection_keeps_what_is_kept`：3つ置き、2つを `keep` にすると1つだけ消え、返り値がその大きさ。
  - `stale_tmp_is_swept_fresh_tmp_is_not`：25 時間前の mtime（`File::set_modified`）の `.x.1-0.asteria-tmp` は消え、今のものは残る。
  - `pins_and_mark_round_trip`、`a_missing_store_reads_as_empty`（`load_pins` は空、`load_mark` は `Default`）。
  - known.rs：`record_then_get`（2つのプロファイルを記録して両方取れる、再度 record で上書き）、`a_corrupt_file_reads_as_nothing`。
- [ ] **Step 2: 走らせて失敗を確かめる**
  Run: `cargo test -p asteria-common-client --lib offline::`
  Expected: FAIL（モジュールが無い）
- [ ] **Step 3: 実装する**（上の Interfaces どおり。`clear` は `<data>/offline/<library id>` を丸ごと消す）
- [ ] **Step 4: 走らせて通るのを確かめる**
  Run: `cargo test -p asteria-common-client --lib offline::`
  Expected: PASS
- [ ] **Step 5: コミット**
  `git commit -m "feat(client): the offline store (pins, answer copies, media) and the known libraries"`

### Task 3: カタログと同期（`offline::catalog`、`offline::sync`）

**Files:**
- Create: `crates/asteria-common-client/src/offline/catalog.rs`、`offline/sync.rs`
- Test: `crates/asteria-common-client/tests/offline_sync.rs`（`tests/common` のスタブサーバーを使う）

**Interfaces:**
- Consumes: Task 2 のすべて、`rpc::{RpcClient, ClientError}`、`session::{Request, Response}`、`profiles::Token`。
- Produces:
  ```rust
  /// One RPC as the sync makes it.
  pub trait Fetch { fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, ClientError>; }

  /// What an app knows about its own answers. Runs on worker threads.
  pub trait OfflineCatalog: Send + Sync + 'static {
      /// Every document `pin` needs, fetched through `rpc` (paging and following links as
      /// the app must). `Err(ClientError::NotFound)` means the server no longer has it.
      fn fetch(&self, pin: &PinTarget, rpc: &mut dyn Fetch) -> Result<Vec<Doc>, ClientError>;
      /// The media `doc` points to.
      fn media_of(&self, doc: &Doc) -> Vec<MediaKey>;
      /// The answer to `request` from the saved documents; `None` if it cannot be answered.
      fn answer(&self, request: &Request, docs: &Docs) -> Option<Result<Response, ClientError>>;
  }

  pub struct Docs { /* (kind, id) -> Doc; ItemId -> file_hash from every media_of */ }
  impl Docs {
      pub fn new(docs: Vec<Doc>, catalog: &dyn OfflineCatalog) -> Docs;
      pub fn get(&self, kind: &str, id: &str) -> Option<&Doc>;
      pub fn of_kind<'a>(&'a self, kind: &'a str) -> impl Iterator<Item = &'a Doc> + 'a; // sorted by id
      pub fn hash_of(&self, item: &ItemId) -> Option<&str>;
  }

  #[derive(Clone, Debug, PartialEq)]
  pub enum SyncPhase { Idle, Syncing { done: u32, total: u32 }, Full, Failed(String) }
  #[derive(Clone, Debug, PartialEq)]
  pub struct SyncProgress { pub phase: SyncPhase, pub unsavable: u32, pub used_bytes: u64, pub limit_bytes: u64 }
  #[derive(Clone, Debug, PartialEq)]
  pub struct PinInfo { pub pin: Pin, pub bytes: u64, pub estimate: Option<u64> }

  /// The window onto one library's offline copies and its sync thread. Clones share it;
  /// the thread stops when the last clone is dropped or `stop` is called.
  #[derive(Clone)]
  pub struct OfflineHandle { /* Arc<…> */ }
  impl OfflineHandle {
      pub fn start(store: OfflineStore, catalog: Arc<dyn OfflineCatalog>, url: Url, token: Token,
                   settings: PathBuf, ctx: egui::Context) -> OfflineHandle;
      pub fn pin(&self, target: PinTarget);
      pub fn unpin(&self, kind: &str, id: &str);
      pub fn status(&self, kind: &str, id: &str) -> Option<PinStatus>;
      pub fn pins(&self) -> Vec<PinInfo>;          // in the order added
      pub fn pin_info(&self, kind: &str, id: &str) -> Option<PinInfo>;
      pub fn progress(&self) -> SyncProgress;
      pub fn sync_now(&self);
      pub fn clear_all(&self);
      pub fn limit_bytes(&self) -> u64;
      pub fn set_limit_bytes(&self, bytes: u64);
      pub fn docs(&self) -> Arc<Docs>;
      pub fn media(&self, file_hash: &str, preset: &str) -> Option<PathBuf>;
      pub fn media_of_item(&self, item: &ItemId, preset: &str) -> Option<PathBuf>; // via Docs::hash_of
      pub fn catalog(&self) -> &Arc<dyn OfflineCatalog>;
      /// The session's view of the server: syncs (if the mark changed) only while online.
      pub(crate) fn set_online(&self, library: Option<&LibraryInfo>);
      pub(crate) fn stop(&self);
  }
  ```
- `settings` は `<data>/offline/settings.toml`（`limit_bytes = <u64>`。無い・読めないときは `DEFAULT_LIMIT_BYTES`）。アプリ全体（全ライブラリ）で1つ。
- 同期の手順は spec §5.10「同期」のとおり。補足の決まり：
  - `set_online(Some(info))` で、`SyncMark { epoch, revision, pins_digest }` が前回の成功と違えば同期する。`pins_digest` はピンの（kind, id）を並べた blake3。`pin`・`sync_now` は常に同期する（オンラインのときだけ。オフラインなら次につながったとき）。`set_online(None)` は進行中の同期を止める。
  - 応答は `Docs` を作り直して `docs()` を差し替えてから、メディアの取得に進む（取得中も新しい写しで答えられるように）。
  - ピンの `status`：応答が取れたら `Saved`（メディアが取りきれていなくても）、`NotFound` なら `Gone`。`Gone` のピンの写しとメディアは残す。
  - `NotFound` 以外のエラーで応答が取れないピンがあれば、その同期では掃除をしない。メディアの取得の 404・422 は `unsavable` を数え、`Unreachable` は同期を止める（`SyncPhase::Failed`）。
  - 容量：`used_bytes + estimate > limit_bytes` なら取得を止めて `Full`。
  - `unpin` はピンと、そのピンだけが使っていた doc を消し、`Docs` を作り直し、どの doc からも指されないメディアをすぐ消す（`collect_garbage`）。
  - ディスクの書き込みエラーは `Failed(<理由>)`。
  - 状態が変わるたびに `ctx.request_repaint()`。
  - `SyncPhase::Syncing { done, total }` は取得するメディアの件数。
  - `PinInfo::bytes` はそのピンの doc が指すメディアのうち、保存済みのファイルの大きさの合計（共有するものはどのピンにも数える）。`estimate` はそのピンの doc の `MediaKey::estimate` の合計で、応答をまだ取れていなければ `None`。

- [ ] **Step 1: 失敗するテストを書く**（tests/offline_sync.rs。カタログは `tests/common/offline.rs` に置く `FakeCatalog`（Task 4・5 も使う）：kind `"set"` のピンに `rpc.call("set.get", {"id"})` をし、返った `{"items":[{"id","file_hash"}]}` の各アイテムに `MediaKey{preset:"thumb", estimate:3}` を出す。スタブは `set.get` と `GET /media/<id>/thumb` に答える）
  - `pinning_saves_docs_and_media`：ピンすると、そのうち `status == Saved`、`progress().phase == Idle`、両方のアイテムの `media()` が `Some`。
  - `media_that_cannot_be_rendered_is_counted`：1つが 422 なら `unsavable == 1`、もう1つは保存される。
  - `a_pin_the_server_no_longer_has_is_gone`：同期し直したとき `set.get` が NOT_FOUND の RPC エラー（`ClientError::NotFound` になる形）なら `Gone`、写しとメディアは残る。
  - `a_sync_cut_off_midway_deletes_nothing`：（Review Focus 1）2つ目のピンの `set.get` で接続を切る（`StubResponse::hang_up`）。前の同期で保存したメディアがどれも残っていて、phase は `Failed`。つながるようにして `sync_now` すると両方 `Saved`。
  - `unpinning_keeps_media_another_pin_uses`：（Review Focus 2）同じアイテムを含む2つのピン。片方を外すと、共有のメディアは残り、外した方だけのメディアは消える。
  - `a_lower_limit_stops_downloads_but_keeps_files`：（Review Focus 4）1つ保存した後に `set_limit_bytes(1)` して2つ目をピンすると phase が `Full`、1つ目のファイルは残る。
  - `pin_info_reports_the_estimate`：ピンして応答が取れた後の `pin_info(..).estimate == Some(6)`（3 × 2 件）、取る前は `None`。
  - `the_limit_is_kept_across_handles`：`set_limit_bytes(2 << 30)` の後、同じ `settings` で作った別の handle の `limit_bytes()` が同じ。
  - `offline_pins_wait_for_the_server`：`set_online(None)` のまま `pin` するとスタブに要求が来ず `Pending`。`set_online(Some(..))` で保存される。
  - `an_unchanged_mark_does_not_sync_again`：同じ `LibraryInfo` で2回 `set_online` しても `set.get` は1回。`revision` を変えると2回目が来る。
- [ ] **Step 2: 走らせて失敗を確かめる**
  Run: `cargo test -p asteria-common-client --test offline_sync`
  Expected: FAIL（型が無い）
- [ ] **Step 3: 実装する**
  同期は1本の制御スレッド（`asteria-offline-sync`）がコマンドのチャンネルを待ち、メディアの取得は同期ごとに `DOWNLOADS_AT_ONCE` 本の作業スレッドで分ける。止める印（`AtomicBool`）を各取得の前に見る。503 は `Retry { after }` の間眠って（止める印で起きられる待ち方：`Condvar::wait_timeout`）最大 `RETRY_ATTEMPTS` 回。`offline-audio` は `RpcClient::with_timeout(.., AUDIO_TIMEOUT)` の別のクライアントで取る。
- [ ] **Step 4: 走らせて通るのを確かめる**
  Run: `cargo test -p asteria-common-client --test offline_sync`
  Expected: PASS
- [ ] **Step 5: コミット**
  `git commit -m "feat(client): offline catalogs and the sync thread"`

### Task 4: セッションのオフライン（状態、写しでの応答、再接続、画像）

**Files:**
- Modify: `crates/asteria-common-client/src/session.rs`、`rpc.rs`（`ClientError` に2つ）、`errors.rs`、`images.rs`、`i18n/{en,ja}/client.ftl`
- Test: `crates/asteria-common-client/tests/session_offline.rs`、`tests/images.rs` に追記

**Interfaces:**
- Consumes: Task 2・3 のすべて。
- Produces:
  - `ClientError::NotSaved`（「not saved for offline use」）と `ClientError::Offline`（「cannot edit while offline」）。`error_text` の ID は `asteria-common-client-servers-error-not-saved` と `…-error-offline`。
  - `SessionState::Offline { library: LibraryInfo }`。
  - `Session::open_with(profile: &ServerProfile, client: ClientInfo, ctx: egui::Context, dirs: SessionDirs, catalog: Option<Arc<dyn OfflineCatalog>>) -> Session`。`open_with_dirs` は `open_with(.., None)` のまま残す。
  - `Session::offline(&self) -> Option<&OfflineHandle>`（`dirs.data` と catalog があり、ライブラリ ID が分かったら。オンラインでもオフラインでも同じもの）。
  - `Session::is_offline(&self) -> bool`：`Offline`、または `Ready` で `degraded()` が `Unreachable`。
  - `Session::reconnect(&mut self)`：`Offline` のとき、すぐハンドシェイクをやり直す。
  - `images::Preset::OfflineImage`（`as_str() == "offline-image"`、`texture_budget() == 8`）。
- 決まり：
  - `Gate::Offline { library_id }` を足す。ハンドシェイクが `Unreachable` で失敗し、`dirs.data` と catalog があり、`KnownLibraries::get(profile)` があれば `Gate::Offline` と `Event::Offline(library)`。それ以外は今までどおり `Closed`。
  - ハンドシェイクが成功するたびに `KnownLibraries::record(profile, &library)`。
  - `OfflineHandle` は最初にライブラリ ID が分かったとき（成功でもオフラインでも）に一度だけ作る（`OnceLock`）。URL とトークンはプロファイルのもの、`settings` は `<data>/offline/settings.toml`。
  - `Offline` の間、`poll` は前回のハンドシェイクから `RECONNECT_EVERY` たったら、やり直しを worker に出す（同時に1つまで）。成功したら `Ready`・`Gate::Open` にし、まだなら `MediaCache` と uploads を始め（今のハンドシェイクと同じ処理を関数にまとめて使い回す）、`set_online(Some(&library))`。届かなければ `Offline` のまま。それ以外の失敗（`Unauthorized`、`LibraryChanged` など）は `Failed`。
  - `Ready` に入ったときと `library.info` が来たとき `set_online(Some(..))`。`degraded` が `Unreachable` になったら `set_online(None)`。セッションを捨てるとき `stop()`。
  - 要求への答え：`Gate::Offline` なら `catalog.answer(&request, &docs)`。`None` なら、`Request::MediaUpdate` と `Request::LibraryRescan` は `ClientError::Offline`、それ以外は `ClientError::NotSaved`。`Request::LibraryInfo` は記録した `LibraryInfo` を返す。`Gate::Open` で `run` が `Unreachable` を返したら、`catalog.answer` が `Some` ならそれを返す（health は `Unreachable` のまま記録する）。
  - `ImageLoader::load` は、まず `OfflineHandle` の写し（`file_hash` があれば `media(hash, preset)`、無ければ `media_of_item(id, preset)`）、次に一時キャッシュ、最後にサーバー。`Gate::Offline` ならサーバーには行かず `ClientError::NotSaved`（失敗の後のやり直しの待ち時間は今の仕組みのまま）。`check_gate` は `Offline` を通す。
  - `ServerSwitcher`：`Offline` は `icons::cloud_off()`、`Swatch::Orange`、`asteria-common-client-status-offline`（「Offline (showing saved items)」/「オフライン（保存したものを表示中）」）。メニューに「再接続」（`SwitcherAction::Reconnect`）を足し、アプリは `session.reconnect()` を呼ぶ。

- [ ] **Step 1: 失敗するテストを書く**（tests/session_offline.rs。スタブは `tests/common` の `serve`、カタログは `tests/common/offline.rs` の `FakeCatalog`）
  - `an_unreachable_server_with_a_known_library_is_offline`：一度スタブにつないで（`Ready`）閉じる。同じプロファイルの URL を閉じたポートにして開くと `Offline { library }` で、`library.library_id` が前と同じ。
  - `an_unknown_library_still_fails`：記録が無いと `Failed(Unreachable)`。
  - `unauthorized_is_not_offline`：記録があっても 401 なら `Failed(Unauthorized)`。
  - `offline_answers_come_from_the_copies`：保存したピンの要求は `Ok`、保存していないものは `Err(NotSaved)`、`MediaUpdate` は `Err(Offline)`。
  - `the_session_comes_back_when_the_server_does`：`Offline` の後にスタブを同じポートで立て、`reconnect()` で `Ready` になる。
  - `reconnecting_to_another_library_fails`：（Review Focus 3）戻ってきたスタブが別の `library_id` なら `Failed(LibraryChanged)`。
  - `a_server_gone_while_ready_is_answered_from_the_copies`：`Ready` の後スタブが接続を切るようにすると、保存した要求は `Ok`、`degraded()` は `Unreachable`、`is_offline()` は true。
  - `dropping_the_session_stops_the_sync`：（Review Focus 5）大きなピン（スタブがメディアを 1 秒ずつ遅らせて返す）の同期中にセッションを捨てると、2 秒以内にスタブへの要求が止まる。
  - tests/images.rs に `an_offline_copy_is_drawn_without_the_server`：写しにある thumb は `Offline` でも `ImageState::Ready`、無いものは失敗。`a_saved_copy_is_used_before_the_network`：`Ready` でも写しにあるものはスタブに要求しない。
- [ ] **Step 2: 走らせて失敗を確かめる**
  Run: `cargo test -p asteria-common-client --test session_offline --test images`
  Expected: FAIL
- [ ] **Step 3: 実装する**（上の決まりどおり。翻訳の en と ja を足す）
- [ ] **Step 4: 走らせて通るのを確かめる**
  Run: `cargo test -p asteria-common-client`
  Expected: PASS（既存の session・images・upload・i18n のテストも）
- [ ] **Step 5: コミット**
  `git commit -m "feat(client): sessions go offline and answer from the saved copies"`

### Task 5: 共通の画面（保存のトグルと「オフライン」ページ）

**Files:**
- Create: `crates/asteria-common-client/src/widgets/offline_toggle.rs`、`crates/asteria-common-client/src/offline/page.rs`
- Modify: `widgets/mod.rs`、`offline/mod.rs`、`i18n/{en,ja}/client.ftl`
- Test: `crates/asteria-common-client/tests/offline_page.rs`（egui の `Context::run` で描いて、ラベルとクリックを確かめる。`tests/servers_page.rs` と同じ作り）

**Interfaces:**
- Consumes: Task 3 の `OfflineHandle`、`PinTarget`、`PinStatus`、`SyncProgress`。
- Produces:
  - `pub fn offline_toggle(ui: &mut egui::Ui, handle: Option<&OfflineHandle>, target: &PinTarget) -> egui::Response`：未保存なら「Save for offline」（`asteria-common-client-offline-save`）。保存済みなら状態（`-saved`・`-syncing`・`-full`・`-gone`）と、`pin_info` の `estimate` があれば「約 <n> MB」（`-estimate`、spec の「保存するときに目安の大きさを出す」。応答を取るまでは出ない）。押すと外す。`handle` が `None`（データディレクトリが無い、まだつないでいない）なら無効のボタン。
  - `pub fn saved_mark(ui: &mut egui::Ui)`：一覧に出す小さな印（`icons::download()` が無ければ `icons::check()`）。
  - `pub struct OfflinePage { … }`、`OfflinePage::new() -> OfflinePage`、`impl OfflinePage { pub fn ui(&mut self, ui: &mut egui::Ui, handle: Option<&OfflineHandle>) }`：使っている容量／上限、上限の選択（`LIMIT_CHOICES_GIB`）、保存したもの（名前、大きさ、状態、「外す」）、「Sync now」「Delete all」（確認のダイアログを1段はさむ）、`unsavable` の件数、`Failed` の理由。`handle` が `None` なら「サーバーにつなぐと使えます」だけ。
  - ステータスバー用 `pub fn offline_status(ui: &mut egui::Ui, handle: Option<&OfflineHandle>)`：`Syncing { done, total }` のとき「Saving for offline n/m」、`Full` と `Failed` のときその旨。ほかは何も出さない。
- アプリの設定のページの並びには、各アプリの Task 7・9 で足す。

- [ ] **Step 1: 失敗するテストを書く**
  - `the_toggle_shows_the_estimate_once_known`：同期の後、トグルのラベルに「MB」が入る。
  - `the_toggle_pins_and_unpins`：テスト用の `OfflineHandle`（`tests/common/offline.rs` のスタブと `FakeCatalog`）で、トグルを押すと `status == Some(Pending|Saved)`、もう一度押すと `None`。
  - `the_page_lists_pins_and_removes_one`：2つのピンが名前で出て、「Remove」で1つになる。
  - `the_limit_choice_is_saved`：16 GiB を選ぶと `limit_bytes() == 16 << 30`。
  - `delete_all_asks_first`：1回目の押下では消えず、確認で消える。
- [ ] **Step 2: 走らせて失敗を確かめる**
  Run: `cargo test -p asteria-common-client --test offline_page`
  Expected: FAIL
- [ ] **Step 3: 実装する**
- [ ] **Step 4: 走らせて通るのを確かめる**
  Run: `cargo test -p asteria-common-client`
  Expected: PASS（i18n のキーの一致のテストも）
- [ ] **Step 5: コミット**
  `git commit -m "feat(client): the save-for-offline toggle and the Offline settings page"`

### Task 6: music のカタログ（`MusicCatalog`）

**Files:**
- Create: `music/crates/asteria-music-gui/src/offline.rs`
- Modify: `music/crates/asteria-music-gui/src/lib.rs`
- Test: `offline.rs` の `#[cfg(test)] mod tests`（`Fetch` は記録した JSON を返す偽物）

**Interfaces:**
- Consumes: Task 3 の `OfflineCatalog`、`Fetch`、`Doc`、`Docs`、`MediaKey`、`PinTarget`。`asteria_music_api` の型と `ALBUM_GET` など。
- Produces: `pub struct MusicCatalog;`（`impl OfflineCatalog`）、ピンの種類の定数 `PIN_ALBUM = "album"`、`PIN_ARTIST = "artist"`、`PIN_PLAYLIST = "playlist"`。
- 写しの種類と中身：
  - `"album"`（id = アルバム ID、value = `Album`）
  - `"album-part"`（id = `"<アルバム ID>\n<アーティスト ID>"`、value = `tracks` を `tags.artists` にそのアーティストの名前を含む曲だけにした `Album`）
  - `"artist"`（value = `Artist`）、`"playlist"`（value = `Playlist`）
- `fetch`：album → `album.get`。artist → `artist.get`、`albums` の各 `album.get`（`"album"`）、`appears_on` の各 `album.get`（`"album-part"`）。playlist → `playlist.get`（`truncated` でもそのまま）。
- `media_of`：曲ごとに `offline-audio`（目安 `duration_ms * 12`、無ければ 4 分ぶん）と `thumb`（20_000）。アルバムの `cover_item` が曲のどれかなら、その曲の `file_hash` で `thumb`（同じキーなので重複しない）。`file_hash` が `None` の曲は出さない。playlist は `track` が `Some` のエントリだけ。
- `answer`（`Request::Call` の `method` と `params` を見る。ほかの `Request` は `None`）：
  - `album.get {id}`：`"album"` があればそれ、無ければ `summary.id == id` の `"album-part"` のうち最初のもの。
  - `artist.get`・`playlist.get`：その写し。
  - `album.list`：`"album"` と `"album-part"` の `summary` を ID で重ねず、`title` の小文字で並べた1ページ（`next_cursor: None`）。`cursor` があれば空のページ。
  - `artist.list`・`playlist.list`：写しの `summary` を `name` の小文字で並べた1ページ。
  - どれも、写しが無ければ `None`。

- [ ] **Step 1: 失敗するテストを書く**
  - `an_artist_pin_fetches_its_albums_and_the_tracks_it_plays_on`：本人のアルバム1つと参加アルバム1つ（3曲中1曲だけ参加）を返す偽物で、`fetch` の結果が `artist` 1・`album` 1・`album-part` 1、`album-part` の曲が1曲。
  - `media_of_an_album_is_audio_and_thumbs`：2曲・`cover_item` が1曲目のアルバムで、`offline-audio` 2・`thumb` 2（重複なし）、目安が `duration_ms * 12`。
  - `offline_lists_are_built_from_the_copies`：アルバム2つ（「b」「A」）の写しで `album.list` の答えが `["A","b"]` の順、`next_cursor` は `None`、`cursor` 付きは空。
  - `album_get_prefers_the_whole_album`：同じ ID の `album` と `album-part` があれば `album` を返す。`album-part` だけならそれを返す。
  - `unsaved_requests_are_not_answered`：保存していない `album.get` は `None`。
- [ ] **Step 2: 走らせて失敗を確かめる**
  Run: `cargo test -p asteria-music-gui --lib offline`
  Expected: FAIL
- [ ] **Step 3: 実装する**
- [ ] **Step 4: 走らせて通るのを確かめる**
  Run: `cargo test -p asteria-music-gui --lib offline`
  Expected: PASS
- [ ] **Step 5: コミット**
  `git commit -m "feat(music): the offline catalog of albums, artists and playlists"`

### Task 7: music の画面と再生

**Files:**
- Modify: `music/crates/asteria-music-gui/src/app.rs`（`AppPaths::data`、`Session::open_with`、エンドポイントをオフラインでも渡す、設定のページ、ステータスバー、再接続）、`connection.rs`、`library.rs`（トグル、右クリック、印、薄い曲）、`tracks.rs`、`player/mod.rs`（`Endpoint::offline`、`OpenerSettings::set_online`）、`player/feed.rs`（ローカルの音源）、`i18n/{en,ja}/music.ftl`
- Modify: `music/desktop/src/main.rs`（`paths` と `temp_paths` に `data`）、`music/crates/asteria-music-gui/tests/support/mod.rs`・`tests/settings.rs`（`AppPaths` の新しい項目）
- Test: `player/feed.rs` の tests、`music/crates/asteria-music-gui/tests/offline.rs`

**Interfaces:**
- Consumes: Task 4 の `Session::open_with`、`offline()`、`is_offline()`、`reconnect()`、`SessionState::Offline`、`SwitcherAction::Reconnect`。Task 5 の部品。Task 6 の `MusicCatalog`。
- Produces:
  - `AppPaths { …, pub data: Option<PathBuf> }`（desktop は `dirs.data()`）。
  - `Endpoint { …, pub offline: Option<OfflineHandle> }`、`OpenerSettings::set_online(&self, online: bool)`、`OpenerSettings::online(&self) -> bool`（既定 true）。
- 決まり：
  - エンドポイントは `Ready` でも `Offline` でも渡す（ライブラリ ID は `SessionState` の `library` から）。`set_online(!session.is_offline())` を毎フレームの見回りで更新する。
  - `HttpOpener::open`・`preload`：`local = endpoint.offline.and_then(|o| o.media(hash, "offline-audio"))`。`(!online || quality == Low)` で `local` があれば、`spawn_feed(Box::new(File::open(local)?), None, Some("ogg"), at_ms, target)`（`Slot` に持たず、プリロードもしない）。`!online` で `local` が無ければ `FailedFeed(TrackError::Unplayable("not saved for offline"))`。それ以外は今までどおりサーバーから。
  - 画面：アルバム・アーティスト・プレイリストの見出しに `offline_toggle`（`PinTarget { kind: PIN_*, id, name: タイトルか名前 }`）、グリッドのタイルの右クリックに同じ操作（「Save for offline」/「Remove from offline」）、保存済みのタイルに `saved_mark`。
  - `is_offline()` のとき、写しに `offline-audio` が無い曲は薄く出し、クリックしても何もしない。`play_from` などでキューに入れるときもその曲を飛ばす。
  - 設定のページの並びに「Offline」（`OfflinePage`）を足す。ステータスバーに `offline_status`。

- [ ] **Step 1: 失敗するテストを書く**
  - feed.rs の tests：`offline_plays_the_saved_file`（`online = false`、写しに Ogg Opus のテスト用ファイルを置くと、テストサーバーに要求が来ずに PCM が出て `End`）、`low_quality_prefers_the_saved_file`（`online = true`、`Quality::Low`、写しがあれば要求しない）、`original_streams_even_when_saved`（`Quality::Original`、オンラインならサーバーの `original` を取る）、`offline_without_a_copy_is_unplayable`。テスト用の Ogg Opus は `music/crates/asteria-music-gui/tests/fixtures/tone.opus` を新しく足す（`generate.sh` に `ffmpeg -i tone.flac -c:a libopus -b:a 96k tone.opus` を足して作り、ファイルもコミットする）。
  - tests/offline.rs：`a_saved_album_opens_with_the_server_gone`（`support` のテストサーバーでアルバムを保存し、サーバーを止めて開き直すと、アルバムの一覧にそのアルバムがあり、開くと曲が出る）、`unsaved_tracks_are_skipped_when_queued_offline`。
- [ ] **Step 2: 走らせて失敗を確かめる**
  Run: `cargo test -p asteria-music-gui`
  Expected: FAIL
- [ ] **Step 3: 実装する**
- [ ] **Step 4: 走らせて通るのを確かめる**
  Run: `cargo test -p asteria-music-gui && cargo test -p asteria-music-desktop`
  Expected: PASS
- [ ] **Step 5: コミット**
  `git commit -m "feat(music): save albums, artists and playlists for offline, and play them without the server"`

### Task 8: photos のカタログ（`PhotosCatalog`）

**Files:**
- Create: `photos/crates/asteria-photos-gui/src/offline.rs`
- Modify: `photos/crates/asteria-photos-gui/src/lib.rs`
- Test: `offline.rs` の `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: Task 3 の型、`asteria_common_api::{FOLDER_LIST, FolderListParams, FolderListing, MEDIA_GET, MediaGetParams, MediaItem}`。
- Produces: `pub struct PhotosCatalog;`（`impl OfflineCatalog`）、`PIN_FOLDER = "folder"`。
- 写し：`"folder"`（id = ライブラリ相対のフォルダのパス、value = そのフォルダの全ページをつないだ `FolderListing`、`next_cursor: None`）。
- `fetch`：ピンのフォルダから幅優先で、各フォルダの `folder.list` を `limit: 200` で `next_cursor` が尽きるまで取り、下のフォルダ（`folders` の名前を `/` でつなぐ。ルートは `""`）も同じく取る。
- `media_of`：各アイテムの `thumb`（20_000）と `offline-image`（130_000）。`file_hash` が `None` のものは出さない。
- `answer`：
  - `Request::FolderList(p)`：`"folder"` の写しが `p.path` にあれば、`p.cursor` が `None` ならそれを、`Some` なら空のページ。写しが無く、`p.path` が写しのどれかのフォルダの上（ルート `""` はすべての上）なら、`folders` がそこから一段下の名前（重ねず、並べる）で `items` が空のページ。どちらでもなければ `None`。
  - `Request::MediaGet(p)`：写しのどれかの `items` にその ID があれば `Response::Media`。
  - ほかは `None`。

- [ ] **Step 1: 失敗するテストを書く**
  - `a_folder_pin_fetches_every_page_and_subfolder`：`2026` に2ページ、その下の `2026/trip` に1ページを返す偽物で、写しが2つ、`2026` の items が両ページぶん。
  - `an_ancestor_lists_only_the_way_down`：`2026/trip` と `2026/home` と `2025/x` の写しで、`""` の答えの `folders` が `["2025","2026"]`、`2026` に写しが無ければ `["home","trip"]`、items は空。
  - `a_saved_folder_answers_its_pages`：`cursor: None` は全部、`Some` は空。
  - `media_get_finds_a_saved_item`、`media_of_is_thumb_and_offline_image`。
- [ ] **Step 2: 走らせて失敗を確かめる**
  Run: `cargo test -p asteria-photos-gui --lib offline`
  Expected: FAIL
- [ ] **Step 3: 実装する**
- [ ] **Step 4: 走らせて通るのを確かめる**
  Run: `cargo test -p asteria-photos-gui --lib offline`
  Expected: PASS
- [ ] **Step 5: コミット**
  `git commit -m "feat(photos): the offline catalog of folders"`

### Task 9: photos の画面と「低画質で表示する」

**Files:**
- Create: `photos/crates/asteria-photos-gui/src/display.rs`（`display.toml` とその設定のページ。music の `settings.rs` と同じ作り）
- Modify: `photos/crates/asteria-photos-gui/src/app.rs`（`AppPaths::display`、`Session::open_with`、設定のページ、ステータスバー、再接続）、`browse.rs`（見出しのトグル、印、薄いアイテム）、`viewer.rs`・`detail.rs`（プリセットの選び方）、`i18n/{en,ja}/photos.ftl`
- Modify: `photos/desktop/src/main.rs`（`paths` と `temp_paths` に `display`）、`photos/crates/asteria-photos-gui/tests/{switch,settings,uploads}.rs` と `tests/support/mod.rs`（`AppPaths` の新しい項目）
- Test: `display.rs` の tests、`photos/crates/asteria-photos-gui/tests/offline.rs`

**Interfaces:**
- Consumes: Task 4・5・8。
- Produces:
  - `pub struct DisplaySettings { pub low_quality: bool }`（`#[serde(default)]`、`load(path)`・`save(path)` は music の `PlaybackSettings` と同じ決まり）、`pub struct DisplayPage`。
  - `AppPaths { …, pub display: PathBuf }`（desktop は `dirs.config().join("display.toml")`）。
  - `pub(crate) fn display_preset(session: &Session, settings: &DisplaySettings) -> Preset`：`settings.low_quality || session.is_offline()` なら `Preset::OfflineImage`、ほかは `Preset::Preview`。
- 決まり：
  - ビューアーと詳細ペインは `Preset::Preview` の代わりに `display_preset` を使う（隣のプリロードも同じ）。
  - 閲覧中のフォルダの見出しに `offline_toggle`（`PinTarget { kind: PIN_FOLDER, id: path, name: フォルダ名（ルートはライブラリの表示名） }`）。保存したフォルダ（とその下）を開いているときは見出しに `saved_mark`。
  - `is_offline()` のとき、写しに `offline-image` が無いアイテムは薄く出し、ビューアーで開かない。
  - 設定に「Display」（`DisplayPage`：「Show in low quality」/「低画質で表示する」）と「Offline」（`OfflinePage`）を足す。ステータスバーに `offline_status`。

- [ ] **Step 1: 失敗するテストを書く**
  - display.rs：`a_missing_file_gives_the_defaults`、`settings_round_trip`。
  - tests/offline.rs：`a_saved_folder_opens_with_the_server_gone`（`support` のテストサーバーで `Inbox` を保存し、サーバーを止めて開き直すと、ルートから `Inbox` にたどれて、アイテムのサムネイルが出る）、`low_quality_shows_offline_image`（設定をオンにすると、ビューアーが `offline-image` を頼む。テストサーバーへの要求のパスで確かめる）、`offline_shows_offline_image_even_with_the_setting_off`。
- [ ] **Step 2: 走らせて失敗を確かめる**
  Run: `cargo test -p asteria-photos-gui`
  Expected: FAIL
- [ ] **Step 3: 実装する**
- [ ] **Step 4: 走らせて通るのを確かめる**
  Run: `cargo test -p asteria-photos-gui && cargo test -p asteria-photos-desktop`
  Expected: PASS
- [ ] **Step 5: コミット**
  `git commit -m "feat(photos): save folders for offline, and show in low quality"`

### Task 10: 仕上げ

**Files:**
- Modify: `README.md`・`README.ja.md`（機能の一覧に「オフラインに保存」を1行）。`CHANGELOG.md` は release-plz が作るので触らない。

- [ ] **Step 1: ワークスペース全体を確かめる**
  Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings && cargo test --workspace && cargo deny check`
  Expected: すべて成功（負荷で落ちることがある既知のテスト #43 は単独で走らせ直して確かめる）
- [ ] **Step 2: iOS 向けのチェック**（macOS が無ければ CI に任せる）
  Run: `cargo check -p asteria-common-client --target aarch64-apple-ios`
  Expected: 成功
- [ ] **Step 3: README を直してコミット**
  `git commit -m "docs(readme): saving for offline"`
- [ ] **Step 4: PR のチェックリストに手動の確認を書く**：music でアルバムを保存 → サーバーを止めて起動し直し → 再生できる、photos でフォルダを保存 → 同じく閲覧できる、「低画質で表示する」で見た目が粗くなる、容量の上限で止まる。
