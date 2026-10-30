# Plan 7a: ファイル操作のまとまりと取り消し 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** サーバーが `organize.apply`（移動、リネーム、ゴミ箱をまとめて実行。`dry_run` あり）、`organize.undo`（まとまりごとの取り消し）、`organize.history`（まとまりの一覧）を受け付け、music では移動した曲を指すプレイリストも書き換える。クライアントは移動にも気づいて一覧を取り直す。

**Architecture:**
- 操作の中身は framework-server の新しいモジュール `organize` に置く。操作は「どのパスからどこへ」の形（`PlannedOp`）に直してから実行するので、`organize.apply`（アイテム ID から作る）と取り消し（ジャーナルから作る）が同じ実行の道を通る。
- 1つの操作は1つの書き込みジョブ。まとまりの見出し（`batch`）、各操作、終わり（`batch_end`、空になったフォルダの削除と `after_moves` を含む）を、書き込みキューに順に入れる。まとまりは独立したタスクで回すので、クライアントが待つのをやめても最後まで進む。
- ジャーナルが唯一の記録。一覧と取り消しは `Journal::read_all` から組み立てる。
- アプリの追加処理は `AppExtension::after_moves`。アプリは「書き換えたい中身」を返すだけで、書き込み、ジャーナル、キャッシュの更新は framework-server が行う。
- `Cache` に変更の通し番号 `revision` を持たせ、`library.info` で返す。framework-client はこれが変わったら一覧を取り直す。

**Tech Stack:** Rust 1.99.0、redb、tokio、axum、serde_json。新しい依存はない。

**Spec:** `docs/superpowers/specs/2026-10-22-asteria-design.md`（特に §3.1、§3.7、§4.3、§4.6、§4.8、§4.9、§7）

## Global Constraints

- ツールチェインは `1.99.0`、edition 2024。CI は clippy `-D warnings`、`cargo fmt --check`、`cargo deny check` を通し、Linux（x86_64、arm64）、macOS、Windows でテストする。手元では `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings` も通すこと。
- `*-api` crate が依存してよいのは `serde` と `grain-id` だけ。wire の追加はマイナーバージョンを上げる：`asteria-framework-api` 2.2.0 → 2.3.0、`asteria-photos-api` 2.2.0 → 2.3.0、`asteria-music-api` 2.3.0 → 2.4.0。依存している crate の `version = "2.2.0"` の指定も 2.3.0 に直す。メジャー（`APP_API`）は変えない。
- 原本ファイル（メディア、サイドカー、プレイリスト）の書き換えは「同じディレクトリの一時ファイル → rename」（`asteria_framework::fsutil::atomic_write`）。移動は `move_no_clobber`（既存の移動先を決して上書きしない）。
- 書き込みはすべて書き込みキュー（`Writer`）を通す。順序は「原本 → ジャーナル → キャッシュ」。
- ジャーナルの `plan_id` キーの名前は変えない（今までのジャーナルを読めるように）。中身はまとまりの ID。
- RPC のエラーにサーバーの絶対パスを入れない。詳細はログにだけ出す。トークンはログに出さない。
- `asteria-framework-client` は eframe に依存しない。
- 値
  - `MAX_OPS = 10_000`（1つのまとまりの操作の上限）
  - `MAX_LABEL_CHARS = 200`（`label` の上限。制御文字は不可）
  - `organize.history` の `limit`：既定 50、上限 200
  - ゴミ箱：`.asteria/trash/<op-id>/<元の相対パス>`（`<op-id>` はその操作の `OpId`）
  - 新しいエラーコード `ALREADY_UNDONE = -32009`

## Review Focus

1. **移動先が埋まっている、サイドカーの移動先だけ埋まっている**：どちらも上書きせず、その操作を `target_exists` で飛ばし、ほかの操作は続く。メディアだけ動いてサイドカーが残る、ということも起きない。→ Task 3 でテストする
2. **作成から実行までの間にファイルが変わった、消えた**：キャッシュの記録と（サイズ、更新時刻）が違えば `changed`、無ければ `missing` で飛ばす。`file_hash` を渡されて違えば `hash_mismatch`。取り消しでも、移動の後に変わったファイルは戻さない。→ Task 3、Task 5 でテストする
3. **取り消しの取り消し（やり直し）とゴミ箱からの復元**：ゴミ箱に入れたまとまりを取り消すと元のパスに戻り、評価（サイドカー）とハッシュも戻る。それをさらに取り消すと、またゴミ箱に入る。同じまとまりを二度取り消すと `ALREADY_UNDONE`。→ Task 5 でテストする
4. **途中で落ちたまとまり**：メディアの rename の後、サイドカーの rename の後、ジャーナルの記録の後のどこで止まっても、原本は失われない（サイドカーが残れば紐付け直しで直る）。終わりの記録がないまとまりは一覧で「途中まで」になり、取り消せる。→ Task 3、Task 5 でテストする
5. **プレイリストの書き方を壊さない**：相対パス、絶対パス、`\` 区切り、BOM、CRLF、`#EXTINF` と `#ASTERIA-HASH` の行が、書き換えた後も元の形のまま残る。UTF-8 でない `.m3u` は書き換えない。→ Task 6 でテストする

---

### Task 1: wire の型、ジャーナルの種類、ゴミ箱のパス

**Files:**
- Create: `crates/asteria-framework-api/src/organize.rs`
- Modify: `crates/asteria-framework-api/src/{lib.rs,rpc.rs,media.rs}`、`crates/asteria-framework-api/Cargo.toml`、`crates/asteria-photos-api/Cargo.toml`、`crates/asteria-music-api/Cargo.toml`、`version = "2.2.0"` で framework-api を指す各 `Cargo.toml`
- Modify: `crates/asteria-framework/src/{journal.rs,paths.rs}`、`PlanId` を使う箇所（`meta.rs`、`crates/asteria-framework-server/src/{hashing.rs,upload_commit.rs}`、テスト）
- Test: `crates/asteria-framework-api/src/organize.rs`（単体）、`crates/asteria-framework/tests/journal.rs`

**Interfaces:**
- Produces（`asteria-framework-api`、`organize.rs`。すべて `Serialize, Deserialize, Debug, Clone, PartialEq`）:
  - `pub struct OrganizeOp { pub item_id: ItemId, #[serde(flatten)] pub action: OrganizeAction, #[serde(default, skip_serializing_if = "Option::is_none")] pub file_hash: Option<String> }`
  - `#[serde(tag = "kind", rename_all = "snake_case")] pub enum OrganizeAction { Move { to: String }, Trash }`。`to` はライブラリ相対、`/` 区切り。
  - `pub struct OrganizeApplyParams { pub ops: Vec<OrganizeOp>, #[serde(default)] pub label: Option<String>, #[serde(default)] pub dry_run: bool }`
  - `pub struct OrganizeUndoParams { pub batch_id: String, #[serde(default)] pub dry_run: bool }`
  - `pub struct OrganizeResult { pub batch_id: Option<String>, pub results: Vec<OpResult> }`（`dry_run` では `batch_id` は `None`）
  - `pub struct OpResult { pub index: u32, pub status: OpStatus, pub path: String, #[serde(default, skip_serializing_if = "Option::is_none")] pub new_path: Option<String>, #[serde(default, skip_serializing_if = "Option::is_none")] pub new_item_id: Option<ItemId>, #[serde(default, skip_serializing_if = "Option::is_none")] pub reason: Option<SkipReason> }`。`path` は元のパス、`new_path` と `new_item_id` は完了したとき（ゴミ箱なら `new_item_id` は無し、`new_path` は無し）。
  - `#[serde(rename_all = "snake_case")] pub enum OpStatus { Done, Skipped }`（`dry_run` では「できる」が `Done`）
  - `#[serde(rename_all = "snake_case")] pub enum SkipReason { Missing, Changed, HashMismatch, TargetExists, Symlink, Io }`
  - `pub struct OrganizeHistoryParams { #[serde(default)] pub limit: Option<u32>, #[serde(default)] pub cursor: Option<String> }`
  - `pub struct OrganizeHistory { pub batches: Vec<BatchSummary>, pub next_cursor: Option<String> }`
  - `pub struct BatchSummary { pub batch_id: String, pub user: String, pub agent: bool, pub at: String /* RFC 3339 */, pub label: Option<String>, pub op_count: u32, pub done: u32, pub skipped: u32, pub complete: bool, pub undo_of: Option<String>, pub undone_by: Option<String> }`
- Produces（`rpc.rs`）：`ORGANIZE_APPLY = "organize.apply"`、`ORGANIZE_UNDO = "organize.undo"`、`ORGANIZE_HISTORY = "organize.history"`、`codes::ALREADY_UNDONE = -32009`
- Produces（`media.rs`）：`LibraryInfo` に `#[serde(default)] pub revision: u64` を足す。古いサーバーは送らないので 0 になる。
- Produces（`asteria-framework`）:
  - `journal::BatchId(pub GrainId)`（`PlanId` を改名。`#[serde(transparent)]`）。`JournalEntry::plan_id: Option<BatchId>` は名前もキーも変えない。
  - `OpKind` に `Batch`、`BatchEnd`、`RewritePlaylist` を足す（snake_case で `batch`、`batch_end`、`rewrite_playlist`）。
  - `paths::TRASH_DIR = "trash"` と `pub fn trash_rel(op_id: &OpId, rel: &str) -> String`（`.asteria/trash/<op-id>/<rel>`）。

- [ ] **Step 1: 失敗するテストを書く**

`organize.rs` の単体テスト：
```rust
#[test]
fn ops_take_the_kind_inline() {
    let op: OrganizeOp = serde_json::from_str(r#"{"item_id":"x","kind":"move","to":"a/b.jpg"}"#).unwrap();
    assert_eq!(op.action, OrganizeAction::Move { to: "a/b.jpg".into() });
    let op: OrganizeOp = serde_json::from_str(r#"{"item_id":"x","kind":"trash","file_hash":"blake3:00"}"#).unwrap();
    assert_eq!(op.action, OrganizeAction::Trash);
    assert_eq!(op.file_hash.as_deref(), Some("blake3:00"));
}

#[test]
fn dry_run_and_label_are_optional() {
    let p: OrganizeApplyParams = serde_json::from_str(r#"{"ops":[]}"#).unwrap();
    assert!(!p.dry_run);
    assert_eq!(p.label, None);
}

#[test]
fn skip_reasons_are_snake_case() {
    assert_eq!(serde_json::to_string(&SkipReason::TargetExists).unwrap(), r#""target_exists""#);
    assert_eq!(serde_json::to_string(&SkipReason::HashMismatch).unwrap(), r#""hash_mismatch""#);
}

#[test]
fn library_info_without_revision_reads_as_zero() {
    let i: LibraryInfo = serde_json::from_str(
        r#"{"library_id":"l","owner":"o","epoch":"e","item_count":1,"scanning":false}"#).unwrap();
    assert_eq!(i.revision, 0);
}
```
`crates/asteria-framework/tests/journal.rs` に：
```rust
#[test]
fn new_kinds_round_trip() {
    for (kind, text) in [(OpKind::Batch, "batch"), (OpKind::BatchEnd, "batch_end"),
                         (OpKind::RewritePlaylist, "rewrite_playlist")] {
        assert_eq!(serde_json::to_value(kind).unwrap(), serde_json::json!(text));
    }
}

#[test]
fn batch_id_is_written_under_plan_id() {
    let mut e = sample();               // 既存のヘルパー
    let id = BatchId(GrainId::random());
    e.plan_id = Some(id);
    let v = serde_json::to_value(&e).unwrap();
    assert_eq!(v["plan_id"], serde_json::json!(id.0.to_string()));
}
```
`paths.rs` の単体テスト：`trash_rel(&OpId(g), "2025/a.jpg") == format!(".asteria/trash/{g}/2025/a.jpg")`。

- [ ] **Step 2: 失敗を確かめる**

Run: `cargo test -p asteria-framework-api organize && cargo test -p asteria-framework --test journal`
Expected: コンパイルエラー（型と種類がない）

- [ ] **Step 3: 型、定数、種類、`trash_rel` を足し、`PlanId` を `BatchId` に改名し、バージョンを上げる**

`server.info` の結果には触らない。`LibraryInfo` を組み立てている `crates/asteria-framework-server/src/rpc.rs` は、このタスクでは `revision: 0` を入れておく（Task 2 で本当の値にする）。

- [ ] **Step 4: 通ることを確かめる**

Run: `cargo test -p asteria-framework-api && cargo test -p asteria-framework && cargo check --workspace --all-targets`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git commit -am "feat(api): organize wire types, batch journal kinds and the trash path"
```

---

### Task 2: キャッシュの移動と `revision`

**Files:**
- Modify: `crates/asteria-framework-server/src/cache.rs`、`crates/asteria-framework-server/src/rpc.rs`（`LIBRARY_INFO`）
- Test: `crates/asteria-framework-server/tests/cache.rs`、`crates/asteria-framework-server/tests/rpc.rs`

**Interfaces:**
- Consumes: `LibraryInfo::revision`（Task 1）
- Produces（`Cache`）:
  - `pub fn move_item(&self, from: &str, to: &str) -> Result<Option<ItemRecord>, ServerError>`：`from` の記録を、`path = to`、`id = item_id_for(library, to)` にして1つのトランザクションで移す。`hash`、`sidecar`、`kind`、`size`、`mtime_ns` はそのまま。`HASHES` と `IDS` の索引、`APP_DATA`（あれば）も移す。`from` が無ければ `Ok(None)` で何もしない。`to` に記録があれば置き換える。移したら新しい記録を返す。`APP_DATA` を動かしたら `app_generation` を進める。
  - `pub fn revision(&self) -> u64`：プロセスの中だけの通し番号。`open` 直後は 0。`ITEMS` を変えたコミット（`put_many_with_app_data`、`remove_many` で実際に消えたとき、`update_records` で1件以上書いたとき、`move_item` で動いたとき）のたびに 1 進む。
- `library.info` は `revision: cache.revision()` を返す。

- [ ] **Step 1: 失敗するテストを書く**

`tests/cache.rs`：
```rust
#[test]
fn move_item_carries_the_record_hash_and_app_data() {
    // a.jpg に StoredHash と APP_DATA を入れてから
    let moved = cache.move_item("a.jpg", "sub/c.jpg").unwrap().unwrap();
    assert_eq!(moved.path, "sub/c.jpg");
    assert_eq!(moved.id, item_id_for(&lib_id, "sub/c.jpg"));
    assert_eq!(moved.hash, before.hash);
    assert!(cache.get_by_path("a.jpg").unwrap().is_none());
    assert!(cache.get_by_id(&before.id).unwrap().is_none());
    assert_eq!(cache.get_by_id(&moved.id).unwrap().unwrap(), moved);
    assert_eq!(cache.paths_with_hash(&h.hash, &h.scope).unwrap(), vec!["sub/c.jpg".to_string()]);
    assert_eq!(cache.app_data("sub/c.jpg").unwrap(), Some(b"tags".to_vec()));
    assert_eq!(cache.app_data("a.jpg").unwrap(), None);
}

#[test]
fn moving_a_missing_record_does_nothing() {
    let r0 = cache.revision();
    assert_eq!(cache.move_item("nope.jpg", "x.jpg").unwrap(), None);
    assert_eq!(cache.revision(), r0);
}

#[test]
fn revision_moves_with_item_changes_only() {
    let r0 = cache.revision();
    cache.put(&rec("a.jpg")).unwrap();             assert_eq!(cache.revision(), r0 + 1);
    cache.set_sidecar("a.jpg", snap(), None).unwrap(); assert_eq!(cache.revision(), r0 + 2);
    cache.set_sidecar("gone.jpg", snap(), None).unwrap(); assert_eq!(cache.revision(), r0 + 2);
    cache.put_extras(&[("p.m3u8".into(), extra())]).unwrap(); assert_eq!(cache.revision(), r0 + 2);
    cache.remove("a.jpg").unwrap();                assert_eq!(cache.revision(), r0 + 3);
    cache.remove("a.jpg").unwrap();                assert_eq!(cache.revision(), r0 + 3);
}
```
`tests/rpc.rs`：`library_info_reports_a_revision_that_moves_after_a_write`（`media.update` で評価を変えると、`library.info` の `revision` が前より大きい）。

- [ ] **Step 2: 失敗を確かめる**

Run: `cargo test -p asteria-framework-server --test cache --test rpc`
Expected: コンパイルエラー（`move_item`、`revision` がない）

- [ ] **Step 3: `move_item` と `revision` を実装し、`library.info` で返す**

- [ ] **Step 4: 通ることを確かめる**

Run: `cargo test -p asteria-framework-server`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git commit -am "feat(server): move a cached item to a new path, count item changes in a revision"
```

---

### Task 3: 1つの操作を実行する（`organize` の中核）

**Files:**
- Create: `crates/asteria-framework-server/src/organize.rs`
- Modify: `crates/asteria-framework-server/src/{lib.rs,fsutil.rs}`（`move_no_clobber` を `organize` から使う。可視性はそのまま `pub(crate)`）
- Test: `crates/asteria-framework-server/src/organize.rs` の `#[cfg(test)] mod tests`（途中で止めるテストがクレート内のフックを使うため）

**Interfaces:**
- Consumes: `Cache::move_item`（Task 2）、`trash_rel`、`OpKind`、`BatchId`、`SkipReason`（Task 1）
- Produces（`organize.rs`）:
  - `pub(crate) struct PlannedOp { pub from: String, pub to: Target, pub expect: Expect }`
  - `pub(crate) enum Target { Path(String), Trash }`
  - `pub(crate) struct Expect { pub size: u64, pub mtime_ns: i128, pub file_hash: Option<String>, pub hash: Option<StoredHash> }`。`hash` はゴミ箱からの復元でキャッシュに戻すためのもの（比べない）。
  - `pub(crate) struct Done { pub to: String, pub record: Option<ItemRecord> }`（`to` は実際の移動先。ゴミ箱ならゴミ箱のパス。`record` は移動先がライブラリの見える場所のときの新しい記録）
  - `pub(crate) struct OpCtx<'a> { pub library: &'a Library, pub cache: &'a Cache, pub ext: &'a dyn AppExtension, pub actor: Actor, pub batch: Option<BatchId> }`
  - `pub(crate) fn check(ctx: &OpCtx, op: &PlannedOp, sim: &Sim) -> Result<(), SkipReason>`：変えずに確かめる。ゴミ箱の移動先（`OpId` ごとに新しい）は、いつも空いているとみなす。
  - `pub(crate) fn apply(ctx: &OpCtx, op: &PlannedOp) -> Result<Done, SkipReason>`：実行する。
  - `pub(crate) struct Sim { taken: HashSet<String>, freed: HashSet<String> }`、`Sim::default()`、`Sim::record(&mut self, from: &str, to: &str)`。`dry_run` と受け付け時の確かめで、同じまとまりの前の操作の結果を考えに入れる（前の操作で空いたパスは空いている、前の操作で埋まったパスは埋まっている）。`apply` は実際のディスクを見るので `Sim` を使わない。

`check` と `apply` が確かめること（この順。最初に当たった理由で飛ばす）：
1. `from` のメディアを `symlink_metadata` で見る。無ければ `Missing`。シンボリックリンクなら `Symlink`。
2. （サイズ、更新時刻）が `expect` と違えば `Changed`。
3. `expect.file_hash` があれば、キャッシュの記録の `StoredHash` が今の（サイズ、更新時刻）に対して有効ならその `file_hash` と、無効なら `asteria_framework::hash::file_hash` で計算した値と比べ、違えば `HashMismatch`。
4. 移動先のメディアか、そのサイドカー（`sidecar_path_for`）が既にあれば（`Sim` の上でも）`TargetExists`。
5. 予期しない I/O エラーは `Io`（詳細は `tracing::warn!` にだけ出す）。

`apply` の手順：
1. 上の確かめ。ゴミ箱なら、ここで `OpId` を作り、移動先を `trash_rel(&op_id, &from)` にする。
2. 移動先の親フォルダを `create_dir_all`。
3. メディアを `move_no_clobber`。`AlreadyExists` なら `TargetExists`。
4. サイドカーがあれば `move_no_clobber`。失敗したら、メディアを元に戻してから（戻すのにも失敗したらログを出して）`TargetExists` か `Io` を返す。
5. ジャーナルに1行。`kind` は、移動先が `.asteria/trash/` の下なら `Trash`、親フォルダが同じなら `Rename`、それ以外は `Move`。`before = {"path": from, "size": size, "mtime_ns": mtime_ns(i64), "hash": StoredHash か null}`、`after = {"path": to, "sidecar": bool}`。ジャーナルに書けなければ `tracing::error!` を出して続ける（原本はもう動いた。§4.6 の「漏れる1件」）。
6. キャッシュ：`from` が見える場所で `to` も見える場所 → `move_item`。`from` が見える場所で `to` がゴミ箱 → `remove(from)`。`from` がゴミ箱で `to` が見える場所 → 新しい記録を作って入れる（`classify`、`stat`、`read_sidecar`、`ext.extract`。`expect.hash` の（サイズ、更新時刻）が今のものと同じならそれを `hash` に）。キャッシュの失敗はログだけ（次のスキャンで直る）。

途中で止めるためのフック：`#[cfg(test)]` のスレッドローカル `FAIL_AT: Cell<Option<FailAt>>`、`enum FailAt { AfterMedia, AfterSidecar, AfterJournal }`。止めるときは `Err(SkipReason::Io)` を返す（パニックはしない）。

- [ ] **Step 1: 失敗するテストを書く**

テスト用のライブラリ（`Library::init` ＋ `with_state_dir`、`a.jpg` と `a.jpg.xmp`、`scan` 済み）で：
- `moves_media_and_sidecar_and_journals_a_move`：`sub/c.jpg` へ。ファイルとサイドカーが動き、ジャーナルの最後の行が `Move`、`plan_id` がまとまりの ID、`after.sidecar == true`。キャッシュは `sub/c.jpg` を持ち、`a.jpg` を持たない。
- `same_folder_is_a_rename`：`b.jpg` へ。`kind == Rename`。
- `trash_goes_under_the_op_id_and_leaves_the_cache`：移動先が `.asteria/trash/<op>/a.jpg`、サイドカーも隣に。キャッシュから `a.jpg` が消える。`kind == Trash`。
- `restoring_from_trash_brings_back_the_record_and_hash`：ゴミ箱へ入れた後、ジャーナルの `before.hash` を `expect.hash` にして元へ戻すと、記録が `hash` 付きで戻る。
- `target_or_its_sidecar_taken_is_skipped_untouched`：`c.jpg` があれば `TargetExists`。`c.jpg.xmp` だけがあっても `TargetExists`。どちらの場合も `a.jpg`、`a.jpg.xmp`、既存のファイルの中身は変わらない。
- `changed_missing_and_hash_mismatch`：`a.jpg` に1バイト足すと `Changed`。消すと `Missing`。違う `file_hash` を渡すと `HashMismatch`。
- `a_symlink_source_is_skipped`（`#[cfg(unix)]`）。
- `sim_sees_earlier_ops_of_the_batch`：`a.jpg → x.jpg` を `Sim` に記録した後、`b.jpg → x.jpg` は `TargetExists`、`c.jpg → a.jpg` は通る。
- 途中で止める：
  - `crash_after_media_leaves_the_sidecar_for_relink`：`AfterMedia` で止めると、メディアは新しい場所、サイドカーは古い場所、ジャーナルに行は無い。`relink_orphans` を呼ぶとサイドカーが新しい場所に動く。
  - `crash_after_sidecar_loses_only_the_journal_line`：`AfterSidecar` で止めると、両方動いていて、ジャーナルに行は無い。
  - `crash_after_journal_is_recorded`：`AfterJournal` で止めると、ジャーナルに行がある（キャッシュは古いまま。次のスキャンで直る）。

- [ ] **Step 2: 失敗を確かめる**

Run: `cargo test -p asteria-framework-server organize::`
Expected: コンパイルエラー（`organize` がない）

- [ ] **Step 3: `check`、`apply`、`Sim`、フックを実装する**

- [ ] **Step 4: 通ることを確かめる**

Run: `cargo test -p asteria-framework-server organize::`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git commit -am "feat(server): move, rename and trash one media file with its sidecar"
```

---

### Task 4: まとまりの実行と `organize.apply`

**Files:**
- Modify: `crates/asteria-framework-server/src/{organize.rs,writer.rs,rpc.rs,ext.rs}`
- Test: `crates/asteria-framework-server/tests/organize.rs`（新規。`tests/common` の `env()` を使う）、`crates/asteria-framework-server/tests/app_ext.rs`

**Interfaces:**
- Consumes: `check`、`apply`、`Sim`、`PlannedOp`（Task 3）
- Produces（`ext.rs`）:
  - `pub struct Rewrite { pub path: String, pub before: String, pub after: String }`
  - `AppExtension::after_moves(&self, _ctx: &ExtContext, _moves: &[(String, String)]) -> Vec<Rewrite> { Vec::new() }`。`moves` はこのまとまりで完了した（元のパス、新しいパス）。ゴミ箱への移動は含めない。
- Produces（`organize.rs`）:
  - `pub(crate) fn plan_apply(cache: &Cache, ops: &[OrganizeOp]) -> Result<Vec<Result<PlannedOp, (String, SkipReason)>>, RpcError>`：受け付けの確かめ（下記）。アイテム ID が無いものは `Err(("".into(), Missing))` の要素にする（`OpResult::path` は空文字）。`expect` はキャッシュの記録の（サイズ、更新時刻）と、渡された `file_hash`。
  - `pub struct Batch { pub id: Option<BatchId>, pub results: Vec<OpResult> }`
- Produces（`Writer`）:
  - `pub async fn organize(&self, actor: Actor, label: Option<String>, ops: Vec<Result<PlannedOp, (String, SkipReason)>>, ext: Arc<dyn AppExtension>, kind: BatchKind) -> Result<Batch, WriteError>`。`pub(crate) enum BatchKind { Apply, DryRun, Undo { of: BatchId, restores: Vec<Rewrite> } }`（`Undo` は Task 5 が使う。`restores` は操作の後に、`Rewrite` と同じ書き方で戻すプレイリスト）。`after_moves` に渡す `ExtContext` の `user` は実行者のユーザー。まとまりは `tokio::spawn` したタスクで回す（`commit_upload` と同じく、呼び手が待つのをやめても最後まで進む）。
  - `pub fn running(&self) -> Arc<Mutex<HashSet<BatchId>>>`（Task 5 が使う）
- RPC `organize.apply`：`OrganizeApplyParams` → `OrganizeResult`。実行者は `Actor::User(user)`。

受け付けの確かめ（1つでも当たれば、何もせずにリクエスト全体をエラーにする）：
- `ops.len() > MAX_OPS` → INVALID_PARAMS「at most 10000 operations per batch」
- `label` が `MAX_LABEL_CHARS` を超える、または制御文字を含む → INVALID_PARAMS
- 同じ `item_id` が2回 → INVALID_PARAMS「item <index> appears twice」
- `Move { to }` が `validate_inbox_path` を通らない → INVALID_PATH（メッセージは `validate_inbox_path` のもの。`to` はクライアントが渡した相対パスなので出してよい）
- `to` の拡張子が元と違う（ASCII の大文字・小文字は区別しない）→ INVALID_PARAMS「operation <index> changes the extension」
- 2つの操作の `to` が同じ → INVALID_PARAMS「operations <i> and <j> have the same target」

まとまりの進め方（`Apply`、`Undo`）：
1. `BatchId` を作り、`running` に入れる。書き込みジョブで見出しを書く：`kind = Batch`、`before = null`、`after = {"label": label, "op_count": n, "undo_of": id か null}`。
2. 操作を1つずつ書き込みジョブにして `apply`。受け付けで `Err` だった要素はそのまま `Skipped`。
3. 最後の書き込みジョブで：
   - 完了した操作の元のフォルダから、空になったフォルダをルートに向かって消す。ライブラリのルート、`.asteria`、`.asteria/trash` は消さない。
   - `Apply` なら、完了した移動（ゴミ箱を除く）で `ext.after_moves` を呼び、返った `Rewrite` ごとに：今の中身が `before` と同じときだけ `atomic_write(after)` し、ジャーナルに `RewritePlaylist`（`before = {"path", "content": before}`、`after = {"path", "content": after}`）、`cache.put_extras` で（サイズ、更新時刻）を更新する。違えば書かずにログを出す。`after_moves` がパニックしても、まとまりは終わる（`catch_unwind`）。
   - 終わりを書く：`kind = BatchEnd`、`after = {"done": d, "skipped": s}`。
   - `running` から外す。
4. `DryRun`：1つの書き込みジョブの中で、`Sim` を使って全部を `check` する。ジャーナルにもディスクにも触らない。`batch_id` は `None`。
5. まとまりで1件でも完了したら、`rescan` は呼ばない（キャッシュは更新済み。監視のイベントで起きるスキャンは差分を見つけない）。

- [ ] **Step 1: 失敗するテストを書く**

`tests/organize.rs`（`env()` は `a.jpg` と `sub/b.jpg` を持つ）：
- `apply_moves_and_reports_new_ids`：`a.jpg → 2025/x.jpg`。結果は `Done`、`new_path == "2025/x.jpg"`、`new_item_id` は `media.get` で引ける。`folder.list("")` に `a.jpg` が無い。
- `trash_reports_no_new_id_and_hides_the_item`。
- `dry_run_changes_nothing`：結果は `Done` だがファイルは元の場所、ジャーナルに行が増えない、`batch_id == null`。
- `dry_run_sees_earlier_ops`：`a.jpg → c.jpg` と `sub/b.jpg → a.jpg`（前の操作で空く場所へ）を `dry_run` すると、どちらも `Done`。順を逆にすると、`sub/b.jpg → a.jpg` は `target_exists`。
- `one_bad_op_does_not_stop_the_batch`：1つ目は `TargetExists`、2つ目は `Done`。
- `request_errors_change_nothing`：`MAX_OPS + 1` 件、同じ `item_id` 2回、`to = "../x.jpg"`、拡張子の変更、同じ `to` 2回、のどれでも RPC エラーで、ディスクもジャーナルも変わらない。
- `unknown_item_is_missing`。
- `emptied_folders_are_removed_but_not_the_root`：`sub/b.jpg → b2.jpg` の後、`sub/` が消える。ルートは残る。
- `journal_has_header_ops_and_end`：見出し（`op_count == 2`、`label`）、各操作の行（`plan_id` が同じ）、終わり（`done`、`skipped`）。
- `revision_moves_after_apply`。
- `tests/app_ext.rs`：`after_moves_rewrites_are_written_and_journaled`（テスト用の拡張が `moves` を受け取り、`list.m3u8` の書き換えを返す。中身が変わり、ジャーナルに `rewrite_playlist`）、`a_rewrite_of_a_changed_file_is_skipped`（`before` と今の中身が違えば書かない）、`a_panicking_after_moves_still_ends_the_batch`。

- [ ] **Step 2: 失敗を確かめる**

Run: `cargo test -p asteria-framework-server --test organize --test app_ext`
Expected: FAIL（METHOD_NOT_FOUND、`after_moves` がない）

- [ ] **Step 3: `plan_apply`、`Writer::organize`、`after_moves`、RPC を実装する**

- [ ] **Step 4: 通ることを確かめる**

Run: `cargo test -p asteria-framework-server`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git commit -am "feat(server): organize.apply runs a batch of moves and trashes, with dry runs"
```

---

### Task 5: 取り消しと一覧

**Files:**
- Modify: `crates/asteria-framework-server/src/{organize.rs,rpc.rs}`
- Test: `crates/asteria-framework-server/tests/organize.rs`

**Interfaces:**
- Consumes: `Writer::organize`、`BatchKind::Undo`、`Writer::running`（Task 4）、`Journal::read_all`
- Produces（`organize.rs`）:
  - `pub(crate) fn history(entries: &[JournalEntry]) -> Vec<BatchSummary>`：新しい順。`done` はそのまとまりの `Move`、`Rename`、`Trash` の行の数（`RewritePlaylist` は数えない）、`op_count` は見出しから、`skipped` と `complete` は終わりの行から（無ければ `skipped = 0`、`complete = false`）。`undone_by` は、`undo_of` がこのまとまりを指す、いちばん新しいまとまり。
  - `pub(crate) fn plan_undo(entries: &[JournalEntry], id: BatchId) -> Result<UndoPlan, UndoError>`：`UndoPlan { ops: Vec<PlannedOp>, restores: Vec<Rewrite> }`、`enum UndoError { NotFound, AlreadyUndone }`。`ops` は新しい順、`restores` は `BatchKind::Undo` に渡す。
- RPC：
  - `organize.history`：`OrganizeHistoryParams` → `OrganizeHistory`。`cursor` はそのページの最後の `batch_id`。それより古いものから `limit` 件。
  - `organize.undo`：`OrganizeUndoParams` → `OrganizeResult`。知らない ID は NOT_FOUND、取り消し済み（`undone_by` がある）は ALREADY_UNDONE、実行中（`running` にある）は BUSY。

取り消しの組み立て（`plan_undo`）：まとまりの行を新しい順にたどり、
- `Move`、`Rename`、`Trash` → `PlannedOp { from: after.path, to: Target::Path(before.path), expect: { size, mtime_ns, file_hash: None, hash: before.hash } }`
- `RewritePlaylist` → `Rewrite { path, before: after.content, after: before.content }`（今の中身が書き換えた後の中身と同じときだけ戻す）
取り消しの実行は `BatchKind::Undo`：見出しに `undo_of`、操作は `apply`、プレイリストは Task 4 と同じ書き方で戻し、`after_moves` は呼ばない。取り消しで生まれる行は、元の操作と同じ規則で `Move`/`Rename`/`Trash`/`RewritePlaylist` になる。そのため、取り消しを取り消すと（やり直し）、もう一度同じ操作になる。

- [ ] **Step 1: 失敗するテストを書く**

`tests/organize.rs` に：
- `undo_puts_moves_back`：`a.jpg → 2025/x.jpg` を取り消すと `a.jpg` に戻り、`2025/` は消え、`media.get(元の id)` が引ける。
- `undo_restores_trash_with_its_rating`：評価 4 を付けた `a.jpg` をゴミ箱に入れて取り消すと、`a.jpg` の評価が 4、`file_hash` も返る（ハッシュを付けてから。`hash_all` を使う）。`.asteria/trash/<op>/` は空になって消える。
- `undo_of_undo_redoes`：取り消しを取り消すと、`a.jpg` がまたゴミ箱へ。
- `twice_is_already_undone`：同じまとまりの2回目は `ALREADY_UNDONE`。
- `undo_skips_files_changed_since`：移動の後に `2025/x.jpg` に書き足すと、その操作は `changed`。ほかの操作は戻る。
- `undo_skips_when_the_old_place_is_taken`：元の `a.jpg` に別のファイルを置くと `target_exists`。
- `undo_dry_run_changes_nothing`。
- `unknown_batch_is_not_found`。
- `an_incomplete_batch_shows_and_undoes`：見出しと操作の行だけを（終わり無しで）書いたジャーナル（Task 3 のフックで止めたものと同じ形を、テストでジャーナルとファイルを直接作る）は、一覧で `complete == false`、取り消せる。
- `history_pages_newest_first`：3つのまとまり、`limit = 2` で2件と `next_cursor`、続きで1件と `None`。`undo_of` と `undone_by` が互いを指す。`agent == false`、`user` は呼んだユーザー。

- [ ] **Step 2: 失敗を確かめる**

Run: `cargo test -p asteria-framework-server --test organize`
Expected: FAIL（METHOD_NOT_FOUND）

- [ ] **Step 3: `history`、`plan_undo`、RPC を実装する**

- [ ] **Step 4: 通ることを確かめる**

Run: `cargo test -p asteria-framework-server`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git commit -am "feat(server): undo a batch and list batches from the journal"
```

---

### Task 6: music のプレイリストを書き換える

**Files:**
- Modify: `music-server/src/{playlist.rs,ext.rs}`
- Test: `music-server/src/playlist.rs`（単体）、`music-server/tests/e2e.rs`

**Interfaces:**
- Consumes: `AppExtension::after_moves`、`Rewrite`（Task 4）、`organize.apply`、`organize.undo`（Task 4、5）
- Produces（`playlist.rs`、ファイルに触らない純粋な関数）:
  - `pub fn rewrite(text: &str, playlist_rel: &str, roots: &Roots, moves: &HashMap<String, String>) -> Option<String>`：書き換える行が無ければ `None`。
- `MusicExtension::after_moves`：`EXTRAS` の `.m3u8`/`.m3u` を読み、UTF-8 として読めるものだけ `rewrite` に通し、変わったものを `Rewrite` で返す。UTF-8 でないものは `tracing::info!` を出して飛ばす。

`rewrite` の規則：
- 行は改行ごとに分け、改行（`\n` か `\r\n`）と先頭の BOM はそのまま残す。
- `#` で始まる行と空行は変えない。
- エントリの行は、`library_relative` で解決したパスが `moves` にあれば、新しいパスに書き換える。
  - 元が絶対パスなら、一致したルートの書き方 ＋ 区切り ＋ 新しい相対パス。
  - 元が相対パスなら、プレイリストのフォルダから新しいパスへの相対パス（必要なだけ `..`）。
  - 元の行に `\` があれば区切りは `\`、無ければ `/`。
- 書き換えた行の前後の空白は残さない。

- [ ] **Step 1: 失敗するテストを書く**

`playlist.rs` の単体テスト：
```rust
#[test]
fn rewrites_relative_entries_from_the_playlist_folder() {
    let moves = HashMap::from([("Music/A/1.flac".to_string(), "Music/B/1.flac".to_string())]);
    let text = "#EXTM3U\n#EXTINF:1,One\n#ASTERIA-HASH:blake3:00\n../A/1.flac\nother.flac\n";
    let out = rewrite(text, "Music/lists/p.m3u8", &root_dir(), &moves).unwrap();
    assert_eq!(out, "#EXTM3U\n#EXTINF:1,One\n#ASTERIA-HASH:blake3:00\n../B/1.flac\nother.flac\n");
}

#[test]
fn keeps_absolute_backslash_crlf_and_bom() {
    let moves = HashMap::from([("A/1.flac".to_string(), "B/C/1.flac".to_string())]);
    let text = "\u{feff}C:\\lib\\A\\1.flac\r\n";
    let out = rewrite(text, "p.m3u", &roots("C:/lib"), &moves).unwrap();
    assert_eq!(out, "\u{feff}C:\\lib\\B\\C\\1.flac\r\n");
}

#[test]
fn nothing_to_change_is_none() {
    assert_eq!(rewrite("x.flac\n", "p.m3u8", &root_dir(), &HashMap::new()), None);
}
```
ほかに：`same_folder_entry`（`1.flac → 2.flac` が `2.flac` に）、`moving_out_of_a_deeper_folder`（`a/b/c.flac` を `c.flac` へ、プレイリストは `a/p.m3u8` → `../c.flac`）、`urls_and_missing_entries_are_left_alone`。

`tests/e2e.rs`（Linux）に1つ：`organize_moves_a_track_and_its_playlist_entry`：曲 `A/1.flac` と `A/list.m3u8`（`1.flac`）を置いて起動し、`organize.apply` で `B/1.flac` へ動かすと、`list.m3u8` が `../B/1.flac` になり、`playlist.get` のエントリが「見つかった」。`organize.undo` でプレイリストも元の `1.flac` に戻る。

- [ ] **Step 2: 失敗を確かめる**

Run: `cargo test -p asteria-music-server`
Expected: コンパイルエラー（`rewrite` がない）

- [ ] **Step 3: `rewrite` と `after_moves` を実装する**

- [ ] **Step 4: 通ることを確かめる**

Run: `cargo test -p asteria-music-server`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git commit -am "feat(music-server): rewrite playlist entries of moved tracks"
```

---

### Task 7: クライアントが移動に気づく、全体の確認

**Files:**
- Modify: `crates/asteria-framework-client/src/session.rs`（`library_info_arrived`）
- Test: `crates/asteria-framework-client/tests/session.rs`

**Interfaces:**
- Consumes: `LibraryInfo::revision`（Task 1、2）
- 規則（`library_info_arrived`）：今までの条件（エポックが変わった、スキャンが終わった、スキャンしていない間にアイテム数が変わった）に加えて、スキャンしていない間（前も今も `scanning == false`）に `revision` が変わったら `refetch_listing` を立てる。

- [ ] **Step 1: 失敗するテストを書く**

`tests/session.rs`（既存のテスト用サーバーを使う）：`a_write_between_scans_refetches`：`library.info` を一度受けて `take_refetch_listing()` を空にし、`media.update` で評価を変え（アイテム数は同じ）、次の `library.info` で `take_refetch_listing() == true`。もう一度 `library.info` を受けても（変化なし）`false`。

- [ ] **Step 2: 失敗を確かめる**

Run: `cargo test -p asteria-framework-client --test session`
Expected: FAIL

- [ ] **Step 3: `library_info_arrived` を直す**

- [ ] **Step 4: 全体を確かめる**

Run:
```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings
cargo fmt --all -- --check
cargo deny check
```
Expected: すべて通る

- [ ] **Step 5: コミット**

```bash
git commit -am "feat(client): refetch the listing when the library revision moves"
```
