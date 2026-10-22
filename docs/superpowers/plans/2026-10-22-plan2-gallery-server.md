# Plan 2: framework-server と gallery-server の最小構成 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `asteria-gallery-server` を起動すると、Pictures ライブラリをスキャンして redb にキャッシュし、HTTP 上の JSON-RPC で閲覧と評価・タグの編集ができ、原本（Range 対応）とサムネイルを配信できる状態にする。

**Architecture:** サーバーの共通部分は `asteria-framework-server`（ライブラリ crate）に置き、`asteria-gallery-server` は「何をメディアとして扱うか」と CLI だけを持つ薄いバイナリにする。原本への書き込みはすべて1本の書き込みキューを通す。DB（redb）は消せば作り直せるキャッシュで、マイグレーションは書かない。サムネイル生成は vips / ffmpeg を子プロセスで呼ぶ。

**Tech Stack:** Rust 1.99.0、tokio、axum、tower-http（ServeFile）、redb、notify、walkdir、clap、tracing-subscriber、dirs。dev では reqwest。外部コマンドとして vips（`vipsthumbnail`）と ffmpeg を使う。

**Spec:** `docs/superpowers/specs/2026-10-22-asteria-design.md`（特に §2、§3.1、§3.3、§4、§6）

## Plan 1 から引き継ぐ前提

- 依存してよい framework の公開 API：`Library::{open, init, root, owner, users, read_sidecar, update_sidecar, set_rating}`、`Sidecar`（`rating`、`user_ratings`、`subjects`、`hierarchical_subjects`、`description`、`label`、`content_hash`）、`hash::{content_hash, detect_format_file, ContentHash, HashScope, MediaFormat}`、`paths::{sidecar_path_for, is_sidecar, media_path_for_sidecar, META_DIR}`、`fsutil::{atomic_write, cleanup_tmp_files}`、`journal::{Journal, JournalEntry, Actor, OpKind, OpId}`、`Error`。
- `update_sidecar` は、`content_hash` が空ならその場でハッシュを計算する。サーバーでは、キャッシュにあるハッシュをクロージャの中で先に設定し、書き込みのたびに大きなファイルを読むのを避ける。
- Plan 1 で先送りした項目のうち、この計画で扱うもの：起動時の一時ファイル掃除、`cleanup_tmp_files` が読めないディレクトリで止まらないこと、ライブラリ外を指すシンボリックリンクの検査、MP3 判定での2つ目のフレームの確認。

## Global Constraints

- ツールチェインは `1.99.0`、edition 2024。CI は clippy `-D warnings` と `cargo fmt --check` を通す。
- 新しい crate `asteria-framework-server`（lib）と `asteria-gallery-server`（bin）はどちらも `GPL-3.0-or-later`。`deny.toml` の GPL 例外に2つとも追加する。
- `*-api` crate が依存してよいのは `serde` と `grain-id` だけ。
- DB はキャッシュ。スキーマのバージョンが合わない、または開けない場合は削除して作り直す。マイグレーションは書かない。
- 原本（メディア、サイドカー、ジャーナル、ライブラリ設定）への書き込みは、すべてライブラリごとに1本の書き込みキューを通す。
- 認証トークンはライブラリの中に置かない。サーバーの設定ファイルには、トークンの BLAKE3 ハッシュだけを書く（`blake3:<hex>`）。トークンそのものはログにも設定にも残さない。
- キャッシュの場所は `<dirs::cache_dir()>/asteria-gallery/<ライブラリID>/`。中身は `cache.redb` と `thumbs/<プリセット>/<hex 先頭2文字>/<hex>.webp`。
- 既定の待ち受けアドレスは `127.0.0.1:7810`（player は計画5で 7820 を使う）。
- アイテム ID は UUID v5。名前空間はライブラリ ID（`library.toml` の UUID）、名前はライブラリ相対パス（UTF-8、区切り "/"）。表記はハイフン付き小文字の36文字。パスから決まるのでキャッシュを作り直しても変わらず、ライブラリが違えば同じパスでも別の ID になる。どの言語でも標準的な UUID ライブラリで同じ値を計算できる。
- API に出すパスは、ライブラリ相対で区切りは `/`。UTF-8 にできない名前のファイルはスキャン対象外にし、警告を出す。
- gallery が扱うアイテムは画像と動画だけ。サイドカー（`.xmp`）、`.asteria/` の中、`.` で始まるファイルとディレクトリは対象外。
- 外部コマンド（vips、ffmpeg）を使うテストは、コマンドが見つからないときは理由を表示してスキップする。CI の Linux ジョブでは `linux-packages` でこれらを入れ、実際に実行する。

## Review Focus

1. **大きなライブラリ**：1万件規模でも、起動直後からリクエストに答えられること。スキャン中は `library.info` が `scanning: true` を返し、`folder.list` はその時点までの結果を返す。→ Task 5 と Task 11 でテストする
2. **壊れた XMP があるアイテム**：一覧には出し、`read_only_reason` を付ける。`media.update` は専用のエラーコードで拒否し、ファイルを上書きしない。→ Task 5 と Task 7 でテストする
3. **外部で消されたり動かされたりしたファイル**：キャッシュが古いままでも、原本の取得は 404、`media.update` は NotFound になる。500 や panic にしない。→ Task 7 と Task 9 でテストする
4. **壊れた画像や動画**：サムネイル生成の失敗やタイムアウトは 422 を返し、サーバーは落ちない。ファイルが変わるまで再試行しない。→ Task 9 でテストする
5. **認証のないアクセス**：トークンがない、または違う場合は、バイナリ配信も含めてすべてのエンドポイントで 401。ログにトークンを出さない。→ Task 8 と Task 9 でテストする

---

### Task 1: メディアの種類の判定と、Plan 1 の残り

**Files:**
- Create: `asteria-framework/src/kind.rs`
- Modify: `asteria-framework-api/src/lib.rs`（`MediaKind` の定義）、`asteria-framework/Cargo.toml`（framework-api への依存がなければ追加）、`asteria-framework/src/lib.rs`、`asteria-framework/src/hash/detect.rs`、`asteria-framework/src/fsutil.rs`
- Test: `asteria-framework/tests/kind.rs`、`asteria-framework/tests/hash_core.rs`、`asteria-framework/tests/fsutil.rs`

**Interfaces:**
- Produces:
  - `pub enum MediaKind { Image, Video, Audio, Other }`（serde は snake_case。Debug, Clone, Copy, PartialEq, Eq, Hash）。**`asteria-framework-api` に定義し**、Task 2 の DTO でも使う。framework は `pub use asteria_framework_api::MediaKind;` で re-export する
  - `pub fn classify(path: &Path) -> Result<MediaKind>`：拡張子（大文字小文字は区別しない）で判定する。ただし `detect_format_file` が JPEG/PNG/WebP を返せば Image、FLAC/MP3 を返せば Audio とし、拡張子より優先する
    - Image：`jpg jpeg png webp gif heic heif avif tif tiff dng cr2 cr3 nef arw orf rw2 raf`
    - Video：`mp4 mov m4v mkv webm avi 3gp`
    - Audio：`flac mp3 m4a aac ogg oga opus wav`
    - それ以外は Other
  - `cleanup_tmp_files` は読めないサブディレクトリを飛ばして続け、`tracing::warn!` を出す。ルート自体が読めない場合だけエラーにする
  - MP3 判定：先頭のフレームヘッダから求めたフレーム長の位置に、もう1つ有効なフレームヘッダがあることも要求する。ファイルがそこまでない場合は1つ目だけで判定する

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn classify_by_extension_case_insensitive() { /* "a.HEIC" → Image, "b.MOV" → Video, "c.opus" → Audio, "d.txt" → Other */ }
#[test] fn magic_overrides_extension() { /* 中身が PNG の "x.mov" → Image */ }
#[test] fn utf16_text_is_not_mp3() { /* FF FE + "hello world" を UTF-16LE で書いたファイル → detect は Unknown */ }
#[test] fn real_mp3_fixture_still_detects() { /* sine440.mp3 → Mp3、ゴールデンの pin は変わらない */ }
#[test] fn cleanup_skips_unreadable_subdir() { /* unix: chmod 000 のサブディレクトリと、別の場所の .x.asteria-tmp → Ok(1) */ }
```

- [ ] **Step 2: 実行して失敗を確認する**
  Run: `cargo test -p asteria-framework --test kind --test hash_core --test fsutil`
  Expected: コンパイルエラー、または新しいテストが FAIL

- [ ] **Step 3: 実装する**
  MP3 のフレーム長は、MPEG-1/2/2.5 と Layer I/II/III のビットレート表・サンプルレート表から計算する（Layer I は `(12*bitrate/sr + pad)*4`、それ以外は `144*bitrate/sr + pad`。MPEG-2/2.5 の Layer III は 72）。

- [ ] **Step 4: 実行して成功を確認する**（Step 2 と同じコマンド。PASS）

- [ ] **Step 5: Commit** `feat(framework): media kind classification; stricter MP3 detection; resilient tmp cleanup`

---

### Task 2: API の wire 型

**Files:**
- Modify: `asteria-framework-api/src/lib.rs`（`rpc.rs` と `media.rs` に分けてもよい）、`asteria-gallery-api/src/lib.rs`、`asteria-player-api/src/lib.rs`
- Test: 各 crate の `#[cfg(test)]`

**Interfaces:**
- Produces（`asteria-framework-api`）:
  - JSON-RPC の封筒：`RpcRequest { jsonrpc: String /* "2.0" */, id: u64, method: String, params: serde_json::Value }`。ただし api crate は serde_json に依存できないので、`params` と `result` は型引数 `P` / `R` を取るジェネリクスにする（`RpcRequest<P>`、`RpcResponse<R>`）
  - `RpcResponse<R> { jsonrpc, id, #[serde(flatten)] outcome: RpcOutcome<R> }`、`enum RpcOutcome<R> { #[serde(rename="result")] Ok(R), #[serde(rename="error")] Err(RpcError) }`
  - `RpcError { code: i32, message: String }` と、エラーコードの定数 `codes::{PARSE_ERROR=-32700, INVALID_REQUEST=-32600, METHOD_NOT_FOUND=-32601, INVALID_PARAMS=-32602, INTERNAL_ERROR=-32603, NOT_FOUND=-32001, INVALID_PATH=-32002, READ_ONLY=-32003, BUSY=-32004, API_MISMATCH=-32005}`
  - メソッド名の定数：`SERVER_INFO="server.info"`、`LIBRARY_INFO="library.info"`、`LIBRARY_RESCAN="library.rescan"`、`FOLDER_LIST="folder.list"`、`MEDIA_GET="media.get"`、`MEDIA_UPDATE="media.update"`
  - `pub struct ItemId(pub String)`（serde transparent。中身は UUID v5 の文字列。api crate に uuid の依存を増やさないため String で持つ）
  - `MediaKind`（Task 1 で定義済み。そのまま使う）
  - `MediaItem { id: ItemId, path: String, kind: MediaKind, size: u64, modified: String /* RFC 3339 UTC */, rating: Option<i8> /* リクエストしたユーザー自身の評価 */, subjects: Vec<String>, hierarchical_subjects: Vec<String>, description: Option<String>, label: Option<String>, content_hash: Option<String>, read_only_reason: Option<String> }`
  - `LibraryInfo { library_id: String, owner: String, epoch: String, item_count: u64, scanning: bool }`
  - `FolderListParams { path: String /* "" はルート */, cursor: Option<String>, limit: Option<u32> }`、`FolderListing { folders: Vec<String> /* 直下のフォルダ名 */, items: Vec<MediaItem>, next_cursor: Option<String> }`
  - `MediaGetParams { id: ItemId }`
  - `MediaUpdateParams { id: ItemId, rating: Option<i8>, subjects: Option<Vec<String>>, hierarchical_subjects: Option<Vec<String>>, description: Option<Option<String>>, label: Option<Option<String>> }`：None は「変更しない」、`Some(None)` は「消す」
  - `ServerInfo` に `epoch: String` を追加する（破壊的変更になるので、framework-api のメジャーを 2.0.0 に上げる）
  - `pub const fn parse_major(s: &str) -> u32`（sapphire と同じ実装）
- Produces（gallery-api / player-api）：`APP_API` を `parse_major(env!("CARGO_PKG_VERSION_MAJOR"))` に変え、crate のバージョンを 2.0.0 にする

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn rpc_response_ok_and_err_shapes() {
    // RpcResponse { id: 1, Ok(5) } → {"jsonrpc":"2.0","id":1,"result":5}
    // Err → {"jsonrpc":"2.0","id":1,"error":{"code":-32001,"message":"x"}}。両方とも往復で一致する
}
#[test] fn media_update_distinguishes_unset_and_clear() {
    // {"id":"..","description":null} → Some(None)、キーなし → None
}
#[test] fn app_api_tracks_crate_major() { /* gallery-api: APP_API == 2 */ }
```
  ヒント：`Option<Option<T>>` を区別するには `#[serde(default, with = "...")]` か `deserialize_with` が必要。serde だけで書くこと。

- [ ] **Step 2〜4: 失敗を確認し、実装し、成功を確認する**
  Run: `cargo test -p asteria-framework-api -p asteria-gallery-api -p asteria-player-api`

- [ ] **Step 5: Commit** `feat(api): JSON-RPC envelope, gallery browse/update types; APP_API from crate major`

---

### Task 3: framework-server の土台：設定とトークン

**Files:**
- Create: `asteria-framework-server/{Cargo.toml,src/lib.rs,src/config.rs,src/auth.rs,src/error.rs}`
- Modify: ルートの `Cargo.toml`（members、workspace.dependencies）、`deny.toml`
- Test: `asteria-framework-server/tests/config.rs`

**Interfaces:**
- Produces:
  - `pub struct ServerConfig { pub library: PathBuf, pub listen: SocketAddr, pub token: Vec<TokenEntry> }`：TOML。`listen` の既定値は呼び出し側から渡す
  - `pub struct TokenEntry { pub user: UserId, pub hash: String /* "blake3:<hex>" */ }`
  - `ServerConfig::load(path: &Path) -> Result<ServerConfig>`、`ServerConfig::save(&self, path: &Path) -> Result<()>`（`atomic_write` を使い、unix ではパーミッションを 0600 にする）
  - `pub fn new_token() -> String`：32バイトの乱数を base64url（パディングなし）にしたもの。乱数は `getrandom` を使う
  - `pub fn token_hash(token: &str) -> String`：`blake3:<hex>`
  - `pub struct Tokens` / `Tokens::from_config(&ServerConfig)`、`Tokens::verify(&self, presented: &str) -> Option<UserId>`
  - `pub enum ServerError`（thiserror）：`Framework(asteria_framework::Error)`、`Config(String)`、`Cache(String)`、`Io(std::io::Error)`
  - `pub fn cache_dir_for(app: &str, library: &LibraryId) -> PathBuf`
- deny.toml の GPL 例外に `asteria-framework-server` と `asteria-gallery-server` を加える。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn config_roundtrip_and_permissions() { /* save → load で一致する。unix ではファイルのモードが 0o600 */ }
#[test] fn token_verifies_only_matching() {
    // t = new_token()。設定の hash = token_hash(&t)。verify(&t) == Some(user)。verify("x") == None
    // 保存した設定ファイルの中に t の文字列が含まれていない
}
#[test] fn tokens_are_distinct_and_long() { /* new_token を2回呼ぶと違う値。長さは 43 */ }
```

- [ ] **Step 2〜4: 失敗を確認し、実装し、成功を確認する**
  Run: `cargo test -p asteria-framework-server --test config`

- [ ] **Step 5: Commit** `feat(server): server config with hashed tokens`

---

### Task 4: redb キャッシュ

**Files:**
- Create: `asteria-framework-server/src/cache.rs`
- Test: `asteria-framework-server/tests/cache.rs`

**Interfaces:**
- Consumes: Task 2 の `ItemId`、`MediaKind`
- Produces:
  - `pub const CACHE_SCHEMA: u32 = 1;`
  - `pub struct ItemRecord { pub id: ItemId, pub path: String, pub kind: MediaKind, pub size: u64, pub mtime_ns: i128, pub hash: Option<StoredHash>, pub sidecar: SidecarSnapshot, pub sidecar_error: Option<String> }`（serde。値は serde_json のバイト列で保存する）
  - `pub struct StoredHash { pub hash: String, pub scope: String, pub size: u64, pub mtime_ns: i128 }`：(size, mtime) が今のファイルと一致するときだけ有効
  - `pub struct SidecarSnapshot { pub owner_rating: Option<i8>, pub user_ratings: BTreeMap<String, i8>, pub subjects: Vec<String>, pub hierarchical_subjects: Vec<String>, pub description: Option<String>, pub label: Option<String>, pub has_sidecar: bool }`。`From<&Sidecar>` を実装する
  - `pub fn item_id_for(library: &LibraryId, rel: &str) -> ItemId`：`Uuid::new_v5(&library.0, rel.as_bytes())` をハイフン付き小文字にしたもの（uuid crate の `v5` feature を使う）
  - `pub struct Cache`
    - `Cache::open(dir: &Path, library_id: &LibraryId) -> Result<Cache, ServerError>`：開けない、`meta.schema != CACHE_SCHEMA`、または `meta.library_id` が違う場合は、ファイルを消して作り直す。作り直すたびに新しい `epoch`（GrainId）を書く
    - `epoch(&self) -> String`
    - `put(&self, rec: &ItemRecord)`、`get_by_id(&self, id: &ItemId) -> Option<ItemRecord>`、`get_by_path(&self, rel: &str) -> Option<ItemRecord>`、`remove(&self, rel: &str)`
    - `all_paths(&self) -> Vec<String>`、`count(&self) -> u64`
    - `list_folder(&self, folder: &str, after: Option<&str>, limit: usize) -> (Vec<String> /* 直下のフォルダ */, Vec<ItemRecord>, Option<String> /* 次のカーソル */)`：アイテムはパス順に並べ、カーソルは最後に返したパス。フォルダ名は最初のページだけに含める
    - `mark_unprocessable(&self, hash: &str, preset: &str, reason: &str)`、`unprocessable(&self, hash: &str, preset: &str) -> Option<String>`
  - テーブル：`meta`（str→bytes）、`items`（path→bytes）、`ids`（id→path）、`unprocessable`（"hash|preset"→reason）

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn item_id_is_uuid_v5_of_library_and_path() {
    // 同じ (ライブラリ, パス) は同じ ID。パスが違えば別の ID。ライブラリが違えば同じパスでも別の ID
    // Uuid::parse_str で読めて、get_version() == Some(Version::Sha1)
    // 名前空間 6ba7b810-9dad-11d1-80b4-00c04fd430c8、パス "2025/京都旅行/a.jpg" の値は "b0a2f96c-dcb1-5b0a-b03e-4c229ab92971"（Python の uuid.uuid5 で計算した独立の値）と一致する
}
#[test] fn put_get_remove_roundtrip() { /* id でもパスでも引ける。remove のあとは両方 None */ }
#[test] fn list_folder_pages_and_subfolders() {
    // "a.jpg", "x/b.jpg", "x/c.jpg", "x/y/d.jpg", "z/e.jpg"
    // list_folder("") → folders ["x","z"]、items ["a.jpg"]
    // list_folder("x", None, 1) → folders ["y"]、items ["x/b.jpg"]、next=Some("x/b.jpg")
    // 続きのページ → items ["x/c.jpg"]、folders []、next None
}
#[test] fn schema_mismatch_rebuilds_with_new_epoch() { /* meta.schema を書き換えて開き直す → 空になり、epoch が変わる */ }
#[test] fn corrupt_file_rebuilds() { /* cache.redb をゴミのバイト列で上書き → open が Ok で、中身は空 */ }
#[test] fn unprocessable_roundtrip() { /* mark したものが unprocessable で返る */ }
```

- [ ] **Step 2〜4: 失敗を確認し、実装し、成功を確認する**
  Run: `cargo test -p asteria-framework-server --test cache`

- [ ] **Step 5: Commit** `feat(server): rebuildable redb cache with folder listing`

---

### Task 5: スキャナー

**Files:**
- Create: `asteria-framework-server/src/scan.rs`
- Test: `asteria-framework-server/tests/scan.rs`

**Interfaces:**
- Consumes: Task 1 の `classify`、Task 4 の `Cache`、`ItemRecord`、`SidecarSnapshot`
- Produces:
  - `pub trait MediaFilter: Send + Sync + 'static { fn accepts(&self, kind: MediaKind) -> bool; }`（gallery は Image と Video を受け付ける）
  - `pub struct ScanReport { pub added: usize, pub updated: usize, pub removed: usize, pub skipped: Vec<(String, String) /* パスと理由 */> }`
  - `pub fn scan(library: &Library, cache: &Cache, filter: &dyn MediaFilter) -> Result<ScanReport, ServerError>`（同期処理。呼び出し側が `spawn_blocking` で包む）
- 規則
  - walkdir でルートからたどり、シンボリックリンクはたどらない。
  - `.` で始まる名前（`.asteria` を含む）は、ディレクトリなら中に入らず、ファイルなら飛ばす。
  - シンボリックリンクのファイルは、`canonicalize` した結果がルートの中にあるときだけ受け付ける。
  - サイドカー（`is_sidecar`）はアイテムにしない。
  - UTF-8 にできない名前は `skipped` に入れる。
  - 既存のレコードと (size, mtime_ns) が同じなら、`hash` はそのまま残す。サイドカーだけは毎回読み直す（サイドカーは小さく、外部のツールで編集されうるため）。
  - サイドカーが壊れている場合は `sidecar_error` に理由を入れ、スナップショットは空にする。
  - ディスクにないパスのレコードは消す。
  - スキャンの最初に `cleanup_tmp_files(root)` を呼ぶ。

- [ ] **Step 1: 失敗するテストを書く**（ライブラリは tempdir に `Library::init` で作る。画像は Plan 1 のテスト用ファイルをコピーする）

```rust
#[test] fn scans_images_and_videos_only() { /* red.jpg, clip.mov（中身は適当なバイト列）, song.mp3, notes.txt → items は 2 件 */ }
#[test] fn skips_hidden_meta_and_sidecars() { /* .asteria/*、.hidden/a.jpg、a.jpg.xmp → アイテムにならない */ }
#[test] fn rescan_detects_add_update_remove() { /* 1回目のあとに追加1・mtime 変更1・削除1 → 戻り値が (1,1,1) */ }
#[test] fn malformed_sidecar_is_listed_with_error() { /* 壊れた xmp の付いた red.jpg → sidecar_error が Some で、アイテムは一覧に残る */ }
#[test] fn symlink_outside_root_is_ignored() { /* unix: ルート外の画像を指すリンク → 一覧に出ない */ }
#[test] fn non_utf8_name_is_skipped() { /* unix (linux): OsStr::from_bytes(b"\xff.jpg") → skipped に入る */ }
#[test] fn leftover_tmp_is_cleaned() { /* .a.jpg.xmp.asteria-tmp を置いてスキャン → 消えている */ }
#[test] fn large_tree_scan_is_reasonable() { /* red.jpg のコピー 5,000 件 → 30 秒以内に終わる（debug ビルドでも余裕のある上限） */ }
```

- [ ] **Step 2〜4: 失敗を確認し、実装し、成功を確認する**
  Run: `cargo test -p asteria-framework-server --test scan`

- [ ] **Step 5: Commit** `feat(server): incremental library scanner`

---

### Task 6: ハッシュの計算と、持ち主のいないサイドカーの紐付け直し

**Files:**
- Create: `asteria-framework-server/src/hashing.rs`
- Test: `asteria-framework-server/tests/hashing.rs`

**Interfaces:**
- Consumes: Task 4、Task 5
- Produces:
  - `pub fn hash_pending(library: &Library, cache: &Cache, cancel: &AtomicBool) -> Result<usize, ServerError>`：`hash` が None、または (size, mtime) が今のファイルと合わないレコードについて `content_hash` を計算し、`StoredHash` を保存する。計算した件数を返す。`cancel` が立ったら途中で止める
  - `pub fn relink_orphans(library: &Library, cache: &Cache) -> Result<RelinkReport, ServerError>`
    - `RelinkReport { relinked: Vec<(String, String)> /* 元のサイドカーと新しいメディア */, ambiguous: Vec<String>, unmatched: Vec<String> }`
    - 対象は、ライブラリ内の `*.xmp` のうち `media_path_for_sidecar` が存在しないもの。
    - サイドカーの `content_hash` と一致するキャッシュのレコードを探す。ちょうど1件で、そのメディアにまだサイドカーがない場合は、`fs::rename` でサイドカーを移し、`OpKind::RelinkSidecar` をジャーナルに記録する（actor は `Actor::User(owner)`）。そのあとキャッシュのスナップショットを更新する。
    - 2件以上なら `ambiguous`、0件なら `unmatched` に入れる。
- 注意：rename は原本への書き込みなので、本番では書き込みキュー（Task 7）から呼ぶ。この関数自体は同期処理のままにする。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn hashes_missing_and_stale() { /* 2件のうち1件に hash がある → 1件だけ計算される。mtime を変えると再計算される */ }
#[test] fn cancel_stops_early() { /* cancel=true → 0 */ }
#[test] fn relinks_unique_match() {
    // red.jpg を評価して xmp を作る。red.jpg を sub/renamed.jpg に移し、スキャンとハッシュ計算をする
    // → relink で sub/renamed.jpg.xmp ができ、評価が引き継がれる。ジャーナルに RelinkSidecar が1件ある
}
#[test] fn ambiguous_match_is_not_moved() { /* 同じ内容の画像が2枚 → ambiguous に入り、サイドカーは元の場所のまま */ }
```

- [ ] **Step 2〜4: 失敗を確認し、実装し、成功を確認する**
  Run: `cargo test -p asteria-framework-server --test hashing`

- [ ] **Step 5: Commit** `feat(server): background hashing and orphan sidecar relinking`

---

### Task 7: 書き込みキューと編集処理

**Files:**
- Create: `asteria-framework-server/src/writer.rs`
- Test: `asteria-framework-server/tests/writer.rs`

**Interfaces:**
- Consumes: Task 2 の `MediaUpdateParams`、Task 4、Task 6
- Produces:
  - `pub struct Writer`（`Clone`。中身は `mpsc::Sender<Job>`）
  - `pub fn spawn_writer(library: Arc<Library>, cache: Arc<Cache>) -> Writer`：専用のタスクを1つ起動し、ジョブを1件ずつ `spawn_blocking` で実行する
  - `Writer::update(&self, user: UserId, params: MediaUpdateParams) -> Result<ItemRecord, WriteError>`
  - `Writer::relink_orphans(&self) -> Result<RelinkReport, WriteError>`
  - `pub enum WriteError { NotFound, InvalidPath(String), ReadOnly(String), Busy(String), Invalid(String), Internal(String) }`。RPC のエラーコードへの対応は Task 8 で行う
- 規則
  - id からキャッシュのレコードを引き、なければ `NotFound`。
  - `sidecar_error` があれば `ReadOnly` を返し、ファイルには触れない。
  - 評価は `Library::set_rating(rel, user, value, Utc::now(), Actor::User(user))`。それ以外の項目は `update_sidecar` のクロージャでまとめて変更する。
  - キャッシュに有効な `StoredHash` があれば、クロージャの中で `sc.content_hash` に先に入れておく。
  - framework のエラーの対応：`MediaNotFound` は `NotFound`、`InvalidPath` は `InvalidPath`、`MalformedSidecar` は `ReadOnly`、`TmpExists` は `Busy`、`InvalidRating` は `Invalid`、それ以外は `Internal`。
  - 書き込んだら、サイドカーを読み直してスナップショットを更新し、レコードを返す。
  - タグや説明に XML で使えない制御文字が入っていたら、書き込む前に `Invalid` で拒否する（Plan 1 の deferred 項目）。

- [ ] **Step 1: 失敗するテストを書く**（tokio のテスト）

```rust
#[tokio::test] async fn update_rating_and_tags_writes_sidecar_and_cache() { /* owner の評価 4、subjects ["cat"] → xmp に反映され、レコードの snapshot も更新される */ }
#[tokio::test] async fn non_owner_rating_goes_to_user_ratings() {}
#[tokio::test] async fn read_only_item_is_refused_untouched() { /* 壊れた xmp → ReadOnly で、バイト列は変わらない */ }
#[tokio::test] async fn deleted_file_is_not_found() { /* スキャンのあとにファイルを消して update → NotFound */ }
#[tokio::test] async fn control_chars_are_invalid() { /* label "\u{1}" → Invalid */ }
#[tokio::test] async fn concurrent_updates_are_serialized() { /* 同じアイテムへの更新を 20 件同時に投げる → すべて成功し、最後の xmp が読める */ }
#[tokio::test] async fn cached_hash_avoids_rehash() { /* StoredHash を偽の値にしておく → サイドカーにはその値が入る */ }
```

- [ ] **Step 2〜4: 失敗を確認し、実装し、成功を確認する**
  Run: `cargo test -p asteria-framework-server --test writer`

- [ ] **Step 5: Commit** `feat(server): single write queue for sidecar edits`

---

### Task 8: HTTP サーバー、認証、JSON-RPC

**Files:**
- Create: `asteria-framework-server/src/{app.rs,rpc.rs,state.rs}`
- Test: `asteria-framework-server/tests/rpc.rs`

**Interfaces:**
- Consumes: Task 2〜7
- Produces:
  - `pub struct AppState { library: Arc<Library>, cache: Arc<Cache>, writer: Writer, tokens: Arc<Tokens>, app: &'static str, app_api: u32, scanning: Arc<AtomicBool>, rescan: mpsc::Sender<()>, filter: Arc<dyn MediaFilter> }`。Task 9 で `processor: Arc<Processor>` を追加する
  - `pub fn router(state: AppState) -> axum::Router`：この Task では `POST /rpc` だけ。Task 9 で同じ router にバイナリ配信のルートを足す
  - 認証は Bearer トークンのミドルウェアで、router 全体にかける。トークンがない、または一致しない場合は 401（本文なし）。成功したら `UserId` を extension に入れる
  - `/rpc`：本文は1件のリクエストだけ（バッチは扱わない）。不正な JSON は PARSE_ERROR、知らないメソッドは METHOD_NOT_FOUND、params の型が合わない場合は INVALID_PARAMS。HTTP ステータスはいつも 200
  - ハンドラ
    - `server.info`：`ServerInfo { app, app_api, server_version: env!("CARGO_PKG_VERSION"), epoch }`
    - `library.info`
    - `library.rescan`：スキャンの再実行を予約し、すぐ `{}` を返す
    - `folder.list`：`limit` の既定は 200、上限は 1000
    - `media.get`
    - `media.update`
  - `MediaItem` の `rating` は、リクエストしたユーザーがオーナーなら `owner_rating`、それ以外なら `user_ratings[user]`
  - エラーコードの対応：NotFound は NOT_FOUND、InvalidPath は INVALID_PATH、ReadOnly は READ_ONLY、Busy は BUSY、Invalid は INVALID_PARAMS、Internal は INTERNAL_ERROR。Internal の詳細はログに出し、レスポンスには一般的な文言だけを返す

- [ ] **Step 1: 失敗するテストを書く**（`tower::ServiceExt::oneshot` で router を直接呼ぶ）

```rust
#[tokio::test] async fn missing_or_wrong_token_is_401() { /* /rpc。バイナリ配信の 401 は Task 9 で確認する */ }
#[tokio::test] async fn server_info_reports_app_and_epoch() {}
#[tokio::test] async fn folder_list_and_media_get() {}
#[tokio::test] async fn media_update_roundtrip() { /* 評価を変え、media.get で読むと反映されている */ }
#[tokio::test] async fn errors_map_to_codes() { /* 存在しない id → -32001。壊れた xmp → -32003。知らないメソッド → -32601。不正な JSON → -32700 */ }
#[tokio::test] async fn ratings_are_per_user() { /* owner と別ユーザーがそれぞれ評価 → それぞれの視点で自分の値が返る */ }
#[tokio::test] async fn token_not_logged() { /* tracing を捕まえるテスト用 subscriber で、ログにトークン文字列が出ないことを確認する */ }
```

- [ ] **Step 2〜4: 失敗を確認し、実装し、成功を確認する**
  Run: `cargo test -p asteria-framework-server --test rpc`

- [ ] **Step 5: Commit** `feat(server): authenticated JSON-RPC over HTTP`

---

### Task 9: 原本の配信とサムネイル

**Files:**
- Create: `asteria-framework-server/src/{media_http.rs,process.rs}`
- Test: `asteria-framework-server/tests/media_http.rs`、`asteria-framework-server/tests/process.rs`

**Interfaces:**
- Produces:
  - `GET /media/{id}/original`：`tower_http::services::ServeFile` で返す（Range、条件付き GET、Content-Type の推定を含む）。レコードがない、またはファイルが消えている場合は 404
  - `GET /media/{id}/{preset}`：preset は `thumb`（長辺 256）か `preview`（長辺 1920）。それ以外は 404
  - `pub struct Processor`
    - `Processor::new(thumbs_dir: PathBuf, max_jobs: usize, timeout: Duration)`
    - `Processor::render(&self, src: &Path, kind: MediaKind, hash: &str, preset: Preset) -> Result<PathBuf, ProcessError>`
    - `pub enum Preset { Thumb, Preview }`、`pub enum ProcessError { Unprocessable(String), ToolMissing(&'static str), Io(String) }`
  - 画像：`vipsthumbnail <src> --size <N>x<N> --export-profile srgb -o <tmp>.webp[Q=80,strip]` を実行し、成功したら rename で置き換える
  - 動画：`ffmpeg -nostdin -v error -ss 1 -i <src> -frames:v 1 -vf scale=w='min(N,iw)':h='min(N,ih)':force_original_aspect_ratio=decrease -c:v libwebp -quality 80 -y <tmp>.webp` を実行する。1秒より短い動画で失敗したら、`-ss 0` でもう一度試す
  - 同時実行数は `max_jobs`（tokio の Semaphore）で制限し、`timeout`（既定 60 秒）を超えたら子プロセスを kill する
  - 同じ (hash, preset) へのリクエストが同時に来たら、生成は1回だけにする（進行中の生成を待つ）
  - 失敗したら `cache.mark_unprocessable` に理由を記録して 422 を返す。記録がある間は再生成しない（ファイルが変われば hash も変わるので、自然に解除される）。hash がまだ計算されていない場合は 503 と `Retry-After: 2` を返す
  - 生成物は `thumbs/<preset>/<hex 先頭2文字>/<hex>.webp`。存在すればそれを返す
  - ツールが見つからない場合は 503 を返し、ログにどのツールがないかを出す

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[tokio::test] async fn media_routes_require_token() { /* /media/{id}/original と /media/{id}/thumb にトークンなし → 401 */ }
#[tokio::test] async fn original_supports_range() { /* Range: bytes=0-9 → 206、10 バイト、Content-Range あり */ }
#[tokio::test] async fn original_of_deleted_file_is_404() {}
#[tokio::test] async fn unknown_preset_is_404() {}
#[tokio::test] async fn thumb_is_webp_and_cached() { /* vips がなければスキップ。1回目は生成、2回目は同じファイルが返る（mtime が変わらない） */ }
#[tokio::test] async fn corrupt_image_is_422_and_not_retried() { /* vips がなければスキップ。JPEG を半分に切ったもの → 422。2回目は vips を呼ばない（Processor の呼び出し回数を数える） */ }
#[tokio::test] async fn hung_tool_times_out() { /* テストでは環境変数 ASTERIA_VIPSTHUMBNAIL を `sleep 10` 相当のスクリプトにする。timeout 1 秒 → Unprocessable("timeout") */ }
#[tokio::test] async fn concurrent_requests_render_once() { /* 同じ thumb を 10 件同時にリクエスト → 生成は 1 回 */ }
#[tokio::test] async fn video_thumb() { /* ffmpeg がなければスキップ。テスト中に ffmpeg で 2 秒の動画を作ってからリクエスト */ }
```
  コマンド名は、環境変数 `ASTERIA_VIPSTHUMBNAIL` と `ASTERIA_FFMPEG` で差し替えられるようにする（テストのため。既定は PATH 上のコマンド）。

- [ ] **Step 2〜4: 失敗を確認し、実装し、成功を確認する**
  Run: `cargo test -p asteria-framework-server --test media_http --test process`

- [ ] **Step 5: Commit** `feat(server): original streaming and thumbnail generation`

---

### Task 10: ファイル監視と定期スキャン

**Files:**
- Create: `asteria-framework-server/src/watch.rs`
- Test: `asteria-framework-server/tests/watch.rs`

**Interfaces:**
- Produces:
  - `pub fn spawn_scan_loop(state: ScanLoop) -> JoinHandle<()>`
  - `pub struct ScanLoop { library: Arc<Library>, cache: Arc<Cache>, filter: Arc<dyn MediaFilter>, writer: Writer, scanning: Arc<AtomicBool>, trigger: mpsc::Receiver<()>, debounce: Duration, period: Duration }`
- ループの流れ
  1. 起動直後に1回スキャンする。
  2. そのあと `hash_pending` を実行し、続けて `writer.relink_orphans()` を呼ぶ。
  3. 以降は、次のいずれかが来たら 1 と 2 を繰り返す。
     - notify のイベント（`debounce` 既定 500ms で、最後のイベントから待つ）
     - `trigger`（`library.rescan`）
     - `period`（既定 15 分）
  - `.asteria/` の中の変更（ジャーナル、一時ファイル）だけのイベントでは、スキャンを起こさない。
  - スキャン中は `scanning` を true にする。スキャン中に新しいトリガーが来たら、終わったあとにもう1回だけ実行する（何回来ても1回にまとめる）。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[tokio::test] async fn new_file_appears_after_debounce() { /* ループを起動し、jpg を置く → 5 秒以内に cache.count が増える */ }
#[tokio::test] async fn journal_writes_do_not_trigger_scans() { /* .asteria/journal に書き込むだけでは、スキャン回数が増えない（カウンタで確認する） */ }
#[tokio::test] async fn triggers_coalesce() { /* スキャン中に trigger を 5 回送る → 追加のスキャンは 1 回だけ */ }
#[tokio::test] async fn moved_file_sidecar_is_relinked() { /* 評価済みの画像を別のフォルダに mv → 新しいパスで評価が見える */ }
```

- [ ] **Step 2〜4: 失敗を確認し、実装し、成功を確認する**
  Run: `cargo test -p asteria-framework-server --test watch`

- [ ] **Step 5: Commit** `feat(server): file watching and periodic rescans`

---

### Task 11: gallery-server バイナリと結合テスト

**Files:**
- Create: `asteria-gallery-server/{Cargo.toml,src/main.rs,tests/e2e.rs}`
- Modify: ルートの `Cargo.toml`、`.github/workflows/ci.yml`（`linux-packages: libvips-tools ffmpeg`）
- Test: `asteria-gallery-server/tests/e2e.rs`

**Interfaces:**
- Produces（CLI。clap derive）
  - `asteria-gallery-server init --library <DIR> --owner <NAME> --config <FILE> [--listen <ADDR>]`
    - `Library::init` でライブラリを作る（既にライブラリなら `open` してそのまま使う）。
    - 新しいトークンを1つ作り、設定にはハッシュだけを保存する。
    - トークンを **標準出力に一度だけ** 表示する。
  - `asteria-gallery-server token add --config <FILE> --user <USER_ID>`：トークンを追加して表示する。ユーザー ID は `users.toml` にあるものだけ受け付ける
  - `asteria-gallery-server serve --config <FILE>`
    - tracing-subscriber で `RUST_LOG` を見る。
    - 起動して待ち受けを始めてから、スキャンのループを起動する（スキャン中もリクエストに答えるため）。
    - Ctrl-C で、書き込みキューを空にしてから終了する。
  - gallery の `MediaFilter` は Image と Video を受け付ける。
- `e2e.rs`：`init` と `serve` を実際のバイナリ（`env!("CARGO_BIN_EXE_asteria-gallery-server")`）で動かす。待ち受けは `127.0.0.1:0` ではなく空いているポートを探して渡す。reqwest で次を確認する。
  1. handshake
  2. スキャンが終わるのを `library.info` で待つ（30秒が上限）
  3. `folder.list`
  4. `media.update`
  5. 原本の Range 取得
  6. サムネイル（vips がない環境ではこの段階だけスキップ）
  7. 401
  8. 再起動後も評価が残っていること（サイドカーが原本）

- [ ] **Step 1: 失敗するテスト（e2e）を書く**
- [ ] **Step 2: 実行して失敗を確認する**
  Run: `cargo test -p asteria-gallery-server --test e2e`
- [ ] **Step 3: main.rs を実装し、CI の設定を更新する**
- [ ] **Step 4: 全体を検証する**
  Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check && cargo deny check`
  Expected: すべて成功
- [ ] **Step 5: Commit** `feat(gallery-server): init/token/serve CLI and end-to-end test`
