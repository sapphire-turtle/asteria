# Plan 8b: クレートの分割と配置 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** photos と music を、サーバーのライブラリ（`-server`）、headless、画面のライブラリ（`-gui`）、desktop、mobile（骨組み）に分け、`photos/` と `music/` の下にまとめる。動きは変えない。

**Architecture:** 今のバイナリ `asteria-<app>-server` の中身をライブラリにし、`pub fn spec() -> AppSpec` を出す。headless はそれを `cli::main` に渡すだけのバイナリ。今のクライアント（`asteria-photos` / `asteria-music`）はライブラリ `asteria-<app>-gui` にし、ウィンドウを開く処理（`desktop.rs`）とドラッグ＆ドロップの受け取りは desktop に移す。desktop は、引数がなければウィンドウを開き、あればサーバーの CLI を動かす。mobile は gui にだけ依存する空のライブラリで、CI の iOS チェックの対象にする。

**Tech Stack:** Rust 1.99.0（edition 2024）、Cargo ワークスペース、corundum-framework 0.5.0。

**Spec:** `docs/superpowers/specs/2026-10-22-asteria-design.md` §2（構成、配布形態）、§5.5、§9 の 9（Plan 8b）。

## Global Constraints

- ツールチェインは `1.99.0`、edition 2024。CI は clippy `-D warnings`、`cargo fmt --check`、`cargo deny check` を通し、Linux（x86_64、arm64）、macOS、Windows でテストする。手元では `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings` も通すこと。
- 配置とパッケージ名（spec §2 のとおり）

| ディレクトリ | パッケージ | 種類 | バイナリ名 |
|---|---|---|---|
| `photos/crates/asteria-photos-api` | `asteria-photos-api` | lib | |
| `photos/crates/asteria-photos-server` | `asteria-photos-server` | lib | |
| `photos/crates/asteria-photos-gui` | `asteria-photos-gui` | lib | |
| `photos/headless` | `asteria-photos-headless` | bin | `asteria-photos` |
| `photos/desktop` | `asteria-photos-desktop` | bin | `asteria-photos-desktop` |
| `photos/mobile` | `asteria-photos-mobile` | lib（`crate-type = ["staticlib", "rlib"]`） | |
| `music/…` | 同じ形（`asteria-music-*`） | | `asteria-music` / `asteria-music-desktop` |

- framework（`crates/asteria-framework*`）は動かさない。アプリから framework へはパスで依存する。
- 依存の向き：headless → server。desktop → server、gui。mobile → gui。gui はサーバー（`asteria-<app>-server`、`asteria-framework-server`）に通常の依存をしない（dev-dependency はよい）。server は gui に依存しない。
- 動きを変えない：サーバーの CLI（`init`、`token add`、`serve`）、待ち受けの既定（photos `127.0.0.1:7810`、music `127.0.0.1:7811`）、画面、設定とデータの置き場所（app 名 `asteria-photos` / `asteria-music`）。
- `*-api` と各クレートのバージョンは上げない。ライセンスは今のまま（`*-api` は MIT OR Apache-2.0、ほかは GPL-3.0-or-later）。`deny.toml` の GPL を許すクレートの一覧を新しい名前に直す。
- ファイルは `git mv` で動かし、履歴を残す。

## Review Focus

1. **desktop が引数で振り分けを誤る**：引数なしならウィンドウ、`serve` / `init` / `token` / `--help` / `--version` ならサーバーの CLI。`--help` でウィンドウが開いてはいけない。→ Task 2 でテストする
2. **e2e テストが別のバイナリを起動する**：headless の e2e は `CARGO_BIN_EXE_asteria-photos` / `CARGO_BIN_EXE_asteria-music` を使い、desktop の実行ファイルと取り違えない（cargo のバイナリ名が違うこと）。→ Task 1、Task 2
3. **gui にサーバーの依存が入る**：`cargo tree -p asteria-photos-gui -e normal` と `-p asteria-music-gui` に `asteria-framework-server`、`axum`、`redb` が出ない。mobile の iOS チェックで守る。→ Task 1、Task 3 で確かめる
4. **ドラッグ＆ドロップが効かなくなる**：photos の desktop でファイルを落とすとアップロードに入り、落とす前の覆いが出る。→ Task 2 でテストする
5. **README の手順が古いまま**：コマンドは `asteria-photos init|token add|serve`、開発中は `cargo run -p asteria-photos-desktop`。→ Task 1、Task 2

---

### Task 1: サーバーのライブラリと headless、api の移動

**Files:**
- Move: `crates/asteria-photos-api` → `photos/crates/asteria-photos-api`、`crates/asteria-music-api` → `music/crates/asteria-music-api`
- Move: `photos-server/src/main.rs` → `photos/crates/asteria-photos-server/src/lib.rs`、`photos-server/Cargo.toml` → `photos/crates/asteria-photos-server/Cargo.toml`
- Move: `music-server/src/*` → `music/crates/asteria-music-server/src/`（`main.rs` → `lib.rs`）、`music-server/Cargo.toml` → 同じ場所
- Create: `photos/headless/{Cargo.toml,src/main.rs}`、`music/headless/{Cargo.toml,src/main.rs}`
- Move: `photos-server/tests/e2e.rs` → `photos/headless/tests/e2e.rs`、`music-server/tests/e2e.rs` → `music/headless/tests/e2e.rs`（テストが使うフィクスチャやヘルパーも一緒に）
- Modify: ルートの `Cargo.toml`（`members`）、各クレートのパス依存、`deny.toml`、`README.md`、`README.ja.md`
- 今の `photos/`（クライアント）と `music/`（クライアント）は Task 2 で動かす。このタスクでは、`photos/crates/`、`photos/headless/` を今の `photos/` の中に並べて作ることになる。ワークスペースの `members` に正しく並べればよい（`photos/Cargo.toml` のパッケージと `photos/crates/*` は別のパッケージ）。

**Interfaces:**
- Produces（`asteria-photos-server`）: `pub fn spec() -> asteria_framework_server::cli::AppSpec`（今の `main` が組み立てている値そのまま。`PhotosExtension` もこのクレートの中）。
- Produces（`asteria-music-server`）: `pub fn spec() -> AppSpec`。今の `mod cover; mod ext; …` はライブラリのモジュールにする（公開する必要があるのは `spec` だけ。テストのために `pub` にしているものがあれば、そのまま）。
- Produces（headless）: `fn main() { asteria_framework_server::cli::main(asteria_<app>_server::spec()) }`。`[[bin]] name = "asteria-<app>"`。

- [ ] **Step 1: 失敗するテストを書く**

`photos/headless/tests/e2e.rs` と `music/headless/tests/e2e.rs` の `const BIN` を `env!("CARGO_BIN_EXE_asteria-photos")` / `env!("CARGO_BIN_EXE_asteria-music")` に変える。あわせて各 headless に `tests/cli.rs` を足す：
- `help_lists_the_server_commands`：`<BIN> --help` の終了コードが 0、標準出力に `init`、`token`、`serve` を含む。

- [ ] **Step 2: 失敗を確かめる**

Run: `cargo test -p asteria-photos-headless -p asteria-music-headless`
Expected: FAIL（パッケージがまだない）

- [ ] **Step 3: 移動と分割をする**

`git mv` で動かし、`Cargo.toml` の `name` と `[lib]` / `[[bin]]`、パス依存（`../crates/…` → `../../../crates/…` など）を直す。サーバーのライブラリの dev-dependencies のうち、バイナリを起動する e2e にだけ要るもの（`reqwest`、`rmcp` のクライアントなど）は headless に移す。`deny.toml` の `asteria-photos-server` / `asteria-music-server` はそのまま（名前が変わらない）、`asteria-photos-headless`、`asteria-music-headless` を足す。README のサーバーのコマンドを `asteria-photos-server …` から `asteria-photos …` に直し、インストールは `cargo install --path photos/headless` と書く（英語と日本語）。

- [ ] **Step 4: 確かめる**

Run:
```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings
cargo fmt --all -- --check
cargo deny check
```
Expected: すべて通る。`cargo build --workspace` に「output filename collision」の警告が出ない。

- [ ] **Step 5: コミット**

```bash
git commit -am "refactor: the servers become libraries, run by asteria-photos and asteria-music (headless)"
```

---

### Task 2: 画面のライブラリと desktop

**Files:**
- Move: `photos/{src,tests,assets,i18n…}` のクライアント → `photos/crates/asteria-photos-gui/`、`photos/Cargo.toml` → `photos/crates/asteria-photos-gui/Cargo.toml`（`[[bin]]` と `src/main.rs` は消す）
- Move: `music/` のクライアントも同じく `music/crates/asteria-music-gui/`
- Move: `photos/src/desktop.rs` → `photos/desktop/src/main.rs`（とその周り）、`music/src/desktop.rs` → `music/desktop/src/main.rs`
- Create: `photos/desktop/Cargo.toml`、`music/desktop/Cargo.toml`
- Modify: `photos/crates/asteria-photos-gui/src/{lib.rs,app.rs}`（`desktop` モジュールを外し、ドロップの受け取りを `PhotosApp::show` から出す）
- Modify: ルートの `Cargo.toml`、`deny.toml`（`asteria-photos` → `asteria-photos-gui`、`asteria-photos-desktop` を足す。music も同じ）、`README.md`、`README.ja.md`
- Test: `photos/desktop/tests/cli.rs`、`music/desktop/tests/cli.rs`、`photos/desktop/src/main.rs` の単体テスト

**Interfaces:**
- Consumes: `asteria_<app>_server::spec()`（Task 1）
- Produces（gui）:
  - photos：`PhotosApp`、`AppPaths`、`PhotosApp::new`、`PhotosApp::show`、`PhotosApp::drop_files`（今どおり `pub`）。`show` はドロップを自分で受け取らない（`#[cfg(not(any(target_os = "ios", target_os = "android")))]` のブロックを外す）。
  - music：`MusicApp`、`AppPaths` など今の公開のまま。
  - 共通：gui の `i18n`、画面のモジュールはそのまま。`APP_NAME` は gui に置き、desktop と mobile が使う（`pub const APP_NAME: &str = "asteria-photos";`）。
- Produces（desktop、`main.rs`）:
  - 今の `desktop.rs` の `paths`、`init_logging`、`run` をここに置く。
  - `fn main()`：`std::env::args_os().nth(1)` が `None` ならウィンドウ（`run()`）、そうでなければ `asteria_framework_server::cli::main(asteria_<app>_server::spec())`。
  - photos だけ：`take_drops`、`drop_overlay` をここに置き、ウィンドウの `eframe::App` を `struct DesktopApp(PhotosApp)` にする。`ui` は、落とされたファイルがあれば `self.0.drop_files(..)`、続けて `self.0.show(ui)`、最後に `drop_overlay`。
  - `[[bin]] name = "asteria-<app>-desktop"`。Windows ではコンソールを出さない設定（今の `main.rs` に `windows_subsystem` があれば、それをそのまま移す。無ければ足さない）。
- Produces（headless との関係）：desktop と headless は同じ `spec()` を使うので、CLI は同じ。

- [ ] **Step 1: 失敗するテストを書く**

`photos/desktop/tests/cli.rs`（music も同じ）：
- `help_runs_the_server_cli`：`env!("CARGO_BIN_EXE_asteria-photos-desktop")` を `--help` で起動すると、終了コードが 0 で、標準出力に `init`、`token`、`serve` を含む（ウィンドウは開かない。テストは 10 秒で切る）。
- `init_and_token_add_work_from_the_desktop_binary`：一時ディレクトリのライブラリで `init --library <dir> --owner me --config <dir>/c.toml` と `token add --config … --user <id>` が headless と同じく成功する（headless の e2e の手順を流用してよい）。

photos の desktop の単体テスト（`main.rs` の `#[cfg(test)]`）：
- `dropped_files_reach_the_app`：`egui::Context` に `RawInput { dropped_files: vec![DroppedFile { path: Some(p), .. }], .. }` を流して 1 フレーム回すと、`DesktopApp` が持つ `PhotosApp` のアップロードの一覧に `p` が入る（gui に一覧を読む公開の手段がなければ、今の `photos/tests/uploads.rs` が使っている手段を使う。無ければ `take_drops` が `p` を返すことだけを確かめる）。

- [ ] **Step 2: 失敗を確かめる**

Run: `cargo test -p asteria-photos-desktop -p asteria-music-desktop`
Expected: FAIL（パッケージがまだない）

- [ ] **Step 3: 移動と分割をする**

`git mv` で今の `photos/` の中身を `photos/crates/asteria-photos-gui/` に移し、`photos/` には `crates/`、`headless/`、`desktop/`（と Task 3 の `mobile/`）だけが残るようにする。gui の `Cargo.toml` から `[[bin]]` を消し、`asteria-framework-server` などを dev-dependencies にだけ残す。README の開発中の起動を `cargo run -p asteria-photos-desktop --release` に、サーバーを desktop から立てる例（`cargo run -p asteria-photos-desktop -- serve --config …`）を一行足す（英語と日本語）。

- [ ] **Step 4: 確かめる**

Run: Task 1 の Step 4 と同じ。加えて
```bash
cargo tree -p asteria-photos-gui -e normal | grep -E "asteria-framework-server|axum|redb" ; echo "found=$?"
cargo tree -p asteria-music-gui -e normal | grep -E "asteria-framework-server|axum|redb" ; echo "found=$?"
```
Expected: すべて通る。`cargo tree` の grep は何も出さず `found=1`。

- [ ] **Step 5: コミット**

```bash
git commit -am "refactor: the clients become asteria-photos-gui and asteria-music-gui, opened by the desktop binaries"
```

---

### Task 3: mobile の骨組みと iOS チェック

**Files:**
- Create: `photos/mobile/{Cargo.toml,src/lib.rs}`、`music/mobile/{Cargo.toml,src/lib.rs}`
- Modify: ルートの `Cargo.toml`、`deny.toml`（`asteria-photos-mobile`、`asteria-music-mobile`）、`.github/workflows/ci.yml`（`ios-check`）
- Modify: spec §9 の 9（8b が済んだことを書く）

**Interfaces:**
- Consumes: gui の `PhotosApp`、`MusicApp`、`AppPaths`、`APP_NAME`（Task 2）
- Produces: `asteria-<app>-mobile`（lib、`crate-type = ["staticlib", "rlib"]`）。中身は、今は gui の型の再エクスポート（`pub use asteria_photos_gui::{PhotosApp, AppPaths, APP_NAME};`）と、「iOS と Android の入口は Plan 11 で足す」という doc コメントだけ。依存は gui と corundum-framework だけ。

- [ ] **Step 1: 確かめる仕組みを先に足す**

`ci.yml` の `ios-check` の最後の手順を、次に変える：
```yaml
      - name: Check the client libraries for iOS
        run: cargo check -p asteria-framework-client -p asteria-photos-mobile -p asteria-music-mobile --target aarch64-apple-ios --locked
```

- [ ] **Step 2: mobile のクレートを作る**

- [ ] **Step 3: 確かめる**

Run: Task 2 の Step 4 と同じ。加えて
```bash
cargo tree -p asteria-photos-mobile -e normal | grep -E "asteria-framework-server|asteria-photos-server|axum|redb|velopack" ; echo "found=$?"
cargo tree -p asteria-music-mobile -e normal | grep -E "asteria-framework-server|asteria-music-server|axum|redb|velopack" ; echo "found=$?"
```
Expected: すべて通り、`found=1`。iOS のチェックは手元（Linux）では動かない（`aws-lc-sys` などが `xcrun` を要る）ので、PR の CI の `ios-check` で確かめる。CI で gui の依存（eframe、cpal、reqwest など）が iOS 向けに通らなければ、その原因をこの計画の中で直すか、直せないものは mobile を `ios-check` から外し、理由をイシューにする（ruling として記録する）。

- [ ] **Step 4: コミット**

```bash
git commit -am "feat: the mobile skeletons, checked for iOS in CI"
```
