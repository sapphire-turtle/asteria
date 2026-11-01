# Plan 8c: ローカルサーバー Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** サーバーの認証を corundum の API キーに置き換え、desktop の初回起動と設定画面から、手元のフォルダをライブラリにしたローカルサーバーを用意・常駐・管理できるようにする。

**Architecture:** framework-server は `KeyStore` / `KeyVerifier` で認証し、どのキーもライブラリの持ち主として扱う。設定ファイルの既定の場所を決め、ローカルサーバーを作る処理（`init` とキーの発行）と外部ツールの確かめを関数として出す。新しいライブラリ `asteria-framework-desktop`（`crates/`）に、photos と music の desktop が共有する初回起動のページ、設定の「ローカルサーバー」ページ、サービスの定義を置く。gui は設定に外から足すページを受け取れるようにするだけで、サーバーには依存しない。

**Tech Stack:** Rust 1.99.0（edition 2024）、corundum-framework 0.6.0（`keys`、`keys-clap`、`service`、`gui-service`、`gui-keys`）、axum、clap。

**Spec:** `docs/superpowers/specs/2026-10-22-asteria-design.md` §1（スコープ外）、§3.1、§4.1、§4.4、§4.6（MCP の実行者）、§5.8、§9 の 9（Plan 8c）。

## Global Constraints

- ツールチェインは `1.99.0`、edition 2024。CI は clippy `-D warnings`、`cargo fmt --check`、`cargo deny check` を通し、Linux（x86_64、arm64）、macOS、Windows でテストし、mobile を iOS 向けに確かめる。手元では `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings` も通すこと。
- 依存の向き：gui と mobile はサーバー（`asteria-framework-server`、`asteria-<app>-server`、`asteria-framework-desktop`）にも、corundum の `service` / `keys` にも通常の依存をしない。`asteria-framework-desktop` に依存するのは desktop だけ。
- キー：`<data_dir>/<アプリ名>/<ライブラリID>/keys/`（`data_dir_for(app, library)` の下の `keys`）。トークンの頭は `asteria-photos-` / `asteria-music-`（`format!("{app}-")`）。どのキーも `library.toml` の `owner` として扱い、キーの `subject` は使わない。トークンとそのハッシュはログに出さない。
- 設定ファイル：`--config` を省いたら `<config_dir>/<アプリ名>/server.toml`（corundum の `AppDirs::new(app).config()` の下）。中身は `library` と `listen` だけ。以前の版の `[[token]]` は、どのコマンドでも設定を開いたときにキーとして取り込み（名前 `token-1`、`token-2`…、`hash` はそのまま）、`[[token]]` を消して保存し直す。取り込みに失敗したら、設定を書き換えずにエラーで終わる。
- 認証の応答：正しいキーがなければ 401、本文なし、`WWW-Authenticate: Bearer`。`Bearer` の大文字・小文字は問わない。足したキーは再起動なしで効き、外したキーは 30 秒以内（`KeyVerifier` の既定）に効かなくなる。
- CLI：`init`（最初のキー `owner` を作り、トークンだけを標準出力に出す）、`api-key add|list|retire|restore|rotate`（corundum の `KeyCommand` そのまま）、`serve`。`token add` はなくす。
- サービス：corundum-framework-service の `ServiceSpec { app_name: <アプリ名>, description: "Asteria Photos server" / "Asteria Music server", args: vec!["serve"], legacy_labels: vec![], post_install: None }`。
- ローカルサーバーの接続先：`http://127.0.0.1:7810`（photos）/ `http://127.0.0.1:7811`（music）、表示名は翻訳の「このコンピューター」、キーの名前は `desktop`（既にあれば `desktop-2`、`desktop-3`…）。
- 外部ツール：`ASTERIA_VIPSTHUMBNAIL` / `ASTERIA_FFMPEG` があればそのパス、無ければ `PATH` の `vipsthumbnail` / `ffmpeg`（Windows では `.exe` も）。`Processor` と desktop の確かめは同じ関数を使う。
- 翻訳は en と ja。新しい文字列の id は `asteria-desktop-*`（共有の crate）、`asteria-photos-*` / `asteria-music-*`（アプリの crate）。

## Review Focus

1. **以前の版の設定を開いたときにトークンが失われる**：`[[token]]` を持つ設定で `serve` すると、同じトークンで認証が通り、設定から `[[token]]` が消え、`keys/` に同じ数のキーができる。取り込みの途中で失敗したら設定は書き換わらない。→ Task 1 でテストする
2. **キーを足したのに再起動まで効かない／外したのに効き続ける**：`api-key add` の直後のトークンで 200、`api-key retire` の後は（キャッシュの時間を短くしたテストで）401。→ Task 1、Task 2
3. **初回起動の流れがほかのサーバーの利用者を邪魔する**：接続先が1つでもあれば初回起動の流れは出ない。「ほかのサーバー」「あとで」を選ぶと、今と同じくサーバーの追加の画面になる。→ Task 4
4. **既にライブラリのフォルダや設定ファイルがある**：既存のライブラリはそのまま使い（ID が変わらない）、既存の設定ファイルは上書きしない（キーを1つ足すだけ）。→ Task 2
5. **外部ツールが無い**：無くても初回起動の流れと設定ページは進み、入れ方の案内が出る。`ASTERIA_*` で別の場所を指していればそれを使う。→ Task 2、Task 3

---

### Task 1: API キーでの認証と、設定ファイルの既定の場所

**Files:**
- Modify: `crates/asteria-framework-server/Cargo.toml`（`corundum-framework` に `keys`、`keys-clap`）
- Modify: `crates/asteria-framework-server/src/{auth.rs,config.rs,state.rs,app.rs,cli.rs,lib.rs}`
- Modify: framework-server のテストの共通部分（`tests/common/mod.rs`）と、トークンを作っているテスト（`tests/{rpc,mcp,media_http,organize,upload_http,app_ext,config,no_token_logs}.rs`）、`crates/asteria-framework-client/tests/{session,upload}.rs`、`photos/crates/asteria-photos-gui/tests/support/server.rs`
- Test: `crates/asteria-framework-server/tests/keys.rs`（新規）

**Interfaces:**
- Produces（`config.rs`）:
  - `ServerConfig { library: PathBuf, listen: SocketAddr }`（`token` をなくす）。読むときは、以前の形の `[[token]]` を別に読める型で受ける（`pub(crate) struct LegacyToken { user: UserId, hash: String }`）。
  - `pub fn default_config_path(app: &str) -> Result<PathBuf>`：`AppDirs::new(app)?.config().join("server.toml")`。
  - `pub fn keys_dir_for(app: &str, library: &LibraryId) -> Result<PathBuf>`：`data_dir_for(app, library)?.join("keys")`。
  - `pub fn open_keys(app: &str, library: &LibraryId) -> Result<KeyStore>`：`KeyStore::open(keys_dir)?.token_prefix(&format!("{app}-"))`。
  - `pub fn load_and_migrate(app: &str, path: &Path) -> Result<ServerConfig>`：設定を読み、`[[token]]` があれば、ライブラリを開いて ID を得て、キーとして `import`（名前 `token-<n>`、`created_at` は今、`hash` は文字列から `KeyHash` に）してから、`[[token]]` の無い設定を保存し直す。どのコマンドも設定はこれで開く。
- Produces（`auth.rs`）: `Tokens` / `new_token` / `token_hash` / `is_valid_hash` はなくす（取り込みのためのハッシュの確かめは corundum の `KeyHash` の `FromStr` で行う）。
- Produces（`state.rs`、`app.rs`）: `AppState::new` は `Arc<Tokens>` の代わりに `Arc<KeyVerifier>` を受ける。認証の層は、`Authorization` の Bearer を `KeyVerifier::verify` で（`spawn_blocking` で）確かめ、`Accepted` なら持ち主の `UserId`（`library.owner()`）をリクエストに入れる。`Refused` か Bearer がなければ 401（本文なし、`WWW-Authenticate: Bearer`）。トークンもヘッダーもログに出さない。
- Produces（`cli.rs`）: `--config` を省けるようにする（既定は `default_config_path(spec.app)`）。`init` は `[[token]]` を書かず、キー `owner` を作ってトークンを標準出力に出す。`token add` をなくし、`api-key` サブコマンド（`KeyCommand` を `clap` の `Subcommand` として載せ、`open_keys` のストアで `run`）を足す。
- テスト用：`tests/common` の `env()` は、`open_keys` のストアにキーを足して、そのトークンを `owner_token` に入れる（今の名前のまま使えるように）。`KeyVerifier` のキャッシュ時間を短くできるよう、`AppState` を作る関数にそれを渡せる形にする（`#[doc(hidden)] pub`）。

- [ ] **Step 1: 失敗するテストを書く**

`tests/keys.rs`：
- `a_new_key_works_without_a_restart`：サーバーを起動したまま `open_keys` で新しいキーを足すと、そのトークンで `server.info` が 200。
- `a_retired_key_stops_working`：キャッシュ時間を 0 にした `AppState` で、キーを `retire` した後の同じトークンは 401。
- `refused_has_no_body_and_names_bearer`：間違ったトークンで 401、本文が空、`WWW-Authenticate: Bearer`。
- `every_key_acts_as_the_owner`：別のキーで `media.update`（評価）すると、評価もジャーナルの実行者も持ち主の ID。
- `old_tokens_are_imported_and_dropped_from_the_config`：`[[token]]` を2つ持つ設定（今の形、ハッシュは `blake3:` の 16進）で `load_and_migrate` すると、`keys/` にキーが2つ（`token-1`、`token-2`）、設定には `[[token]]` が無く、元のトークンで認証が通る。
- `a_failed_import_leaves_the_config_alone`：キーのディレクトリが作れない状態（`keys` の場所に同名のファイルを置く）では `load_and_migrate` がエラーで、設定ファイルの中身が元のまま。
- `the_config_defaults_to_the_apps_config_dir`：`default_config_path("asteria-photos")` が `AppDirs::new("asteria-photos").config().join("server.toml")`。

- [ ] **Step 2: 失敗を確かめる**

Run: `cargo test -p asteria-framework-server --test keys`
Expected: FAIL（コンパイルエラー）

- [ ] **Step 3: 実装し、既存のテストをキーに合わせる**

- [ ] **Step 4: 確かめる**

Run:
```bash
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings
cargo fmt --all -- --check
cargo deny check
```
Expected: すべて通る

- [ ] **Step 5: コミット**

```bash
git commit -am "feat(server): API keys from corundum, every key acts as the owner; the config has a default place"
```

---

### Task 2: ローカルサーバーを作る関数と、外部ツールの確かめ、CLI の仕上げ

**Files:**
- Modify: `crates/asteria-framework-server/src/{cli.rs,process.rs,lib.rs}`、新しく `crates/asteria-framework-server/src/local.rs`
- Modify: `photos/headless/tests/{cli.rs,e2e.rs}`、`music/headless/tests/{cli.rs,e2e.rs}`、`photos/desktop/tests/cli.rs`、`music/desktop/tests/cli.rs`
- Modify: `README.md`、`README.ja.md`（`token add` → `api-key add`、`--config` を省けること）

**Interfaces:**
- Consumes: `default_config_path`、`open_keys`（Task 1）
- Produces（`local.rs`）:
  - `pub struct LocalServer { pub config: PathBuf, pub listen: SocketAddr, pub token: String }`
  - `pub fn setup_local(spec: &AppSpec, library: &Path, owner: &str, key_name: &str) -> Result<LocalServer>`：既定の場所に設定が無ければ、`library` を `Library::init`（既にライブラリなら開く）して設定を書く（`listen` は `spec.default_listen`）。設定が既にあれば書き換えずに読む（そのときの `library` が引数と違えば、エラー「a local server for another library is already set up at <config>」）。どちらでも、`key_name`（使われていれば `<key_name>-2`、`-3`…）のキーを足してトークンを返す。`init` はこの関数の上に組み直す（`init --force` は今どおり設定を書き直す）。
- Produces（`process.rs`）:
  - `pub enum Tool { Vipsthumbnail, Ffmpeg }`、`pub fn tool_program(tool: Tool) -> OsString`（今の `Processor::new` の中の決め方をここに出す。`Processor` もこれを使う）。
  - `pub fn find_tool(tool: Tool) -> Option<PathBuf>`：`tool_program` が区切り文字を含めばそのパスが実在するか、含まなければ `PATH` の各ディレクトリで探す（Windows は `PATHEXT` の拡張子、少なくとも `.exe` を試す）。起動はしない。
- CLI：README の手順を `asteria-photos init --library ~/Pictures --owner me`（`--config` を省いた形）、`asteria-photos api-key add laptop`、`asteria-photos serve` に直す（`--config` を付ければ別の場所も使えることを1行で）。

- [ ] **Step 1: 失敗するテストを書く**

`local.rs` の単体テスト（一時ディレクトリに `HOME`、`XDG_*`、`APPDATA`、`LOCALAPPDATA` を向けられないので、`setup_local` は設定のパスを受け取る内側の関数 `setup_local_at(spec, config: &Path, library, owner, key_name)` を作って、それを試す）：
- `a_new_library_gets_a_config_and_a_key`：空のフォルダで、設定ができ、`library.toml` ができ、返ったトークンで `KeyVerifier` が `Accepted`。
- `an_existing_library_keeps_its_id`：既に `init` したフォルダで、ライブラリ ID が変わらない。
- `an_existing_config_is_not_overwritten`：設定が既にあれば中身が同じまま、キーが1つ増える。2回目の名前は `desktop-2`。
- `another_librarys_config_is_refused`：別のフォルダを指す設定があればエラーで、何も書かない。

`process.rs` の単体テスト：
- `an_env_path_that_exists_is_found` / `an_env_path_that_does_not_is_not`：`tool_program` が返すパスの扱い（環境変数を変えるテストは、変えずに済むよう `find_program(program: &OsStr, path_var: Option<&OsStr>)` を内側に作り、それを試す）。
- `a_program_on_the_path_is_found`：一時ディレクトリに実行できるファイル（unix は 0o755）を置き、その場所だけの `PATH` で見つかる。無い名前は `None`。

CLI（headless と desktop の `tests/cli.rs`）：
- `init` を `--config` なしで動かすと、`HOME`（と `XDG_CONFIG_HOME`）の下の `asteria-<app>/server.toml` ができる（unix だけ。Windows は corundum の `dirs` が環境変数を見ないため `#[cfg(unix)]`）。
- `api-key add laptop --config <c>` がトークンだけを標準出力に出し、`api-key list --config <c>` に `owner` と `laptop` が出る。

- [ ] **Step 2: 失敗を確かめる**

Run: `cargo test -p asteria-framework-server local process -- --nocapture` と `cargo test -p asteria-photos-headless -p asteria-photos-desktop`
Expected: FAIL

- [ ] **Step 3: 実装する**

- [ ] **Step 4: 確かめる**

Run: Task 1 の Step 4 と同じ
Expected: すべて通る

- [ ] **Step 5: コミット**

```bash
git commit -am "feat(server): set up a local server, find the external tools, api-key on the command line"
```

---

### Task 3: desktop が共有する部品（`asteria-framework-desktop`）

**Files:**
- Create: `crates/asteria-framework-desktop/{Cargo.toml,src/lib.rs,src/service.rs,src/first_run.rs,src/local_page.rs,src/tools.rs,src/i18n.rs,i18n/en/desktop.ftl,i18n/ja/desktop.ftl}`
- Modify: ルートの `Cargo.toml`（`members`）、`deny.toml`（`asteria-framework-desktop` の GPL）
- Test: `crates/asteria-framework-desktop/tests/{i18n.rs,local_page.rs,first_run.rs}`

**Interfaces:**
- Consumes: `setup_local`、`find_tool`、`open_keys`、`default_config_path`、`ServerConfig`（Task 1、2）、`asteria_framework_client::profiles::{Profiles, Token}`
- Produces:
  - `pub struct LocalServerSpec { pub app: &'static str, pub server: fn() -> AppSpec, pub service: fn() -> ServiceSpec, pub url: &'static str }`（各 desktop が `static` で1つ持つ。`url` は Global Constraints の値）
  - `pub fn service_spec(app: &'static str, description: &str) -> ServiceSpec`
  - `pub fn first_run_pages(spec: &'static LocalServerSpec, servers: PathBuf) -> Vec<Box<dyn FirstRunPage>>`：Global Constraints と spec §5.8 の順の4ページ（ライブラリの場所 → ローカルサーバーを作る → corundum の `ServicePage` → 外部ツール）。「ほかのサーバー」「あとで」を選んだら、続くページを飛ばして終わる。ローカルサーバーを作るページは、フォルダを選ぶ（corundum のフォルダ選び）→ `setup_local(..., owner = OS のユーザー名（無ければ "me"）, key_name = "desktop")` を別スレッドで → `servers`（`servers.toml`）に「このコンピューター」と `url` とトークンを足して選ぶ。失敗したら理由を出し、やり直すか戻れる。
  - `pub struct LocalServerPage`：設定のページ。ライブラリの場所と待ち受けアドレス（既定の設定ファイルを読む。無ければ「まだ用意していない」と、初回起動の流れを開き直すボタン）、`ServiceRow`、`KeysPage`（`LocalKeys`）、外部ツールの有無と入れ方の案内。`pub fn new(spec: &'static LocalServerSpec) -> Self`、`pub fn ui(&mut self, ui: &mut egui::Ui)`。
  - `pub fn tools_ui(ui: &mut egui::Ui)`：`find_tool` の結果と、無いときの案内（Linux はパッケージ名 `libvips-tools` / `ffmpeg`、macOS は Homebrew の `vips` / `ffmpeg`、Windows は配布元への案内文）。初回起動のページと設定ページで共有。
  - 翻訳：`pub const RESOURCES: Resources`（`asteria-desktop-*`）と `pub fn register(ctx)`。
- Cargo.toml：`asteria-framework-server`、`asteria-framework-client`、`corundum-framework`（`gui`、`serde`、`eframe`、`gui-service`、`gui-keys`）。

- [ ] **Step 1: 失敗するテストを書く**

- `tests/i18n.rs`：en と ja が同じキー、`src` の id がすべてある（gui の `tests/i18n.rs` と同じ形、接頭辞 `asteria-desktop-`）。
- `tests/first_run.rs`：`first_run_pages` が4ページを返す。ライブラリの場所のページで「ほかのサーバー」を選ぶと、続くページを飛ばして終わる（corundum の `FirstRun` を `egui::Context` で1フレームずつ回して確かめる。描画の出力の `textures_delta` は消す）。
- `tests/local_page.rs`：設定ファイルが無いとき、`LocalServerPage` が「まだ用意していない」を出す（1フレーム回して、`ui` の中のテキストを `egui::Context` の出力から確かめる。確かめにくければ、ページの状態を返す `pub fn state(&self) -> PageState`（`NotSetUp`、`Ready { library, listen }`）を作り、それを確かめる）。

- [ ] **Step 2: 失敗を確かめる**

Run: `cargo test -p asteria-framework-desktop`
Expected: FAIL（パッケージがない）

- [ ] **Step 3: 実装する**

- [ ] **Step 4: 確かめる**

Run: Task 1 の Step 4 と同じ。加えて `cargo tree -p asteria-photos-gui -e normal`、`-p asteria-photos-mobile` などに `asteria-framework-desktop`、`corundum-framework-service`、`corundum-framework-keys` が出ないこと。
Expected: すべて通る

- [ ] **Step 5: コミット**

```bash
git commit -am "feat(desktop): the first run and the local server page, shared by the desktop apps"
```

---

### Task 4: photos と music の desktop に組み込む

**Files:**
- Modify: `crates/asteria-framework-client/src/settings.rs`（外から足す設定ページの口）
- Modify: `photos/crates/asteria-photos-gui/src/app.rs`、`music/crates/asteria-music-gui/src/app.rs`（その口を受け取り、設定の一覧に並べる）
- Modify: `photos/desktop/{Cargo.toml,src/main.rs}`、`music/desktop/{Cargo.toml,src/main.rs}`
- Modify: `README.md`、`README.ja.md`（desktop の初回起動とローカルサーバーの節）、spec §9（8c を済みにする）
- Test: `photos/crates/asteria-photos-gui/tests/settings.rs`（新規）、`photos/desktop/src/main.rs` の単体テスト

**Interfaces:**
- Consumes: `LocalServerSpec`、`first_run_pages`、`LocalServerPage`、`service_spec`、`asteria_framework_desktop::register`（Task 3）
- Produces（framework-client）: `pub trait ExtraSettingsPage { fn label(&self, ctx: &egui::Context) -> String; fn ui(&mut self, ui: &mut egui::Ui); }`
- Produces（gui）: `PhotosApp::with_extra_settings(self, pages: Vec<Box<dyn ExtraSettingsPage>>) -> Self`（music も同じ）。設定の一覧で、サーバーの次に並ぶ。
- Produces（desktop）:
  - `static LOCAL: LocalServerSpec`（photos：`app = "asteria-photos"`、`server = asteria_photos_server::spec`、`service = || service_spec("asteria-photos", "Asteria Photos server")`、`url = "http://127.0.0.1:7810"`。music も同じ形）。`service` は `fn` ポインタなので、名前のある関数にする。
  - ウィンドウの `eframe::App`（photos の `DesktopApp`、music も同じ形の型を作る）は、`servers.toml` に接続先が無く、corundum の `FirstRun::done(ctx)` が `false` の間は初回起動の流れを描き、終わったら `PhotosApp::new` / `MusicApp::new` を作って（そのとき `servers.toml` を読み直す）、`with_extra_settings(vec![Box::new(LocalServerPage::new(&LOCAL))])` を付けて描く。接続先が既にあれば初回起動の流れは出さない。
  - `asteria_framework_desktop::register(ctx)` を、ウィンドウを作るときに呼ぶ。

- [ ] **Step 1: 失敗するテストを書く**

- `photos/crates/asteria-photos-gui/tests/settings.rs`：`ExtraSettingsPage` のテスト用の実装を `with_extra_settings` で渡すと、設定の画面を開いた1フレームで、その `label` が一覧に出て、選ぶと `ui` が呼ばれる（今の `tests/support` の `frame` で回す）。
- `photos/desktop/src/main.rs`：`first_run_needed(servers: &Profiles, done: bool) -> bool` を出し、接続先が無く `done == false` のときだけ `true`。接続先が1つでもあれば `false`。

- [ ] **Step 2: 失敗を確かめる**

Run: `cargo test -p asteria-photos-gui --test settings` と `cargo test -p asteria-photos-desktop`
Expected: FAIL

- [ ] **Step 3: 実装する**

README の desktop の節に、初回起動でローカルサーバーを用意できること、設定の「ローカルサーバー」で常駐と API キーを扱えること、外部ツールの入れ方を書く（英語と日本語）。

- [ ] **Step 4: 確かめる**

Run: Task 1 の Step 4 と同じ。加えて Task 3 の `cargo tree` の確かめ。
Expected: すべて通る

- [ ] **Step 5: コミット**

```bash
git commit -am "feat(desktop): photos and music offer a local server on first run and in Settings"
```
