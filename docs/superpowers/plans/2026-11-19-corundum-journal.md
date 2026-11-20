# Journal on corundum-framework-journal — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every server write runs as a batch of registered operations in corundum's `Journal` over a `JsonlStore`, so any write can be undone for 30 days, and asteria's own journal goes away.

**Architecture:** asteria-common-server gets a `journal` module: the context the operations run against, the operations (move, trash, organize tidy, rewrite playlist, write sidecar, rate, upload, relink sidecar), the mapping of actors, origins, skips and errors, and the start-up preparation of `journal/`. The `Writer` stays the one write queue: each job submits a batch to the `Journal` and waits for it before taking the next. `organize.*` reads status and history from the `Journal`, and undoes with `undo_batch`. asteria-common-core loses `journal.rs`.

**Tech Stack:** Rust 2024, corundum-framework 0.12 (`journal-jsonl` feature), tokio, axum, rmcp.

**Spec:** `docs/superpowers/specs/2026-10-22-asteria-design.md` — §3.7 (the operations table, undo window, start-up), §4.3 (Writer, BUSY), §4.4 (API 2.5), §4.6 (organize, status, history), §6, §7.

## Global Constraints

- corundum crates `0.12.0` from crates.io (not git); the server uses the facade feature `journal-jsonl`. grain-id stays `0.15`.
- `UndoPolicy { max_batches: usize::MAX, max_age: 30 days }`; `may_undo` is corundum's default (same principal).
- `Origin`: `key:<API key id>` for `/rpc`, `mcp:<session id>` for `/mcp`, `scan` for the scanner's relinks.
- `Actor { principal: <user id>, agent }` — `/mcp` writes have `agent: true`.
- Journal directory: `<data_dir>/<app>/<library id>/journal/`; old format moves to `journal-v1/` (then `journal-v1-2/`, …); month files whose last day is more than 30 days ago are removed before the store opens.
- API crate `asteria-common-api` → `2.5.0`; every dependent's requirement → `2.5.0`. New codes `UNDO_EXPIRED = -32010`, `FORBIDDEN = -32011`; `OrganizeHistoryParams.kinds: Option<Vec<String>>`.
- Skip reasons on the wire stay `SkipReason`; corundum's `Skip` has no hash-mismatch, so a hash mismatch is recorded as `PreconditionFailed` and reported as `changed` (API 2.5 no longer sends `hash_mismatch`).
- `organize.apply` / `organize.undo` wait at most 10 s (`APPLY_WAIT`), polling `Journal::status` every 50 ms; `media.update` and `ops.apply` answer BUSY when their job has not started within 10 s.
- Tests use temporary directories only; no token is ever logged.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` (also `--target x86_64-pc-windows-gnu`), `cargo fmt --all --check`, `cargo deny check` pass at the end of every task.

## Review Focus

1. **A crash in the middle of a batch, then a restart** — the batch is listed as not complete, and its done operations can still be undone. Test: Task 3 `an_interrupted_batch_is_listed_and_undoable`.
2. **Undoing after the state moved on** (the moved file was moved again; the playlist or the rating changed since) — that operation is skipped, nothing is overwritten. Tests: Task 3 `undo_skips_a_file_moved_since`, Task 4 `undo_skips_a_rating_changed_since`.
3. **First start with an old-format `journal/`** — it moves to `journal-v1/`, the server starts, and `organize.apply` works. Test: Task 2 `a_server_starts_over_an_old_journal`.
4. **A long organize batch while a client rates** — `organize.apply` answers `running: true`; `media.update` answers BUSY after its wait; the client queue retries and the rating lands after the batch. Tests: Task 4 `update_is_busy_behind_a_long_batch`, client `busy_is_retried_not_dropped`.
5. **An agent's batch undone by its principal, and someone else's refused** — allowed for the same user, FORBIDDEN for another. Test: Task 3 `undo_is_for_the_same_user`.

---

### Task 1: corundum 0.12 and the journal directory at start

**Files:**
- Modify: `Cargo.toml` (corundum crates `0.12.0`), `crates/asteria-common-server/Cargo.toml` (facade feature `journal-jsonl`)
- Create: `crates/asteria-common-server/src/journal/mod.rs`, `crates/asteria-common-server/src/journal/prepare.rs`
- Modify: `crates/asteria-common-server/src/lib.rs`
- Test: `crates/asteria-common-server/tests/journal_prepare.rs`

**Interfaces:**
- Produces:
  ```rust
  pub const UNDO_WINDOW: Duration = Duration::from_secs(30 * 24 * 60 * 60);
  /// What `prepare` did to `dir` (the journal directory) before the store opens.
  pub struct Prepared { pub moved_to: Option<PathBuf>, pub removed: Vec<PathBuf> }
  /// Moves an old-format `dir` (no `journal.lock` but some `*.jsonl`) aside to the first free
  /// `journal-v1`, `journal-v1-2`, … beside it, then removes the month files of `dir` whose
  /// last day ends more than `keep` before `now_ms`. A missing `dir` is fine (nothing to do).
  pub fn prepare(dir: &Path, now_ms: i64, keep: Duration) -> std::io::Result<Prepared>;
  ```
- The move is logged once at `warn` with both paths; removals at `info`.

- [ ] **Step 1: Write the failing tests** in `tests/journal_prepare.rs`:
  `an_old_journal_moves_to_v1` (dir with `2026-10.jsonl`, no lock → `moved_to == Some(<parent>/journal-v1)`, file now there, `dir` absent); `a_second_old_journal_gets_a_number` (`journal-v1` exists → `journal-v1-2`); `a_corundum_journal_stays` (dir with `journal.lock` and `2026-11.jsonl` → `moved_to == None`, files intact); `old_months_are_removed` (now = 2026-11-19, keep 30 days: `2026-09.jsonl` removed, `2026-10.jsonl` kept since Oct 31 is within 30 days, `2026-11.jsonl` kept, `notes.txt` untouched); `a_missing_dir_is_nothing`.
- [ ] **Step 2: Run** `cargo test -p asteria-common-server --test journal_prepare` — Expected: FAIL (no `journal` module).
- [ ] **Step 3: Bump corundum to 0.12.0, add the feature, implement `prepare`.** The month of a file comes from its name `YYYY-MM.jsonl`; other names are left alone.
- [ ] **Step 4: Run** the test, then `cargo test --workspace`. Expected: PASS (the bump needs no code change; checked while planning).
- [ ] **Step 5: Commit** — `feat(server): corundum 0.12, and the journal directory prepared at start`.

---

### Task 2: The server's Journal

**Files:**
- Modify: `crates/asteria-common-server/src/journal/mod.rs`
- Create: `crates/asteria-common-server/src/journal/map.rs` (actors, origins, skips, errors)
- Modify: `crates/asteria-common-server/src/state.rs`, `serve` start-up (where `AppState` is built), `crates/asteria-common-core/src/library.rs` (`Library::journal()` now uses `journal-v1/`)
- Modify: `crates/asteria-common-api/src/rpc.rs` (codes), `crates/asteria-common-api/Cargo.toml` (2.5.0) and the dependents' requirements
- Test: `crates/asteria-common-server/tests/journal_ctx.rs`

**Interfaces:**
- Consumes: Task 1 `prepare`, `UNDO_WINDOW`.
- Produces:
  ```rust
  /// What every operation runs against.
  pub struct JournalCtx { pub library: Arc<Library>, pub cache: Arc<Cache>, pub ext: Arc<dyn AppExtension> }
  pub type ServerJournal = corundum_framework::journal::Journal<JournalCtx>;
  /// Prepares `<state>/journal`, opens the JsonlStore and starts the Journal with every
  /// operation registered (`register_all`), the 30-day policy and `clock` (None = system).
  pub fn open_journal(ctx: JournalCtx, clock: Option<Clock>) -> Result<ServerJournal, ServerError>;
  pub fn register_all(reg: &mut Registry<JournalCtx>, ext: &dyn AppExtension);
  /// Polls `status` every 50 ms; `Ok(None)` when not ended within `wait`. Generic so a test can
  /// drive it with a `Journal<()>` over a `MemoryStore` and an op of its own.
  pub async fn wait_up_to<C: Send + Sync + 'static>(j: &Journal<C>, id: BatchId, wait: Duration) -> Result<Option<BatchRecords>, JournalError>;
  // map.rs
  pub fn actor_of(a: asteria_common_core::Actor) -> journal::Actor;   // User(u) → person(u), Agent{on_behalf_of: u} → agent(u)
  pub fn asteria_actor(a: &journal::Actor) -> Option<asteria_common_core::Actor>;
  pub fn origin_key(key: &KeyId) -> Origin;  // "key:<id>"
  pub fn origin_mcp(session: &str) -> Origin; // "mcp:<id>"
  pub fn origin_scan() -> Origin;             // "scan"
  pub fn skip_of(r: SkipReason) -> Skip;      // Missing→NotFound, Changed/HashMismatch→PreconditionFailed, TargetExists→Exists, Symlink→InvalidPath, Io→Io(msg)
  pub fn reason_of(s: &Skip) -> SkipReason;   // NotFound→Missing, PreconditionFailed→Changed, Exists→TargetExists, InvalidPath→Symlink, else Io
  pub fn rpc_error(e: JournalError) -> RpcError; // Expired→UNDO_EXPIRED, Forbidden→FORBIDDEN, AlreadyUndone→ALREADY_UNDONE, NotFound→NOT_FOUND, Busy→BUSY, else INTERNAL_ERROR
  ```
- `AppState::journal() -> &ServerJournal`. A store that cannot open fails `serve` with the error (another process holding `journal.lock` cannot happen while `server.lock` is held).
- `Library::journal()` keeps working for the old-format writers until Task 6 removes it, but writes under `journal-v1/` so it never mixes with the JsonlStore.

- [ ] **Step 1: Write the failing tests:** `actors_round_trip`; `skips_map_both_ways` (each `SkipReason` → `Skip` → reason, with `HashMismatch` coming back as `Changed`); `expired_and_forbidden_have_their_codes`; `wait_up_to_gives_none_on_time` (a `Journal<()>` over a `MemoryStore` with a test op that sleeps 300 ms; wait 50 ms → `None`, wait 1 s → `Some`); `a_server_starts_over_an_old_journal` (state dir with an old-format `journal/2026-11.jsonl` → `open_journal` succeeds, `journal-v1/2026-11.jsonl` exists, `journal/journal.lock` exists).
- [ ] **Step 2: Run** `cargo test -p asteria-common-server --test journal_ctx` — Expected: FAIL to compile.
- [ ] **Step 3: Implement.** `register_all` registers nothing yet beyond what later tasks add (each task appends its operations). Add the two codes and bump the API to 2.5.0.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** — `feat(server): the server's corundum Journal, its context and mappings`.

---

### Task 3: File operations and organize through the Journal

**Files:**
- Create: `crates/asteria-common-server/src/journal/files.rs`
- Modify: `crates/asteria-common-server/src/organize.rs`, `writer.rs`, `rpc.rs`, `mcp.rs`, `auth.rs`; delete `batches.rs`
- Modify: `crates/asteria-common-api/src/organize.rs` (`OrganizeHistoryParams.kinds`)
- Modify: `music/crates/asteria-music-server/src/*` (only what the `Rewrite`/journal changes force)
- Test: `crates/asteria-common-server/tests/organize.rs`, `tests/mcp.rs`, `tests/journal_files.rs` (new)

**Interfaces:**
- Consumes: Task 2 `ServerJournal`, `JournalCtx`, `map::*`, `wait_up_to`.
- Produces (kinds as the spec's table names them):
  - `move` — plan `{from, to, expect: {size, mtime_ns, file_hash?}}`, done `{from, to, item_id}`; inverse = `move` from `to` back to `from` expecting the moved file's stamp. `apply` runs today's `organize::apply` steps 1–4 (check, folders, rename media then sidecar, cache).
  - `trash` — plan `{from, trash_id, expect}` (`trash_id` is a fresh grain id chosen when planning; the trash path is `trash_rel(trash_id, from)`), done `{from, to}`; inverse = `move` back to `from`.
  - `organize_tidy` — the last operation of an `organize.apply` batch: plan `{user, moves: [{from, to: Option<String>}]}` (`user` for the extension's `ExtContext`). `apply` keeps the pairs whose `to` exists and `from` does not, removes emptied source folders, and returns `ext.after_moves(..)` as `rewrite_playlist` children. Its inverse is a tidy with no moves (changes nothing).
  - `rewrite_playlist` — plan `{path, before, after}` (today's `Rewrite`); skipped as `PreconditionFailed` if the file's content is not `before`; done = the plan; inverse swaps `before` and `after`.
  - `Writer::organize(actor, origin, label, ops, kind)`: the Writer's job submits the batch and waits until it ends (`Journal::wait`), so later jobs queue behind it (§4.3); the RPC caller waits only `wait_up_to(APPLY_WAIT)` and answers `running: true` when that ends first. Plan-time skips (`Planned::Err`) are not submitted; results are merged back by request index. Results list only the top-level `move`/`trash` records, in order. Dry runs stay as today (`organize::dry_run` on the Writer).
  - `organize.undo` → `journal.undo_batch(id, origin, actor)` (dry run: `check_undo` then today's dry-run of the inverses). Empty source folders of an undo are removed by the Writer once its batch ends.
  - `organize.status` and `organize.history` from `Journal::status` / `history`; `kinds` keeps batches with at least one top-level op of those kinds (read each listed batch's records only when `kinds` is given). A batch without a label gets one: `"Move N files"`, `"Rate N items"`, `"Edit metadata"`, `"Upload N files"`, `"Relink N sidecars"` (by its first op's kind).
  - `auth.rs` inserts `Caller { user: UserId, key: KeyId }`; `/rpc` passes `origin_key(&caller.key)`; `/mcp` passes `origin_mcp(<Mcp-Session-Id header, else the key id>)`.
  - MCP instructions: "…you can undo it with organize_undo within 30 days."
- Remove: `BatchBook`, `RunningGuard`, `claim_running`, `write_header`, `finish`, `history`/`plan_undo`/`inverse*` over `JournalEntry`.

- [ ] **Step 1: Write the failing tests** (`tests/journal_files.rs` drives the Journal over a temp library; `tests/organize.rs` keeps its RPC-level tests and changes only how it reads the journal):
  - `move_then_undo_restores_file_sidecar_and_cache`
  - `trash_then_undo_restores_the_path`
  - `undo_skips_a_file_moved_since` (move A→B, move B→C in a 2nd batch, undo the 1st → its move skipped `PreconditionFailed`, C intact)
  - `playlist_rewrite_is_undone_unless_changed` (music ext: move updates the playlist; undo restores it; with the playlist edited in between, the rewrite is skipped and the edit stays)
  - `an_interrupted_batch_is_listed_and_undoable` (a fail point stops after op 2 of 3 and the store is reopened → history shows `complete: false`, `done: 2`; undo moves both back)
  - `undo_is_for_the_same_user` (agent batch for user U undone by U as a person → ok; user V → FORBIDDEN code)
  - `an_old_batch_cannot_be_undone` (clock advanced 31 days → UNDO_EXPIRED code)
  - `history_filters_by_kinds_and_labels_unnamed_batches`
  - `a_long_batch_answers_running_then_status_finishes` (wait set to 50 ms)
- [ ] **Step 2: Run** `cargo test -p asteria-common-server --test journal_files --test organize --test mcp` — Expected: FAIL.
- [ ] **Step 3: Implement.** Keep `FailAt` fail points working inside `move`'s `apply`.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** — `feat(server): organize runs its batches in the corundum Journal`.

---

### Task 4: Sidecar writes and ratings through the Journal

**Files:**
- Create: `crates/asteria-common-server/src/journal/sidecar.rs`
- Modify: `crates/asteria-common-core/src/meta.rs` (`update_sidecar` loses the journal and `actor`), `crates/asteria-common-server/src/writer.rs`, `rpc.rs`
- Modify: `crates/asteria-common-client/src/queue/mod.rs` (BUSY is a retry)
- Test: `crates/asteria-common-server/tests/journal_sidecar.rs` (new), `tests/writer.rs`, `tests/ops.rs`, `crates/asteria-common-core/tests/meta.rs`, `crates/asteria-common-client/tests/op_queue.rs`

**Interfaces:**
- Consumes: Task 2, Task 3's `Writer` job shape.
- Produces:
  - core: `pub fn update_sidecar(&self, media_rel: &Path, f: impl FnOnce(&mut Sidecar)) -> Result<bool>` (no journal, no actor; `NoStateDir` is no longer raised here).
  - `write_sidecar` — plan `{path, set: {tags?, description?, label?}}`, done `{path, before: {..same keys..}, after: {..}}`; skipped `PreconditionFailed` when, at undo, the current values of those keys are not `after`. Inverse sets `before` with precondition `after`.
  - `rate` — plan `{path, user, value, at_ms}`, done `{path, user, before: Option<{value, at_ms}>, after: Option<{value, at_ms}>}` (`before == after` when superseded); inverse restores `before` with precondition `after`, and changes nothing when they are equal. Rejections: not_found → `Skip::NotFound`, read_only → `Skip::Denied`, invalid → `Skip::InvalidPlan(msg)`; mapped back for `RatingOpResult`.
  - `media.update` = one batch: `write_sidecar` (if any of tags/description/label) then `rate` (if rating). `ops.apply` = one batch of `rate`s.
  - Writer jobs carry a deadline: a job taken off the queue after `APPLY_WAIT` (10 s) is answered BUSY without running (`media.update`, `ops.apply`).
  - Client queue: a whole-call BUSY from `ops.apply`, and a per-item BUSY from the `media.update` fallback, back off and retry the batch (no `RatingRejected`).

- [ ] **Step 1: Write the failing tests:**
  - `metadata_edit_undoes_to_the_old_values`
  - `undo_skips_a_rating_changed_since` (rate 3, rate 5 in a later batch, undo the first → skipped, rating stays 5)
  - `a_superseded_rating_undoes_to_nothing`
  - `update_is_busy_behind_a_long_batch` (a held organize batch; `media.update` with a short test deadline → BUSY; after the batch, the same call applies)
  - `ops_apply_results_keep_their_wire_values` (applied / superseded / rejected not_found, read_only)
  - client: `busy_is_retried_not_dropped` (stub answers BUSY once, then applies → pending reaches 0, no notice)
- [ ] **Step 2: Run** `cargo test -p asteria-common-server --test journal_sidecar --test writer --test ops` and `cargo test -p asteria-common-client --test op_queue` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** — `feat(server): metadata edits and ratings are undoable journal operations`.

---

### Task 5: Uploads and relinks through the Journal

**Files:**
- Modify: `crates/asteria-common-server/src/upload_commit.rs`, `hashing.rs`, `writer.rs`, `journal/files.rs`
- Test: `crates/asteria-common-server/tests/upload_commit.rs`, `tests/hashing.rs`, `tests/upload_http.rs`

**Interfaces:**
- Consumes: Task 3 `trash`, `move`.
- Produces:
  - `upload` — plan `{staged, to, session}` (the staged file the first phase prepared), done `{to, item_id}`; inverse = `trash` of `to`. The commit's bookkeeping (tombstones, stored answer) stays outside the operation. One batch per commit; actor = the committing user; origin as the caller's.
  - `relink_sidecar` — plan `{from, to}` (sidecar paths), done = plan; inverse = `{from: to, to: from}`, skipped `Exists`/`NotFound` as a move would. One batch per relink pass, origin `scan`, actor = the library owner as a person.

- [ ] **Step 1: Write the failing tests:** `an_upload_can_be_undone_to_the_trash`; `a_relink_can_be_undone`; the existing upload and hashing tests read the journal through `history`/`status`.
- [ ] **Step 2: Run** `cargo test -p asteria-common-server --test upload_commit --test hashing --test upload_http` — Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** — `feat(server): uploads and relinks are undoable journal operations`.

---

### Task 6: Remove asteria's own journal

**Files:**
- Delete: `crates/asteria-common-core/src/journal.rs`, `crates/asteria-common-core/tests/journal.rs`
- Create: `crates/asteria-common-core/src/actor.rs` (`Actor`, kept)
- Modify: `crates/asteria-common-core/src/{lib.rs,library.rs,paths.rs,error.rs,dirs.rs}`, every `use asteria_common_core::journal::…` in the workspace, `photos/headless/tests/e2e.rs`
- Modify: `CHANGELOG`/README only if they mention the journal format

**Interfaces:**
- `asteria_common_core::Actor` stays (moved to `actor.rs`, same serde form). `OpId`, `BatchId`, `OpKind`, `JournalEntry`, `Journal`, `Library::journal` are gone; the server uses corundum's ids. `paths::trash_rel(id: &str, rel: &str) -> String`. `Error::JournalSerialize` and `Error::NoStateDir` go if nothing raises them.

- [ ] **Step 1:** Delete the module and fix every use; `rg -n "journal::|JournalEntry|Library::journal|\.journal\(\)" crates photos music` finds nothing outside `asteria-common-server/src/journal/`.
- [ ] **Step 2: Run** `cargo test --workspace`, both clippy runs, `cargo fmt --all --check`, `cargo deny check`. Expected: all clean.
- [ ] **Step 3: Commit** — `refactor(core): drop asteria's own journal for corundum's`.
