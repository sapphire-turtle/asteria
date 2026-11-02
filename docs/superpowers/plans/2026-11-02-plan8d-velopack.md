# Plan 8d: Velopack distribution — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `asteria-photos` and `asteria-music` desktops as Velopack packages (install, self-update, PATH, single instance, bundled tools) and the headless binaries as release files with an `update` subcommand, all from one `v<version>` tag.

**Architecture:** The runtime comes from corundum-framework 0.7 (`package::start`, `Updater`, `cli_path`, `console::forward`, `Services`, `instance`, `launcher`, `UpdatesPanel`); asteria wires it into a shared `package` module in `asteria-framework-desktop` that both desktop `main`s call. The server learns two things: tools bundled beside its executable, and a `sips` image backend on macOS. A tag-triggered `release.yml` packs both apps on three OSes (after corundum's `demo-release.yml`) and attaches the headless binaries through sapphire-turtle's shared `release-binary.yml`.

**Tech Stack:** Rust 1.99 / edition 2024, corundum-framework 0.7.0 (`gui-package`, `single-instance`, `console`, `service`), corundum-framework-update 0.7.0 (`cli`), corundum-framework-build 0.7.0, `webp` 0.3, `image` 0.25 (`jpeg`), Velopack `vpk` 1.2.161, GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-10-22-asteria-design.md` §5.9 (and §4.5's `sips` bullet, §5.8).

## Global Constraints

- packId `asteria-photos-desktop` / `asteria-music-desktop`; data-dir / `PackageSpec::app_name` stays `asteria-photos` / `asteria-music`.
- Channels `<app>-<os>`: `photos-linux`, `photos-osx`, `photos-win`, `music-linux`, `music-osx`, `music-win`.
- Command name `asteria-photos` / `asteria-music`. Cargo binaries keep their names (`asteria-<app>` headless, `asteria-<app>-desktop`, new `asteria-<app>-console`); the package renames (macOS/Linux: desktop → `asteria-<app>`; Windows: shim → `asteria-<app>.exe`, `--mainExe asteria-<app>-desktop.exe`).
- `--packTitle` / launcher name `Asteria Photos` / `Asteria Music`; launcher id `asteria-photos` / `asteria-music`, `bundle_prefix` `net.sapphire-turtle`, macOS `--bundleId net.sapphire-turtle.asteria-<app>`.
- Releases live in `https://github.com/sapphire-turtle/asteria`, tag `v<workspace version>`; headless files `asteria-<app>-headless-<os>-<arch>[.exe]`.
- Tool lookup order: `ASTERIA_*` → beside the executable (`<exe dir>`, `<exe dir>/vips/bin`) → `PATH` → well-known dirs.
- sips path: no enlarging; EXIF orientation applied in Rust; WebP quality 80 via the `webp` crate; Rust only reads the JPEG sips wrote.
- Signing only when the secrets exist (`MACOS_CERTIFICATE` and the rest, as in corundum's demo); otherwise unsigned.
- `vpk` 1.2.161 (same as the `velopack` crate corundum-framework-package uses).
- Tokens are never logged. OS-specific APIs stay in corundum. Tests never write the developer's real config/data dirs.

## Review Focus

- The `serve` the service manager starts (Windows: `current\asteria-<app>-desktop.exe serve`) must never pass through `package::start` — it would restart its own service in a loop. Task 4 pins it (`entry` test).
- `--veloapp-install` and friends must reach `package::start` before the "any argument means CLI" branch; clap must never see them. Task 4 pins it.
- A portrait iPhone JPEG/HEIC (EXIF orientation 6) must come out portrait on the sips path. Task 2 pins it.
- The executable's directory has spaces in a package (`Asteria Photos.app/Contents/MacOS`, `%LocalAppData%\…\current`); bundled lookup must still find the tools. Task 1 pins it.
- `update` run from a desktop build must never download and install the headless binary over the GUI. Task 3 pins it.

---

### Task 1: Find tools bundled beside the executable

**Files:**
- Modify: `crates/asteria-framework-server/src/process.rs` (`find_tool`, `tool_to_run`, `find_program`, `resolve_program` and their tests)

**Interfaces:**
- Produces: `fn bundled_dirs(exe_dir: &Path) -> Vec<PathBuf>` (`[exe_dir, exe_dir/vips/bin]`); `find_program(program: &OsStr, before: &[PathBuf], path_var: Option<&OsStr>, after: &[PathBuf]) -> Option<PathBuf>`; `find_tool(Tool)` and `tool_to_run(Tool)` now search `bundled_dirs(<current_exe's dir>)` before `PATH`. Public signatures of `find_tool` / `tool_program` unchanged.

- [ ] **Step 1: Write the failing tests** (in `process.rs`'s test module, next to the existing `find_program` tests; make each fake tool with the existing test helper that writes an executable file, or `fs::write` + permissions)

```rust
#[test]
fn a_tool_beside_the_executable_wins_over_path() {
    let exe_dir = tempdir_with_space(); // a TempDir with a child "Asteria Photos.app/Contents/MacOS"
    let on_path = tempfile::tempdir().unwrap();
    let bundled = fake_tool(&exe_dir, "ffmpeg");
    fake_tool(on_path.path(), "ffmpeg");
    let found = find_program(OsStr::new("ffmpeg"), &bundled_dirs(&exe_dir), Some(on_path.path().as_os_str()), &[]);
    assert_eq!(found, Some(bundled));
}

#[test]
fn vipsthumbnail_is_found_in_its_bundled_bin_folder() {
    // exe_dir/vips/bin/vipsthumbnail[.exe]
    let found = find_program(OsStr::new("vipsthumbnail"), &bundled_dirs(&exe_dir), None, &[]);
    assert_eq!(found, Some(exe_dir.join("vips").join("bin").join(exe_name("vipsthumbnail"))));
}

#[test]
fn a_path_in_the_variable_is_used_as_it_is() {
    // program with a directory part ignores `before`, PATH and `after`, as today
}
```

`exe_name` adds `.exe` on Windows. Keep the existing tests compiling by passing `&[]` for `before`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p asteria-framework-server process::tests -- bundled beside`
Expected: compile error (`bundled_dirs` missing, `find_program` arity).

- [ ] **Step 3: Implement** `bundled_dirs`, the new `find_program` order (`before` → `PATH` → `after`), and have `find_tool` / `tool_to_run` / `resolve_program` pass `bundled_dirs(dir)` where `dir` is `std::env::current_exe()`'s parent (no canonicalizing: a Windows `\\?\` path or a resolved symlink would move the search). Update the module doc of `find_tool` to list the order from Global Constraints.

- [ ] **Step 4: Run the server's tests**

Run: `cargo test -p asteria-framework-server`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/asteria-framework-server/src/process.rs
git commit -m "feat(server): find the external tools bundled beside the executable first"
```

---

### Task 2: The `sips` image backend on macOS

**Files:**
- Create: `crates/asteria-framework-server/src/sips.rs`
- Modify: `crates/asteria-framework-server/src/process.rs` (image tool selection, `run_sips`), `crates/asteria-framework-server/src/lib.rs` (exports), `crates/asteria-framework-server/Cargo.toml` (`webp = "0.3"`, `image` features `["webp", "jpeg"]` for this crate), root `Cargo.toml` (`webp` in `[workspace.dependencies]`)
- Modify: `crates/asteria-framework-desktop/src/tools.rs`, `crates/asteria-framework-desktop/i18n/{en,ja}/desktop.ftl`

**Interfaces:**
- Consumes: Task 1's `find_tool`.
- Produces:
  - `pub enum ImageTool { Vips(OsString), Sips(OsString) }` (exported from the crate root).
  - `pub fn find_image_tool() -> Option<ImageTool>`: `find_tool(Tool::Vipsthumbnail)` as `Vips`, else on macOS `/usr/bin/sips` if it is a file, as `Sips`; else `None`.
  - `Processor::with_image_tool(thumbs_dir: PathBuf, max_jobs: usize, timeout: Duration, image: ImageTool, ffmpeg: impl Into<OsString>) -> Processor`; `with_tools` delegates with `ImageTool::Vips`; `new` uses `find_image_tool()`, falling back to `ImageTool::Vips(tool_to_run(Tool::Vipsthumbnail))` (so a missing tool still fails as `ToolMissing("vipsthumbnail")`).
  - In `sips.rs`: `pub(crate) fn parse_dimensions(stdout: &str) -> Option<(u32, u32)>`, `pub(crate) fn target_size(width: u32, height: u32, longest: u32) -> u32` (the `-Z` value: `min(longest, max(width, height))`), `pub(crate) fn finish(jpeg: &[u8], quality: f32) -> Result<Vec<u8>, String>` (decode with `image`'s JPEG decoder, read its `orientation()`, `apply_orientation`, encode lossy WebP with `webp::Encoder::from_image(..).encode(quality)`).

- [ ] **Step 1: Write the failing unit tests** in `sips.rs` (all platforms)

```rust
#[test]
fn sips_dimensions_are_read() {
    let out = "/tmp/x.heic\n  pixelWidth: 6000\n  pixelHeight: 4000\n";
    assert_eq!(parse_dimensions(out), Some((6000, 4000)));
    assert_eq!(parse_dimensions("Error 13"), None);
}

#[test]
fn the_size_never_enlarges() {
    assert_eq!(target_size(6000, 4000, 256), 256);
    assert_eq!(target_size(400, 200, 1920), 400);
    assert_eq!(target_size(200, 400, 256), 256);
}

#[test]
fn finishing_applies_the_exif_orientation() {
    // a 400x200 JPEG from `image`, with an APP1 Exif segment (Orientation = 6) inserted after SOI
    let webp = finish(&jpeg_with_orientation(400, 200, 6), 80.0).unwrap();
    let img = image::load_from_memory_with_format(&webp, image::ImageFormat::WebP).unwrap();
    assert_eq!((img.width(), img.height()), (200, 400));
}

#[test]
fn finishing_garbage_is_an_error() {
    assert!(finish(b"not a jpeg", 80.0).is_err());
}
```

`jpeg_with_orientation` is a test helper: encode with `image::codecs::jpeg::JpegEncoder`, then splice `FF E1 <len> "Exif\0\0" II*\0 08000000 0100 1201 0300 01000000 <o> 0000 00000000` after the first two bytes.

- [ ] **Step 2: Write the failing macOS integration tests** in `process.rs`'s tests, `#[cfg(target_os = "macos")]`, through `Processor::with_image_tool(.., ImageTool::Sips("/usr/bin/sips".into()), "ffmpeg")` and the existing render-request helper the vips tests use:
  - `sips_renders_a_heic_thumbnail`: make a 1200×800 JPEG, convert it with `sips -s format heic`, render `Preset::Thumb` → a WebP whose longer side is 256.
  - `sips_keeps_a_portrait_photo_upright`: the orientation-6 JPEG from Step 1's helper, `Preset::Preview` → 200×400 (no enlarging, rotated).
  - `sips_marks_a_broken_file_unprocessable`: 100 random bytes named `x.jpg` → `ProcessError::Unprocessable`.

- [ ] **Step 3: Run to see them fail**

Run: `cargo test -p asteria-framework-server sips`
Expected: compile errors (`sips` module, `ImageTool` missing).

- [ ] **Step 4: Implement**
  - `sips.rs` with the three functions.
  - In `process.rs`: replace the `vips: OsString` field with `image: ImageTool`; `render_once` dispatches the image arm to `run_vips` or `run_sips` on the same staged copy.
  - `run_sips(&self, sips: &OsStr, src: &Path, tmp: &Path, preset: Preset)`: (1) `sips -g pixelWidth -g pixelHeight <src>`, stdout captured by a new `run_tool_stdout(cmd, name, timeout) -> Result<String, ProcessError>` that shares `run_tool`'s exit-code / not-found / timeout handling; (2) `sips -Z <target_size> -s format jpeg -s formatOptions 90 <src> --out <tmp>.sips.jpg`; (3) `finish(.., 80.0)` in `spawn_blocking`, written to `tmp`; the intermediate is removed whatever happens. Both sips runs share one timeout budget, as the ffmpeg retry does. A `finish` error is `Unprocessable`.
  - Export `ImageTool` and `find_image_tool` from `lib.rs`.

- [ ] **Step 5: The desktop's tool display**: in `tools.rs`, the `vipsthumbnail` entry shows, when `find_tool` finds nothing but `find_image_tool()` is `Some(ImageTool::Sips(_))`, the new string `asteria-desktop-tools-sips` instead of "missing" and its hint. en: `Not installed: images are made with sips, which macOS has built in.` ja: `入っていません。画像は macOS に入っている sips で作ります。`

- [ ] **Step 6: Run the tests**

Run: `cargo test -p asteria-framework-server && cargo test -p asteria-framework-desktop`
Expected: PASS (the macOS-only tests run on the macOS CI job).

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock crates/asteria-framework-server crates/asteria-framework-desktop
git commit -m "feat(server): make image thumbnails with sips on macOS when vipsthumbnail is missing"
```

---

### Task 3: The `update` subcommand

**Files:**
- Modify: `crates/asteria-framework-server/src/cli.rs`, `crates/asteria-framework-server/Cargo.toml` (`corundum-framework-update = { version = "0.7.0", features = ["cli"] }`, workspace entry in root `Cargo.toml`)
- Modify: `photos/crates/asteria-photos-server/src/lib.rs`, `music/crates/asteria-music-server/src/lib.rs` (`release_asset: None` in `spec()`), `photos/headless/src/main.rs`, `music/headless/src/main.rs`, every other `AppSpec { .. }` literal (tests)

**Interfaces:**
- Produces: `AppSpec::release_asset: Option<&'static str>` — the release file base this binary updates from (`Some("asteria-photos-headless")` in the headless `main`s, `None` elsewhere); `fn release_source(spec: &AppSpec) -> ReleaseSource`.

- [ ] **Step 1: Write the failing tests** in `cli.rs`

```rust
#[test]
fn update_takes_check_and_yes() {
    let m = command(&spec()).try_get_matches_from(["x", "update", "--check", "-y"]).unwrap();
    let (name, m) = m.subcommand().unwrap();
    assert_eq!(name, "update");
    let args = UpdateArgs::from_arg_matches(m).unwrap();
    assert!(args.check && args.yes);
}

#[test]
fn only_a_binary_with_a_release_asset_replaces_itself() {
    let mut s = spec();
    assert_eq!(release_source(&s).asset_base(), None);
    s.release_asset = Some("asteria-photos-headless");
    assert_eq!(release_source(&s).asset_base(), Some("asteria-photos-headless"));
    assert_eq!(release_source(&s).repository(), "https://github.com/sapphire-turtle/asteria");
}

#[test]
fn a_build_without_a_release_asset_points_to_its_installer() {
    // dispatch(update without --check) on a spec with release_asset None returns 1 and does
    // no network call: assert the returned code via a helper `update(spec, &args)` that is
    // split out of dispatch and takes no network path for this case.
}
```

Check `repository()`'s exact form against corundum-framework-update 0.7 before asserting it.

- [ ] **Step 2: Run to see them fail**

Run: `cargo test -p asteria-framework-server cli::tests::update cli::tests::only cli::tests::a_build`
Expected: compile errors.

- [ ] **Step 3: Implement**
  - The `update` subcommand: `UpdateArgs::augment_args(Command::new("update").about("Replace this program with its newest release"))`.
  - `fn update(spec: &AppSpec, args: &UpdateArgs) -> AnyResult<i32>`:
    - `release_asset == None` and not `--check`: print `{program} is updated by the app it came with (Settings › About), or by reinstalling it` and return 1.
    - Otherwise: record the current executable's modified time, run `corundum_framework_update::run(args, &release_source(spec), &program_name(spec), env!("CARGO_PKG_VERSION"))` on a current-thread tokio runtime. If it returns 0 and the modified time changed, print `Restart a running \`{program} serve\` (or its background service) to use the new version.`
  - The headless `main`s set `release_asset` on the spec before `cli::main`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p asteria-framework-server -p asteria-photos-headless -p asteria-music-headless`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/asteria-framework-server photos music
git commit -m "feat(cli): an update subcommand that replaces a release binary with the newest release"
```

---

### Task 4: The desktop's package runtime (`asteria-framework-desktop::package`)

**Files:**
- Create: `crates/asteria-framework-desktop/src/package.rs`, `crates/asteria-framework-desktop/src/about.rs`
- Modify: `crates/asteria-framework-desktop/src/lib.rs`, `crates/asteria-framework-desktop/Cargo.toml` (corundum features add `gui-package`, `single-instance`, `console`), `crates/asteria-framework-desktop/i18n/{en,ja}/desktop.ftl`, `deny.toml` (the four velopack entries from corundum-framework's `deny.toml`, reasons copied, "Drop with corundum-framework#64")

**Interfaces:**
- Consumes: `LocalServerSpec` (existing), `asteria_framework_server::cli::main`.
- Produces:
  - `pub enum Entry { Serve, Cli, Window }`; `pub fn entry(args: &[OsString]) -> Entry` — first argument `serve` → `Serve`; any argument → `Cli`; none → `Window`.
  - `pub fn start(local: &'static LocalServerSpec, package: &'static PackageSpec) -> Instance` — the start-up order of spec §5.9: `Serve` → `cli::main((local.server)())`; else `corundum_framework::package::start(package)`; `Cli` → `console::attach_parent()` then `cli::main`; `Window` → `instance::acquire_or_activate(local.app)` (none: `std::process::exit(0)`), then `start_updates(package)`, returning the instance.
  - `pub fn updater() -> Option<Updater>`; `fn start_updates(package: &'static PackageSpec)` (`Updater::new(package, source())`, `apply_pending_at_start`, `run_in_background(24 h)`, kept in a `OnceLock`; errors logged with `tracing::warn!`).
  - `fn is_prerelease(version: &str) -> bool`; `source()` is `Source::Github { repo: "https://github.com/sapphire-turtle/asteria", prerelease: is_prerelease(env!("CARGO_PKG_VERSION")) }`.
  - `pub struct AboutPage` implementing `ExtraSettingsPage`; `AboutPage::new(label: &'static str, cli: CliSpec, launcher: LauncherSpec) -> AboutPage`. The page: the label and version, an `UpdatesPanel` with `Target::this_app(label, updater(), UpdateTarget { label, current: Some(version), source: ReleaseSource::github("sapphire-turtle", "asteria"), exe: current_exe, after: AfterUpdate::Relaunch })` (made on first `ui`, so nothing checks before the page opens), `CliRow::new(cli)` except on Windows, `LauncherRow::new(launcher)`, and the licence line.
  - New strings (en / ja): `asteria-desktop-about` `About` / `情報`; `asteria-desktop-about-version` `Version { $version }` / `バージョン { $version }`; `asteria-desktop-about-licence` `Asteria is free software under GPL-3.0-or-later. Packages for Windows and macOS include ffmpeg (GPLv3) and, on Windows, libvips (LGPL); their licences and sources are listed in tools/LICENSES beside the program.` / `Asteria は GPL-3.0-or-later のフリーソフトウェアです。Windows と macOS のパッケージには ffmpeg（GPLv3）、Windows にはさらに libvips（LGPL）が入っています。それぞれのライセンスとソースの場所は、プログラムの横の tools/LICENSES にあります。`

- [ ] **Step 1: Write the failing tests** in `package.rs`

```rust
fn args(a: &[&str]) -> Vec<OsString> { a.iter().map(OsString::from).collect() }

#[test]
fn serve_starts_the_server_before_anything_else() {
    assert!(matches!(entry(&args(&["serve"])), Entry::Serve));
    assert!(matches!(entry(&args(&["serve", "--listen", "0.0.0.0:7810"])), Entry::Serve));
}

#[test]
fn a_velopack_hook_is_not_the_server() {
    // package::start handles it before the CLI ever sees it
    assert!(matches!(entry(&args(&["--veloapp-install", "0.1.0"])), Entry::Cli));
    assert!(matches!(entry(&args(&["api-key", "list"])), Entry::Cli));
}

#[test]
fn no_arguments_open_the_window() {
    assert!(matches!(entry(&[]), Entry::Window));
}

#[test]
fn a_pre_release_reads_pre_releases() {
    assert!(is_prerelease("0.1.0-rc.1"));
    assert!(!is_prerelease("0.1.0"));
}
```

And in `about.rs` an i18n test in the crate's existing style: every new id resolves in `en` and `ja`.

- [ ] **Step 2: Run to see them fail**

Run: `cargo test -p asteria-framework-desktop package`
Expected: compile errors.

- [ ] **Step 3: Implement** `package.rs` and `about.rs` as the Interfaces say; export `Entry`, `entry`, `start`, `updater`, `AboutPage` from `lib.rs`; add the `deny.toml` entries.

- [ ] **Step 4: Run the tests and the checks**

Run: `cargo test -p asteria-framework-desktop && cargo deny check`
Expected: PASS; cargo-deny clean.

- [ ] **Step 5: Commit**

```bash
git add deny.toml Cargo.lock crates/asteria-framework-desktop
git commit -m "feat(desktop): the package runtime — start order, single instance, updates, About page"
```

---

### Task 5: The photos and music desktops as packages

**Files:**
- Modify: `photos/desktop/src/main.rs`, `music/desktop/src/main.rs`, `photos/desktop/Cargo.toml`, `music/desktop/Cargo.toml`
- Create: `photos/desktop/src/bin/console.rs`, `music/desktop/src/bin/console.rs`, `photos/desktop/build.rs`, `music/desktop/build.rs`, `photos/desktop/examples/icons.rs`, `music/desktop/examples/icons.rs`

**Interfaces:**
- Consumes: Task 4's `start`, `AboutPage`; `LocalServerSpec` statics already in each `main.rs`.
- Produces: per app `static PACKAGE: PackageSpec` (`app_name` = `APP_NAME`, `cli: Some(CliSpec { command: "asteria-<app>" })`, `services: Services::new(&[service])`, hooks `None`) and `const LAUNCHER: LauncherSpec` (`id` `asteria-<app>`, `name` `Asteria Photos` / `Asteria Music`, `description` `Your photo library, served from your own computer` / `Your music library, served from your own computer`, `category` `Graphics` / `AudioVideo`, `icon_svg: include_bytes!("../../../docs/icons/asteria-<app>.svg")`, `bundle_prefix` `net.sapphire-turtle`); cargo bins `asteria-<app>-console`; the example `icons` writes `icon.ico`, `icon.icns`, `icon.png` (512 px) into the directory given, for Task 6.

- [ ] **Step 1: Write the failing tests** in each `main.rs`'s tests

```rust
#[test]
fn the_package_is_the_apps_command_and_server() {
    assert_eq!(PACKAGE.app_name, APP_NAME);
    assert_eq!(PACKAGE.cli.map(|c| c.command), Some("asteria-photos"));
    assert_eq!(LAUNCHER.id, "asteria-photos");
    assert_eq!(LAUNCHER.name, "Asteria Photos");
}
```

(music: `asteria-music`, `Asteria Music`.) Check `Services`' accessors in corundum-framework-package 0.7 and also assert the one service when it exposes them.

- [ ] **Step 2: Run to see them fail**

Run: `cargo test -p asteria-photos-desktop -p asteria-music-desktop`
Expected: compile errors.

- [ ] **Step 3: Implement**
  - `#![cfg_attr(windows, windows_subsystem = "windows")]` at the top of both `main.rs`.
  - `main`: `let instance = asteria_framework_desktop::start(&LOCAL, &PACKAGE);` then `run(instance)`; `run` passes `WindowOptions::new(APP_NAME).remember(..).instance(instance).launcher(LAUNCHER)`. The old "any argument means CLI" check goes away (Task 4's `start` does it).
  - The settings get `AboutPage::new("Asteria Photos", CliSpec { command: "asteria-photos" }, LAUNCHER)` after `LocalServerPage`.
  - `src/bin/console.rs`: on Windows `corundum_framework::package::console::forward("asteria-<app>-desktop.exe")`; elsewhere print `asteria-<app>-console is for Windows only: run asteria-<app>-desktop` and exit 2.
  - `build.rs`: `corundum_framework_build::embed_icon("../../docs/icons/asteria-<app>.svg");` (build-dependency `corundum-framework-build = "0.7.0"`).
  - `examples/icons.rs`: as corundum's `demo/examples/icons.rs`, from the same SVG.
  - Cargo: corundum features add `gui-package`, `single-instance`, `console`.
  - The existing window tests still construct `DesktopApp` directly; keep them passing.

- [ ] **Step 4: Run the tests and the builds**

Run: `cargo test -p asteria-photos-desktop -p asteria-music-desktop && cargo build -p asteria-photos-desktop -p asteria-music-desktop --bins --examples && cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings`
Expected: PASS; both `asteria-<app>-desktop` and `asteria-<app>-console` built; Windows clippy clean.

- [ ] **Step 5: Try the start order by hand** (Linux): `target/debug/asteria-photos-desktop --version` prints the version; starting the window twice brings the first to the front and the second exits.

- [ ] **Step 6: Commit**

```bash
git add Cargo.lock photos/desktop music/desktop
git commit -m "feat(desktop): photos and music start as Velopack packages, one window each, with an About page"
```

---

### Task 6: The release workflow, bundled tools and docs

**Files:**
- Create: `.github/workflows/release.yml`, `packaging/tools.env`, `packaging/fetch-tools.sh`, `packaging/LICENSES-tools.md`
- Modify: `README.md`, `docs/superpowers/specs/2026-10-22-asteria-design.md` (§5.9: the pinned tool versions, if they differ from what it says)

**Interfaces:**
- Consumes: Task 5's binaries and `icons` examples; sapphire-turtle/.github `release-binary.yml@v1` (inputs `tag`, `package`, `binary`, `asset`).

- [ ] **Step 1: Pin the tools** in `packaging/tools.env`: for each of `WIN_FFMPEG` (BtbN FFmpeg-Builds `win64-gpl`, a dated `autobuild-*` release, not `latest`), `WIN_VIPS` (libvips build-win64-mxe `vips-dev-w64-all-<version>.zip`), `MAC_FFMPEG_ARM64` and `MAC_FFMPEG_X86_64` (martin-riedl.de, the versioned URL the `latest` redirect points to) a `<NAME>_URL` and `<NAME>_SHA256`, computed by downloading each once (`curl -fsSL <url> | sha256sum`).

- [ ] **Step 2: Write `packaging/fetch-tools.sh <os> <dest>`** (`os`: `windows` | `macos`; macOS fetches both architectures and joins them with `lipo`): download each pinned file, check its SHA-256 (fail on mismatch), extract to `<dest>` as `ffmpeg[.exe]` and (Windows) `vips/bin/…` with its DLLs, copy `packaging/LICENSES-tools.md` and the archives' licence files to `<dest>/tools/LICENSES/`, then run `<dest>/ffmpeg -hide_banner -encoders` and fail unless it lists `libopus` and `libwebp`, and on Windows run `vips/bin/vipsthumbnail.exe --vips-version`.

- [ ] **Step 3: Check it where it can run here**: `bash -n packaging/fetch-tools.sh`, and `packaging/fetch-tools.sh windows /tmp/…` up to the checksum and extraction (the encoder check needs Windows; skip it when `uname` is not the target with a printed note).

- [ ] **Step 4: Write `release.yml`**, after corundum's `demo-release.yml`:
  - `on: push: tags: ['v*']`.
  - `check`: fail unless the tag equals `v` + the workspace version (`cargo metadata` / `Cargo.toml`); then `gh release create "$TAG" --verify-tag --generate-notes`, with `--prerelease` when the version has a `-`.
  - `pack` matrix: `app` ∈ {photos, music} × {ubuntu-22.04 → channel `<app>-linux`, macos-latest → `<app>-osx`, windows-latest → `<app>-win`}. Steps: toolchain, Linux deps (`libwayland-dev libxkbcommon-dev libasound2-dev`), dotnet 8, `vpk` 1.2.161, the build, `cargo run --release --locked -p asteria-<app>-desktop --example icons -- icons`, `packaging/fetch-tools.sh` into `pack/` (Windows, macOS), the macOS signing setup (copied from the demo, unchanged), then `vpk pack --packId asteria-<app>-desktop --packVersion <version> --packDir pack --mainExe <asteria-<app> | asteria-<app>-desktop.exe> --packTitle "Asteria <App>" --packAuthors sapphire-turtle --icon icons/<icon> --channel <app>-<os> --delta None --outputDir releases` (+ `--bundleId net.sapphire-turtle.asteria-<app>` on macOS, `--categories Graphics|AudioVideo` on Linux), and `gh release upload "$TAG" releases/*`.
  - macOS is one universal package: build `aarch64-apple-darwin` and `x86_64-apple-darwin` (`rustup target add x86_64-apple-darwin`) and `lipo -create` the desktop binary; `fetch-tools.sh macos` fetches both ffmpeg builds and `lipo -create`s them, checking each half's encoders before joining (the arm64 runner runs the x86_64 half under Rosetta).
  - `headless-photos`, `headless-music`: `uses: sapphire-turtle/.github/.github/workflows/release-binary.yml@v1` with `tag`, `package: asteria-<app>-headless`, `binary: asteria-<app>`, `asset: asteria-<app>-headless`, `needs: check`.
  - Every job pins actions to the majors the existing workflows use (`checkout@v7`, `setup-dotnet@v6`, `upload-artifact@v7`).

- [ ] **Step 5: Lint**: `python3 -c 'import yaml,sys; yaml.safe_load(open(".github/workflows/release.yml"))'` and `actionlint` if it is installed.
Expected: no errors.

- [ ] **Step 6: README**: install the desktop (links to the release's `Setup.exe`, `.pkg`, AppImage), that Windows and macOS include the tools and Linux uses the package manager, the headless release files and `asteria-<app> update`, and that the packages are unsigned until signing is set up (SmartScreen / Gatekeeper warnings).

- [ ] **Step 7: Commit**

```bash
git add .github/workflows/release.yml packaging README.md docs/superpowers/specs/2026-10-22-asteria-design.md
git commit -m "ci: release both desktops as Velopack packages and the headless binaries from one tag"
```

- [ ] **Step 8: The real run is a manual step for the user**: pushing a tag publishes a release, so ask before pushing `v0.1.0-rc.1` (on a branch whose workspace version is `0.1.0-rc.1`), then `rc.2`, and check install → update → uninstall on the three OSes. Record the result in the PR.
