# Plan 5: photos のアップロード 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `asteria-photos` のウィンドウにファイルやフォルダをドロップすると、写真・動画（と対応する XMP）がサーバーの `Inbox/` に送られる。送信はアプリを閉じても次に開いたときに続きから進み、既にライブラリにあるものは、ユーザーが「保存」か「破棄」を選ぶまで送らない。あわせて、意図して残した重複があってもサイドカーの紐付け直しが働くようにする。

**Architecture:** サーバー側では、未完了のセッションを `asteria-framework-server` の `UploadStore` が `.asteria/uploads/<session>/` に持つ。確定（`upload.commit`）は書き込みキューのジョブとして直列に行う。クライアント側では、`asteria-framework-client` の `upload` モジュールが3つの部品を持つ。
- ドロップの組み立て（`plan`）
- 送信キューの保存（`queue`）
- 送信を進めるスレッド（`engine`）

`engine` は `Session` が持つので、Session を drop すると止まる。`photos` は、ドロップの受け口とアップロードの画面だけを持つ。

**Tech Stack:** Rust 1.99.0、axum 0.8、redb、reqwest（blocking）、egui、corundum-framework 0.2、blake3、serde_json。

**Spec:** `docs/superpowers/specs/2026-10-22-asteria-design.md`（特に §3.3 `FileHash`、§4.2-5、§4.7、§5.3）

## Global Constraints

- ツールチェインは `1.99.0`、edition 2024。CI は clippy `-D warnings` と `cargo fmt --check` を通す。
- `*-api` crate が依存してよいのは `serde` と `grain-id` だけ。今回は wire 型を足すだけなので、`asteria-framework-api`、`asteria-photos-api`、`asteria-music-api` は 2.2.0 にする。
- 原本ファイル（サイドカー、メディア）は「一時ファイル → rename」で書き、既存のファイルを上書きしない（`move_no_clobber`）。
- 受け取ったファイルは常に `Inbox/` の下に置く。`path` は `/` 区切りの相対パスで、`..`、空の要素、`.` で始まる要素、`\`、NUL を含まない。組の中のファイルは同じフォルダに置く。
- チャンクサイズは 8 MiB（`8 * 1024 * 1024`）。1ファイルの上限は 20 GiB。ユーザーごとの未完了セッションは合計 64 GiB、1000 個まで。7日間チャンクが届かないセッションは消す。
- 重複を自動では捨てない。サーバーはクライアントの申告を信じず、確定のときに自分でハッシュを計算する。
- セッションは作ったユーザーのもの。ほかのユーザーには 404 を返す（RPC では `NOT_FOUND`）。
- `asteria-framework-client` は eframe に依存しない。iOS 向けの `cargo check` が通ること（`asteria-framework` を通常の依存に加えても、純 Rust なので通るはず。Task 9 で確かめる）。
- トークンはログに出さない。UI スレッドでネットワークやディスクの I/O をしない。
- 文字列は `tr!` を通し、英語と日本語の Fluent ファイルを用意する。
- ドラッグ＆ドロップの受け口は `photos/src/desktop.rs` 側に置き、それ以外の画面は OS に依存させない。

## Review Focus

1. **パスの抜け道**：`path` に `../`、絶対パス、`Inbox/../.asteria/x`、`.hidden/a.jpg`、`a\b.jpg`、Windows の `C:` などを入れても、`Inbox/` の外にも隠しディレクトリにも置けない。→ Task 6 でテストする
2. **再開の取り違え**：送信の途中でローカルのファイルを書き換えたり差し替えたりしても、古いチャンクと新しいチャンクが混ざったファイルが置かれない。確定のときのハッシュ検証で止まり、クライアントは最初から送り直す。→ Task 6、Task 10 でテストする
3. **名前の衝突と既存ファイル**：`Inbox/IMG_0001.HEIC` が既にあるところに HEIC＋MOV＋XMP の組を送ると、3つとも同じ ` (2)` が付く。組の途中で失敗しても、置いたファイルが残って組が欠けることがない。→ Task 6 でテストする
4. **日本語やUnicode のファイル名**：`京都旅行/IMG_0001.HEIC` がキューの保存、wire、Inbox のパスを往復しても変わらない。macOS の NFD のファイル名も、そのままのバイト列で扱う。→ Task 8、Task 9 でテストする
5. **大量のドロップ**：1万ファイルのフォルダをドロップしても UI が固まらない。ハッシュの計算と `upload.check` はワーカーで行い、500件ずつ問い合わせる。キューの保存は数秒おきにまとめる。→ Task 10 でテストする

---

### Task 1: サイドカーの `FileHash` とジャーナルの `upload`

**Files:**
- Modify: `crates/asteria-framework/src/sidecar/{model.rs,read.rs,write.rs}`、`crates/asteria-framework/src/meta.rs`、`crates/asteria-framework/src/journal.rs`
- Test: `crates/asteria-framework/tests/`（サイドカーと meta の既存テストのファイルに足す）

**Interfaces:**
- Produces:
  - `Sidecar.file_hash: Option<ContentHash>`。`asteria:FileHash` に対応する。スコープは常に `file/1` なので、値だけを持つ。
  - `OpKind::Upload`（wire は `"upload"`）
- `update_sidecar` の変更点：書き込むときは、`file_hash` を必ずその時点の値にする。
  - 呼び出し側が `f` の中で `sc.file_hash` を入れていれば、それを使う。
  - 入っていなければ、`hash::file::file_hash` で計算する。
  - `file_hash` だけが変わった場合は書かない。今の `content_hash` と同じ扱いにする。
- `crates/asteria-framework-server/src/writer.rs` の `do_update` は、使えるキャッシュの `StoredHash.file_hash` があれば `f` の中で設定する。大きな動画を書き込みのたびにハッシュし直さないため。

```rust
#[test] fn file_hash_roundtrips() { /* asteria:FileHash="blake3:…" を読み書きして一致する */ }
#[test] fn missing_file_hash_is_none() { /* 古いサイドカー → None。壊れたとはしない */ }
#[test] fn malformed_file_hash_is_malformed_sidecar() { /* "blake3:xyz" → MalformedSidecar */ }
#[test] fn update_sidecar_writes_current_file_hash() { /* 評価を付ける → サイドカーの FileHash == file_hash(メディア) */ }
#[test] fn file_hash_alone_does_not_rewrite() { /* 既存のサイドカーで FileHash だけが違う更新 → Ok(false)、バイト列はそのまま */ }
#[test] fn op_kind_upload_wire() { /* serde_json で "upload" */ }
```

- [ ] **Step 1:** 失敗するテストを書く
- [ ] **Step 2:** 実装する。管理項目の一覧（`write.rs` の先頭）に `(ns::ASTERIA, "FileHash")` を足す。書き込み後の読み戻しの確認にも `file_hash` を入れる
- [ ] **Step 3:** `cargo test -p asteria-framework -p asteria-framework-server` が通ることを確かめる
- [ ] **Step 4: Commit** `feat(framework): asteria:FileHash in sidecars and the upload journal kind`

### Task 2: キャッシュのハッシュ索引

**Files:**
- Modify: `crates/asteria-framework-server/src/cache.rs`
- Test: `crates/asteria-framework-server/tests/cache.rs`

**Interfaces:**
- Produces:
  - `const HASHES: TableDefinition<&str, ()>`：キーは `"{scope}\n{hash}\n{path}"`。前方一致の範囲で引く。
  - `CACHE_SCHEMA` を 3 に上げる。古いキャッシュは、既存の仕組みで作り直される。
  - `Cache::paths_with_hash(&self, hash: &str, scope: &str) -> Result<Vec<String>, ServerError>`：その（ハッシュ、スコープ）の `StoredHash` を持ち、記録のサイズと更新時刻がハッシュの計算時と一致するパスを返す。
- 索引の更新は、`ITEMS` を書き換えるのと同じトランザクションで行う。対象は `put`、`put_many`、`set_hash`、`set_hash_many`、`remove`、`remove_many`。古いハッシュのキーは消す。

```rust
#[test] fn hash_index_follows_set_hash_and_remove() {}
#[test] fn stale_hash_is_not_returned() { /* サイズか mtime が変わった記録（put で更新）→ 返さない */ }
#[test] fn same_payload_two_paths() { /* 2つのパスが同じハッシュ → 両方返る、順序はパス順 */ }
#[test] fn schema_2_cache_is_rebuilt() { /* 既存の作り直しテストに倣う */ }
```

- [ ] **Step 1:** 失敗するテストを書く
- [ ] **Step 2:** 実装する
- [ ] **Step 3: Commit** `feat(server): hash index in the cache`

### Task 3: 紐付け直しの候補の絞り込み

**Files:**
- Modify: `crates/asteria-framework-server/src/hashing.rs`（`relink_orphans` の Phase 1）
- Test: `crates/asteria-framework-server/tests/hashing.rs`

**Interfaces:**
- Consumes: Task 1 の `Sidecar.file_hash`、Task 2 の `Cache::paths_with_hash`（今の自前の `index` はこれで置き換える）
- 候補の選び方（spec §4.2-5）：
  1. `paths_with_hash` で引いた候補から、`rec.sidecar.has_sidecar` のものを外す。
  2. 候補が2つ以上で、サイドカーに `file_hash` があれば、`StoredHash.file_hash` が一致するものに絞る。一致が0件なら絞らない。
  3. 1つなら `single` に入れ、それ以外は `ambiguous` にする。
- Phase 2 の「移動の直前にサイドカーができていたら衝突」は今のままにする。

```rust
#[test] fn duplicate_with_sidecar_is_not_a_candidate() {
    /* a/x.flac と b/x.flac が同じ payload、b に評価あり。a/x.flac を c/x.flac に移動 → a の orphan は c/x.flac に紐付く */
}
#[test] fn file_hash_breaks_the_tie() {
    /* 同じ payload でタグが違う2ファイル（どちらもサイドカーなし）と、FileHash 付きの orphan → FileHash が一致するほうへ */
}
#[test] fn identical_copies_stay_ambiguous() { /* バイト列まで同じ2ファイル → ambiguous、何も動かさない */ }
#[test] fn no_file_hash_falls_back_to_ambiguous() { /* 古いサイドカー、候補2つ → ambiguous */ }
```

- [ ] **Step 1:** 失敗するテストを書く（FLAC のテスト用ファイルは、既存のテストの作り方に倣う。タグ違いは lofty で作る）
- [ ] **Step 2:** 実装する
- [ ] **Step 3: Commit** `fix(server): relink sidecars past intentional duplicates`

### Task 4: アップロードの wire 型

**Files:**
- Create: `crates/asteria-framework-api/src/upload.rs`
- Modify: `crates/asteria-framework-api/src/{lib.rs,rpc.rs}`、3つの `*-api` の `Cargo.toml`（2.2.0）、依存側の `version = "2.1.0"` の指定

**Interfaces:**
- Produces（どれも `Debug, Clone, PartialEq, Serialize, Deserialize`）:
  - `pub const UPLOAD_CHUNK_SIZE: u64 = 8 * 1024 * 1024;`、`pub const UPLOAD_MAX_FILE: u64 = 20 * 1024 * 1024 * 1024;`
  - `HashRef { hash: String, scope: String }`
  - `UploadCheckParams { hashes: Vec<HashRef> }` → `UploadCheckResult { existing: Vec<ExistingMedia> }`
    - `ExistingMedia { hash: String, scope: String, paths: Vec<String> }`
    - `hashes` は 1000 件まで。超えたら `INVALID_PARAMS` にする。
  - `UploadBeginParams { size: u64, file_hash: String }` → `UploadBegin { session: String, chunk_size: u64 }`
  - `UploadSessionParams { session: String }`：`upload.status` と `upload.abort` で使う
  - `UploadStatus { size: u64, missing: Vec<u64> }`
  - `UploadCommitParams { files: Vec<UploadFile>, allow_duplicate: bool }`
    - `UploadFile { session: String, path: String }`。`path` は `Inbox/` からの相対パス
  - `#[serde(tag = "outcome", rename_all = "snake_case")] enum UploadCommitResult`
    - `Placed { files: Vec<PlacedFile>, warnings: Vec<UploadWarning> }`
    - `Duplicate { existing: Vec<String> }`
  - `PlacedFile { session: String, path: String }`。`path` はライブラリからの相対パス（`Inbox/…`）
  - `#[serde(tag = "kind", rename_all = "snake_case")] enum UploadWarning { SidecarDropped { session: String, reason: String } }`
  - メソッド名：`UPLOAD_CHECK = "upload.check"`、`UPLOAD_BEGIN`、`UPLOAD_STATUS`、`UPLOAD_COMMIT`、`UPLOAD_ABORT`
  - エラーコード：`QUOTA = -32006`、`UPLOAD_INCOMPLETE = -32007`、`HASH_MISMATCH = -32008`

```rust
#[test] fn commit_result_wire() {
    // {"outcome":"duplicate","existing":["2025/a.jpg"]}
    // {"outcome":"placed","files":[{"session":"s","path":"Inbox/a.jpg"}],"warnings":[{"kind":"sidecar_dropped","session":"t","reason":"…"}]}
}
#[test] fn app_api_is_still_2() { /* photos-api の既存テストがそのまま通る（メジャーは 2 のまま） */ }
```

- [ ] **Step 1:** 型とテストを書き、`cargo test -p asteria-framework-api -p asteria-photos-api` を通す
- [ ] **Step 2: Commit** `feat(api): upload wire types (2.2)`

### Task 5: `UploadStore`（未完了のセッション）

**Files:**
- Create: `crates/asteria-framework-server/src/upload.rs`
- Test: `crates/asteria-framework-server/tests/upload_store.rs`

**Interfaces:**
- Produces:
  - `pub struct UploadStore`。`UploadStore::open(root: &Path) -> Result<UploadStore, ServerError>` で `<root>/.asteria/uploads/` を作る。
  - 1セッションのディレクトリの中身：
    - `session.json`：`owner`、`size`、`file_hash`、`created`。一時ファイル＋rename で書く。
    - `data`：`size` の長さに `set_len` しておき、チャンクは位置を指定して書く。
    - `received`：1チャンク1バイト（0/1）。チャンクを書いてから該当バイトを書く。
  - `begin(&self, owner: UserId, size: u64, file_hash: ContentHash) -> Result<UploadBegin, UploadError>`：セッション ID は `GrainId::random()` の文字列。
  - `put_chunk(&self, owner: UserId, session: &str, index: u64, body: &[u8]) -> Result<(), UploadError>`：
    - 最後のチャンクの長さは `size - index * CHUNK` でなければならない。それ以外のチャンクは `CHUNK` ちょうど。
    - 同じセッションへの書き込みは、セッションごとの `Mutex` で直列にする。
    - 書いたら、セッションディレクトリの mtime を更新する（期限の判定に使う）。
  - `status(&self, owner, session) -> Result<UploadStatus, UploadError>`、`abort(&self, owner, session) -> Result<(), UploadError>`
  - `completed(&self, owner, session) -> Result<CompletedUpload, UploadError>`：全チャンクが揃っていなければ `Incomplete` を返す。`CompletedUpload { data_path: PathBuf, size: u64, file_hash: ContentHash }`
  - `remove(&self, session)`：確定した後に使う
  - `purge_expired(&self, now: SystemTime) -> usize`：mtime が7日より古いセッションを消す。
    - ディレクトリ名がセッション ID の形でないもの、`session.json` が読めないものも消す。
  - `pub enum UploadError { NotFound, Invalid(String), Quota(String), Incomplete, HashMismatch, Io(String) }`
    - RPC との対応：`NOT_FOUND`、`INVALID_PARAMS`、`QUOTA`、`UPLOAD_INCOMPLETE`、`HASH_MISMATCH`、`INTERNAL_ERROR`
    - HTTP との対応：404、400、413、409、409、500
- 上限の確認は `begin` で行う。
  - `size` が 0、または `UPLOAD_MAX_FILE` を超えていたら `Invalid` にする。
  - ユーザーの未完了セッションの `size` の合計が 64 GiB を超える場合と、1000 個を超える場合は `Quota` にする。

```rust
#[test] fn chunks_in_any_order_complete_the_file() {}
#[test] fn resending_a_chunk_is_idempotent() {}
#[test] fn wrong_chunk_length_is_invalid() { /* 最後以外が短い、最後が長い、index が範囲外 */ }
#[test] fn status_lists_missing_chunks() {}
#[test] fn survives_reopen() { /* drop して open し直しても status が同じ */ }
#[test] fn other_user_gets_not_found() {}
#[test] fn quota_by_bytes_and_count() {}
#[test] fn purge_removes_only_stale_sessions() { /* now を 7日+1秒進める。届いたばかりのセッションは残る */ }
```

- [ ] **Step 1:** 失敗するテストを書く
- [ ] **Step 2:** 実装する
- [ ] **Step 3: Commit** `feat(server): resumable upload sessions under .asteria/uploads`

### Task 6: 確定（`upload.commit`）

**Files:**
- Create: `crates/asteria-framework-server/src/upload_commit.rs`
- Modify: `crates/asteria-framework-server/src/writer.rs`（`Job::Commit` と `Writer::commit_upload`）
- Test: `crates/asteria-framework-server/tests/upload_commit.rs`

**Interfaces:**
- Consumes: Task 2 の `paths_with_hash`、Task 5 の `UploadStore::completed` / `remove`、Task 1 の `OpKind::Upload`
- Produces:
  - 同期関数 `commit(library: &Library, cache: &Cache, store: &UploadStore, user: UserId, params: UploadCommitParams) -> Result<UploadCommitResult, UploadError>`
  - `Writer::commit_upload(&self, store: Arc<UploadStore>, user: UserId, params: UploadCommitParams) -> Result<UploadCommitResult, UploadError>`。書き込みキューで直列に実行する。
- 手順（spec §4.7）：
  1. パスを検証する（Global Constraints）。組が空、`path` が重複、フォルダが揃っていない場合は `Invalid`。
  2. 各セッションについて `completed` を呼ぶ。`data` の `file/1` ハッシュが申告と違えば、そのセッションを消して `HashMismatch` を返す。
  3. メディア（拡張子が `.xmp` でないもの）ごとに `content_hash` を計算する。
     - `allow_duplicate == false` で `paths_with_hash` が空でなければ、`Duplicate { existing }` を返し、何も置かない。
     - セッションは残す。
  4. XMP は `Sidecar::read` で読んでみる。壊れていれば組から外し、`SidecarDropped` の警告を付けて、そのセッションを消す。
     - XMP の名前が組のどのメディアのサイドカー名（`<メディア名>.xmp`）とも一致しなければ、`Invalid` にする。
  5. 連番を決める。名前の最初の `.` までを base とし、n = 1（そのまま）、2、3… の順に試す。組の全ファイルの `{base} (n){残り}` が既に存在しない最初の n を採る。
  6. フォルダを作り、`move_no_clobber`（`hashing.rs` から `fsutil` 相当の場所に移して共有）で順に置く。
     - 途中で失敗したら、この組で置いたファイルを `data` に戻して（rename で）エラーにする。
  7. 置いたファイルごとにジャーナルへ `OpKind::Upload` を記録する。`before: {"session": …}`、`after: {"path": …}`。
  8. セッションを消す。再スキャンの依頼は、呼び出し側（RPC）で行う。

```rust
#[test] fn places_a_group_under_inbox() { /* a/IMG_0001.HEIC と a/IMG_0001.MOV → Inbox/a/ に2つ、journal に2行 */ }
#[test] fn collision_numbers_the_whole_group() { /* Inbox/IMG_0001.MOV だけが既存 → HEIC/MOV/HEIC.xmp がすべて " (2)" */ }
#[test] fn rejects_escaping_paths() {
    for p in ["../x.jpg", "/x.jpg", "a/../../x.jpg", ".asteria/x.jpg", "a/.hidden/x.jpg", "a\\x.jpg", "C:x.jpg", "", "a//x.jpg"] { /* Invalid、何も置かない */ }
}
#[test] fn group_in_two_folders_is_invalid() {}
#[test] fn hash_mismatch_discards_the_session() { /* 申告と違う file_hash で begin → HashMismatch、status は NotFound */ }
#[test] fn duplicate_waits_for_permission() { /* 既存と同じ JPEG（EXIF 違い）→ Duplicate、セッションは残る。allow_duplicate=true で再度 → Placed */ }
#[test] fn broken_xmp_is_dropped_with_warning() {}
#[test] fn japanese_path_roundtrips() { /* "京都旅行/IMG_0001.HEIC" → Inbox/京都旅行/IMG_0001.HEIC */ }
#[test] fn failure_midway_leaves_no_partial_group() { /* 2つ目の置き場所に先回りしてディレクトリを作っておく → エラー、1つ目も残らない */ }
```

- [ ] **Step 1:** 失敗するテストを書く
- [ ] **Step 2:** 実装する
- [ ] **Step 3: Commit** `feat(server): commit uploads into Inbox through the write queue`

### Task 7: HTTP と RPC、photos-server への組み込み

**Files:**
- Modify: `crates/asteria-framework-server/src/{app.rs,rpc.rs,state.rs,lib.rs}`、`photos-server/src/main.rs`
- Create: `crates/asteria-framework-server/src/upload_http.rs`
- Test: `crates/asteria-framework-server/tests/upload_http.rs`

**Interfaces:**
- Consumes: Task 5、Task 6
- Produces:
  - `AppState` に `uploads: Arc<UploadStore>` を足す（`AppState::new` の引数に追加）。
  - `PUT /upload/{session}/{chunk}`：成功したら 204 を返す。エラーの対応は Task 5 の表のとおり。
    - このルートだけ `DefaultBodyLimit::max(UPLOAD_CHUNK_SIZE as usize)` にする。
    - 書き込みは `spawn_blocking` で行う。
  - RPC：`upload.check`、`upload.begin`、`upload.status`、`upload.abort`、`upload.commit`。
    - `upload.commit` が `Placed` で終わったら、`library.rescan` と同じ方法で再スキャンを依頼する。
  - photos-server の `serve`：
    - `UploadStore::open` は `cleanup_tmp_files` の後に呼ぶ。
    - 起動時と1時間ごとに `purge_expired(SystemTime::now())` を実行する（`spawn_blocking`）。

```rust
#[test] fn full_upload_over_http() { /* begin → PUT ×3（最後は端数）→ status は空 → commit → Inbox に置かれ、再スキャン後に folder.list に出る */ }
#[test] fn chunk_over_limit_is_413() { /* 8 MiB + 1 */ }
#[test] fn chunk_without_token_is_401() {}
#[test] fn other_users_session_is_404() {}
#[test] fn rpc_limit_still_64k() { /* /rpc の 413 は変わらない */ }
#[test] fn check_reports_existing_paths() {}
```

- [ ] **Step 1:** 失敗するテストを書く
- [ ] **Step 2:** 実装する。`DefaultBodyLimit` をルートごとに上書きできるかは、`chunk_over_limit_is_413` と `full_upload_over_http` で確かめる
- [ ] **Step 3:** `cargo test -p asteria-framework-server -p asteria-photos-server` を通す
- [ ] **Step 4: Commit** `feat(server): upload routes and RPC; purge stale sessions`

### Task 8: ドロップの組み立て（クライアント）

**Files:**
- Create: `crates/asteria-framework-client/src/upload/{mod.rs,plan.rs}`
- Modify: `crates/asteria-framework-client/{Cargo.toml,src/lib.rs}`（`asteria-framework` を通常の依存にする）
- Test: `crates/asteria-framework-client/tests/upload_plan.rs`

**Interfaces:**
- Produces:
  - `pub struct PlannedFile { local: PathBuf, inbox_path: String }`
  - `pub struct PlannedGroup { files: Vec<PlannedFile> }`
  - `pub struct DropPlan { groups: Vec<PlannedGroup>, excluded: usize, warnings: Vec<PlanWarning> }`
  - `pub enum PlanWarning { AmbiguousSidecar { path: PathBuf }, Unreadable { path: PathBuf, reason: String } }`
  - `pub fn plan_drop(dropped: &[PathBuf], accept: impl Fn(MediaKind) -> bool) -> DropPlan`：ブロッキング。ワーカーで呼ぶ。
- 規則（spec §5.3）：
  - ドロップしたフォルダ `F` の中の `F/x/y.jpg` は `F/x/y.jpg` に、ドロップしたファイル `y.jpg` は `y.jpg` になる。
  - 走査は walkdir を使い、シンボリックリンクはたどらない。`.` で始まる名前は、ファイルもディレクトリも飛ばして `excluded` に数える。
  - `classify` が `accept` を満たすものと、拡張子が `.xmp`（大文字小文字を問わない）のものだけを残す。
  - 組は、同じディレクトリの中で最初の `.` までの名前が同じファイルの集まり。
  - XMP の扱い：
    - `<メディア名>.xmp` は、そのメディアの組に入れる。
    - `<stem>.xmp`（Lightroom 式）は、同じ stem のメディアが1つだけなら `<メディア名>.xmp` に名前を変えて入れ、2つ以上なら `AmbiguousSidecar` にする。
    - 対応するメディアがない XMP は `excluded` に数える。
  - 2つのドロップ元が同じ `inbox_path` になる場合（別々の場所の `a.jpg` を2つ）は、別の組のままにする。衝突はサーバーの連番に任せる。
  - パスの区切りは `/` にする。ファイル名をパスに変換できない場合は（UTF-8 でない名前）、`Unreadable` にする。

```rust
#[test] fn folder_keeps_its_structure() {}
#[test] fn live_photo_pair_is_one_group() {}
#[test] fn darktable_and_lightroom_sidecars() { /* a.HEIC.xmp はそのまま、b.xmp は b.JPG.xmp に、c.xmp（c.CR2 と c.JPG）は警告 */ }
#[test] fn non_media_and_hidden_are_excluded() { /* .DS_Store、Thumbs.db、notes.txt → excluded = 3 */ }
#[test] fn japanese_names_pass_through() {}
#[cfg(unix)] #[test] fn symlinks_are_not_followed() {}
```

- [ ] **Step 1:** 失敗するテストを書く
- [ ] **Step 2:** 実装する
- [ ] **Step 3:** `cargo check -p asteria-framework-client --target aarch64-apple-ios` を（macOS が手元になければ CI で）確かめる
- [ ] **Step 4: Commit** `feat(framework-client): plan uploads from dropped files`

### Task 9: 送信キューの保存

**Files:**
- Create: `crates/asteria-framework-client/src/upload/queue.rs`
- Test: `crates/asteria-framework-client/tests/upload_queue.rs`

**Interfaces:**
- Produces:
  - `pub struct UploadQueue { groups: Vec<QueuedGroup> }`。`load(path) -> UploadQueue`、`save(&self, path) -> io::Result<()>`
    - `load` は、ファイルがなければ空にする。読めない形なら `.bad-<時刻>` に退避して空で始める（`profiles.rs` と同じ方針）。
    - `save` は一時ファイル＋rename で書く。
    - ファイルは `{"version":1,"groups":[…]}` の形にする。
  - `pub struct QueuedGroup { id: GroupId, files: Vec<QueuedFile>, state: GroupState }`。`GroupId(GrainId)`
  - `pub struct QueuedFile { local: PathBuf, inbox_path: String, size: u64, mtime_ns: i128, file_hash: Option<String>, payload: Option<HashRef>, session: Option<String> }`
  - `pub enum GroupState`
    - `Hashing`、`Checking`、`AwaitingDecision { existing: Vec<String> }`
    - `Uploading { allow_duplicate: bool }`、`Committing { allow_duplicate: bool }`
    - `Done { placed: Vec<String>, warnings: Vec<String> }`、`Discarded`、`Failed { reason: String }`
  - `pub fn queue_path(data_root: &Path, library_id: &str) -> Option<PathBuf>`：`<data_root>/uploads/<library_id>.json`。ID が安全なディレクトリ名でなければ `None` を返す（`MediaCache::open` と同じ確かめ方）。
  - `UploadQueue::push(plan: DropPlan)`、`clear_finished()`（`Done` と `Discarded` を消す）、`decide(group: GroupId, keep: bool)`、`decide_all(keep: bool)`、`retry(group)`（`Failed` を `Hashing` に戻す）、`cancel(group)`（どの状態からでも `Discarded`。セッションの `abort` はエンジンが行う）
  - `pub struct QueueSummary { total: usize, done: usize, awaiting: usize, failed: usize, active: Option<(GroupId, u64, u64)> }`（送信済みのバイト数と全体のバイト数）。`summary()` で取る。

```rust
#[test] fn roundtrip() { /* 日本語の inbox_path を含む */ }
#[test] fn unreadable_file_is_set_aside() {}
#[test] fn decide_keep_and_discard() { /* AwaitingDecision → Uploading{allow_duplicate:true} / Discarded */ }
#[test] fn clear_finished_keeps_pending() {}
#[test] fn unsafe_library_id_has_no_queue_path() { /* "../x"、"" → None */ }
```

- [ ] **Step 1:** 失敗するテストを書く
- [ ] **Step 2:** 実装する
- [ ] **Step 3: Commit** `feat(framework-client): persistent upload queue`

### Task 10: 送信エンジンと Session への組み込み

**Files:**
- Create: `crates/asteria-framework-client/src/upload/engine.rs`
- Modify: `crates/asteria-framework-client/src/{session.rs,rpc.rs}`
- Test: `crates/asteria-framework-client/tests/upload.rs`（`tests/common` の本物のサーバーを使う）

**Interfaces:**
- Consumes: Task 4、Task 7、Task 8、Task 9
- Produces:
  - `RpcClient::put_chunk(&self, session: &str, index: u64, body: &[u8]) -> Result<(), ClientError>`
    - タイムアウトは `UPLOAD_TIMEOUT = 5 分`。そのため、アップロード用の `RpcClient` は `RpcClient::with_timeout(base, token, timeout)` で別に作る。
    - 413、400、409 は `ClientError::Rpc` に写して、コードと本文を残す。
  - `Session::open_with_cache` に `data_root: Option<PathBuf>` を足す。名前を `open_with_dirs(profile, client, ctx, SessionDirs { cache, data })` に変え、`open_with_cache` は削除する。呼び出し側はすべて直す。
  - ハンドシェイクで library ID が分かったら、`UploadEngine` を起動する。キューは `queue_path` から読む。
  - `Session::uploads(&self) -> Option<&Uploads>` を足す。`Uploads` は UI から使う窓口で、キューの写しを持つ。
    - `drop_paths(&self, paths: Vec<PathBuf>)`：`plan_drop` をエンジンのスレッドで行う
    - `decide`、`decide_all`、`retry`、`cancel`、`clear_finished`
    - `snapshot(&self) -> UploadSnapshot`：UI 用の写し。`groups`、`summary`、直近のドロップの `excluded` と `warnings` を持つ
  - エンジンは専用のスレッドを1本持つ。`Session` の drop で止める。仕組みは `Worker` の `cancelled` と同じ。
    - 状態が変わったら `ctx.request_repaint()` を呼ぶ。
    - キューは状態が変わったときに保存する。チャンクの進み具合だけのときは、2秒おきにまとめて保存する。
- エンジンの1周（spec §4.7、§5.3）：
  1. `Hashing` の組は、各ファイルのサイズと mtime、`file_hash`、`content_hash` を求めて `Checking` にする。メディアでない XMP は payload を持たない。
  2. `Checking` の組は、最大500件ずつまとめて `upload.check` に問い合わせる。
     - 既にあれば `AwaitingDecision`、なければ `Uploading { allow_duplicate: false }` にする。
  3. `Uploading` の組は、1ファイルずつ送る。
     - `session` がなければ `upload.begin` を呼ぶ。
     - `upload.status` の `missing` だけを送る。
     - 送る前に、ローカルのサイズと mtime がキューの値と同じか確かめる。違えば `abort` してファイルの記録を消し、組を `Hashing` に戻す。
     - `status` が `NotFound` なら、`session` を消して `begin` からやり直す。
  4. 全ファイルが揃ったら `upload.commit` を呼ぶ。
     - `Placed` なら `Done` にする。
     - `Duplicate` なら `AwaitingDecision`（送信済みのセッションは残る）にする。
     - `HashMismatch` なら `Hashing` に戻す。
  5. `Discarded` に変わった組にセッションがあれば、`upload.abort` を呼ぶ（失敗は無視する）。
  6. `Unreachable` と `Retry` のときは、2秒から始めて最大60秒まで倍にしながら待つ。`Unauthorized` のときは止めて、`Uploads` に理由を出す。
  7. `AwaitingDecision` の組は飛ばして、ほかの組を先に進める。

```rust
#[test] fn drop_to_commit() { /* 3枚（1枚は 8 MiB を超える）→ すべて Done、サーバーの Inbox にある */ }
#[test] fn resumes_after_session_drop() {
    /* 1つ目のチャンクが届いたところで Session を drop → 新しい Session を同じ data_root で開く → missing だけを送る（サーバー側で PUT の回数を数える） */
}
#[test] fn changed_local_file_restarts() { /* 途中でローカルを書き換え → 最後に置かれたファイルが新しい内容 */ }
#[test] fn duplicate_waits_while_others_go() { /* 既存と同じ JPEG と新しい JPEG → 新しいほうは Done、既存のほうは AwaitingDecision。decide(keep) で Done */ }
#[test] fn discard_aborts_session() {}
#[test] fn unreachable_server_backs_off_without_busy_loop() {}
#[test] fn switching_servers_pauses_queue() { /* A でドロップ、B に切り替え → A のキューはファイルに残り、B には送られない */ }
#[test] fn ten_thousand_files_do_not_block_poll() { /* plan と hash がワーカーで動き、poll が 50 ms 以内に戻る（ファイルは 0 バイトに近い小さなもので） */ }
```

- [ ] **Step 1:** 失敗するテストを書く
- [ ] **Step 2:** `RpcClient` を拡張する
- [ ] **Step 3:** エンジンを実装し、Session に組み込む
- [ ] **Step 4:** `cargo test -p asteria-framework-client` を通す
- [ ] **Step 5: Commit** `feat(framework-client): upload engine on the session`

### Task 11: photos の画面

**Files:**
- Create: `photos/src/uploads.rs`
- Modify: `photos/src/{app.rs,desktop.rs,connection.rs,lib.rs}`、`photos/i18n/{en,ja}/*.ftl`
- Test: `photos/tests/uploads.rs`（`tests/support` のヘッドレス駆動と本物のサーバー）

**Interfaces:**
- Consumes: Task 10 の `Session::uploads`、`Uploads`、`UploadSnapshot`
- Produces:
  - `AppPaths` に `data: Option<PathBuf>` を足す。`desktop::paths` では `dirs.data()` を入れる。
  - `Screen::Uploads` を足し、レールの「ライブラリ」の下に置く。アイコンは corundum の既存のものから選ぶ。合うものがなければ、Lucide の `upload.svg` を `lock.svg` と同じように同梱する。
  - `PhotosApp::drop_files(&mut self, paths: Vec<PathBuf>)`：つながっていればキューに入れて、アップロードの画面を開く。つながっていなければ、ステータスバーに「サーバーにつながっていないので送れません」と出す。
  - `desktop.rs` の受け口：
    - 毎フレーム `ctx.input(|i| i.raw.dropped_files)` を見て、ファイルのパスを `drop_files` に渡す。
    - `hovered_files` が空でない間は、画面全体に「Inbox に送る」と重ねて表示する。
    - この処理はアプリの `show` から `#[cfg(not(any(target_os = "ios", target_os = "android")))]` で呼ぶ。
  - アップロードの画面（`uploads.rs`）：
    - 一覧の上に「n 件がライブラリにあります」を出し、「すべて保存」「すべて破棄」を置く。
    - 組ごとの行に、名前（組の最初のファイルの `inbox_path`）、状態、進み具合のバー（送信中だけ）を出す。
    - 状態に応じて「保存」と「破棄」（判断待ち）、「再試行」（失敗）、「取り消し」（未完了）を出す。
    - 判断待ちの行は開いて、既存のパスを確認できる。
    - 一番上に「完了を消す」を置く。直近のドロップの除外件数と警告も出す。
    - 一覧はスクロールし、見えている行だけを描く。
  - ステータスバー：未完了の組があるときだけ `↑ 12 / 40` を出す。判断待ちがあれば `・判断待ち 3` を足す。クリックするとアップロードの画面を開く。

```rust
#[test] fn dropping_a_folder_uploads_it() { /* ヘッドレスで drop_files → Uploads 画面 → サーバーの Inbox に置かれるまで待つ */ }
#[test] fn duplicate_row_offers_keep_and_discard() {}
#[test] fn drop_while_disconnected_is_refused() {}
#[test] fn status_bar_counts_pending() {}
#[test] fn i18n_keys_exist_in_both_languages() { /* 既存の i18n テストに倣う */ }
```

- [ ] **Step 1:** 失敗するテストを書く
- [ ] **Step 2:** 実装する
- [ ] **Step 3: Commit** `feat(photos): drop files to upload them to Inbox`

### Task 12: 仕上げ

- [ ] **Step 1:** README（英語と日本語）に、アップロードの手順（ドロップ、Inbox、重複の選択、再開）を足す
- [ ] **Step 2:** 全体を検証する
  Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check && cargo deny check`
  Expected: すべて成功。iOS 向けの check は CI の macOS ジョブで確かめる
- [ ] **Step 3:** 手で確かめる（PR のチェックリストに書く）
  - 数 GB の動画を送り、途中でアプリを閉じて開き直すと続きから送られる
  - 同じフォルダを2回ドロップすると、2回目はすべて判断待ちになる
  - サーバーを止めると再試行を待ち、起動すると再開する
  - 1万枚のフォルダをドロップしても固まらない
- [ ] **Step 4: Commit** `docs: uploading from the photos client`

## この計画で扱わないもの

- iOS の PhotoKit からのアップロード、送信済みの識別子リスト（iOS のクライアントの計画）
- ファイル選択のダイアログ（`rfd`）。spec にないため
- 複数ファイルの並列送信、帯域の制限
- 曖昧や衝突になったサイドカーを画面で解決する仕組み（Plan 7）
- music からのアップロード（仕組みは framework にあるので、Plan 6 で必要になったら使う）
