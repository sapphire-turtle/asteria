# Plan 9b: Operation Queue for Ratings — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ratings go through a client-side operation queue (JSONL, per library and profile) that is sent to a new batch RPC `ops.apply`, so stars can be set online or offline in photos and music (#19).

**Architecture:** The server gains `ops.apply` (API 2.4): a batch of rating operations with the time they were made, merged last-writer-wins by the existing `Sidecar::apply_rating`. The client gains `asteria-common-client::queue`: `OpQueue` owns the queue file and one sender thread, started by the `Session` handshake like the upload engine. `Session::rate` queues, `Session::rating_of` overlays what is not yet confirmed when drawing, and notices report ratings the server refused or the queue could not store. photos' detail pane and music's track rows and side pane draw the shared `widgets::stars`.

**Tech Stack:** Rust workspace, axum server, egui (corundum-framework), serde/serde_json, chrono, grain-id.

**Spec:** `docs/superpowers/specs/2026-10-22-asteria-design.md` — §5.2 (the whole of Plan 9b), plus §4.4 (wire promise for `ops.apply`), §5.7, §5.10, §6.

## Global Constraints

- `ops.apply` is added in API 2.4: `asteria-common-api` goes from 2.3.0 to 2.4.0; dependents that use it require `version = "2.4.0"`.
- At most 200 operations per `ops.apply` (`MAX_OPS = 200`); more is `INVALID_PARAMS`.
- Status values: `applied`, `superseded`, `rejected`; reject reasons: `not_found`, `read_only`, `invalid`.
- An `at` later than the server's now is clamped to now.
- The server records no operation ids for ratings.
- Queue file: `<data>/queue/<library id>/<profile id>.jsonl`; lines `{"add":{"id","kind":"rating","item","value","at"}}` and `{"done":"<id>"}`; operation ids are `GrainId` strings; `at` is RFC 3339 UTC with milliseconds.
- Compact: empty the file when nothing is pending; rewrite (temp file + rename) when more than 1000 done lines.
- Sender backoff: 1, 2, 4, 8, 16 min, then 30 min; send at once on reconnect and on a new rating.
- An old server (`METHOD_NOT_FOUND`, -32601) gets `media.update` one rating at a time for the rest of the session.
- No disk I/O on the UI thread; never log tokens; tests never touch real user directories (temp dirs only).
- Translations in en and ja for every new string.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Before each commit: `cargo fmt --all`, and `cargo clippy --workspace --all-targets -- -D warnings` clean.

## Review Focus

1. **Many quick clicks on one item across a reconnect** — the star shows the last click throughout, and the server ends with the last click's value (coalescing keeps the newest `at`; an `applied` reply for an older op must not hide a newer pending one). Test: Task 4 `last_click_wins_across_reconnect`.
2. **Switching profile with ratings pending** — nothing is sent to the other server; the ratings go out when that profile's session opens again. Test: Task 4 `pending_ratings_stay_with_their_profile`.
3. **The server now serves another library (`LibraryChanged`) with ratings pending** — the sender stops (gate `Closed`), the file keeps the ratings. Test: Task 4 `library_changed_stops_sending_and_keeps_the_file`.
4. **A backlog larger than one batch** (450 ratings) — sent as 200 + 200 + 50, all acknowledged, file empties. Test: Task 3 `a_large_backlog_goes_in_batches`.
5. **A refused rating while the item is on screen** — the star returns to the server's value, and a notice appears once (not every frame). Test: Task 6 `a_refused_rating_reverts_and_notices_once`.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/asteria-common-api/src/ops.rs` (new) | wire types of `ops.apply` |
| `crates/asteria-common-api/src/rpc.rs` | `OPS_APPLY` method name |
| `crates/asteria-common-server/src/writer.rs` | `Writer::apply_ops`, `Job::Ops`, `do_ops` |
| `crates/asteria-common-server/src/rpc.rs` | dispatch `ops.apply` |
| `crates/asteria-common-server/tests/ops.rs` (new) | server tests |
| `crates/asteria-common-client/src/claim.rs` (new) | process-wide file claim, moved out of `upload/engine.rs` |
| `crates/asteria-common-client/src/queue/file.rs` (new) | the JSONL file: load, append, done, compact, coalesce |
| `crates/asteria-common-client/src/queue/mod.rs` (new) | `OpQueue` handle, `Notice`, sender thread |
| `crates/asteria-common-client/src/session.rs` | start the queue; `rate`, `rating_of`, `pending_ratings`, `take_notices` |
| `crates/asteria-common-client/src/widgets/stars.rs` (new) | shared stars and `rating_after_click` |
| `crates/asteria-common-client/src/offline/page.rs`, `widgets/offline_toggle.rs` | pending count |
| `photos/crates/asteria-photos-gui/src/{detail,app}.rs` | stars via `rate`, notices |
| `music/crates/asteria-music-gui/src/{tracks,now_playing,library,app}.rs` | stars in rows and side pane, notices |

---

### Task 1: `ops.apply` on the server

**Files:**
- Create: `crates/asteria-common-api/src/ops.rs`
- Modify: `crates/asteria-common-api/src/lib.rs` (`pub mod ops; pub use ops::*;` as the other modules are exported), `crates/asteria-common-api/src/rpc.rs` (`pub const OPS_APPLY: &str = "ops.apply";`), `crates/asteria-common-api/Cargo.toml` (`version = "2.4.0"`)
- Modify: `crates/asteria-common-server/src/writer.rs`, `crates/asteria-common-server/src/rpc.rs`
- Test: `crates/asteria-common-server/tests/ops.rs`

**Interfaces:**
- Produces (in `asteria_common_api`):
  ```rust
  pub const MAX_OPS: usize = 200;
  #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
  #[serde(rename_all = "snake_case")]
  pub enum OpKind { Rating }
  #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
  pub struct RatingOp { pub id: String, pub kind: OpKind, pub item: ItemId, pub value: i8, pub at: String }
  #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
  pub struct OpsApplyParams { pub ops: Vec<RatingOp> }
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
  #[serde(rename_all = "snake_case")]
  pub enum OpStatus { Applied, Superseded, Rejected }
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
  #[serde(rename_all = "snake_case")]
  pub enum RejectReason { NotFound, ReadOnly, Invalid }
  #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
  pub struct OpResult {
      pub id: String,
      pub status: OpStatus,
      #[serde(default, skip_serializing_if = "Option::is_none")] pub item: Option<MediaItem>,
      #[serde(default, skip_serializing_if = "Option::is_none")] pub reason: Option<RejectReason>,
  }
  #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
  pub struct OpsApplyResult { pub results: Vec<OpResult> }
  ```
- Produces (server): `Writer::apply_ops(&self, actor: Actor, ops: Vec<RatingOp>) -> Result<Vec<(String, Result<(bool, ItemRecord), RejectReason>)>, WriteError>` — per op its id, and either (changed, record after) or why it was refused. `rpc.rs` turns `changed` into `Applied`/`Superseded` and the record into `media_item(rec, user, owner)`.

- [ ] **Step 1: Write the failing tests** in `tests/ops.rs`, using `common::{env, call, item_id}` and `scan` exactly as `tests/rpc.rs` does:

```rust
fn op(id: &str, item: &str, value: i8, at: &str) -> Value {
    json!({"id": id, "kind": "rating", "item": item, "value": value, "at": at})
}

#[tokio::test]
async fn applies_and_reports_the_item() {
    let e = env();
    let a = item_id(&e, "a.jpg");
    let v = call(&e.app, &e.owner_token, "ops.apply",
        json!({"ops": [op("o1", &a, 4, "2026-11-01T00:00:00Z")]})).await;
    let r = &v["result"]["results"][0];
    assert_eq!((r["id"].as_str(), r["status"].as_str()), (Some("o1"), Some("applied")));
    assert_eq!(r["item"]["rating"], 4);
    let v = call(&e.app, &e.owner_token, "media.get", json!({"id": a})).await;
    assert_eq!(v["result"]["rating"], 4);
}

#[tokio::test]
async fn an_older_rating_is_superseded() {
    // apply 5 at 10:00, then 2 at 09:00 → second is "superseded", item rating stays 5
}

#[tokio::test]
async fn same_time_takes_the_greater_value() {
    // 2 at T, then 4 at T → applied, rating 4; then 3 at T → superseded, rating 4
}

#[tokio::test]
async fn sending_the_same_op_twice_changes_nothing() {
    // op o1 (3 at T) twice in two calls → first applied, second superseded, rating 3
}

#[tokio::test]
async fn a_future_time_is_clamped_to_now() {
    // 5 at "2999-01-01T00:00:00Z" → applied; then media.update rating 1 (server time now)
    // → media.get rating is 1 (the clamped op did not win forever)
}

#[tokio::test]
async fn refusals_are_per_op() {
    // ops: [missing item "00000000-0000-0000-0000-000000000000" → rejected/not_found,
    //       value 9 → rejected/invalid, at "yesterday" → rejected/invalid,
    //       a good op → applied] in one call; results in the same order, "reason" set only on rejected
    // then broken sidecar as in rpc.rs errors_map_to_codes: fs::write a.jpg.xmp "<not xml", scan
    // → rejected/read_only
}

#[tokio::test]
async fn more_than_max_ops_is_invalid_params() {
    // 201 ops → v["error"]["code"] == codes::INVALID_PARAMS; 200 ops → result with 200 results
}
```

Fill each commented test with the calls and asserts its comment states, in the style of the first test.

- [ ] **Step 2: Run** `cargo test -p asteria-common-server --test ops`. Expected: FAIL — `ops.apply` answers `METHOD_NOT_FOUND`.

- [ ] **Step 3: Implement.**
  - API types and constant as in Interfaces; bump the crate to 2.4.0, and the `version = "2.3.0"` requirements in `asteria-common-client`, `asteria-photos-gui` and `asteria-music-gui` to `"2.4.0"`.
  - `writer.rs`: `Job::Ops { actor, ops, reply }`, run by `run_blocking(move || do_ops(&lib, &cache, actor, ops))`. `do_ops` processes ops in order, each like `do_update` (cache lookup → `RejectReason::NotFound`; `sidecar_error` → `ReadOnly`; one `library.update_sidecar` call with the hash fields set as `do_update` sets them, calling `sc.apply_rating(owner, user, value, at)` and keeping its bool; `refresh_cache`; read the record back). A value outside `-1..=5` or an `at` that `DateTime::parse_from_rfc3339` rejects → `Invalid`. `at = at.min(Utc::now())`. Any other write error for one op is `WriteError` for the whole call (the client retries the batch; ratings are idempotent).
  - `rpc.rs`: `OPS_APPLY` arm: parse `OpsApplyParams`; `ops.len() > MAX_OPS` → `rpc_error(codes::INVALID_PARAMS, format!("at most {MAX_OPS} operations per call"))`; else `state.writer().apply_ops(actor, p.ops)` and map to `OpsApplyResult`.
  - Spec §4.4 already lists the wire promise; add `ops.apply` wherever the MCP or method lists in `rpc.rs` docs enumerate methods, if they do.

- [ ] **Step 4: Run** `cargo test -p asteria-common-server --test ops` then `cargo test -p asteria-common-server`. Expected: all PASS.

- [ ] **Step 5: Commit** — `feat(server): ops.apply merges rating operations by time (API 2.4)`.

---

### Task 2: The queue file

**Files:**
- Create: `crates/asteria-common-client/src/claim.rs`, `crates/asteria-common-client/src/queue/file.rs`, `crates/asteria-common-client/src/queue/mod.rs` (for now only `mod file;` and `pub fn queue_path`)
- Modify: `crates/asteria-common-client/src/upload/engine.rs` (use `crate::claim`), `crates/asteria-common-client/src/lib.rs` (`mod claim; pub mod queue;`), `crates/asteria-common-client/Cargo.toml` (`chrono.workspace = true`)
- Test: unit tests in `queue/file.rs` (`#[cfg(test)] mod tests`, temp dirs via `tempfile`)

**Interfaces:**
- Produces:
  ```rust
  // claim.rs — moved verbatim from upload/engine.rs (OWNED, Claim, claim), now pub(crate)
  pub(crate) struct Claim(PathBuf);
  pub(crate) fn claim(path: &Path, give_up: impl Fn() -> bool) -> Option<Claim>;

  // queue/mod.rs
  /// `<data_root>/queue/<library_id>/<profile_id>.jsonl`, or `None` if either id is not a safe file name.
  pub fn queue_path(data_root: &Path, library_id: &str, profile_id: &str) -> Option<PathBuf>;

  // queue/file.rs
  #[derive(Debug, Clone, PartialEq)]
  pub(crate) struct QueuedRating { pub id: String, pub item: ItemId, pub value: i8, pub at: DateTime<Utc> }
  pub(crate) struct QueueFile { /* path, pending: Vec<QueuedRating>, done_lines: usize */ }
  impl QueueFile {
      /// Missing file → empty. A torn last line is dropped; an unreadable middle line is skipped
      /// with a `tracing::warn!`; the file is never deleted or moved. Other read errors are returned.
      pub fn open(path: &Path) -> io::Result<QueueFile>;
      pub fn pending(&self) -> &[QueuedRating];
      /// Appends one `add` line (creating parent dirs), then records it as pending.
      pub fn append(&mut self, op: QueuedRating) -> io::Result<()>;
      /// Appends one `done` line per id still pending, drops them, then compacts.
      pub fn mark_done(&mut self, ids: &[String]) -> io::Result<()>;
      /// The newest pending rating of each item (greatest `at`; on a tie the later line), in the
      /// order the items first appear; and the ids of the older ones, which can be marked done unsent.
      pub fn coalesced(&self) -> (Vec<QueuedRating>, Vec<String>);
  }
  ```
  Line format via private serde types: `#[serde(rename_all = "snake_case")] enum Line { Add(AddLine), Done(String) }` where `AddLine { id, kind: OpKind, item: ItemId, value: i8, at: String }`; `at` written with `to_rfc3339_opts(SecondsFormat::Millis, true)`.

- [ ] **Step 1: Write the failing tests** in `queue/file.rs`:

```rust
#[test] fn append_and_reopen_keeps_pending()            // 2 appends, reopen → same 2, in order
#[test] fn done_lines_remove_ops_on_reopen()             // append a,b; mark_done([a]); reopen → [b]
#[test] fn the_file_empties_when_nothing_is_pending()    // append a; mark_done([a]) → file len 0
#[test] fn many_done_lines_are_compacted()               // append 1002, mark_done first 1001 one by one
                                                          // → file has exactly 1 line; reopen → [last]
#[test] fn a_torn_last_line_is_dropped()                 // write 1 good line + `{"add":{"id":"x"` → open → 1 pending, file untouched
#[test] fn an_unreadable_middle_line_is_skipped()        // good, "garbage", good → 2 pending
#[test] fn coalesced_keeps_the_newest_per_item()         // item A at 10:00 v1, B v2, A at 11:00 v3, A at 09:00 v4
                                                          // → send [A v3, B v2]; obsolete ids = A v1, A v4
#[test] fn queue_path_rejects_unsafe_ids()               // "../x" → None; ("lib1","p1") → <root>/queue/lib1/p1.jsonl
```

- [ ] **Step 2: Run** `cargo test -p asteria-common-client --lib queue::file`. Expected: FAIL to compile (types missing).

- [ ] **Step 3: Implement** as the Interfaces say. Compaction writes the pending lines to `.<name>.tmp` next to the file and renames it (the crate's `fsutil` has the write-then-rename helper the upload queue uses; use it). Move `claim` into `claim.rs` with no behaviour change; `upload/engine.rs` imports it.

- [ ] **Step 4: Run** `cargo test -p asteria-common-client --lib` and `cargo test -p asteria-common-client --test upload_queue --test upload`. Expected: PASS.

- [ ] **Step 5: Commit** — `feat(client): the rating queue file (JSONL, coalesced, compacted)`.

---

### Task 3: `OpQueue` and its sender

**Files:**
- Modify: `crates/asteria-common-client/src/queue/mod.rs`
- Test: `crates/asteria-common-client/tests/op_queue.rs` (new), with `common::serve` stubs

**Interfaces:**
- Consumes: Task 1 types; Task 2 `QueueFile`, `claim`, `queue_path`; `session::{Gate, Health, lock_gate, note_health}`; `RpcClient`.
- Produces:
  ```rust
  #[derive(Debug, Clone, PartialEq)]
  pub enum Notice {
      RatingRejected { item: ItemId, reason: RejectReason },
      RatingNotSaved { item: ItemId },
  }
  pub(crate) struct QueueConfig {
      pub url: Url, pub token: Token, pub ctx: egui::Context,
      pub gate: Arc<Mutex<Gate>>, pub health: Health,
      pub cancelled: Arc<AtomicBool>, pub path: PathBuf,
  }
  pub struct OpQueue { /* Arc<Inner>, mpsc::Sender<Msg> */ }
  impl OpQueue {
      pub(crate) fn start(cfg: QueueConfig) -> OpQueue;
      /// Records the rating in memory at once and hands it to the thread to append and send.
      pub fn rate(&self, item: ItemId, value: i8);
      /// Not yet confirmed: the newest pending value of `item`, else the value the server returned
      /// for it this session (applied or superseded), else None.
      pub fn rating(&self, item: &ItemId) -> Option<i8>;
      /// Pending ratings, counted after coalescing (one per item).
      pub fn pending(&self) -> usize;
      pub fn take_notices(&self) -> Vec<Notice>;
      /// Send now if anything is pending (reconnect).
      pub(crate) fn wake(&self);
  }
  impl Drop for OpQueue // sets dropped; the thread exits, releasing the claim
  #[doc(hidden)] pub fn set_retry_backoff(initial: Duration, max: Duration); // for tests, as upload's
  ```
- Behaviour: the thread claims `cfg.path` (waits while another queue of this process holds it, gives up when cancelled/dropped), opens the `QueueFile` (an open error: log, mark every later `rate` as `RatingNotSaved`), then loops: drain `Msg::Rate` (append; on error remove from memory and push `RatingNotSaved`), `Msg::Wake`; if the gate is `Open` and something is pending and no backoff is running, mark `coalesced().1` done, send `coalesced().0` in chunks of `MAX_OPS` with `OPS_APPLY`; on each result mark done, store `item.rating` (applied/superseded) in the confirmed map, push `RatingRejected` for rejected; on `Unreachable`/`Retry`/5xx: backoff 1, 2, 4, 8, 16 min then 30 min (`Retry { after }` waits at least `after`); `Gate::Closed` or `Unauthorized`: stop sending (keep the file). On `Rpc { code: METHOD_NOT_FOUND, .. }` switch this queue to `media.update` (`MediaUpdateParams { id, rating: Some(value), ..Default }` — add `Default` to `MediaUpdateParams` if it lacks it), one op at a time: `NOT_FOUND`/`READ_ONLY`/`INVALID_PARAMS` → rejected with the matching reason. Each change asks `ctx` for a repaint. `rate` while the file is still being opened is held in memory and appended once open.
- A pending value always wins over the confirmed one: an `applied` reply for an op older than a still-pending one leaves the pending value showing (Review Focus 1).

- [ ] **Step 1: Write the failing tests** in `tests/op_queue.rs`. A helper starts an `OpQueue` through a test-only constructor — add `#[doc(hidden)] pub fn start_for_test(url: Url, token: Token, path: PathBuf, ctx: egui::Context, gate_open: bool) -> (OpQueue, TestGate)` where `TestGate` can `open()`/`close(err)`/`offline()` the gate. Stub servers record the `ops.apply` bodies.

```rust
#[test] fn rated_ops_are_sent_and_the_file_empties()
    // stub answers applied for each op with item rating = value; rate(A,4) → wait until pending()==0;
    // file length 0; rating(&A) == Some(4) (confirmed)
#[test] fn nothing_is_sent_while_offline_and_all_goes_on_wake()
    // gate offline; rate A,B → 0 stub calls after 300 ms; open gate + wake → one call with 2 ops
#[test] fn an_unreachable_server_keeps_ops_and_retries()
    // set_retry_backoff(50ms, 200ms); stub hangs up first 2 calls then answers → eventually pending()==0; file had 1 add line meanwhile
#[test] fn a_large_backlog_goes_in_batches()
    // 450 distinct items rated while offline; open+wake → stub sees calls of 200, 200, 50 ops; pending()==0
#[test] fn rejected_ops_are_dropped_with_a_notice()
    // stub: rejected/read_only → take_notices() == [RatingRejected{A, ReadOnly}] once; second take is empty; rating(&A)==None
#[test] fn superseded_shows_the_servers_value()
    // stub: superseded with item rating 5 for rate(A,2) → rating(&A)==Some(5)
#[test] fn an_old_server_gets_media_update()
    // stub: ops.apply → rpc_error(-32601); media.update → MediaItem with that rating
    // rate A,B → 2 media.update calls, 1 ops.apply call total; pending()==0
#[test] fn ops_survive_a_restart()
    // gate offline; rate A; drop queue; start a new queue on the same path with gate open → stub receives A
#[test] fn a_second_queue_waits_for_the_first()
    // two queues on one path: the second sends nothing until the first is dropped
#[test] fn an_unwritable_file_notices_not_saved()
    // path under a regular file (parent is a file) → rate(A,3) → take_notices() == [RatingNotSaved{A}], rating(&A)==None
```

- [ ] **Step 2: Run** `cargo test -p asteria-common-client --test op_queue`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** `OpQueue` as the Interfaces and Behaviour say, modelled on `upload/engine.rs` (`Msg` channel, `recv_timeout` for the backoff, `Inner` behind a `Mutex` read by the UI-thread methods without I/O).

- [ ] **Step 4: Run** `cargo test -p asteria-common-client --test op_queue` (also with `-- --test-threads=1` once, to catch timing assumptions). Expected: PASS.

- [ ] **Step 5: Commit** — `feat(client): OpQueue sends queued ratings with ops.apply`.

---

### Task 4: Session integration

**Files:**
- Modify: `crates/asteria-common-client/src/session.rs`
- Test: `crates/asteria-common-client/tests/session_queue.rs` (new), using the in-process server of `tests/session.rs` (`TestServer`; copy its setup into `tests/common` if it is private) and the offline fixtures of `tests/common/offline.rs`

**Interfaces:**
- Consumes: Task 3 `OpQueue`, `QueueConfig`, `Notice`, `queue_path`.
- Produces on `Session`:
  ```rust
  /// Queues `value` (0 to 5; photos may also send -1) for `item`. Err(ClientError::Offline) when
  /// the session has no queue: no data root, or the library id is not known yet.
  pub fn rate(&mut self, item: &ItemId, value: i8) -> Result<(), ClientError>;
  /// The rating to draw: the queue's unconfirmed value, else `item.rating`.
  pub fn rating_of(&self, item: &MediaItem) -> Option<i8>;
  pub fn pending_ratings(&self) -> usize;
  pub fn take_notices(&mut self) -> Vec<Notice>;
  pub fn ops(&self) -> Option<&OpQueue>;
  ```
- Behaviour: `Session` gets `ops: Arc<OnceLock<OpQueue>>`; `Handshake::connected` and the offline branch of `Handshake::run` start it (once, if `dirs.data` is set and `queue_path` accepts the ids; not if `cancelled`). `tell_sync` calls `ops.wake()` when the server starts answering again. `Request::MediaUpdate` is unchanged (tags, description, label; still `ClientError::Offline` offline).

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn a_rating_reaches_the_server()
    // real server; rate(a.jpg, 4) → wait until pending_ratings()==0 → media.get rating 4; rating_of(old item)==Some(4)
#[test] fn rating_offline_shows_at_once_and_arrives_after_reconnect()
    // session Offline (server down, recorded library with a pin, as session_offline.rs does);
    // rate(A,3) → rating_of(A)==Some(3), pending_ratings()==1; start server on the same addr, reconnect()
    // → pending 0; server's media.get rating 3
#[test] fn last_click_wins_across_reconnect()
    // a stub server (as in op_queue.rs) that holds the first ops.apply until released;
    // offline: rate A 1,2,3; reconnect; rate A 5 while the first batch is held; release → last value sent is 5,
    // rating_of(A)==Some(5) at every poll in between (never 3 after 5 was clicked)
#[test] fn pending_ratings_stay_with_their_profile()
    // profile P1 offline rates A; drop session; open profile P2 (other server) → P2's server sees no ops.apply;
    // open P1 again with its server up → ops arrive
#[test] fn library_changed_stops_sending_and_keeps_the_file()
    // offline with pending; server comes back serving another library id → state Failed(LibraryChanged);
    // no ops.apply received; the jsonl still has the add line
#[test] fn rate_without_a_data_root_is_offline_error()
    // Session::open (no dirs) → rate(..) == Err(ClientError::Offline)
#[test] fn refused_rating_becomes_a_notice()
    // broken sidecar on a.jpg; rate → take_notices() == [RatingRejected{a, ReadOnly}]
```

- [ ] **Step 2: Run** `cargo test -p asteria-common-client --test session_queue`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** as Interfaces and Behaviour say. Update the doc comments of `open_with_dirs`/`open_with` (data root now also holds `queue/`) and `SessionDirs::data`.

- [ ] **Step 4: Run** `cargo test -p asteria-common-client`. Expected: PASS.

- [ ] **Step 5: Commit** — `feat(client): Session::rate queues ratings, online or offline`.

---

### Task 5: Shared stars and the pending count

**Files:**
- Create: `crates/asteria-common-client/src/widgets/stars.rs`
- Modify: `crates/asteria-common-client/src/widgets/mod.rs`, `offline/page.rs`, `widgets/offline_toggle.rs`, `i18n/{en,ja}/client.ftl`
- Modify (callers of the changed signatures only): `photos/crates/asteria-photos-gui/src/*`, `music/crates/asteria-music-gui/src/*` and their tests
- Test: `crates/asteria-common-client/tests/stars.rs` (new), `tests/offline_page.rs`

**Interfaces:**
- Produces:
  ```rust
  pub const MAX_RATING: i8 = 5;
  /// That many stars, or 0 when `star` is the rating already set. (Moved from photos' detail.rs.)
  pub fn rating_after_click(current: Option<i8>, star: i8) -> i8;
  /// Five stars at `size` points; returns the rating a click asks for. Disabled UI draws but never returns.
  pub fn stars(ui: &mut egui::Ui, rating: Option<i8>, size: f32) -> Option<i8>;
  ```
  Hover texts move to `asteria-common-client-rating-set = Set { $stars } stars` / `{ $stars } つ星にする` and `asteria-common-client-rating-clear = Clear the rating` / `評価を消す` (copy photos' existing en/ja wording if it differs, and delete the photos keys).
- `OfflinePage::ui(&mut self, ui, handle: Option<&OfflineHandle>, pending_ratings: usize)` and `offline_status(ui, handle: Option<&OfflineHandle>, pending_ratings: usize)`: when `pending_ratings > 0`, a line `asteria-common-client-ratings-pending = { $count -> [one] 1 rating *[other] { $count } ratings } not sent yet` / `未送信の評価 { $count } 件`. offline_status shows it even with no handle.

- [ ] **Step 1: Write the failing tests:** `rating_after_click` cases moved from photos (`(None,3)→3`, `(Some(3),3)→0`, `(Some(3),5)→5`); `stars` clicked on star 4 returns `Some(4)` (headless frame + click as `tests/offline_page.rs` does); offline page with `pending_ratings = 3` shows "3 ratings not sent yet", with 0 shows no such text.
- [ ] **Step 2: Run** `cargo test -p asteria-common-client --test stars --test offline_page`. Expected: FAIL.
- [ ] **Step 3: Implement**; photos' `detail.rs` uses `widgets::stars::{stars, rating_after_click, MAX_RATING}` in place of its own; callers pass `session.pending_ratings()`.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** — `feat(client): shared stars widget and the pending ratings count`.

---

### Task 6: photos — stars through the queue

**Files:**
- Modify: `photos/crates/asteria-photos-gui/src/detail.rs`, `photos/crates/asteria-photos-gui/src/app.rs`, `photos/crates/asteria-photos-gui/i18n/{en,ja}/photos.ftl`
- Test: `photos/crates/asteria-photos-gui/tests/detail.rs`, `tests/offline.rs`

**Interfaces:**
- Consumes: `Session::{rate, rating_of, take_notices}`, `Notice`, `widgets::stars`.
- Behaviour: a star click calls `session.rate(&item.id, r)`; the pane draws `session.rating_of(item)`; the rating editor is never disabled for "sending" (`Field::Rating` and `Edit::Rating` go away; the other fields keep `media.update`). Offline, the stars stay enabled and the other editors are disabled. A read-only item shows its reason in place of all editors, as now. The module docs ("shows the server's values only") are updated: ratings show at once. The app calls `take_notices()` after `poll` and shows each in the existing status-bar notice: `asteria-photos-rating-rejected = Could not apply a rating ({ $reason })` / `評価を反映できませんでした（{ $reason }）` with reasons `asteria-photos-rating-reason-not-found = not found` / `見つかりません`, `…-read-only = read-only` / `読み取り専用です`, `…-invalid = invalid` / `不正な値です`; and `asteria-photos-rating-not-saved = Could not save a rating` / `評価を保存できませんでした`.

- [ ] **Step 1: Write the failing tests:** `a_star_click_shows_at_once` (stub server delays `ops.apply` 2 s; click star 4 → the next frame shows 4 filled stars and the server has not answered yet); `stars_work_offline` (offline session as `tests/offline.rs` sets up; click star 2 → `pending_ratings()==1`, stars show 2; label editor disabled); `a_refused_rating_reverts_and_notices_once` (stub rejects read_only; after the reply the stars show the item's server rating, the notice text appears, and it is pushed once — 10 more frames do not re-push it).
- [ ] **Step 2: Run** `cargo test -p asteria-photos-gui --test detail --test offline`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p asteria-photos-gui`. Expected: PASS.
- [ ] **Step 5: Commit** — `feat(photos): ratings go through the queue and work offline`.

---

### Task 7: music — stars in rows and the side pane (#19)

**Files:**
- Modify: `music/crates/asteria-music-gui/src/tracks.rs`, `now_playing.rs`, `library.rs`, `app.rs`, `i18n/{en,ja}/music.ftl`; `README.md`, `README.ja.md`
- Test: `music/crates/asteria-music-gui/tests/library.rs`, `tests/offline.rs`, unit tests in `tracks.rs`

**Interfaces:**
- Consumes: `Session::{rate, rating_of, take_notices, pending_ratings}`, `widgets::stars`.
- Produces: `TrackAction::Rate(usize, i8)`; `tracks::show(ui, salt, rows, album_artist, available, ratings: &dyn Fn(&MediaItem) -> Option<i8>, wide: bool)`; `now_playing::side(ui, state, images, rating: Option<i8>) -> Option<i8>` (the clicked rating).
- Behaviour:
  - Wide layout: a sixth column `RATING_W` (5 stars at 12 pt) right of the artist, before the length; each star is its own `ui.interact` rect registered after the row's response, so a star click rates and does not play. A missing playlist entry has no stars.
  - Narrow layout (`wide == false`): no stars column; the row's context menu gets a submenu `asteria-music-tracks-rate = Rating` / `評価` with 0–5 (`asteria-music-tracks-rate-none = No rating` / `評価なし` and `★`×n labels).
  - Offline, stars are shown and work for every track, saved or not (rating needs no audio).
  - Side pane: stars (18 pt) under the album line for the current entry, drawing `session.rating_of(&entry.media)`; a click rates `entry.media.id`.
  - `library.rs` turns `TrackAction::Rate(i, v)` into `session.rate(&track.item.id, v)` (not a player `Command`).
  - Notices: music gets the same status-bar notice as photos (`notice: Option<(String, Instant)>`, the same timeout constant value, shown at the bottom on narrow screens), and the same four strings under `asteria-music-rating-*`.
  - README/README.ja.md: in the offline section, one sentence each that ratings can be set offline and are sent when the server is back.

- [ ] **Step 1: Write the failing tests:** `a_star_in_a_row_rates_without_playing` (album open, wide; click the 3rd star of row 0 → stub receives `ops.apply` with value 3; player queue still empty); `narrow_rows_rate_from_the_menu` (narrow; context menu → Rating → ★★ → value 2); `the_side_pane_rates_the_current_track` (play row 0; click star 5 in side pane → value 5 for that item); `rating_offline_counts_as_pending` (offline fixture; click a star → status shows "1 rating not sent yet" / pending count 1).
- [ ] **Step 2: Run** `cargo test -p asteria-music-gui --test library --test offline`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings`, `cargo fmt --all --check`, `cargo deny check`. Expected: all clean.
- [ ] **Step 5: Commit** — `feat(music): rate tracks from rows and the side pane (#19)`.
