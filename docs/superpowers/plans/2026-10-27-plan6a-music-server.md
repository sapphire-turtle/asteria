# Plan 6a: music-server 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `asteria-music-server` を動かすと、音楽のライブラリをスキャンして埋め込みタグを読み、アルバム、アーティスト、フォルダ、プレイリスト（M3U8）で閲覧できる。曲のジャケットは `thumb`/`preview` で、低音質版は `offline-audio`（Ogg Opus 96 kbps）で取れる。

**Architecture:**
- framework-server に `AppExtension` トレイトを足し、`MediaFilter` を置き換える。アプリは次の口を使って、自分の処理を framework に差し込む：
  - スキャンのときにアイテムごとのデータを取り出す（`extract`）
  - アイテムではないファイルを記録させる（`wants_extra`）
  - サムネイルの元画像を差し替える（`thumb_source`）
  - 共通にない RPC メソッドを処理する（`rpc`）
- キャッシュには `APP_DATA` と `EXTRAS` のテーブルと、それらを書き換えるたびに増える世代番号を足す。
- photos-server の起動処理（`init`、`token add`、`serve`）は framework-server の `cli` モジュールに移す。photos-server と music-server は、どちらも `AppSpec` を渡すだけの小さな `main` になる。
- music-server は次のモジュールに分ける。どれも framework を通さずにテストできる純粋な部分を持つ。
  - `tags`：lofty でタグを読む
  - `index`：アルバムとアーティストの索引
  - `playlist`：M3U の解析と解決
  - `cover`：ジャケットの選択
  - `ext`：`AppExtension` の実装と RPC

**Tech Stack:** Rust 1.99.0、axum 0.8、redb、lofty 0.22、ffmpeg（外部コマンド）、unicode-normalization、clap、serde_json。

**Spec:** `docs/superpowers/specs/2026-10-22-asteria-design.md`（特に §4.5、§4.8、§4.9）

## Global Constraints

- ツールチェインは `1.99.0`、edition 2024。CI は clippy `-D warnings`、`cargo fmt --check`、`cargo deny check` を通し、Linux、macOS、Windows でテストする。
- `*-api` crate が依存してよいのは `serde` と `grain-id` だけ。`asteria-music-api` は wire 型を足すので 2.3.0 にする。`asteria-framework-api` と `asteria-photos-api` は変えない。
- `CACHE_SCHEMA` は 4。
- 原本（メディア、サイドカー、M3U）に music-server は書き込まない。タグもプレイリストも読むだけ。
- エラーの文言とログ以外の応答に、サーバーの絶対パスを含めない（`tag_error` も理由だけ）。トークンはログに出さない。
- framework-server に music の知識（タグ、アルバム、M3U）を入れない。framework に足すのは、アプリに依らない口だけ。
- 新しい依存は `unicode-normalization` だけ（MIT/Apache-2.0）。`deny.toml` に例外を足さずに通ること。
- ffmpeg が必要なテストは、今のテストと同じく ffmpeg がなければ飛ばす。
- 新しいメソッド名は `asteria-music-api` に定数で置く：`album.list`、`album.get`、`artist.list`、`artist.get`、`playlist.list`、`playlist.get`。
- 一覧のページ送りは、`limit` の既定 200、上限 1000（`folder.list` と同じ `DEFAULT_LIMIT`、`MAX_LIMIT`）。

## Review Focus

1. **タグが壊れた、またはタグのない曲**：lofty が読めないファイル、タグが1つもないファイル、`ARTIST` が空文字のファイルも、曲として一覧に出る。曲名はファイル名、アルバムはフォルダ名になり、索引の組み立て全体が失敗することはない。→ Task 6、Task 7 でテストする
2. **プレイリストのパスの抜け道**：`../../etc/passwd`、`/etc/passwd`、ライブラリ外の絶対パス、`.asteria/library.toml`、`.hidden/a.flac`、`C:\Music\a.flac` を含む M3U を開いても、ライブラリの外や隠しディレクトリのファイルを指す曲は返さず、「見つからない」になる。→ Task 8 でテストする
3. **大きなライブラリと大きなプレイリスト**：2万曲の索引の組み立てが1秒以内に終わる（release でなくてよい。debug で 5 秒以内）。10,001 エントリの M3U は 10,000 で切られ、`truncated` になる。→ Task 7、Task 8 でテストする
4. **索引の古さ**：曲のタグだけを書き換えて（サイズと更新時刻は変わる）再スキャンすると、次の `album.list` には新しいアルバム名が出る。曲を消すと、空になったアルバムも消える。→ Task 1、Task 10 でテストする
5. **同じ名前の別表記**：`ＡＢＣ` と `abc`、`Beatles` と `beatles` は同じアーティストになり、表示はいちばん多い表記。別のフォルダにある同名のアルバムは別のアルバムになる。`CD1`/`Disc 2` のフォルダは1つのアルバムにまとまる。→ Task 7 でテストする

---

### Task 1: `AppExtension` とキャッシュのアプリ用テーブル

**Files:**
- Create: `crates/asteria-framework-server/src/ext.rs`
- Modify: `crates/asteria-framework-server/src/{lib.rs,cache.rs,scan.rs,watch.rs,state.rs,hashing.rs,upload_commit.rs,writer.rs}`、`photos-server/src/main.rs`（`PhotosFilter` を `AppExtension` に）
- Test: `crates/asteria-framework-server/tests/app_ext.rs`（新規）

**Interfaces:**
- Produces（`ext.rs`、`lib.rs` から re-export）:

```rust
pub trait AppExtension: Send + Sync + 'static {
    fn accepts(&self, kind: MediaKind) -> bool;
    /// Called for each item the scan adds or whose (size, mtime) changed. `abs` is absolute.
    fn extract(&self, _rel: &str, _abs: &Path) -> Option<Vec<u8>> { None }
    /// Non-item files the app wants recorded in EXTRAS (checked before classification).
    fn wants_extra(&self, _rel: &str) -> bool { false }
    /// Source for the `thumb`/`preview` of `rec` (Task 3).
    fn thumb_source(&self, _ctx: &ExtContext, _rec: &ItemRecord, _abs: &Path) -> ThumbSource { ThumbSource::Original }
    /// App-specific RPC methods (Task 2). `None`: not this app's method.
    fn rpc(&self, _ctx: &ExtContext, _method: &str, _params: serde_json::Value)
        -> Option<Result<serde_json::Value, RpcError>> { None }
}
```

  - `ExtContext` と `ThumbSource` の型は Task 1 で定義し、使うのは Task 2 と Task 3。
    - `ExtContext { pub library: Arc<Library>, pub cache: Arc<Cache>, pub user: UserId, pub owner: UserId }`
    - `ThumbSource::{Original, Image { key: String, bytes: Vec<u8>, ext: &'static str }, File { key: String, path: PathBuf }, None}`。`key` は `blake3:<hex>`。
  - `MediaFilter` は消す。`scan`、`ScanLoop.filter`、`AppState::new`、`hash_pending`/`relink_orphans` のテスト、`upload_commit` は `Arc<dyn AppExtension>` を受け取る。`AppState` のアクセサーは `extension()` にする。
- Cache（`cache.rs`）:
  - テーブル `APP_DATA: &str → &[u8]` と `EXTRAS: &str → &[u8]` を追加する。`EXTRAS` の値は `ExtraRecord { size: u64, mtime_ns: i128 }` を serde_json にしたもの。
  - 追加するメソッド：
    - `put_app_data_many(&[(String, Option<Vec<u8>>)])`：`None` は消す。
    - `app_data(rel) -> Option<Vec<u8>>`
    - `all_app_data() -> Vec<(String, Vec<u8>)>`
    - `put_extras(&[(String, ExtraRecord)])`
    - `extras() -> Vec<(String, ExtraRecord)>`
    - `remove_extras(&[String])`
    - `app_generation() -> u64`
  - `remove`/`remove_many` は、`APP_DATA` の同じキーも同じトランザクションで消す。
  - `app_generation` は `AtomicU64`。`APP_DATA` か `EXTRAS` を変える書き込みが成功するたびに増やす。`Cache::open` のときは 0 から始まる。
  - `CACHE_SCHEMA = 4`。
- Scan（`scan.rs`）:
  - 走査中、`is_sidecar` の判定の後で、classify の前に `ext.wants_extra(&rel)` を見る。真なら `EXTRAS` 用に（サイズ、更新時刻）を集め、記録と違うものだけ書いて、`continue` する。
  - アイテムが新規か、（サイズ、更新時刻）が変わったときだけ `ext.extract(&rel, abs)` を呼び、`put_many` と一緒に `put_app_data_many` へ渡す。
  - 走査で見なかった extras は消す。読めなかったサブツリーの下のものは消さない（アイテムと同じ扱い）。

- [ ] **Step 1: 失敗するテストを書く**（`tests/app_ext.rs`。テスト用の `AppExtension` は、`extract` がファイルの中身の長さを返し、`wants_extra` は `.m3u8` で終わるときに真）

```rust
#[test] fn extract_runs_for_new_and_changed_items_only() { /* 2回目のスキャンで変わっていないファイルには呼ばれない（呼び出し回数を数える）。書き換えたファイルには呼ばれ、APP_DATA が新しい値になる */ }
#[test] fn app_data_goes_with_its_item() { /* ファイルを消して再スキャン → app_data(rel) == None */ }
#[test] fn extras_are_recorded_and_removed() { /* list.m3u8 が extras() に出る。アイテムにはならない（count に入らない）。消すと extras() から消える */ }
#[test] fn hidden_extras_are_ignored() { /* .hidden/x.m3u8 と .asteria/ の下は記録されない */ }
#[test] fn generation_moves_only_on_app_changes() { /* 変化のない再スキャンでは app_generation が変わらない。タグ相当の変更（中身を変える）で増える */ }
#[test] fn schema_4_rebuilds_an_old_cache() { /* CACHE_SCHEMA を 3 と書いた古い meta のキャッシュを開くと作り直される（既存の schema テストの書き方に合わせる） */ }
```

- [ ] **Step 2:** テストが失敗することを確かめる。`cargo test -p asteria-framework-server --test app_ext` を実行する。
- [ ] **Step 3:** 実装し、`MediaFilter` を使っていた箇所をすべて置き換える。photos-server の `PhotosFilter` は `PhotosExtension`（`accepts` だけ）にする。
- [ ] **Step 4:** `cargo test --workspace` が通ることを確かめる。
- [ ] **Step 5: Commit** `feat(server): app extensions with per-item app data and extra files`

---

### Task 2: RPC をアプリに渡す

**Files:**
- Modify: `crates/asteria-framework-server/src/rpc.rs`、`crates/asteria-framework-server/src/lib.rs`
- Test: `crates/asteria-framework-server/tests/app_ext.rs`（Task 1 のファイルに足す）

**Interfaces:**
- Produces:
  - `dispatch` で、共通のメソッドに当たらなければ、`state.extension().rpc(&ctx, method, params)` を `spawn_blocking` の中で呼ぶ。`None` なら今と同じく METHOD_NOT_FOUND を返す。
  - 共通のメソッド名は常に framework が先に処理し、アプリに渡さない。
  - `pub fn media_item(rec: ItemRecord, user: UserId, owner: UserId) -> MediaItem` を `lib.rs` から公開する。アプリが曲を返すときに使う。
  - `pub fn rpc_error(code: i32, message: impl Into<String>) -> RpcError` も公開する。今の `err` を公開名にする。

- [ ] **Step 1: 失敗するテストを書く**（router を tower の `oneshot` で呼ぶ。既存の RPC テストの書き方に合わせる）

```rust
#[tokio::test] async fn unknown_method_reaches_the_extension() { /* "test.echo" → 拡張の返した値。ctx.user がトークンのユーザー */ }
#[tokio::test] async fn unhandled_method_is_method_not_found() { /* 拡張が None → METHOD_NOT_FOUND (-32601) */ }
#[tokio::test] async fn common_methods_never_reach_the_extension() { /* 拡張が "folder.list" を横取りしようとしても framework の結果が返る */ }
#[tokio::test] async fn extension_errors_pass_through() { /* 拡張の NOT_FOUND がそのまま返る */ }
```

- [ ] **Step 2:** テストが失敗することを確かめる。
- [ ] **Step 3:** 実装する。
- [ ] **Step 4:** テストが通ることを確かめる。
- [ ] **Step 5: Commit** `feat(server): hand app-specific RPC methods to the app extension`

---

### Task 3: `offline-audio` とジャケットの元画像

**Files:**
- Modify: `crates/asteria-framework-server/src/{process.rs,media_http.rs}`
- Test: `crates/asteria-framework-server/tests/`（既存の process と media のテストのファイルに足す）

**Interfaces:**
- Produces:
  - `Preset::OfflineAudio`（URL の名前は `offline-audio`）。
  - `Preset` のメソッドを足す：
    - `ext()`：`"webp"` か `"ogg"`
    - `mime()`：`image/webp` か `audio/ogg`
    - `timeout(default: Duration) -> Duration`：`OfflineAudio` は 10 分、ほかは `default`
  - 出力のパスは `<thumbs>/<preset>/<hex[..2]>/<hex>.<ext>`。
  - `Processor::render(src, kind, hash, preset)` は、`(Audio, OfflineAudio)` を ffmpeg で変換する：

    ```
    ffmpeg -nostdin -y -protocol_whitelist file -i <src> -map 0:a:0 -map_metadata -1 -vn -c:a libopus -b:a 96k -vbr on -f ogg <tmp>
    ```

    - `-format_whitelist` は今の動画と同じ考えで、音声のコンテナ（`flac,mp3,mov,mp4,m4a,ogg,wav,aac`）に限る。
    - 一時ファイルの名前の拡張子は `.ogg`。
  - ffmpeg の stderr に `Unknown encoder 'libopus'` か `Encoder libopus not found` があれば、`ProcessError::ToolMissing("ffmpeg with libopus")` にする。
  - `Processor::render_image_bytes(bytes: &[u8], ext: &str, key: &str, preset)`：バイト列を `.src-<pid>-<n>.<ext>` に書いて vips に渡す。同じキーの生成は1回にまとめる（`render` と同じ）。
  - `media_http::thumbnail` の変更：
    - `OfflineAudio` は、`rec.kind == Audio` のときだけ（それ以外は 404）、ファイル全体のハッシュをキーに `render` する。
    - `Thumb`/`Preview` は、まず `extension().thumb_source(...)` を `spawn_blocking` で呼ぶ：
      - `Original`：今の動き（Image と Video だけ、ほかは 404）
      - `Image{key,bytes,ext}`：`render_image_bytes`
      - `File{key,path}`：`kind = Image` として `render(path, …, key, …)`
      - `None`：404
    - 処理不可（422）の記録と照会は、どれも使った `key` で行う。
  - 生成物は `Content-Type` を `preset.mime()` にし、Range に対応して返す（`ServeFile` のまま）。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn offline_audio_preset_parses_and_names() { /* parse("offline-audio"), ext()=="ogg", mime()=="audio/ogg", timeout は10分 */ }
#[tokio::test] async fn offline_audio_renders_ogg_opus() { /* ffmpeg が無ければ skip。libopus が無い ffmpeg なら ToolMissing を確かめて終わる。sine440.flac → 先頭が "OggS"、"OpusHead" を含む */ }
#[tokio::test] async fn offline_audio_is_404_for_images() { /* Image のアイテムに /media/{id}/offline-audio → 404 */ }
#[tokio::test] async fn thumb_source_image_bytes_are_rendered_once_per_key() { /* 拡張が同じ key と bytes を2つの曲に返す → renders_started が 1 */ }
#[tokio::test] async fn thumb_source_none_is_404_not_422() { /* 404。unprocessable に記録されない */ }
#[tokio::test] async fn libopus_missing_is_not_unprocessable() { /* "Unknown encoder 'libopus'" を出して 1 で終わる偽の ffmpeg スクリプトを ASTERIA_FFMPEG に（Unix のみ）→ 503。unprocessable に記録されない */ }
```

- [ ] **Step 2:** テストが失敗することを確かめる。
- [ ] **Step 3:** 実装する。
- [ ] **Step 4:** テストが通ることを確かめる。
- [ ] **Step 5: Commit** `feat(server): offline-audio preset and app-chosen thumbnail sources`

---

### Task 4: 起動処理を framework-server の `cli` に移す

**Files:**
- Create: `crates/asteria-framework-server/src/cli.rs`
- Modify: `crates/asteria-framework-server/{Cargo.toml,src/lib.rs}`、`photos-server/{Cargo.toml,src/main.rs}`
- Test: photos-server の単体テスト（ロックと古い状態の検出）は `cli.rs` に移す。`photos-server/tests/e2e.rs` はそのまま通ること。

**Interfaces:**
- Produces:

```rust
pub struct AppSpec {
    pub app: &'static str,           // "asteria-photos"
    pub app_api: u32,
    pub about: &'static str,         // clap の about
    pub default_listen: &'static str, // photos "127.0.0.1:7810"
    pub extension: Arc<dyn AppExtension>,
}
/// Parses the command line (init / token add / serve) and runs it; exits the process with 1 on error.
pub fn main(spec: AppSpec) -> !;
```

- photos-server の `main.rs` は、`PhotosExtension` と `AppSpec` を作って `cli::main` を呼ぶだけになる。
- 動き、定数、ログの文言は今のまま変えない（`RENDER_JOBS`、`RENDER_TIMEOUT`、`PURGE_PERIOD`、`server.lock` など）。
- `clap` は derive ではなく builder で組み立てる。about と既定の待ち受けアドレスを実行時に渡すため。サブコマンドとオプションの名前は今と同じにする。
- clap、tokio（`rt-multi-thread`、`signal`、`net`）、tokio-util、tracing-subscriber を framework-server の依存に移す。

- [ ] **Step 1:** 移す前に、`photos-server/tests/e2e.rs` と photos-server の単体テストが通ることを確かめる。
- [ ] **Step 2:** 移す。単体テストは `cli.rs` の `#[cfg(test)]` に移す。
- [ ] **Step 3:** `cargo test -p asteria-photos-server -p asteria-framework-server` が通ること、`--help` の出力のサブコマンドが変わっていないことを確かめる。
- [ ] **Step 4: Commit** `refactor(server): share the server command line between apps`

---

### Task 5: `asteria-music-api` の wire 型

**Files:**
- Modify: `crates/asteria-music-api/{Cargo.toml,src/lib.rs}`（2.3.0）
- Test: `crates/asteria-music-api/src/lib.rs` の `#[cfg(test)]`

**Interfaces:**
- Produces（すべて `Serialize`、`Deserialize`、`Debug`、`Clone`、`PartialEq`）:

```rust
pub const ALBUM_LIST: &str = "album.list";      // 他の5つも同じ形で
pub struct TrackTags {
    pub title: Option<String>, pub artists: Vec<String>, pub album: Option<String>,
    pub album_artist: Option<String>, pub artist_sort: Option<String>,
    pub album_artist_sort: Option<String>, pub album_sort: Option<String>,
    pub track: Option<u32>, pub track_total: Option<u32>, pub disc: Option<u32>, pub disc_total: Option<u32>,
    pub year: Option<i32>, pub genres: Vec<String>, pub duration_ms: Option<u64>,
    pub has_embedded_cover: bool,
    /// Why the tags could not be read; no server paths.
    pub tag_error: Option<String>,
}
pub struct MusicTrack { pub item: MediaItem, pub tags: TrackTags, /** title or file stem */ pub display_title: String }
pub struct AlbumId(pub String);   // #[serde(transparent)]
pub struct ArtistId(pub String);  // #[serde(transparent)]
pub struct PlaylistId(pub String);// #[serde(transparent)]
pub struct ListParams { pub cursor: Option<String>, pub limit: Option<u32> }
pub struct AlbumSummary { pub id: AlbumId, pub title: String, pub album_artist: String,
    pub album_artist_id: ArtistId, pub year: Option<i32>, pub track_count: u32, pub cover_item: Option<ItemId> }
pub struct AlbumListing { pub albums: Vec<AlbumSummary>, pub next_cursor: Option<String> }
pub struct AlbumGetParams { pub id: AlbumId }
pub struct Album { pub summary: AlbumSummary, /** album folder, library-relative */ pub folder: String, pub tracks: Vec<MusicTrack> }
pub struct ArtistSummary { pub id: ArtistId, pub name: String, pub album_count: u32 }
pub struct ArtistListing { pub artists: Vec<ArtistSummary>, pub next_cursor: Option<String> }
pub struct ArtistGetParams { pub id: ArtistId }
pub struct Artist { pub summary: ArtistSummary, pub albums: Vec<AlbumSummary>, pub appears_on: Vec<AlbumSummary> }
pub struct PlaylistSummary { pub id: PlaylistId, pub name: String, pub path: String }
pub struct PlaylistListing { pub playlists: Vec<PlaylistSummary>, pub next_cursor: Option<String> }
pub struct PlaylistGetParams { pub id: PlaylistId }
#[serde(rename_all = "snake_case")] pub enum EntryStatus { Found, Moved, Missing }
pub struct PlaylistEntry { pub raw_path: String, pub title_hint: Option<String>, pub status: EntryStatus, pub track: Option<MusicTrack> }
pub struct Playlist { pub summary: PlaylistSummary, pub entries: Vec<PlaylistEntry>, pub truncated: bool }
```

- `TrackTags` のフィールドはすべて `#[serde(default)]`。サーバーは `APP_DATA` にもこの型を serde_json で入れるので、後でフィールドを足しても古いキャッシュを読める。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn track_tags_roundtrip_and_defaults() { /* "{}" → 全部 None/空/false。全フィールドを埋めた値が往復する */ }
#[test] fn entry_status_wire_names() { /* "found" / "moved" / "missing" */ }
#[test] fn app_api_tracks_crate_major() { /* 既存。2 のまま */ }
```

- [ ] **Step 2:** 実装し、`cargo test -p asteria-music-api` が通ることを確かめる。
- [ ] **Step 3: Commit** `feat(music-api): album, artist and playlist wire types`

---

### Task 6: music-server の骨組みとタグの読み取り

**Files:**
- Create: `music-server/{Cargo.toml,src/main.rs,src/tags.rs}`
- Modify: ルートの `Cargo.toml`（members に `music-server`）
- Test: `music-server/src/tags.rs` の `#[cfg(test)]`（フィクスチャーは `../crates/asteria-framework/tests/fixtures/` を `CARGO_MANIFEST_DIR` から引く）

**Interfaces:**
- Produces:
  - パッケージ名は `asteria-music-server`（`publish = false`）。
  - `main.rs` は `cli::main(AppSpec { app: asteria_music_api::APP, app_api: APP_API, about: "Asteria Music server", default_listen: "127.0.0.1:7811", extension })` を呼ぶ。
  - `extension` は Task 10 までは `MusicExtension::new()`。最初は `accepts` が Audio、`extract` が `tags::read` を JSON にしたものだけを持つ。
  - `pub fn read(abs: &Path) -> TrackTags`：失敗しても `TrackTags`（`tag_error` に理由）を返し、パニックしない。
    - `lofty::read_from_path` の主タグ（なければ最初のタグ）から読む。
    - アーティストは、`ItemKey::TrackArtist` の全部の値を出てきた順に読む。空文字と前後の空白は落とす。`;` などで分けない。
    - 長さは `properties().duration()` から取る。
    - `tag_error` には lofty のエラーの種類だけを入れる。パスは入れない。
  - `pub fn display_title(tags: &TrackTags, rel: &str) -> String`：曲名がなければ、ファイル名から拡張子を除いたもの。

- [ ] **Step 1: 失敗するテストを書く**（`crates/asteria-framework/tests/hash_audio.rs` の `tagged` と同じ方法で、テストの中でタグを書いたコピーを作る）

```rust
#[test] fn reads_vorbis_comments_from_flac() { /* 曲名、2つの ARTIST、ALBUM、ALBUMARTIST、TRACKNUMBER=3、DISCNUMBER=2、DATE=1999、GENRE */ }
#[test] fn reads_id3v2_from_mp3() {}
#[test] fn embedded_cover_is_flagged() {}
#[test] fn untagged_file_gives_defaults() { /* フィクスチャーそのまま → title None、tag_error None、duration_ms Some */ }
#[test] fn garbage_file_sets_tag_error_without_path() { /* .flac という名前の乱数バイト → tag_error Some、パス（tempdir の名前）を含まない */ }
#[test] fn empty_artist_values_are_dropped() {}
#[test] fn display_title_falls_back_to_stem() { /* "Artist/Album/01 Song.flac" → "01 Song" */ }
```

- [ ] **Step 2:** テストが失敗することを確かめる。
- [ ] **Step 3:** 実装する。
- [ ] **Step 4:** テストが通ることを確かめる。
- [ ] **Step 5: Commit** `feat(music-server): read embedded tags during the scan`

---

### Task 7: アルバムとアーティストの索引

**Files:**
- Create: `music-server/src/index.rs`
- Modify: ルートの `Cargo.toml`（`unicode-normalization` を workspace の依存に追加）、`music-server/Cargo.toml`
- Test: `music-server/src/index.rs` の `#[cfg(test)]`

**Interfaces:**
- Produces:

```rust
pub struct Index { /* albums sorted, artists sorted, by-id maps */ }
pub struct IndexedAlbum { pub id: AlbumId, pub key_folder: String, pub title: String, pub album_artist: String,
    pub album_artist_id: ArtistId, pub year: Option<i32>, pub tracks: Vec<String> /* rel paths, ordered */,
    pub embedded_cover_item: Option<String> /* rel path of the first track with an embedded cover */ }
pub struct IndexedArtist { pub id: ArtistId, pub name: String, pub albums: Vec<AlbumId>, pub appears_on: Vec<AlbumId> }
impl Index {
    pub fn build(library: &LibraryId, tracks: impl IntoIterator<Item = (String, TrackTags)>) -> Index;
    pub fn albums(&self) -> &[IndexedAlbum];
    pub fn album(&self, id: &AlbumId) -> Option<&IndexedAlbum>;
    pub fn artists(&self) -> &[IndexedArtist];
    pub fn artist(&self, id: &ArtistId) -> Option<&IndexedArtist>;
}
pub fn album_folder(rel: &str) -> &str;  // CD1 / Disc 2 → parent
pub fn name_key(s: &str) -> String;      // NFKC + to_lowercase + trim
```

- 規則は spec §4.9「アルバムの判定」「アーティスト」のとおり。ここで決めること：
  - ディスクのフォルダの判定は、正規表現なしで、名前を小文字にして `cd`、`disc`、`disk` のどれかで始まり、残りが「空白（任意）＋1文字以上の ASCII 数字」だけのもの。
  - アルバム ID は `Uuid::new_v5(library_uuid, "album\n{key_folder}\n{name_key(title)}")`。アーティスト ID は `Uuid::new_v5(library_uuid, "artist\n{name_key(name)}")`。どちらも文字列にして使う。
  - `Various Artists` は固定の表記で、`name_key` は `various artists`。
  - アルバムの並びは（アルバムアーティストのソート用の名前の `name_key`、年（ないものは後ろ）、タイトルの `name_key`、ID）。アーティストの並びは（ソート用の名前の `name_key`、ID）。ソート用のタグがなければ表示名を使う。
  - 表記の選択（いちばん多い表記、同数なら文字列の順で先）は、アーティスト名とアルバムアーティスト名の両方に使う。
  - `appears_on` には、そのアーティストの `albums` に入っているアルバムを入れない。

- [ ] **Step 1: 失敗するテストを書く**（`TrackTags` を直接組み立てる。ファイルは使わない）

```rust
#[test] fn album_artist_tag_wins_majority() {}
#[test] fn single_artist_without_album_artist() {}
#[test] fn mixed_artists_become_various_artists() {}
#[test] fn missing_album_tag_uses_folder_name() {}
#[test] fn same_album_name_in_two_folders_stays_apart() {}
#[test] fn disc_folders_merge_into_one_album() { /* "A/Album/CD1/01.flac" と "A/Album/Disc 2/01.flac" → 1つ。並びはディスク→トラック */ }
#[test] fn disc_folder_detection() { /* "CD1" "cd 2" "Disc 10" "disk3" は真。"CDs" "Disco" "Disc" "Disc 2 bonus" は偽 */ }
#[test] fn unnumbered_tracks_sort_after_numbered() {}
#[test] fn album_year_is_earliest() {}
#[test] fn artist_names_fold_width_and_case() { /* "ＡＢＣ"×1 "abc"×2 → 1人で表示は "abc" */ }
#[test] fn appears_on_excludes_own_albums() {}
#[test] fn sort_tags_order_artists() { /* "The Beatles"（ALBUMARTISTSORT "Beatles, The"）が "Carpenters" より前 */ }
#[test] fn embedded_cover_item_is_first_track_with_embedded_cover() {}
#[test] fn ids_are_stable_across_builds() {}
#[test] fn twenty_thousand_tracks_build_quickly() { /* 2万曲・2千アルバム → debug で 5 秒以内 */ }
```

- [ ] **Step 2:** テストが失敗することを確かめる。
- [ ] **Step 3:** 実装する。
- [ ] **Step 4:** テストが通ることを確かめる。
- [ ] **Step 5: Commit** `feat(music-server): album and artist index from the tags`

---

### Task 8: プレイリストの解析と解決

**Files:**
- Create: `music-server/src/playlist.rs`
- Test: `music-server/src/playlist.rs` の `#[cfg(test)]`

**Interfaces:**
- Produces:

```rust
pub const MAX_ENTRIES: usize = 10_000;
pub struct RawEntry { pub path: String, pub title_hint: Option<String>, pub hash: Option<String> }
pub struct Parsed { pub entries: Vec<RawEntry>, pub truncated: bool }
/// UTF-8 with lossy replacement; strips a BOM; CRLF and LF.
pub fn parse(bytes: &[u8]) -> Parsed;
pub enum Resolved { Found(String), Moved(String), Missing }
/// `playlist_rel`: the playlist's library-relative path. `exists(rel)`: an item at that path.
/// `by_hash(hash)`: item paths with that media-payload hash, sorted.
pub fn resolve(playlist_rel: &str, entry: &RawEntry, exists: impl Fn(&str) -> bool,
               by_hash: impl Fn(&str) -> Vec<String>) -> Resolved;
pub fn library_relative(playlist_rel: &str, raw: &str, root: &Path) -> Option<String>;
```

- `parse` の扱い：
  - `#EXTM3U` は読み飛ばす。
  - `#EXTINF:<秒>,<曲名>` の曲名と `#ASTERIA-HASH:<値>` は、次のエントリに付ける。
  - そのほかの `#` 行と空行は無視する。
- `library_relative` の扱い：
  - `\` を `/` にしてから解決する。
  - `scheme://` で始まるもの（URL）は `None`。
  - 絶対パスは、`root` の下を指していれば相対パスに直し、そうでなければ `None`。Windows のドライブ文字（`C:`）で始まるものも同じ扱い。
  - 相対パスは、プレイリストのフォルダからつなぎ、`.` と `..` を文字列の上で畳む。ライブラリの外に出れば `None`。
  - 結果のどの要素も `.` で始まらず、空でないこと。満たさなければ `None`。
  - ファイルシステムには触らない。
- `resolve`：パスがあり `exists` なら `Found`。なければ、`hash` があって `by_hash` が空でないとき、先頭のものを `Moved` にする。それ以外は `Missing`。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn parses_extinf_and_asteria_hash() {}
#[test] fn bom_crlf_and_latin1_bytes_are_tolerated() { /* 0xE9 を含む行は U+FFFD に置き換わり、ほかの行は読める */ }
#[test] fn truncates_after_ten_thousand_entries() { /* 10,001 件 → 10,000 件、truncated */ }
#[test] fn relative_paths_resolve_from_the_playlist_folder() { /* "Lists/x.m3u8" の "../A/1.flac" → "A/1.flac" */ }
#[test] fn backslashes_are_separators() {}
#[test] fn escapes_and_hidden_paths_are_rejected() { /* "../../etc/passwd"、"/etc/passwd"、".asteria/library.toml"、".hidden/a.flac"、"C:\\Music\\a.flac"、"http://x/a.mp3" → None */ }
#[test] fn absolute_path_inside_root_is_accepted() { /* root/A/1.flac の絶対パス → "A/1.flac" */ }
#[test] fn missing_path_with_hash_is_moved() {}
#[test] fn missing_path_without_hash_is_missing() {}
```

- [ ] **Step 2:** テストが失敗することを確かめる。
- [ ] **Step 3:** 実装する。
- [ ] **Step 4:** テストが通ることを確かめる。
- [ ] **Step 5: Commit** `feat(music-server): parse and resolve M3U playlists`

---

### Task 9: ジャケットの選択

**Files:**
- Create: `music-server/src/cover.rs`
- Test: `music-server/src/cover.rs` の `#[cfg(test)]`

**Interfaces:**
- Produces:

```rust
pub const NAMES: [&str; 4] = ["cover", "folder", "front", "album"];
pub const EXTS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];
/// True for a file name `<NAMES>.<EXTS>` (ASCII case-insensitive). Used by `wants_extra`.
pub fn is_cover_name(file_name: &str) -> bool;
/// Embedded picture: front cover first, else the first picture. `None` if none or unreadable.
pub fn embedded(abs: &Path) -> Option<(Vec<u8>, &'static str /* ext from MIME */)>;
/// The folder image for the track at `rel`, from the extras' paths: track folder first, then
/// the album folder (`index::album_folder`); within a folder, NAMES order then EXTS order.
pub fn folder_image<'a>(rel: &str, extras: impl IntoIterator<Item = &'a str>) -> Option<String>;
pub fn thumb_source(root: &Path, rel: &str, abs: &Path, extras: &[String]) -> ThumbSource;
```

- `thumb_source` は、次の順で決める。
  1. 埋め込み画像があれば `Image { key: blake3(bytes), bytes, ext }`。
  2. なければ、フォルダの画像（`folder_image` が返したパス `img`）を読み、`File { key: blake3(その中身), path: root.join(img) }`。
  3. それもなければ `None`。
- 埋め込み画像の MIME が jpeg、png、webp 以外（gif、bmp など）なら、拡張子は MIME から決める。vips が読めなければ 422 になるが、それでよい（ファイルが変わるまで再試行しない）。
- フォルダの画像は、シンボリックリンクでライブラリの外を指していないことを、`canonicalize` で確かめる。外を指していれば使わない。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn cover_names() { /* "Cover.JPG" "folder.png" は真。"cover.gif" "covers.jpg" "back.jpg" は偽 */ }
#[test] fn front_cover_preferred_over_other_pictures() {}
#[test] fn track_folder_beats_album_folder() { /* "A/Album/CD1/cover.jpg" と "A/Album/folder.jpg" → CD1 のもの */ }
#[test] fn names_order_within_a_folder() { /* folder.jpg と cover.png → cover.png */ }
#[test] fn embedded_beats_folder() {}
#[test] fn same_image_gives_same_key() {}
#[test] fn nothing_gives_none() {}
#[cfg(unix)] #[test] fn folder_image_symlink_outside_library_is_ignored() {}
```

- [ ] **Step 2:** テストが失敗することを確かめる。
- [ ] **Step 3:** 実装する。
- [ ] **Step 4:** テストが通ることを確かめる。
- [ ] **Step 5: Commit** `feat(music-server): choose album art from embedded pictures or folder images`

---

### Task 10: `MusicExtension` と RPC

**Files:**
- Create: `music-server/src/ext.rs`
- Modify: `music-server/src/main.rs`
- Test: `music-server/src/ext.rs` の `#[cfg(test)]`（一時ディレクトリにライブラリを作り、`scan` してから `rpc` を直接呼ぶ）

**Interfaces:**
- Produces: `pub struct MusicExtension`。`impl AppExtension` は次のとおり：
  - `accepts`：Audio だけ
  - `extract`：`serde_json::to_vec(&tags::read(abs))`
  - `wants_extra`：拡張子が `m3u8`/`m3u`（大文字・小文字を区別しない）か、`cover::is_cover_name` が真
  - `thumb_source`：`cover::thumb_source`
  - `rpc`：6つのメソッド
- 索引は `Mutex<Option<(u64, Arc<Index>, Arc<Vec<String>> /* extras paths */)>>` に持つ。`ctx.cache.app_generation()` が違えば、`all_app_data()` と `extras()` から組み直す。
  - JSON として読めない `APP_DATA` は、`TrackTags { tag_error: Some("unreadable cache entry"), ..Default }` とみなす。
- `AlbumSummary.cover_item` は、`embedded_cover_item` があればその曲、なければ最初の曲にフォルダの画像（`cover::folder_image`）があればその曲、どちらもなければ `None`。
- 曲を返すときは、`cache.get_by_path(rel)` の `ItemRecord` から `media_item` を作る。レコードがなければ、その曲は飛ばす。
- カーソルは、前のページの最後の要素の位置ではなく、その要素の ID（文字列）にする。次のページは、並びの中でその ID の直後から始める。
  - 索引が組み直されてその ID が消えていたら、INVALID_PARAMS（"cursor is no longer valid"）を返す。クライアントは最初から取り直す。
- `playlist.list` は extras のうち `m3u8`/`m3u` を、パスの順に返す。ID は `Uuid::new_v5(library, "playlist\n{rel}")`。
- `playlist.get` の手順：
  1. ファイルを `library.root().join(rel)` から読む。読む前に `canonicalize` し、ライブラリの中で `.` で始まる要素がないことを確かめる。満たさなければ NOT_FOUND。
  2. `parse` と `resolve` を通す。
  3. `exists` はキャッシュの `get_by_path` が Audio のアイテムを返すこと、`by_hash` は `paths_with_hash(h, "media-payload/1")` を使う。
- パラメーターの形が違えば INVALID_PARAMS、未知の ID は NOT_FOUND。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn album_list_pages_with_cursor() { /* 3アルバム、limit 2 → 2件と next_cursor、続きで1件と None */ }
#[test] fn stale_cursor_is_invalid_params() {}
#[test] fn album_get_returns_ordered_tracks_with_ratings() { /* サイドカーの評価が MusicTrack.item.rating に出る */ }
#[test] fn retag_and_rescan_updates_the_index() { /* タグを書き換えて scan → album.list に新しい名前、古い名前は消える（Review Focus 4） */ }
#[test] fn deleted_tracks_drop_empty_albums() {}
#[test] fn artist_get_lists_albums_and_appears_on() {}
#[test] fn playlist_list_and_get() { /* Found、Moved（ファイルを移動し、#ASTERIA-HASH で見つかる）、Missing */ }
#[test] fn unknown_ids_are_not_found() {}
#[test] fn unknown_method_is_not_handled() { /* rpc("album.nope") → None */ }
```

- [ ] **Step 2:** テストが失敗することを確かめる。
- [ ] **Step 3:** 実装し、`main.rs` で使う。
- [ ] **Step 4:** テストが通ることを確かめる。
- [ ] **Step 5: Commit** `feat(music-server): album, artist and playlist methods`

---

### Task 11: e2e と README

**Files:**
- Create: `music-server/tests/e2e.rs`（`photos-server/tests/e2e.rs` と同じ形で、Linux だけで動く）
- Modify: `README.md`、`README.ja.md`（music-server の起動と、閲覧できるもの）、`.github/workflows/`（ワークスペースを丸ごとテストしていれば変更不要。変更が要るか確かめる）

**Interfaces:**
- Consumes: Task 4 の CLI（`init`、`token add`、`serve`）、Task 10 のメソッド。

- [ ] **Step 1: e2e を書く**

```rust
#[test] fn music_server_end_to_end() {
    /* init → タグを書いた FLAC 2曲（同じアルバム、1曲に埋め込み画像）＋ MP3 1曲（別アルバム、フォルダに cover.png）＋ list.m3u8 を置く → serve
       → library.info でスキャンを待つ → album.list が2件 → album.get の曲順 → artist.list
       → playlist.get が Found 2件と Missing 1件
       → /media/{id}/thumb は vips があれば 200 の image/webp、なければ 503
       → /media/{id}/offline-audio は libopus 付きの ffmpeg があれば 200 の audio/ogg（先頭が "OggS"）、なければ 503
       → /media/{id}/original に Range: bytes=0-3 → 206
       → <library>/.asteria には library.toml と users.toml だけ */
}
```

- [ ] **Step 2:** `cargo test -p asteria-music-server --test e2e` が通ることを確かめる。
- [ ] **Step 3:** README（en と ja）に music-server の節を足す。中身は起動方法、既定のポート 7811、閲覧の軸、ジャケットの探し方、ffmpeg（libopus 付き）が必要なことと、タグとプレイリストは読むだけであること。
- [ ] **Step 4:** `cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --check`、`cargo deny check` がすべて通ることを確かめる。
- [ ] **Step 5: Commit** `test(music-server): end to end; docs: running the music server`
