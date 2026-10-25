# Plan 4: framework-client と photos のデスクトップクライアント 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `asteria-photos` をデスクトップで起動すると、登録したサーバーの1つにつながり、フォルダをたどってサムネイルの一覧とプレビューを見られ、評価・タグ・説明・色ラベルを編集できる。サーバーは複数登録でき、ステータスバーから1操作で切り替えられる。

**Architecture:** クライアントの共通部分は `asteria-framework-client`（lib、egui に依存し eframe には依存しない）に置き、`asteria-photos`（bin）は画面の構成と photos 固有の表示だけを持つ。接続しているサーバーに結び付く状態は、すべて `Session` 1つにまとめる。サーバーの切り替えは、`Session` を drop して新しく作るだけで行う。通信は UI スレッドの外（ワーカースレッド）で行い、結果はチャネルで受けて `egui::Context::request_repaint` で描き直す。キャッシュはプロファイルではなくライブラリ ID ごとに分ける。

**Tech Stack:** Rust 1.99.0、egui、corundum-framework 0.2（`gui`、`eframe`、`serde`）、reqwest（blocking、rustls）、image（webp のデコードだけ）、serde、toml、grain-id。

**Spec:** `docs/superpowers/specs/2026-10-22-asteria-design.md`（特に §1 要件8、§4.4、§5.1、§5.5、§5.6、§9）

## Plan 2 から引き継ぐ前提

- サーバーの RPC：`server.info`（ハンドシェイク）、`library.info`（`library_id`、`owner`、`epoch`、`item_count`、`scanning`）、`library.rescan`、`folder.list`（`path`、`cursor`、`limit` → `folders`、`items`、`next_cursor`）、`media.get`、`media.update`。
- バイナリ：`GET /media/{id}/original`（Range 対応）、`GET /media/{id}/thumb`（長辺 256px の WebP）、`GET /media/{id}/preview`（長辺 1920px の WebP）。503 には `Retry-After: 2`、422 は処理不可（再試行しない）。
- `MediaItem.file_hash` は API 2.1 から。`null` のあいだはクライアントのキャッシュに入れない。
- エラーコード：`NOT_FOUND`（-32001）、`READ_ONLY`（-32003）、`BUSY`（-32004）。401 は本文なし。
- `ServerInfo` には `library_id` がない。キャッシュを分けるためのライブラリ ID は `library.info` から取るので、API は変えない。

## Global Constraints

- ツールチェインは `1.99.0`、edition 2024。CI は clippy `-D warnings` と `cargo fmt --check` を通す。
- 新しいクレート `asteria-framework-client`（lib、`crates/`）と `asteria-photos`（bin、`photos/`）はどちらも `GPL-3.0-or-later` で、App Store 例外を付ける。`deny.toml` の GPL 例外に2つとも足す。
- `asteria-framework-client` は eframe、vips、ffmpeg に依存しない。corundum は `default-features = false, features = ["gui", "serde"]` で使う。`asteria-photos` だけが `eframe` を有効にする。
- ディレクトリは `corundum_framework::dirs`（`AppDirs`）を通してだけ取る。アプリ名は `asteria-photos`。
- OS 固有の処理は `#[cfg]` のモジュールに分ける。この計画では、デスクトップ専用の処理は `photos/src/desktop.rs` にだけ置く。
- サーバーに結び付く状態（接続、閲覧中の位置、選択、キャッシュの読み書き先、実行中のリクエスト）は、`Session` の外に置かない。`Session` を drop したら、その時点で実行中のリクエストの結果は捨てる（古いサーバーの結果を新しい画面に出さない）。
- トークンはログに出さない。`Debug` の出力では伏せる。
- 文字列はすべて corundum の `tr!` を通し、英語と日本語の Fluent ファイルを用意する。
- UI スレッドでネットワークやディスクの I/O をしない。

## Review Focus

1. **切り替えの取り違え**：サーバー A のリクエストが終わる前に B に切り替えても、A の結果（一覧、サムネイル、編集の結果）が B の画面やキャッシュに入らない。→ Task 3 と Task 8 でテストする
2. **キャッシュの置き場所**：同じライブラリを指す2つのプロファイルはキャッシュを共有し、違うライブラリは混ざらない。キャッシュの鍵は `file_hash` だけで、`null` のものは入れない。→ Task 4 でテストする
3. **トークンの扱い**：`servers.toml` は Unix で 0o600。ログと `Debug` にトークンが出ない。→ Task 2 でテストする
4. **サーバーが応えないとき**：接続できない、401、API の不一致、スキャン中、503 のそれぞれで、画面が固まらずに理由を出す。503 は `Retry-After` に従って再試行し、422 は再試行しない。→ Task 3、Task 5、Task 9 でテストする
5. **大きなフォルダ**：1万件のフォルダでも、スクロールは見えている行だけを描き、サムネイルは見えている分だけを取りに行く。→ Task 6 でテストする

---

### Task 1: クレートの土台と CI

**Files:**
- Create: `crates/asteria-framework-client/{Cargo.toml,src/lib.rs}`、`photos/{Cargo.toml,src/main.rs}`（`license.workspace = true`。App Store 例外はリポジトリ直下の `LICENSE.App-Store-Exception` が全体に掛かる）
- Modify: `Cargo.toml`（members、`workspace.dependencies` に `egui`、`image`、`reqwest` の `rustls-tls`）、`deny.toml`、`.github/workflows/ci.yml`

**Interfaces:**
- `asteria-framework-client` は空の lib から始める。`asteria-photos` は corundum の `window::run` で空の Shell を開くだけ。
- CI：共有ワークフロー（`shared-rust-ci.yml`）の後に、macOS で `rustup target add aarch64-apple-ios && cargo check -p asteria-framework-client --target aarch64-apple-ios` を実行するジョブを足す。

- [ ] **Step 1:** クレートを作り、`cargo build --workspace` が通ることを確かめる
- [ ] **Step 2:** iOS 向けの `cargo check` を手元（macOS）で通す。通らない依存があれば、ここで feature を絞る
- [ ] **Step 3:** CI にジョブを足す
- [ ] **Step 4:** `cargo deny check` を通す
- [ ] **Step 5: Commit** `chore: add asteria-framework-client and asteria-photos crates`

### Task 2: サーバーの登録（プロファイル）

**Files:**
- Create: `crates/asteria-framework-client/src/profiles.rs`
- Test: `crates/asteria-framework-client/tests/profiles.rs`

**Interfaces:**
- `pub struct ProfileId(GrainId)`：プロファイルを作るときに決まり、変わらない
- `pub struct ServerProfile { id: ProfileId, name: String, url: Url, token: Token }`
- `pub struct Token(String)`：`Debug` は `Token(***)`。`Serialize` だけは中身を出す
- `pub struct Profiles { servers: Vec<ServerProfile>, active: Option<ProfileId> }`
  - `load(path) -> Result<Profiles>`：ファイルがなければ空。読めない形なら、ファイルを `servers.toml.bad-<時刻>` に退避して空で始め、警告を出す（トークンは再入力すればよいので、起動できないよりよい）
  - `save(path)`：一時ファイル＋rename。Unix ではモード 0o600
  - `add`、`update`、`remove`（選んでいるものを消したら `active` は `None`）、`set_active`、`active()`
- ファイル：`<config_dir>/servers.toml`

```toml
active = "123abcd"

[[server]]
id = "123abcd"
name = "自宅"
url = "http://192.168.1.10:7810"
token = "…"
```

```rust
#[test] fn roundtrip_and_permissions() { /* save → load で一致。unix ではモード 0o600 */ }
#[test] fn removing_active_clears_it() {}
#[test] fn unreadable_file_is_set_aside() { /* ゴミのバイト列 → 空の Profiles、元のファイルは .bad-* に残る */ }
#[test] fn token_is_not_in_debug() { /* format!("{:?}", profile) にトークンの文字列が含まれない */ }
#[test] fn url_must_be_http_or_https() { /* "ftp://…" や相対 URL は add で拒否 */ }
```

- [ ] **Step 1:** 失敗するテストを書く
- [ ] **Step 2:** 実装する
- [ ] **Step 3: Commit** `feat(framework-client): server profiles in servers.toml`

### Task 3: RPC クライアントとセッション

**Files:**
- Create: `crates/asteria-framework-client/src/{rpc.rs,session.rs,worker.rs}`
- Test: `crates/asteria-framework-client/tests/session.rs`（dev-dependency で `asteria-framework-server` を使い、一時ライブラリでサーバーを立てる。Plan 2 の `tests/common` と同じやり方）

**Interfaces:**
- `pub struct RpcClient`：`reqwest::blocking::Client` を包む。`call<P, R>(method, params) -> Result<R, ClientError>`、`get_bytes(path) -> Result<Bytes, ClientError>`
- `pub enum ClientError { Unreachable, Unauthorized, Handshake(HandshakeError), Rpc { code, message }, Retry { after: Duration }, Unprocessable, NotFound, Io }`
  - 503 は `Retry`（`Retry-After` がなければ 2 秒）、422 は `Unprocessable`、404 は `NotFound`
- `pub struct Session`：1つのプロファイルとの接続
  - `Session::open(profile, ctx: egui::Context) -> Session`：ワーカースレッドを起こし、`server.info` → `check_handshake` → `library.info` の順に行う。状態は `Connecting` → `Ready { library: LibraryInfo }` か `Failed(ClientError)`
  - リクエストは `Session` 経由でだけ出す。結果は `Session::poll()` で UI スレッドが受け取る
  - 各リクエストには `Session` ごとの世代番号を付ける。drop した後に戻った結果は捨てる。ワーカーは `Session` の drop で止める（実行中の HTTP は終わるのを待たず、結果を捨てる）
  - `library.info` の `epoch` が変わったら、閲覧中の一覧を取り直す印を立てる

```rust
#[test] fn handshake_ok_reports_library_id() {}
#[test] fn wrong_token_is_unauthorized() {}
#[test] fn app_mismatch_is_handshake_error() { /* music-api の APP で photos-server につなぐ */ }
#[test] fn unreachable_server_fails_without_hanging() { /* 閉じたポート → 接続のタイムアウト以内に Failed */ }
#[test] fn results_after_drop_are_discarded() { /* 遅いリクエストを出して drop → 新しい Session の poll に出てこない */ }
```

- [ ] **Step 1:** 失敗するテストを書く
- [ ] **Step 2:** 実装する
- [ ] **Step 3: Commit** `feat(framework-client): RPC client and per-server session`

### Task 4: サムネイルの一時キャッシュ

**Files:**
- Create: `crates/asteria-framework-client/src/media_cache.rs`
- Test: `crates/asteria-framework-client/tests/media_cache.rs`

**Interfaces:**
- 場所：`<cache_dir>/<library_id>/temp/<preset>/<hex 先頭2文字>/<hex>.webp`。鍵は `file_hash`（`blake3:` を除いた hex）とプリセット名。`file_hash` が `null` のアイテムは、取りに行っても保存しない
- `pub struct MediaCache { root, limit_bytes }`：`get(hash, preset) -> Option<PathBuf>`、`put(hash, preset, bytes)`（一時ファイル＋rename）、`trim()`（合計が上限を超えたら、更新時刻の古いものから消す。`get` で当たったら更新時刻を進める）
- 上限の既定は 512 MiB。設定画面で変えられるようにするのは Plan 8 で、ここでは定数にする
- `Session` が `library_id` を知った時点で `MediaCache` を開く。`trim` はワーカーで、開いたときと 100 件書くごとに行う

```rust
#[test] fn same_library_shares_cache() { /* プロファイルが2つ、同じ library_id → 片方で put したものがもう片方で get できる */ }
#[test] fn different_libraries_do_not_mix() {}
#[test] fn null_hash_is_not_cached() {}
#[test] fn trim_removes_oldest_first() {}
#[test] fn partial_write_is_not_visible() { /* 一時ファイルだけ残った状態 → get は None */ }
```

- [ ] **Step 1:** 失敗するテストを書く
- [ ] **Step 2:** 実装する
- [ ] **Step 3: Commit** `feat(framework-client): temporary media cache keyed by library and file hash`

### Task 5: 画像の取得とテクスチャ

**Files:**
- Create: `crates/asteria-framework-client/src/images.rs`
- Test: `crates/asteria-framework-client/tests/images.rs`

**Interfaces:**
- `pub struct ImageLoader`：`Session` が持つ。`request(item: &MediaItem, preset) -> ImageState`
  - `ImageState::{Loading, Ready(egui::TextureHandle), Failed(reason)}`
  - キャッシュにあればそれを読み、なければ `/media/{id}/{preset}` を取りに行く。デコード（`image` の webp）もワーカーで行う
  - 同じ（アイテム、プリセット）の重複リクエストはまとめる。同時に取りに行く数は 6 まで
  - 503 は `Retry-After` の後にもう一度（最大 30 回）。422 は `Failed` にして、そのセッションでは再試行しない
  - 使われなくなったテクスチャは、テクスチャの数が上限（thumb 1,000 枚、preview 8 枚）を超えたら古いものから捨てる
  - フレームごとに、そのフレームで描かれなかったアイテムの待ち行列からの取り出しを後回しにする（スクロールで見えなくなったものを先に取りに行かない）

```rust
#[test] fn thumb_is_fetched_once_and_cached() {}
#[test] fn retry_after_is_honoured() { /* テスト用のサーバーが 1回目だけ 503 → 2回目で Ready */ }
#[test] fn unprocessable_is_not_retried() {}
#[test] fn offscreen_requests_are_deprioritised() {}
```

- [ ] **Step 1:** 失敗するテストを書く
- [ ] **Step 2:** 実装する
- [ ] **Step 3: Commit** `feat(framework-client): image loader with retry and texture budget`

### Task 6: サムネイルのグリッド（共通部品）

**Files:**
- Create: `crates/asteria-framework-client/src/widgets/thumb_grid.rs`
- Test: `crates/asteria-framework-client/tests/thumb_grid.rs`（egui の `Context` を描画なしで回すテスト。corundum の `test_support` があれば使う）

**Interfaces:**
- `ThumbGrid::new(items: &[MediaItem]).cell_size(f32).show(ui, &mut ImageLoader) -> ThumbGridResponse { clicked: Option<ItemId>, double_clicked: Option<ItemId> }`
- 列の数は幅から決める。`ScrollArea::show_rows` で見えている行だけを描く
- 動画には印を、評価には星を、読み取り専用には錠前のアイコン（corundum の Lucide）を重ねる
- 選択はキーボードの矢印でも動かせる

```rust
#[test] fn only_visible_rows_request_images() { /* 10,000 件、高さ 600px → request の回数は見えている行の分だけ */ }
#[test] fn columns_follow_width() {}
```

- [ ] **Step 1:** 失敗するテストを書く
- [ ] **Step 2:** 実装する
- [ ] **Step 3: Commit** `feat(framework-client): virtualised thumbnail grid`

### Task 7: 接続設定の画面

**Files:**
- Create: `crates/asteria-framework-client/src/settings/servers.rs`
- Modify: `crates/asteria-framework-client/i18n/{en,ja}/*.ftl`

**Interfaces:**
- corundum の Settings に入れるページ `ServersPage`。一覧、追加、編集、削除、「このサーバーを使う」
- 追加と編集では、保存の前に「接続を確かめる」でハンドシェイクだけを試せる。結果（ライブラリの件数、オーナー、または失敗の理由）を出す
- トークンの入力欄は伏せ字で、表示の切り替えがある。ページは何も保存せず、変更を `ServersChange` として返す（corundum の「ページは保存しない」に合わせる）。保存と `Session` の作り直しはアプリが行う

- [ ] **Step 1:** 画面を作る
- [ ] **Step 2:** 手で確かめる：追加、接続の確認、編集、削除、選んでいるものの削除
- [ ] **Step 3: Commit** `feat(framework-client): server settings page`

### Task 8: photos のアプリと切り替え

**Files:**
- Modify: `photos/src/main.rs`
- Create: `photos/src/{app.rs,browse.rs,desktop.rs}`、`photos/i18n/{en,ja}/*.ftl`
- Test: `photos/tests/switch.rs`

**Interfaces:**
- `PhotosApp`：`Profiles`、`Option<Session>`、corundum の `Shell` を持つ。画面は Library と Settings
- Library 画面：パンくず、サブフォルダの一覧、`ThumbGrid`。`folder.list` はカーソルで続きを読み、スクロールが下に着いたら次のページを取る。スキャン中は「スキャン中（n 件）」を出し、一定間隔で `library.info` を見直す
- ステータスバー（`status_ui`）の左端に、選んでいるサーバーの名前と状態の点（つながっている、つないでいる、失敗）を出す。押すと登録の一覧が出て、選ぶとすぐ切り替わる。狭い画面では同じものを `foot_ui` から出す
- 切り替え：`servers.toml` の `active` を書き、`Session` を drop して新しく開く。閲覧の位置はルートに戻す
- 登録がひとつもないときは、Settings の接続設定を開いて案内する
- 前回選んでいたサーバーには、起動時に自動でつなぐ

```rust
#[test] fn switching_drops_pending_results() { /* サーバー A で遅い folder.list を出した直後に B に切り替える → B の一覧には A のアイテムが出ない */ }
#[test] fn switching_resets_location() {}
#[test] fn active_profile_is_restored_on_start() {}
```

- [ ] **Step 1:** 失敗するテストを書く
- [ ] **Step 2:** 実装する
- [ ] **Step 3:** 手で確かめる：photos-server を2つ（別のライブラリ、別のポート）立てて、切り替えを繰り返す
- [ ] **Step 4: Commit** `feat(photos): browse a library and switch between servers`

### Task 9: 詳細ペインと編集

**Files:**
- Create: `photos/src/detail.rs`
- Test: `photos/tests/detail.rs`

**Interfaces:**
- corundum の詳細ペイン（`side_ui`）に、選んだアイテムの `preview`、ファイル名、サイズ、更新日時、評価（0〜5 の星、クリックで変更）、色ラベル、タグ（`dc:subject` と階層タグ）、説明を出す
- ダブルクリックか Enter で、プレビューを画面いっぱいに出す。左右の矢印で前後のアイテムに移る
- 編集は `media.update` で送り、返ってきた `MediaItem` で一覧を置き換える。送っている間は欄を無効にする
- `read_only_reason` があれば編集欄を出さず、理由を出す。`READ_ONLY` が返ったときも同じ表示にする。`BUSY` は少し待って1回だけ送り直す
- 動画は、この計画では `preview`（静止画）だけを出す。再生は後の計画で扱う

```rust
#[test] fn rating_edit_roundtrips() { /* 星を押す → サーバーのサイドカーに反映され、一覧も更新される */ }
#[test] fn read_only_item_shows_reason_and_no_editors() {}
#[test] fn busy_is_retried_once() {}
```

- [ ] **Step 1:** 失敗するテストを書く
- [ ] **Step 2:** 実装する
- [ ] **Step 3: Commit** `feat(photos): detail pane, full-screen preview and metadata editing`

### Task 10: 仕上げ

- [ ] **Step 1:** README（英語と日本語）に、クライアントの起動とサーバーの登録の手順を足す
- [ ] **Step 2:** 全体を検証する
  Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check && cargo deny check && cargo check -p asteria-framework-client --target aarch64-apple-ios`
  Expected: すべて成功
- [ ] **Step 3:** 手で確かめる：1万件のライブラリでスクロールの重さ、切り替え、オフライン（サーバーを止める）での表示
- [ ] **Step 4: Commit** `docs: running the photos client`

## この計画で扱わないもの

- オフラインキャッシュ、操作キュー、オフラインでの評価の編集（Plan 8）。この計画では、サーバーにつながらないときは理由を出すだけにする
- アップロード（Plan 5）、整理計画の承認画面（Plan 7）
- 動画の再生、タグ検索と保存した検索（仮想アルバム）。サーバーに検索の RPC が要るので、別の計画で扱う
- OS のキーチェーンへのトークンの保存。今はサーバーの設定と同じく、権限を絞ったファイルに置く
