# Plan 8a: corundum-framework 0.5.0 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** corundum-framework を 0.2.0 から 0.5.0 に上げ、破壊的変更に対応する。新しい機能（Velopack、二重起動の防止、メディアセッション、0.5.0 のサービス登録と API キー）は使わない。0.5.0 の破壊的変更（`PackageSpec::services`、`store::Stored` の新しいフィールド）は、asteria が使っていない部分なので対応は要らない。

**Architecture:** 依存のバージョンを上げ、使われなくなった `Shell` のメソッドを外す。Windows でデータの置き場所が Roaming から Local に変わるので、設定やデータに触る前に `AppDirs::migrate_from_roaming` を呼ぶ。対象はクライアント（photos、music）とサーバー（`asteria_framework_server::cli::main`）の入口で、失敗したら何も読まずに終わる。

**Tech Stack:** Rust 1.99.0（edition 2024）、corundum-framework 0.5.0。

**Spec:** `docs/superpowers/specs/2026-10-22-asteria-design.md` §9 の 9（Plan 8a）。corundum の変更点は corundum-framework の `CHANGELOG.md` の 0.5.0、0.4.0、0.3.0。

## Global Constraints

- ツールチェインは `1.99.0`、edition 2024。CI は clippy `-D warnings`、`cargo fmt --check`、`cargo deny check` を通し、Linux（x86_64、arm64）、macOS、Windows でテストする。手元では `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings` も通すこと。
- corundum の新しい機能（`package`、`single-instance`、`media-session`、`qr-*`、`media-picker`、`service`、`keys` とその `gui-*`）は有効にしない。フィーチャーは今のもの（`gui`、`serde`、`eframe`）のまま。
- 移行は `corundum_framework::platform::dirs::AppDirs::migrate_from_roaming(app_name)` を、設定・データ・キャッシュのどれかに触る前に一度だけ呼ぶ。エラーなら、その内容を標準エラーに出し、終了コード 1 で終わる。設定もデータも読まない。
- app 名はクライアントもサーバーも同じ（`asteria-photos`、`asteria-music`）。同じディレクトリを共有するので、どちらが先に起動しても一度の移行で済む。
- バージョン（`*-api` を含む）は上げない。

## Review Focus

1. **移行の前に設定やデータに触る**：クライアントの `AppDirs::new` / `Store::new` / ログの初期化でファイルを開くもの、サーバーの `data_dir_for` / `cache_dir_for` より前に移行が走ること。→ Task 2 でコードを読んで確かめる（Windows でしか中身が動かないため、テストはその呼び出しの順と失敗時の終了を見る）
2. **移行に失敗したとき**：設定もデータも読まず、終了コード 1 で終わる。→ Task 2 でテストする
3. **Linux と macOS では何も変わらない**：移行は何もせず `Ok` を返し、今までの置き場所のまま動く。→ Task 2 でテストする
4. **非推奨の警告**：`Shell::with_status_bar` と `with_side_pane` は 0.4.0 から非推奨。clippy `-D warnings` を通すために外し、見た目（ステータスバー、サイドペイン）が変わらないこと。→ Task 1
5. **ドキュメントの置き場所**：README の Windows の設定ディレクトリを `%LOCALAPPDATA%` に直す。→ Task 2

---

### Task 1: corundum を 0.5.0 に上げる

**Files:**
- Modify: `Cargo.toml`（`[workspace.dependencies]` の `corundum-framework = "0.2.0"` → `"0.5.0"`）、`Cargo.lock`
- Modify: `photos/src/app.rs:140-141`（`.with_status_bar()`、`.with_side_pane()` を外す）、`music/src/app.rs:167`（`.with_status_bar()` を外す）

**Interfaces:**
- Produces: corundum 0.5.0 の上でビルドが通るワークスペース。

- [ ] **Step 1: バージョンを上げて、警告を確かめる**

Run: `cargo check --workspace --all-targets`
Expected: `with_status_bar` と `with_side_pane` の非推奨の警告だけが出る（2026-10-31 に一時的なコピーで 0.4.0 と 0.5.0 の両方を確かめた。ほかのエラーは出ない）。

- [ ] **Step 2: 非推奨のメソッドを外す**

0.4.0 からは `Shell::show` がいつもステータスバーとサイドペインを描く。呼び出しを外すだけで、見た目は変わらない。

- [ ] **Step 3: 確かめる**

Run:
```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings
cargo fmt --all -- --check
cargo deny check
```
Expected: すべて通る（`cargo deny` は既存の「unmatched license allowance」の警告だけ）

- [ ] **Step 4: コミット**

```bash
git commit -am "chore(deps): corundum-framework 0.5.0"
```

---

### Task 2: Windows の Roaming から Local への移行

**Files:**
- Modify: `photos/src/desktop.rs`、`music/src/desktop.rs`（`run()` の最初）
- Modify: `crates/asteria-framework-server/src/cli.rs`（`main` の最初）、`crates/asteria-framework-server/src/config.rs:79-86`（`data_dir_for` の doc コメント）
- Modify: `README.md:67`、`README.ja.md:67`
- Test: `crates/asteria-framework-server/src/cli.rs`（単体）、`photos/src/desktop.rs` または `music/src/desktop.rs`（単体）

**Interfaces:**
- Consumes: `AppDirs::migrate_from_roaming(app_name: &str) -> io::Result<Migrated>`（corundum 0.4.0 から）
- Produces（`asteria-framework-server`、`cli.rs`）: `pub(crate) fn migrate_dirs(app: &str, migrate: impl FnOnce(&str) -> std::io::Result<()>) -> Result<(), String>`。`migrate` を呼び、エラーなら `"cannot move <app>'s settings and data from Roaming to Local: <error>; nothing was read"` を返す。`main` は `migrate_dirs(spec.app, |a| AppDirs::migrate_from_roaming(a).map(drop))` を `command(&spec).get_matches()` の後、`dispatch` の前に呼び、`Err` なら `eprintln!("error: {e}")` して `exit(1)`。（`--help` や引数の誤りでは移行しない）
- Produces（クライアント）: `photos::desktop::run` と `music::desktop::run` は、`init_logging()` の直後、`AppDirs::new` の前に移行する。エラーなら同じ文の `Err` を返す（`main` が `Result` を返すので、終了コードは 1 になる）。共通の文言と関数は `asteria-framework-client` に置いてよい（`pub fn migrate_dirs(app: &str) -> Result<(), String>`）。サーバーと同じ形にすること。

- [ ] **Step 1: 失敗するテストを書く**

`cli.rs` の単体テスト：
- `a_failed_migration_stops_before_anything_is_read`：`migrate_dirs("asteria-photos", |_| Err(io::Error::other("locked")))` が `Err`、文に `asteria-photos` と `locked` と `nothing was read` を含む。
- `a_migration_that_succeeds_lets_the_app_start`：`migrate_dirs("asteria-photos", |_| Ok(()))` が `Ok(())`。
- `migration_is_a_no_op_off_windows`（`#[cfg(not(windows))]`）：一時ディレクトリを `HOME`（と `XDG_*`）に向けた状態で `AppDirs::migrate_from_roaming("asteria-test")` が `Ok` で、何も作らない。テストが環境変数を変えるなら、ほかのテストと並んで動いても安全な方法にする（変えられないなら、このテストは省いて報告に書く）。

クライアント側にも、共通の関数を使うなら `asteria-framework-client` に同じ 2 つの単体テストを置く。

- [ ] **Step 2: 失敗を確かめる**

Run: `cargo test -p asteria-framework-server migrate` と `cargo test -p asteria-framework-client migrate`
Expected: FAIL（`migrate_dirs` がない）

- [ ] **Step 3: 実装する**

`config.rs` の `data_dir_for` の doc コメントから「Windows では Roaming」の段落を外し、corundum 0.4.0 からは `%LOCALAPPDATA%` だと書く。README の `%APPDATA%\asteria-photos\` を `%LOCALAPPDATA%\asteria-photos\` に直し、以前の版の設定は初回の起動で移ることを一文で添える（英語と日本語）。

- [ ] **Step 4: 確かめる**

Run: Task 1 の Step 3 と同じコマンド
Expected: すべて通る

- [ ] **Step 5: コミット**

```bash
git commit -am "feat: move settings and data from Roaming to Local on Windows before reading them"
```
