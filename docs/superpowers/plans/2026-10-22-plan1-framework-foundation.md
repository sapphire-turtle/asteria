# Plan 1: framework の土台 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cargo ワークスペースと API の型 crate を用意し、asteria-framework のコアを作る。コアの中身は、ライブラリ設定、payload ハッシュ（第1段階）、XMP サイドカーの読み書き、ジャーナル、必要時だけサイドカーを作る更新 API。単独でテストできるライブラリとして仕上げる。

**Architecture:** `asteria-framework` は I/O を持つ純粋なライブラリ crate にする（サーバーにもクライアントにも依存しない）。原本ファイルはすべて「一時ファイル → rename」で書く。ハッシュは形式ごとのモジュールで実装し、形式はマジックバイトで判定する。

**Tech Stack:** Rust 1.99.0（edition 2024）、serde、toml、serde_json、blake3、xml-rs 0.8、grain-id、chrono、thiserror、uuid。dev では tempfile と lofty を使う。

**Spec:** `docs/superpowers/specs/2026-10-22-asteria-design.md`

## 計画の分割（ロードマップ）

spec は複数のサブシステムにまたがるので、次のように分割する。各計画は、前の計画が終わってから書く。

1. **本計画**：framework の土台
2. framework-server と gallery-server の最小構成（スキャン、redb キャッシュ、JSON-RPC、バイナリ配信、サムネイル）
3. iOS の検証用試作（spec §8 の4項目。捨てるコードとして作る）
4. gallery のデスクトップクライアントとアップロード（Inbox）
5. player のサーバー、デスクトップクライアント、再生エンジン、埋め込みタグの書き換え
6. MCP と整理計画（計画の作成、承認、実行、取り消し、ゴミ箱）
7. オフラインキャッシュと操作キュー
8. iOS のクライアント（PhotoKit を使ったアップロード、バックグラウンド再生）
9. payload ハッシュ第2段階（HEIC / AVIF / M4A / Ogg）

## Global Constraints

- ツールチェインは `1.99.0` に固定する（sapphire-turtle 共通）。edition は 2024。CI は clippy を `-D warnings` で回し、`cargo fmt --check` も通す。
- ライセンス：`asteria-framework-api` / `asteria-gallery-api` / `asteria-player-api` は `MIT OR Apache-2.0`。それ以外は `GPL-3.0-or-later` に App Store 例外（`LICENSE.App-Store-Exception`）を付ける。
- `cargo deny` で、依存ライブラリは寛容なライセンスに限る。GPL を許可するのは asteria 自身の crate だけ。
- `*-api` crate が依存してよいのは `serde` と `grain-id` だけ。
- ID には grain-id（`GrainId`）を使う。sapphire と揃えるため。
- 原本ファイル（サイドカー、設定、メディア）は必ず「同じディレクトリの一時ファイル `.<元の名前>.asteria-tmp` に書く → rename」で書く。
- ハッシュ文字列の形式は `blake3:<64桁の小文字 hex>`。スコープ文字列は `media-payload/1` か `file/1`。
- XMP の名前空間 URI
  - xmp：`http://ns.adobe.com/xap/1.0/`
  - dc：`http://purl.org/dc/elements/1.1/`
  - lr：`http://ns.adobe.com/lightroom/1.0/`
  - asteria：`https://sapphire-turtle.github.io/ns/asteria/1.0/`
  - rdf：`http://www.w3.org/1999/02/22-rdf-syntax-ns#`
  - x：`adobe:ns:meta/`
- サイドカーのパスは `<メディアのファイル名>.xmp`（拡張子を含めたファイル名に `.xmp` を足す）。
- メタデータ用ディレクトリは `<ライブラリルート>/.asteria/`。

## Review Focus

1. **日本語を含むパス**（`2025/京都旅行/IMG_0001.HEIC`）：サイドカーのパス、一時ファイル名、ジャーナルの記録が文字化けせず、往復で一致すること。→ Task 2、Task 8 でテストする
2. **途中で切れた、または壊れたメディア**：ハッシュ計算が panic せず、`file/1` に落ちて理由を返すこと。→ Task 4、Task 5、Task 6 でテストする
3. **拡張子が嘘をついているファイル**（中身が PNG なのに `.jpg`）：マジックバイトで正しい形式として扱うこと。→ Task 3 でテストする
4. **他のツールが書いた XMP**：属性形式と要素形式のどちらで書かれていても読めること。`<?xpacket?>` の有無や、知らない名前空間があっても扱えること。→ Task 7、Task 8 でテストする
5. **手で編集して壊れた XMP**：更新 API がエラーを返し、ファイルを上書きしないこと。→ Task 10 でテストする

---

### Task 1: ワークスペースの土台と API crate

**Files:**
- Create: `Cargo.toml`（ワークスペース）、`rust-toolchain.toml`、`deny.toml`、`.github/workflows/ci.yml`、`.github/workflows/cargo-deny.yml`
- Create: `LICENSE-MIT`、`LICENSE-APACHE`（API crate 用）、`LICENSE-GPL`（GPLv3 全文）、`LICENSE.App-Store-Exception`
- Create: `asteria-framework-api/{Cargo.toml,src/lib.rs}`、`asteria-gallery-api/{Cargo.toml,src/lib.rs}`、`asteria-player-api/{Cargo.toml,src/lib.rs}`
- Create: `asteria-framework/{Cargo.toml,src/lib.rs}`
- Test: `asteria-framework-api/src/lib.rs`（`#[cfg(test)]`）、`asteria-gallery-api/src/lib.rs`（`#[cfg(test)]`）

**Interfaces:**
- Produces（`asteria-framework-api`）:
  - `pub struct ClientInfo { pub app: String, pub app_api: u32 }`
  - `pub struct ServerInfo { pub app: String, pub app_api: u32, pub server_version: String }`
  - `pub enum HandshakeError { AppMismatch { client: String, server: String }, ApiMismatch { client: u32, server: u32 } }`。Display と std::error::Error を実装する
  - `pub fn check_handshake(client: &ClientInfo, server: &ServerInfo) -> Result<(), HandshakeError>`
  - いずれも Serialize / Deserialize / Debug / Clone / PartialEq を derive する
- Produces（`asteria-gallery-api` / `asteria-player-api`）: `pub use asteria_framework_api::*;`、`pub const APP: &str`（`"asteria-gallery"` / `"asteria-player"`）、`pub const APP_API: u32 = 1;`、`pub fn client_info() -> ClientInfo`

- [ ] **Step 1: ワークスペースの設定ファイルを作る**
  - `Cargo.toml`
    - `members` に上の4つの crate を入れる。`resolver = "3"`。
    - `[workspace.package]` は `version = "0.1.0"`、`edition = "2024"`、`license = "GPL-3.0-or-later"`、`repository = "https://github.com/sapphire-turtle/project-asteria"`。
    - `[workspace.dependencies]` に入れるもの：serde（derive）、serde_json、toml、blake3、xml-rs（当初は xmltree だったが Task 7 で置き換え）、`grain-id = { version = "0.15", features = ["serde"] }`、chrono（serde）、thiserror、`uuid = { version = "1", features = ["v4", "serde"] }`、tempfile、lofty。
  - API crate は `version = "1.0.0"` を個別に指定し（ワークスペースのバージョンは使わない）、`license = "MIT OR Apache-2.0"`、`publish = false` とする。
  - `asteria-framework` はワークスペースのバージョンとライセンスを使い、`license-file` は使わず SPDX 表記にする。
  - `rust-toolchain.toml` は sapphire-journal のものをコメントごとコピーする。
  - `deny.toml` は `project-sapphire/sapphire-journal/deny.toml` をベースにする。`exceptions` の GPL 許可は `asteria-framework` に付け替え、`ignore` は空にする。
  - CI の2ファイルは sapphire-journal の `ci.yml` / `cargo-deny.yml` と同じく、`sapphire-turtle/sapphire-framework` の共有ワークフローを呼ぶ。`linux-packages` は今は指定しない。
  - `LICENSE.App-Store-Exception` は `sapphire-journal/desktop/LICENSE.App-Store-Exception` の製品名を `project-asteria` に置き換えたもの。

- [ ] **Step 2: 失敗するテストを書く**

```rust
// asteria-framework-api/src/lib.rs
#[test] fn handshake_accepts_same_app_and_api() {
    let c = ClientInfo { app: "asteria-gallery".into(), app_api: 1 };
    let s = ServerInfo { app: "asteria-gallery".into(), app_api: 1, server_version: "0.1.0".into() };
    assert_eq!(check_handshake(&c, &s), Ok(()));
}
#[test] fn handshake_rejects_api_mismatch() {
    // client 2 / server 1 → Err(ApiMismatch { client: 2, server: 1 })
}
#[test] fn handshake_rejects_other_app() {
    // client "asteria-player" / server "asteria-gallery" → Err(AppMismatch{..})
}
#[test] fn server_info_json_roundtrip() {
    // serde_json で直列化してから戻し、元と一致すること
}
// asteria-gallery-api/src/lib.rs
#[test] fn client_info_uses_app_constants() {
    assert_eq!(client_info(), ClientInfo { app: "asteria-gallery".into(), app_api: 1 });
}
```

- [ ] **Step 3: 実行して失敗を確認する**
  Run: `cargo test --workspace`
  Expected: コンパイルエラー（`check_handshake` などが未定義）

- [ ] **Step 4: 型と関数を実装する**
  `check_handshake` は app を先に比べ、一致したら app_api を比べる。player-api も gallery-api と同じ形で作る。

- [ ] **Step 5: テスト、clippy、fmt、deny を通す**
  Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings && cargo fmt --check && cargo deny check`
  Expected: すべて成功

- [ ] **Step 6: Commit**
  `git add -A && git commit -m "chore: scaffold workspace, api crates, licenses and CI"`

---

### Task 2: ライブラリ設定とユーザー

**Files:**
- Create: `asteria-framework/src/{error.rs,ids.rs,library.rs,paths.rs}`
- Modify: `asteria-framework/src/lib.rs`（モジュールを公開する）
- Test: `asteria-framework/tests/library.rs`

**Interfaces:**
- Produces:
  - `pub struct UserId(pub GrainId)`：serde は transparent、Display / FromStr / Ord / Hash を実装
  - `pub struct LibraryId(pub uuid::Uuid)`：serde は transparent
  - `pub struct LibraryConfig { pub format: u32, pub id: LibraryId, pub owner: UserId }`：`library.toml` の内容。`format` は 1 にする
  - `pub struct User { pub id: UserId, pub name: String }`
  - `pub struct Users { pub user: Vec<User> }`：`users.toml` の内容（`[[user]]` の配列）
  - `pub struct Library { root: PathBuf, config: LibraryConfig, users: Users }`
    - `Library::init(root: &Path, owner_name: &str) -> Result<Library>`：オーナーの UserId を `GrainId::random()` で作る。`.asteria/` が既にある場合は `Error::AlreadyInitialized`
    - `Library::open(root: &Path) -> Result<Library>`：`.asteria/library.toml` がなければ `Error::NotALibrary`。`format != 1` なら `Error::UnsupportedFormat(u32)`
    - `root() -> &Path`、`config() -> &LibraryConfig`、`users() -> &Users`、`owner() -> UserId`
  - `paths` モジュール
    - `pub const META_DIR: &str = ".asteria";`
    - `pub fn sidecar_path_for(media: &Path) -> PathBuf`：`a/b.HEIC` → `a/b.HEIC.xmp`
    - `pub fn is_sidecar(path: &Path) -> bool`
    - `pub fn media_path_for_sidecar(sidecar: &Path) -> Option<PathBuf>`
  - `pub enum Error`（thiserror）：`Io(std::io::Error)`、`NotALibrary(PathBuf)`、`AlreadyInitialized(PathBuf)`、`UnsupportedFormat(u32)`、`Toml(String)`。以降のタスクで variant を追加していく
  - `pub type Result<T> = std::result::Result<T, Error>`

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn init_then_open_roundtrips_config() {
    // tempdir で init(root, "fluo10")、そのあと open(root)
    // config().id と owner() が一致し、users() に名前 "fluo10" のオーナーがいる
    // root/.asteria/library.toml と users.toml が存在する
}
#[test] fn open_non_library_fails() { /* 空の tempdir → Err(Error::NotALibrary(_)) */ }
#[test] fn init_twice_fails() { /* 2回目は Err(Error::AlreadyInitialized(_)) */ }
#[test] fn unsupported_format_fails() { /* library.toml の format を 2 に書き換える → Err(UnsupportedFormat(2)) */ }
#[test] fn sidecar_path_keeps_extension_and_japanese() {
    assert_eq!(sidecar_path_for(Path::new("2025/京都旅行/IMG_0001.HEIC")),
               PathBuf::from("2025/京都旅行/IMG_0001.HEIC.xmp"));
    assert_eq!(media_path_for_sidecar(Path::new("2025/京都旅行/IMG_0001.HEIC.xmp")),
               Some(PathBuf::from("2025/京都旅行/IMG_0001.HEIC")));
    assert!(is_sidecar(Path::new("a.jpg.xmp")) && !is_sidecar(Path::new("a.jpg")));
}
```

- [ ] **Step 2: 実行して失敗を確認する**
  Run: `cargo test -p asteria-framework --test library`
  Expected: コンパイルエラー

- [ ] **Step 3: 実装する**
  `init` では `.asteria/` を作り、2つの TOML を書く。この時点ではアトミック書き込みはまだないので普通に書き、Task 8 で `fsutil::atomic_write` に置き換える。

- [ ] **Step 4: 実行して成功を確認する**
  Run: `cargo test -p asteria-framework --test library`
  Expected: PASS

- [ ] **Step 5: Commit**
  `git commit -am "feat(framework): library config, users and sidecar paths"`（新規ファイルは `git add` してから）

---

### Task 3: ハッシュの型、形式の判定、`file/1`

**Files:**
- Create: `asteria-framework/src/hash/{mod.rs,detect.rs,file.rs}`
- Test: `asteria-framework/tests/hash_core.rs`

**Interfaces:**
- Produces:
  - `pub struct ContentHash([u8; 32])`：Display は `blake3:<hex>`、FromStr は不正な形式を `Error::InvalidHash` にする。serde は文字列として扱う
  - `pub enum HashScope { MediaPayload1, File1 }`：Display / FromStr は `media-payload/1` と `file/1`。それ以外の文字列は `Error::InvalidHashScope`
  - `pub enum MediaFormat { Jpeg, Png, WebP, Flac, Mp3, Unknown }`
  - `pub fn detect_format(head: &[u8]) -> MediaFormat`：先頭 16 バイトだけで判定する
    - JPEG：`FF D8 FF`
    - PNG：`89 50 4E 47 0D 0A 1A 0A`
    - WebP：`RIFF????WEBP`
    - FLAC：`fLaC`、または先頭が ID3v2 で、そのタグを飛ばした位置が `fLaC`。後者は先頭 16 バイトでは判定できないので、`detect_format_file` で扱う
    - MP3：`ID3`、または先頭がフレーム同期（`FF Ex`/`FF Fx`）
  - `pub fn detect_format_file(path: &Path) -> Result<MediaFormat>`：ID3v2 の後ろに `fLaC` がある場合も見る
  - `pub struct HashOutcome { pub hash: ContentHash, pub scope: HashScope, pub fallback_reason: Option<String> }`
  - `pub fn content_hash(path: &Path) -> Result<HashOutcome>`：形式ごとの payload ハッシュに振り分ける
    - 形式が Unknown：`file/1` を返し、`fallback_reason` は None
    - payload ハッシュが `Error::MalformedMedia` を返した：`file/1` を返し、`fallback_reason` にエラー文を入れる
    - I/O エラー：そのまま Err
  - `pub(crate) fn file_hash(path: &Path) -> Result<ContentHash>`：ファイル全体の BLAKE3
  - Error に `InvalidHash(String)`、`InvalidHashScope(String)`、`MalformedMedia { format: MediaFormat, reason: String }` を追加する
  - この Task では、どの形式の payload ハッシュも未実装のまま `file/1` に落ちる。Task 4〜6 で `content_hash` の振り分けに追加していく

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn content_hash_display_roundtrip() {
    // "blake3:" に 64 桁の hex → parse → to_string で同じ文字列に戻る
    // "sha256:..."、hex が 63 桁、大文字の hex → Err
}
#[test] fn scope_roundtrip() { /* "media-payload/1" と "file/1" は往復で一致し、"media-payload/2" は Err */ }
#[test] fn detects_by_magic_not_extension() {
    // PNG のシグネチャを持つ中身を "x.jpg" に書く → detect_format_file は Png
}
#[test] fn unknown_format_falls_back_to_file_hash() {
    // 中身が "hello" のファイル → scope File1、fallback_reason は None、
    // hash は blake3::hash(b"hello") と一致する
}
```

- [ ] **Step 2: 実行して失敗を確認する**
  Run: `cargo test -p asteria-framework --test hash_core`
  Expected: コンパイルエラー

- [ ] **Step 3: 実装する**
  `file_hash` は `blake3::Hasher::update_reader` でストリームとして読む。

- [ ] **Step 4: 実行して成功を確認する**
  Run: `cargo test -p asteria-framework --test hash_core`
  Expected: PASS

- [ ] **Step 5: Commit**
  `git commit -m "feat(framework): content hash types, format detection and file/1"`

---

### Task 4: payload ハッシュ（JPEG）とテスト用ファイル

**Files:**
- Create: `asteria-framework/tests/fixtures/generate.sh`、およびそのスクリプトが出力するファイル一式（コミットする）
- Create: `asteria-framework/src/hash/jpeg.rs`
- Create: `asteria-framework/tests/common/mod.rs`（メタデータを差し込むヘルパー）
- Modify: `asteria-framework/src/hash/mod.rs`（Jpeg を振り分けに追加する）
- Test: `asteria-framework/tests/hash_image.rs`

**Interfaces:**
- Consumes: Task 3 の `content_hash`、`MediaFormat`、`Error::MalformedMedia`
- Produces:
  - `pub(crate) fn jpeg_payload_hash(r: &mut impl Read) -> Result<ContentHash>`
  - テスト用ファイル（16×16 の単色画像、長さ 0.2 秒のサイン波）
    - 画像：`red.jpg` `blue.jpg` `red.png` `blue.png` `red.webp` `blue.webp`（WebP はロスレス）
    - 音声：`sine440.flac` `sine880.flac` `sine440.mp3` `sine880.mp3`
  - `tests/common/mod.rs`
    - `jpeg_insert_app1(jpeg: &[u8], payload: &[u8]) -> Vec<u8>`：SOI の直後に APP1 を差し込む
    - `jpeg_insert_com(jpeg: &[u8], text: &str) -> Vec<u8>`
    - Task 5 で PNG / WebP 用のヘルパーを追加する
- `generate.sh` は ffmpeg だけを使う。例：`ffmpeg -f lavfi -i color=red:s=16x16 -frames:v 1 red.jpg`、`ffmpeg -f lavfi -i sine=frequency=440:duration=0.2 sine440.flac`。生成物はバイト単位で再現できなくてよい（コミットしたファイルを正とする）。`-map_metadata -1 -fflags +bitexact` を付けて、エンコーダ情報のタグを入れないようにする。

**JPEG の切り出し規則**（この手順をそのまま実装する）:

```
SOI(FFD8) を確認し、ハッシュに入れる
マーカーを順に読む:
  APP0..APP15(FFE0..FFEF), COM(FFFE): 長さ分を読み飛ばす（ハッシュに入れない）
  その他のセグメント: マーカー2バイト＋長さ＋本体をハッシュに入れる
  SOS(FFDA): セグメントをハッシュに入れ、圧縮データに入る:
     バイトを1つずつハッシュに入れる。0xFF の次のバイトで分岐する:
       00 か D0..D7 → 圧縮データの続き
       D9(EOI) → ハッシュに入れて終了（EOI より後ろのデータは対象外）
       それ以外 → 新しいマーカー。上の規則で処理する（プログレッシブの DHT や SOS など）
途中でファイルが終わった場合 → MalformedMedia
```

- [ ] **Step 1: テスト用ファイルを生成してコミットする**
  Run: `cd asteria-framework/tests/fixtures && sh generate.sh && ls`
  Expected: 10 個のファイルができる

- [ ] **Step 2: 失敗するテストを書く**

```rust
#[test] fn jpeg_hash_ignores_app_and_com_segments() {
    // red.jpg と、jpeg_insert_app1(red, b"Exif\0\0...") と jpeg_insert_com(red, "hi") の3つを
    // それぞれ tempdir に書く → 3つとも content_hash の hash が同じで、scope は MediaPayload1
}
#[test] fn jpeg_hash_differs_for_different_pixels() { /* red.jpg と blue.jpg → hash が違う */ }
#[test] fn jpeg_hash_ignores_trailing_bytes_after_eoi() { /* red.jpg の末尾に 100 バイト足す → hash が同じ */ }
#[test] fn truncated_jpeg_falls_back_without_panic() {
    // red.jpg を半分で切る → scope は File1、fallback_reason は Some
}
```

- [ ] **Step 3: 実行して失敗を確認する**
  Run: `cargo test -p asteria-framework --test hash_image`
  Expected: FAIL（scope が File1 になる）

- [ ] **Step 4: `jpeg_payload_hash` を実装し、`content_hash` の振り分けに追加する**
  `BufReader` で読む。

- [ ] **Step 5: 実行して成功を確認する**
  Run: `cargo test -p asteria-framework --test hash_image`
  Expected: PASS

- [ ] **Step 6: Commit**
  `git commit -m "feat(framework): JPEG payload hash with fixtures"`

---

### Task 5: payload ハッシュ（PNG・WebP）

**Files:**
- Create: `asteria-framework/src/hash/{png.rs,webp.rs}`
- Modify: `asteria-framework/src/hash/mod.rs`、`asteria-framework/tests/common/mod.rs`
- Test: `asteria-framework/tests/hash_image.rs`（テストを追加する）

**Interfaces:**
- Produces:
  - `pub(crate) fn png_payload_hash(r: &mut impl Read) -> Result<ContentHash>`
  - `pub(crate) fn webp_payload_hash(r: &mut impl Read) -> Result<ContentHash>`
  - common に追加するヘルパー
    - `png_insert_chunk(png: &[u8], kind: &[u8; 4], data: &[u8]) -> Vec<u8>`：IHDR の直後に差し込む。CRC も正しく計算する
    - `webp_add_exif(webp: &[u8], exif: &[u8]) -> Vec<u8>`：VP8X がなければ作り（EXIF フラグを立て、キャンバスサイズを書く）、EXIF チャンクを足し、RIFF のサイズを直す

**切り出し規則**:
- PNG：8 バイトのシグネチャを確認する（ハッシュには入れない）。チャンクごとに「タイプ4バイト＋データ」をハッシュに入れる。長さと CRC は入れない。対象のチャンクは IHDR、PLTE、IDAT、IEND、tRNS、acTL、fcTL、fdAT。tRNS と APNG のチャンクは画素の内容そのものなので、spec の「その他の補助チャンクは除外」を具体化した結果として含める。IEND まで読んだら終了する。
- WebP：`RIFF....WEBP` を確認する。チャンクごとに「FourCC＋データ」をハッシュに入れる。対象は `VP8 `、`VP8L`、`ALPH`、`ANIM`、`ANMF`。`VP8X` はメタデータの有無を示すフラグを含むので、対象外にする。ICCP、EXIF、XMP、未知のチャンクも対象外。チャンクのデータ長が奇数のときは、パディングの1バイトを読み飛ばす。
- どちらも、途中でファイルが終わった場合と、シグネチャが合わない場合は MalformedMedia を返す。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn png_hash_ignores_text_and_exif_chunks() {
    // red.png と、png_insert_chunk(red, b"tEXt", b"Comment\0hi") と png_insert_chunk(red, b"eXIf", ..)
    // → hash が同じで、scope は MediaPayload1
}
#[test] fn png_hash_differs_for_different_pixels() { /* red.png と blue.png */ }
#[test] fn webp_hash_ignores_exif_and_vp8x() { /* red.webp と webp_add_exif(red, ..) → hash が同じ */ }
#[test] fn webp_hash_differs_for_different_pixels() { /* red.webp と blue.webp */ }
#[test] fn truncated_png_and_webp_fall_back_without_panic() { /* 半分で切る → File1 で、fallback_reason は Some */ }
```

- [ ] **Step 2: 実行して失敗を確認する**
  Run: `cargo test -p asteria-framework --test hash_image`
  Expected: 新しいテストが FAIL

- [ ] **Step 3: 2つの関数を実装し、振り分けに追加する**

- [ ] **Step 4: 実行して成功を確認する**
  Run: `cargo test -p asteria-framework --test hash_image`
  Expected: PASS

- [ ] **Step 5: Commit**
  `git commit -m "feat(framework): PNG and WebP payload hash"`

---

### Task 6: payload ハッシュ（FLAC・MP3）

**Files:**
- Create: `asteria-framework/src/hash/{flac.rs,mp3.rs,id3.rs}`
- Modify: `asteria-framework/src/hash/mod.rs`
- Test: `asteria-framework/tests/hash_audio.rs`

**Interfaces:**
- Produces:
  - `pub(crate) fn id3v2_total_len(head: &[u8; 10]) -> Option<u64>`：ID3v2 ヘッダならタグ全体の長さを返す。長さは syncsafe 整数で、ヘッダの 10 バイトと、フッタフラグが立っていればさらに 10 バイトを足す
  - `pub(crate) fn flac_payload_hash(f: &mut File) -> Result<ContentHash>`
  - `pub(crate) fn mp3_payload_hash(f: &mut File) -> Result<ContentHash>`：末尾から切り詰めるので、Read ではなく File（Seek）を受け取る

**切り出し規則**:
- FLAC：先頭に ID3v2 があれば飛ばす。`fLaC` を確認する。メタデータブロックは、ヘッダの最上位ビット（最後のブロックかどうか）が立つまで読み飛ばす。STREAMINFO も含めて、メタデータはすべて対象外。残りの音声フレームを、ファイルの末尾まですべてハッシュに入れる。
- MP3
  - 先頭：ID3v2 があれば、その長さ分を飛ばす。
  - 末尾：最後の 128 バイトが `TAG` で始まれば ID3v1 として除く。さらにその手前の 32 バイトが `APETAGEX` フッタなら、フッタに書かれたタグのサイズ（＋ヘッダがあれば 32 バイト）だけ除く。
  - 残りの区間をすべてハッシュに入れる。
  - 残りの区間が 0 バイト以下になったら MalformedMedia を返す。

- [ ] **Step 1: 失敗するテストを書く**（タグ付きの版は lofty で作る）

```rust
#[test] fn flac_hash_ignores_vorbis_comments_and_pictures() {
    // sine440.flac をコピーし、lofty でタイトルと埋め込み画像を書き込む → 元の版と hash が同じ
}
#[test] fn flac_hash_differs_for_different_audio() { /* sine440 と sine880 */ }
#[test] fn mp3_hash_ignores_id3v2_id3v1_and_ape() {
    // lofty で ID3v2 を書いた版、ID3v1 を書いた版、APE を書いた版 → 元の版と hash がすべて同じ
}
#[test] fn mp3_hash_differs_for_different_audio() { /* sine440 と sine880 */ }
#[test] fn audio_hash_scope_is_media_payload() { /* 4つのテスト用ファイルすべてで MediaPayload1 */ }
#[test] fn truncated_flac_header_falls_back() { /* 先頭の 20 バイトだけ → File1 で、fallback_reason は Some */ }
```

- [ ] **Step 2: 実行して失敗を確認する**
  Run: `cargo test -p asteria-framework --test hash_audio`
  Expected: FAIL

- [ ] **Step 3: 実装し、振り分けに追加する**

- [ ] **Step 4: 実行して成功を確認する**
  Run: `cargo test -p asteria-framework --test hash_audio`
  Expected: PASS

- [ ] **Step 5: Commit**
  `git commit -m "feat(framework): FLAC and MP3 payload hash"`

---

### Task 7: XMP サイドカーの読み込み

**Files:**
- Create: `asteria-framework/src/sidecar/{mod.rs,model.rs,read.rs,ns.rs}`
- Create: `asteria-framework/tests/fixtures/xmp/`（`darktable_attr.xmp`、`element_form.xmp`、`xpacket_wrapped.xmp`、`asteria_full.xmp`、`malformed.xmp`）
- Test: `asteria-framework/tests/sidecar_read.rs`

**Interfaces:**
- Consumes: Task 2 の `UserId`、Task 3 の `ContentHash`、`HashScope`
- Produces:
  - `pub struct RatingEntry { pub value: i8, pub modified: Option<DateTime<Utc>> }`：value は -1..=5（-1 は Lightroom の「除外」）
  - `pub struct Sidecar`
    ```rust
    pub struct Sidecar {
        pub rating: Option<RatingEntry>,                       // xmp:Rating ＋ asteria:RatingModified
        pub user_ratings: BTreeMap<UserId, RatingEntry>,        // asteria:UserRatings
        pub subjects: BTreeSet<String>,                        // dc:subject（Bag）
        pub hierarchical_subjects: BTreeSet<String>,           // lr:hierarchicalSubject（Bag、区切りは "|"）
        pub description: Option<String>,                       // dc:description（Alt、x-default）
        pub label: Option<String>,                             // xmp:Label
        pub content_hash: Option<(ContentHash, HashScope)>,    // asteria:ContentHash / HashScope
        pub(crate) doc: Option<XmpDoc>,                        // 元の文書（知らない項目を保持するため）
    }
    ```
  - `pub(crate) struct XmpDoc { root: xml::Element, xpacket: bool }`（xml は sidecar/xml.rs の自前ツリー。xmltree は属性の名前空間を落とすため Task 7 で置き換えた）
  - `impl Sidecar { pub fn new() -> Self; pub fn parse(bytes: &[u8]) -> Result<Sidecar>; pub fn read(path: &Path) -> Result<Option<Sidecar>> }`：`read` はファイルがなければ `Ok(None)` を返す
  - Error に `MalformedSidecar { path: Option<PathBuf>, reason: String }` を追加する
- 読み込みの規則
  - 単純な値（Rating、Label、RatingModified、ContentHash、HashScope）は、`rdf:Description` の属性形式と子要素形式のどちらでも読む。
  - Bag と Alt は子要素形式で読む。
  - 値の形式が不正な場合（Rating が数値でない、範囲外、ハッシュ文字列が不正など）は `MalformedSidecar` を返す。

- [ ] **Step 1: XMP のテスト用ファイルを書く**
  - `darktable_attr.xmp`：`xmp:Rating="3"` と、`darktable:` 名前空間の属性を持つ。
  - `element_form.xmp`：`<xmp:Rating>4</xmp:Rating>` という子要素形式。
  - `xpacket_wrapped.xmp`：`<?xpacket begin=…?>` で囲まれている。
  - `asteria_full.xmp`：spec §3.3 の例と同じ内容。
  - `malformed.xmp`：閉じタグが欠けている。

- [ ] **Step 2: 失敗するテストを書く**

```rust
#[test] fn reads_attribute_form_rating() { /* darktable_attr → rating.value == 3, modified == None */ }
#[test] fn reads_element_form_rating() { /* element_form → 4 */ }
#[test] fn reads_xpacket_wrapped() { /* 読めること。doc.xpacket == true */ }
#[test] fn reads_full_asteria_sidecar() {
    // asteria_full → rating 4（modified は 2026-10-22T09:00:00Z）、user_ratings に 1 件（value 5）、
    // content_hash の scope は MediaPayload1
}
#[test] fn malformed_xml_is_error() { /* Err(MalformedSidecar{..}) */ }
#[test] fn out_of_range_rating_is_error() { /* xmp:Rating="9" → Err(MalformedSidecar{..}) */ }
#[test] fn missing_file_reads_none() { /* Sidecar::read(存在しないパス) → Ok(None) */ }
```

- [ ] **Step 3: 実行して失敗を確認する**
  Run: `cargo test -p asteria-framework --test sidecar_read`
  Expected: コンパイルエラー

- [ ] **Step 4: 実装する**
  xml-rs ベースの自前ツリー（sidecar/xml.rs）で読み込む。xpacket の有無は、パースの前にバイト列の先頭に `<?xpacket` があるかどうかで記録しておく。名前空間は URI で照合し、接頭辞（`xmp:` など）では判断しない。

- [ ] **Step 5: 実行して成功を確認する**
  Run: `cargo test -p asteria-framework --test sidecar_read`
  Expected: PASS

- [ ] **Step 6: Commit**
  `git commit -m "feat(framework): XMP sidecar reader"`

---

### Task 8: XMP サイドカーの書き込みとアトミック書き込み

**Files:**
- Create: `asteria-framework/src/fsutil.rs`、`asteria-framework/src/sidecar/write.rs`
- Modify: `asteria-framework/src/library.rs`（TOML の書き込みを `atomic_write` に置き換える）
- Test: `asteria-framework/tests/{fsutil.rs,sidecar_write.rs}`

**Interfaces:**
- Consumes: Task 7 の `Sidecar`、`XmpDoc`
- Produces:
  - `pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()>`：同じディレクトリの `.<ファイル名>.asteria-tmp` に書き、fsync してから rename する
  - `pub fn tmp_path_for(path: &Path) -> PathBuf`
  - `pub fn cleanup_tmp_files(root: &Path) -> Result<usize>`：再帰的にたどって `.*.asteria-tmp` を削除し、削除した数を返す
  - `impl Sidecar { pub fn to_bytes(&self) -> Vec<u8>; pub fn write(&self, path: &Path) -> Result<()> }`
- 書き込みの規則
  - 元の文書（`doc`）があれば、それを複製し、asteria が管理する項目だけを置き換える。管理する項目は、Rating、RatingModified、UserRatings、subject、hierarchicalSubject、description、Label、ContentHash、HashScope。
  - 管理項目を置き換えるときは、属性形式と要素形式の両方を一度消してから、正規形で書き直す。正規形は、単純な値は属性、Bag と Alt は子要素。
  - 値が None や空の項目は書かない。
  - それ以外の要素、属性、名前空間宣言はすべてそのまま残す。
  - `doc` がない場合は、`x:xmpmeta` / `rdf:RDF` / `rdf:Description` の最小限の骨組みを作る。
  - 元の文書に xpacket があった場合だけ、xpacket で囲んで書く。

- [ ] **Step 1: 失敗するテストを書く**

```rust
// fsutil.rs
#[test] fn atomic_write_replaces_content_and_leaves_no_tmp() {
    // "京都旅行/a.txt" に2回書く → 内容は2回目のもので、tmp ファイルは残っていない
}
#[test] fn cleanup_removes_only_asteria_tmp_files() {
    // ".a.jpg.asteria-tmp" と "a.jpg" を置く → 戻り値は 1 で、a.jpg は残る
}
// sidecar_write.rs
#[test] fn roundtrip_preserves_unknown_namespaces() {
    // darktable_attr を読み、rating を 5 にして書き、読み直す → rating は 5
    // 書き出したテキストに darktable: の属性が残っている
}
#[test] fn element_form_rating_is_rewritten_once() {
    // element_form を読み、rating を 2 にして書く → xmp:Rating がちょうど1回だけ出現する
}
#[test] fn xpacket_wrapper_is_preserved() { /* 書いたあとも "<?xpacket begin" で始まる */ }
#[test] fn new_sidecar_roundtrips_all_fields() {
    // Sidecar::new() のすべての項目に値を入れて書き、読む → 一致する（doc は比べない）
}
```

- [ ] **Step 2: 実行して失敗を確認する**
  Run: `cargo test -p asteria-framework --test fsutil --test sidecar_write`
  Expected: コンパイルエラー

- [ ] **Step 3: 実装する**
  `library.rs` の TOML の書き込みも `atomic_write` に置き換える。

- [ ] **Step 4: 実行して成功を確認する**
  Run: `cargo test -p asteria-framework`
  Expected: PASS（これまでのテストもすべて）

- [ ] **Step 5: Commit**
  `git commit -m "feat(framework): XMP sidecar writer and atomic writes"`

---

### Task 9: ジャーナル

**Files:**
- Create: `asteria-framework/src/journal.rs`
- Test: `asteria-framework/tests/journal.rs`

**Interfaces:**
- Consumes: Task 2 の `Library`、`UserId`
- Produces:
  - `pub struct OpId(pub GrainId)`、`pub struct PlanId(pub GrainId)`：serde は transparent
  - `pub enum Actor { User(UserId), Agent { on_behalf_of: UserId } }`：serde は `{"user": "<id>"}` / `{"agent": "<id>"}` の形
  - `pub enum OpKind { Move, Rename, Trash, WriteEmbeddedTags, RelinkSidecar, WriteSidecar }`：serde は snake_case
  - `pub struct JournalEntry { pub op_id: OpId, pub plan_id: Option<PlanId>, pub actor: Actor, pub at: DateTime<Utc>, pub kind: OpKind, pub before: serde_json::Value, pub after: serde_json::Value }`
  - `pub struct Journal { dir: PathBuf }`
    - `Journal::new(library: &Library) -> Journal`：dir は `.asteria/journal`
    - `append(&self, e: &JournalEntry) -> Result<()>`：`at` の年月から `YYYY-MM.jsonl` を選び、1行を追記して fsync する
    - `read_all(&self) -> Result<Vec<JournalEntry>>`：ファイル名の順、ファイル内は行の順に返す
  - 最終行が途中で切れている場合（クラッシュで追記が途中まで）は、その行だけを読み飛ばし、`tracing::warn!` で警告する。最終行以外が壊れている場合は `Error::MalformedJournal { file, line }` を返す。tracing はワークスペースの依存に追加する。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn append_and_read_back_in_order() { /* 3件を追記 → read_all で同じ順に、内容も一致して返る */ }
#[test] fn entries_split_by_month() {
    // at が 2026-10-31 と 2026-11-01 の2件 → 2026-10.jsonl と 2026-11.jsonl ができる。read_all の順は 10月、11月
}
#[test] fn japanese_paths_survive_roundtrip() { /* before が {"path":"2025/京都旅行/a.jpg"} の記録 → 読み直して一致する */ }
#[test] fn truncated_last_line_is_skipped() { /* 最終行の後半を削る → read_all は Ok で、件数は 1 少ない */ }
#[test] fn corrupt_middle_line_is_error() { /* 途中の行を壊す → Err(MalformedJournal{..}) */ }
```

- [ ] **Step 2: 実行して失敗を確認する**
  Run: `cargo test -p asteria-framework --test journal`
  Expected: コンパイルエラー

- [ ] **Step 3: 実装する**
  追記は `OpenOptions::new().append(true).create(true)` で開き、`sync_data` まで行う。

- [ ] **Step 4: 実行して成功を確認する**
  Run: `cargo test -p asteria-framework --test journal`
  Expected: PASS

- [ ] **Step 5: Commit**
  `git commit -m "feat(framework): append-only JSONL journal"`

---

### Task 10: サイドカーの更新 API（必要時だけ作成、ハッシュの記録、評価）

**Files:**
- Create: `asteria-framework/src/meta.rs`
- Test: `asteria-framework/tests/meta.rs`

**Interfaces:**
- Consumes: Task 2 の `Library`、Task 3 の `content_hash`、Task 7・8 の `Sidecar`、Task 9 の `Journal`、`JournalEntry`、`Actor`
- Produces（`impl Library`）:
  - `pub fn read_sidecar(&self, media_rel: &Path) -> Result<Option<Sidecar>>`
  - `pub fn update_sidecar(&self, media_rel: &Path, actor: Actor, f: impl FnOnce(&mut Sidecar)) -> Result<bool>`
    1. メディアがなければ `Error::MediaNotFound(PathBuf)`。
    2. サイドカーを読む。壊れていれば `MalformedSidecar` をそのまま返し、ファイルには触れない。
    3. サイドカーがなければ `Sidecar::new()` から始める。
    4. `f` を適用する。
    5. 結果が「中身なし」（管理項目がすべて空で、元のファイルもない）なら、何も書かずに `Ok(false)` を返す。
    6. `content_hash` が None なら、`content_hash()` を計算して設定する。
    7. 書き込み、`OpKind::WriteSidecar` としてジャーナルに記録する（before / after は `{"path": …}` と変更のあった項目名）。
    8. `Ok(true)` を返す。
  - `pub fn set_rating(&self, media_rel: &Path, user: UserId, value: i8, at: DateTime<Utc>, actor: Actor) -> Result<()>`：
    - `user == owner()` なら `rating`、それ以外は `user_ratings[user]` に `RatingEntry { value, modified: Some(at) }` を入れる。
    - **後勝ち**：すでにある `modified` が `at` より新しければ、何もしない。
    - value が範囲外なら `Error::InvalidRating(i8)` を返す。
  - Error に `MediaNotFound(PathBuf)` と `InvalidRating(i8)` を追加する

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn no_sidecar_is_created_for_noop_update() {
    // テスト用の red.jpg を Library に置き、update_sidecar(|_| {}) → Ok(false)。.xmp はできない
}
#[test] fn first_rating_creates_sidecar_with_payload_hash() {
    // set_rating(owner, 4) → "red.jpg.xmp" ができる。rating.value == 4
    // content_hash は content_hash(red.jpg) と一致し、scope は MediaPayload1。ジャーナルに WriteSidecar が1件
}
#[test] fn non_owner_rating_goes_to_user_ratings() { /* 別のユーザーで評価 5 → rating は None、user_ratings に 1 件 */ }
#[test] fn older_rating_does_not_overwrite_newer() {
    // at = t2 で 4 を付けたあと、at = t1（t2 より前）で 2 を付ける → 4 のまま
}
#[test] fn malformed_sidecar_is_not_overwritten() {
    // malformed.xmp を red.jpg.xmp としてコピー → set_rating は Err(MalformedSidecar)。ファイルのバイト列は変わらない
}
#[test] fn japanese_media_path_works() { /* "京都旅行/red.jpg" → サイドカーは "京都旅行/red.jpg.xmp" */ }
#[test] fn invalid_rating_rejected() { /* 6 → Err(InvalidRating(6)) */ }
```

- [ ] **Step 2: 実行して失敗を確認する**
  Run: `cargo test -p asteria-framework --test meta`
  Expected: コンパイルエラー

- [ ] **Step 3: 実装する**

- [ ] **Step 4: 全体を検証する**
  Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings && cargo fmt --check && cargo deny check`
  Expected: すべて成功

- [ ] **Step 5: Commit**
  `git commit -m "feat(framework): lazy sidecar updates with payload hash and LWW ratings"`
