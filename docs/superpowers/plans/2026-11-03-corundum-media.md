# asteria onto corundum's image and audio crates Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The server makes image thumbnails in its own process with `corundum-framework-image`, keeping the external tools only for what it cannot decode. asteria-music plays through `corundum-framework-audio`'s engine, keeping only what is asteria's own.

**Architecture:**
- **Thumbnails:** the `(Image, Thumb | Preview)` arm of `process.rs` first calls `corundum_framework_image::thumbnail` then `encode_webp` on a blocking thread, and falls back to the external tool (vipsthumbnail, or `sips` on macOS) when that fails. The music server (embedded covers) therefore needs no tool, and photos needs vipsthumbnail only for HEIC, AVIF and ORF.
- **Music:** the music GUI's `player` module shrinks to asteria's own parts:
  - its HTTP opener (`HttpOpener` as `Opener<QueueEntry>`, `Stream` as `Watch`, `StreamReader` as `Source`);
  - `QueueEntry` (as `QueueItem`);
  - `Endpoint`/`Quality`, now held in settings the app shares with the opener.

  Everything else comes from corundum.

**Tech Stack:** Rust 1.99 / edition 2024; corundum-framework-image (`raw`, `webp`), corundum-framework-audio (`cpal`, `test-support` in tests); tokio (server).

**Spec:** corundum `docs/superpowers/specs/2026-11-02-media-image-audio-design.md`, "Order" item 3 (in the corundum repository), and asteria's `docs/superpowers/specs/2026-10-22-asteria-design.md` §4.5 (media processing), which this plan updates.

**Prerequisite (the user's action):** a corundum release that publishes `corundum-framework-image` and `corundum-framework-audio` to crates.io, i.e. merging corundum's release-plz PR once it lists both. Task 1 starts from whatever versions that release published (look them up on crates.io; the facade is expected at 0.7.1).

## Global Constraints

- asteria depends on the published corundum crates (no `path`/`git` dependencies in the final branch).
- Thumbnails keep their current output: lossy WebP, quality 80, no metadata, sRGB, the longest side shrunk to `Preset::size()` (Thumb 256, Preview 1920) and never enlarged, EXIF orientation applied.
- In-process decoding is bounded by `corundum_framework_image::Limits` tightened to 256 MiB of file and 64 MP for small home servers (the default 512 MiB / 200 MP can take 2-3 GB per decode) and runs on `tokio::task::spawn_blocking`, inside the existing per-render permit and `timeout`; the blocking task returns the WebP bytes and only the async side writes the temporary file, so a timed-out decode never writes after cleanup.
- The external tool stays for what the in-process path cannot do: any `corundum_framework_image::Error` except `Io` falls back to the tool when one is found, and with no tool the render is `ProcessError::Unprocessable` with the in-process error as the reason. `Io` (the source cannot be read) is `ProcessError::Io`, as today.
- Video thumbnails (ffmpeg) and offline audio are unchanged.
- The music GUI keeps every behaviour its tests pin today: streaming in `Range` chunks into the per-process stream directory, preload after the current track is fetched, Low quality asking for `offline-audio`, the 3 s "previous" rule, three unplayable tracks stopping playback.
- Changing the server (`SetEndpoint`'s old behaviour) still stops playback, empties the queue, clears the error and forgets every fetched track.
- Tests never write into the developer's real config or data dirs; no test needs a sound device or ffmpeg/vips unless it already did.

## Review Focus

1. **A HEIC, AVIF or ORF photo on a machine with vipsthumbnail (or macOS `sips`)** still gets a thumbnail through the tool. → Task 1 `an_image_the_crate_cannot_decode_goes_to_the_tool` (a fake tool script, as the existing vips tests use).
2. **The same photo with no tool at all** is `Unprocessable` with a reason naming what failed, not a panic or an `Io` error that is retried forever. → Task 1 `with_no_tool_an_undecodable_image_is_unprocessable`.
3. **A huge image (over 64 MP) or a decompression bomb** does not take the server's memory: it is refused in-process and handed to the tool, which streams it. → Task 1 `a_picture_over_the_limits_goes_to_the_tool`.
4. **The server is changed while a track plays or is being fetched**: playback stops, the queue is empty, no error stays, and the old server's temporary files are deleted. → Task 3 `changing_the_server_stops_and_forgets` (the app-level test, through the shared settings and `Command::Reset`).
5. **An embedded cover in a music file** (bytes, no extension or a wrong one) still becomes a thumbnail with no external tool. → Task 1 `an_embedded_cover_needs_no_tool`.

---

### Task 1: In-process image thumbnails on the server

**Files:**
- Modify: `Cargo.toml` (workspace: the corundum versions; add `corundum-framework-image = { version = "<published>", features = ["raw", "webp"] }` and, for Task 3, `corundum-framework-audio`)
- Create: `crates/asteria-framework-server/src/inprocess_image.rs`
- Modify: `crates/asteria-framework-server/{Cargo.toml,src/lib.rs,src/process.rs}`
- Test: `crates/asteria-framework-server/src/process.rs`'s tests (or `tests/` where the existing render tests live)

**Interfaces:**
- Produces: `inprocess_image::render(src: InSource, longest: u32) -> Result<Vec<u8>, corundum_framework_image::Error>` where `InSource` is `Path(PathBuf)` or `Bytes { bytes: Vec<u8>, ext: Option<String> }` (owned, for `spawn_blocking`); quality `80.0`; the server limits (64 MP, 256 MiB).
- `Processor`'s public API does not change. `with_tools`/`with_image_tool` keep working, and tests use them to make the fallback tool a fake script.

- [ ] **Step 1: Write the tests** (images generated in the test with the `image` crate, as the existing tests and `sips.rs` do; read the WebP back with `image`/`webp` to check):

```rust
#[tokio::test] async fn a_jpeg_thumbnail_is_made_in_process() {
    // Processor whose image tool is a script that fails if run (e.g. `false`): a 1000×500 JPEG,
    // Thumb → a WebP 256×128; the tool was never run.
}
#[tokio::test] async fn a_small_picture_is_not_enlarged() { /* 100×50 PNG, Preview → 100×50 */ }
#[tokio::test] async fn the_exif_orientation_is_applied() { /* a 200×100 JPEG with EXIF orientation 6, Thumb → a WebP 128×256 (portrait) */ }
#[tokio::test] async fn an_embedded_cover_needs_no_tool() { /* render_image_bytes(png bytes, ext "jpg" (wrong), …) → WebP; tool never run */ }
#[tokio::test] async fn an_image_the_crate_cannot_decode_goes_to_the_tool() {
    // A file named x.heic holding bytes the crate reports Unsupported for; the image tool is a fake
    // script that writes a valid WebP to its output: the render succeeds and the script ran once.
}
#[tokio::test] async fn a_picture_over_the_limits_goes_to_the_tool() {
    // A PNG whose header claims 20000×20000 (over the limit; written by hand, a few hundred bytes):
    // the fake tool runs and its WebP is the result.
}
#[tokio::test] async fn with_no_tool_an_undecodable_image_is_unprocessable() {
    // Same .heic bytes, the image tool a path that does not exist: Err(ProcessError::Unprocessable(reason)),
    // the reason non-empty; the unprocessable hook is called once.
}
#[tokio::test] async fn a_missing_source_is_an_io_error() { /* render of a path that does not exist → ProcessError::Io */ }
```
  Fill each body with the assertions its comment states. Existing vips/sips tests that relied on the tool for JPEG/PNG now pass through the in-process path: keep them by giving them an input the crate cannot decode, or by asserting the in-process result, so each still tests what its name says.
- [ ] **Step 2: Run to see them fail.** `cargo test -p asteria-framework-server process` → the in-process tests fail (the tool is run, or does not exist).
- [ ] **Step 3: Implement** `inprocess_image::render` and the new image arm: in-process in `spawn_blocking` under the same `timeout` (on timeout `ProcessError::TimedOut`, as the tool path), then on a non-`Io` error the existing tool path with the staged copy, else the result. A panic in the blocking task (a `JoinError`) is treated like a decode error (falls back). Log at `debug` which path made a thumbnail.
- [ ] **Step 4: Run.** `cargo test -p asteria-framework-server` and `cargo test -p asteria-music-server -p asteria-photos-server` → all pass; `cargo clippy --workspace --all-targets -- -D warnings` and `cargo deny check` (rawler's LGPL-2.1 needs the same per-crate exception corundum's `deny.toml` has; the photos and music server binaries are GPL-3.0-or-later apps, so LGPL is compatible — say so in the exception's comment) → clean.
- [ ] **Step 5: Commit.** `feat(server): image thumbnails are made in-process; the tools only for what the crate cannot read`

### Task 2: Tools, packaging and docs follow

**Files:**
- Modify: `music/crates/asteria-music-desktop` (or wherever music's `LocalServerSpec { tools, .. }` is) → `tools: &[]`; photos keeps `[Vipsthumbnail, Ffmpeg]`
- Modify: `crates/asteria-framework-desktop/i18n/{en,ja}/desktop.ftl` (`asteria-desktop-tools-vips-purpose`)
- Modify: `packaging/fetch-tools.sh`, `packaging/LICENSES-tools.md`, `.github/workflows/release.yml` (the music package carries no vips)
- Modify: `docs/superpowers/specs/2026-10-22-asteria-design.md` (§4.5 and the table at line ~106), `README.md`/`README.ja.md` where they list the tools

- [ ] **Step 1: Test first.** Whatever test pins music's tool list today (search `tools` in the music desktop tests and `asteria-framework-desktop` tests) is changed to expect none, and the About page's licence line for music names no tool; run → fails.
- [ ] **Step 2: Implement.** Strings (en / ja): `asteria-desktop-tools-vips-purpose = Thumbnails of HEIC, AVIF and some camera RAW files.` / `HEIC・AVIF と一部のカメラの RAW のサムネイル。`. `fetch-tools.sh`: vips only for photos on Windows. Spec §4.5:
  - images are decoded in the process (corundum-framework-image: JPEG, PNG, WebP, GIF, TIFF, and RAW previews), and vips/sips are a fallback;
  - the line saying the server reads originals only in child processes is revised: the in-process decoders are pure Rust, bounded by `Limits`, and a panic falls back;
  - record the date (2026-11-03) and this plan.
- [ ] **Step 3: Run.** `cargo test --workspace` (to a file; read the tail), `cargo clippy --workspace --all-targets -- -D warnings`, `bash -n packaging/fetch-tools.sh`, and `actionlint` if installed → pass.
- [ ] **Step 4: Commit.** `docs, ci: vips is only for HEIC, AVIF and some RAW; music needs no tool`

### Task 3: asteria-music plays through corundum-framework-audio

**Files:**
- Delete: `music/crates/asteria-music-gui/src/player/{decode,resample,engine,thread,output,fakes}.rs`, `tests/decode.rs`, `tests/fixtures/` (the ones only `decode.rs` read; keep any `feed.rs`'s tests read, e.g. `tone.flac`)
- Modify: `music/crates/asteria-music-gui/src/player/{mod.rs,feed.rs,stream.rs}`, `src/{app.rs,library.rs,now_playing.rs,queue.rs}`, `tests/{playback.rs,library.rs,support/mod.rs}`, `Cargo.toml`

**Interfaces:**
- Consumes (corundum-framework-audio): `Engine`, `Player<T>`, `Command<T>` (`Replace { items, start }`, …, `Reset`), `PlayerState<T>`, `Status`, `PlayerError`, `QueueItem`, `Opener<T>`, `Feed`, `FeedEvent`, `TrackError`, `Wait`, `OutputConfig`, `feed::{spawn_feed, Watch, SourceStatus, FailedFeed}`, `decode::Source`, `CpalOutputs`; in tests `fakes::{FakeOpener, FakeItem, FakeOutputs, item}`.
- Produces (asteria's `player` module):
  - `QueueEntry: QueueItem` — `title()` is `title`, `duration_ms()` is the tag's; derives `PartialEq, Debug` as now.
  - `OpenerSettings` (`Clone`, an `Arc<Mutex<…>>` of `Option<Endpoint>` and `Quality`): `new()`, `set_endpoint(Option<Endpoint>)`, `set_quality(Quality)`, `endpoint() -> Option<Endpoint>`, `quality() -> Quality`.
  - `HttpOpener::new(settings: OpenerSettings)`, `impl Opener<QueueEntry>`.
  - `impl Watch for Stream`, `impl Source for StreamReader` (its `canceller()` wraps `ReaderCancel::cancel`).
  - Type aliases `pub type Command = corundum_framework_audio::Command<QueueEntry>;`, `PlayerState`, `Player` likewise, so the rest of the crate keeps its names.
  - In `tests/support`: `EntryOpener(FakeOpener)`, an `Opener<QueueEntry>` that maps an entry to `FakeItem { id: media.id, title, duration_ms }` and delegates — so the app-level tests keep scripting tracks by id.
- The app (`app.rs`):
  - changing the server becomes `settings.set_endpoint(e)` then `player.send(Command::Reset)`;
  - changing the quality becomes `settings.set_quality(q)`, with no command;
  - the device is `corundum_framework_audio::CpalOutputs`.

- [ ] **Step 1: Move the tests to the new API first.** `feed.rs`'s HTTP tests construct `HttpOpener::new(settings)` with the settings holding the endpoint and quality. `tests/playback.rs`'s quality test sets it through the settings and asserts the next fetch asks for `offline-audio`. Add `changing_the_server_stops_and_forgets` at app level, through `tests/support`:
  - play a track, then change the server;
  - state: `Stopped`, empty queue, `current == None`, `error == None`;
  - the stream directory holds no `.part` files within 2 s.
- [ ] **Step 2: Run to see them fail** (do not compile). `cargo test -p asteria-music-gui`.
- [ ] **Step 3: Implement.** Delete the moved files, depend on `corundum-framework-audio = { workspace = true, features = ["cpal"] }` (dev: `test-support`), and drop the music GUI's direct `symphonia`, `ogg`, `opus-pure`, `rubato`, `cpal` and `rtrb` dependencies if nothing else uses them. `stream.rs`'s status maps onto `SourceStatus` (`Waiting(w)` → `Waiting(w)`, `Failed(e)` → `Failed(e)`, else `Ready`). `HttpOpener` reads the settings when it starts a fetch (endpoint and quality as of that moment), and its failed starts return `FailedFeed`.
- [ ] **Step 4: Run.** `cargo test -p asteria-music-gui` (to a file; read the tail), then `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo deny check` → all pass.
- [ ] **Step 5: Commit.** `refactor(music): play through corundum-framework-audio; the player module keeps the HTTP opener and the queue entry`

## Out of scope

- Previews in a detail pane that takes bytes from the server. asteria does not use corundum's `FileExplorer` yet (photos has its own detail view of server-made previews); this belongs with corundum #97 when it does.
- HEIC and AVIF in-process.
- The music player's media-session (OS media keys) wiring.
