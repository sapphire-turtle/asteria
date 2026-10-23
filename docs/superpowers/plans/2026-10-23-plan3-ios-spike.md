# Plan 3: iOS の検証用試作 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** spec §8 の4項目（iOS 上の egui、ロック中の音声再生とロック画面の操作、純 Rust の Opus デコード、PhotoKit での列挙と原本の取得）を確かめる試作アプリを作り、ユーザーが Mac と iPhone で確認して、結果を spec に反映する。

**Architecture:** 試作は `spikes/ios/` の独立した Cargo workspace に置き、本体の crate には依存しないし、本体からも依存されない（捨てるコード）。Opus の比較は `opus-probe`（どの OS でも動く lib と bin）、アプリは `asteria-ios-spike`（staticlib。デスクトップでも起動できる）に分ける。iOS 固有の処理は `#[cfg(target_os = "ios")]` の `ios` モジュールに閉じ込め、objc2 で UIKit、AVFAudio、MediaPlayer、Photos を呼ぶ。Xcode プロジェクトは XcodeGen の `project.yml` から生成し、`main.m` から Rust のエントリを呼ぶ。この Linux 機には Xcode がないので、iOS 向けのビルドと、シミュレーターでの起動とスクリーンショットは CI の macOS ジョブで確かめる。実機の確認はユーザーがチェックリストに沿って行う。

**Tech Stack:** Rust 1.99.0、eframe / egui 0.36（wgpu）、corundum-gui（git 参照）、cpal、objc2 0.6 と objc2-* 0.3（ui-kit、avf-audio、media-player、photos、foundation）、block2 0.6、blake3、ogg 0.9、Opus デコーダーの候補3つ（opus-decoder 0.1、opuscule 0.2、opus-pure 0.2）、XcodeGen、GitHub Actions（macos-latest、ubuntu-latest）。

**Spec:** `docs/superpowers/specs/2026-10-22-asteria-design.md`（特に §5.3、§5.4、§5.5、§7、§8）

## Global Constraints

- 試作は `spikes/ios/` に置く独立した workspace（`[workspace]` を持つ）。ルートの `Cargo.toml` には `exclude = ["spikes"]` を足す。ルートの crate から試作に、試作からルートの crate に依存しない。
- ツールチェインはルートの `rust-toolchain.toml`（1.99.0）をそのまま使う。edition 2024。
- 試作の crate はどれも `publish = false`、`license = "GPL-3.0-or-later"`。依存の license は GPL-3.0 と両立するもの（MIT、Apache-2.0、BSD、Zlib、MPL-2.0、CC0）に限る。`cargo deny` は試作には回さない。
- egui / eframe は 0.36 に固定する（corundum-gui に合わせる。0.37 は使わない）。eframe は `default-features = false` で `wgpu` と `default_fonts` を有効にし、Linux だけ `x11` と `wayland` を足す。
- corundum-gui は `{ git = "https://github.com/sapphire-turtle/sapphire-framework", rev = "bb7c34fbdf35ea7a46121aa70d53f3107d033619" }`（#321 の対応を含む）。
- iOS 固有のコードと `unsafe` は `src/ios/` の中だけに置く。例外は `asteria_spike_main` の `#[unsafe(no_mangle)]` だけ。
- Photos、MediaPlayer、AVAudioSession のコールバックは、どのスレッドで呼ばれてもよいように書く。egui の状態には直接触らず、`std::sync::mpsc` で UI に送り、`egui::Context::request_repaint` を呼ぶ。
- FFI の境界（`asteria_spike_main`、objc2 のブロック）では panic を外に出さない（`std::panic::catch_unwind` で受けてログに出す）。
- 原本の読み出しは、ファイル全体をメモリに載せずに、受け取った断片ごとに blake3 に流す。
- CI：`.github/workflows/ios-spike.yml` を新しく作り、`spikes/**` かこのワークフローが変わった PR で動かす。clippy は `-D warnings`、`cargo fmt --check` も通す。iOS 向け（`aarch64-apple-ios` と `aarch64-apple-ios-sim`）の clippy は macOS ジョブで行う。Linux 機での `cargo check --target aarch64-apple-ios` は、Apple の SDK が要る build script があれば失敗してよい（CI の macOS が正）。
- blake3 は `features = ["pure"]` で使う（C と asm のビルドを避け、Linux 機から iOS 向けの check を通しやすくする）。

## Review Focus

1. **起動の入口**：`main.m` → `asteria_spike_main()` → `eframe::run_native` は、メインスレッドで呼ばれて戻らない。起動時のエラーや panic はクラッシュではなく、Xcode のコンソールに理由を出してから終わること。→ Task 2 と Task 3
2. **バックグラウンドと復帰**：ホーム画面に戻ってからアプリに戻ったり、ロックして解除したりしても落ちず、描画が戻ること（wgpu のサーフェスの作り直し）。→ Task 3 のシミュレーター CI では確かめられないので、Task 6 のチェックリストに入れる
3. **写真へのアクセスが拒否・制限されている**：「許可しない」と「選択した写真のみ」のどちらでも落ちず、状態を画面とレポートに出すこと。→ Task 5 で、許可の状態から表示への対応を純粋な関数にしてテストする
4. **大きな原本**：数 GB の動画や iCloud にしかない原本も、メモリに載せずに読み、進み具合と失敗を表示すること。→ Task 5 でストリーミングのハッシュを純粋な部分としてテストし、チェックリストに4K 動画と iCloud の項目を入れる
5. **音声の中断**：着信や Siri で中断されたら止まり、中断が終わって「再開してよい」と言われたら再開する。イヤホンを抜いたら止まる。→ Task 4 で状態遷移を純粋な関数にしてテストする

---

### Task 1: 試作の workspace と Opus デコーダーの比較（opus-probe）

**Files:**
- Create: `spikes/ios/Cargo.toml`（`[workspace] members = ["opus-probe", "app"]`、`resolver = "3"`）、`spikes/ios/.gitignore`（`target/`）、`spikes/ios/opus-probe/Cargo.toml`、`spikes/ios/opus-probe/src/lib.rs`、`spikes/ios/opus-probe/src/bin/opus-probe.rs`、`spikes/ios/opus-probe/fixtures/make.sh`、`spikes/ios/opus-probe/fixtures/sample.opus`、`.github/workflows/ios-spike.yml`
- Modify: ルートの `Cargo.toml`（`exclude = ["spikes"]`）、`.gitattributes`（`spikes/ios/opus-probe/fixtures/** -text`）
- Test: `spikes/ios/opus-probe/tests/decode.rs`

Task 2 で `app` を作るまでは、workspace の members は `["opus-probe"]` だけにしておく。

**Interfaces:**
- Produces（`opus_probe`）：
  - `pub const SAMPLE: &[u8]`：`include_bytes!("../fixtures/sample.opus")`
  - `pub struct OggOpus { pub channels: u8, pub pre_skip: u16, pub packets: Vec<Vec<u8>>, pub final_granule: u64 }`
  - `pub fn read_ogg_opus(bytes: &[u8]) -> Result<OggOpus, ProbeError>`：`ogg` crate でページを読み、1つ目のパケット（`OpusHead`）から channels と pre_skip を、2つ目（`OpusTags`）は読み飛ばし、残りを音声のパケットとする。`final_granule` は最後のページの granule position
  - `pub enum Candidate { OpusDecoder, Opuscule, OpusPure }`、`Candidate::ALL`、`fn name(self) -> &'static str`（crate 名）、`fn license(self) -> &'static str`
  - `pub fn decode(c: Candidate, s: &OggOpus) -> Result<Vec<f32>, ProbeError>`：48 kHz、インターリーブの f32。先頭の `pre_skip` サンプルを捨て、全体を `(final_granule - pre_skip) * channels` の長さに切る
  - `pub fn snr_db(reference: &[f32], test: &[f32]) -> f64`：短いほうの長さで比べる。誤差が 0 なら `f64::INFINITY`
  - `pub fn pcm_hash(pcm: &[f32]) -> String`：f32 のリトルエンディアンのバイト列の blake3（`blake3:<hex>`）。端末とデスクトップで出力が同じかを比べるのに使う
  - `pub struct Measurement { pub candidate: Candidate, pub audio_secs: f64, pub decode_secs: f64, pub x_realtime: f64, pub snr_db: Option<f64>, pub pcm_hash: Option<String>, pub error: Option<String> }`
  - `pub fn measure(c: Candidate, s: &OggOpus, reference: Option<&[f32]>) -> Measurement`：失敗は `error` に入れ、panic は `catch_unwind` で受けて `error` にする
  - `pub enum ProbeError`（thiserror）
- bin `opus-probe [FILE]`：FILE を省略したら `SAMPLE`。`ffmpeg` があれば `ffmpeg -c:a libopus -i FILE -f f32le -ar 48000 -` を参照にする（`-c:a libopus` を入力側に付けて、ffmpeg 自身のデコーダーではなく libopus で復号させる）。release ビルドで、候補ごとに1行（name、license、x_realtime、snr_db、pcm_hash の先頭16文字、error）を表で出す

デコーダーの3 crate の API はこの計画では決めない。docs.rs を読んで `decode` の中に閉じ込めること。どれかが 1.99.0 でビルドできない、または Ogg Opus のパケットを復号する手段がない場合は、その候補を `decode` が `Err(ProbeError::Unsupported(理由))` を返すようにして残し、報告に書く。

- [ ] **Step 1: テスト用のファイルを作る**
  `fixtures/make.sh`：`ffmpeg -f lavfi -i "sine=frequency=440:duration=10" -f lavfi -i "anoisesrc=color=pink:amplitude=0.1:duration=10" -filter_complex "[0][1]amix=inputs=2,aformat=channel_layouts=stereo" -c:a libopus -b:a 96k -y sample.opus`。実行して `sample.opus` をコミットする（100〜150 KB 程度）。

- [ ] **Step 2: 失敗するテストを書く**

```rust
#[test] fn reads_head_of_the_sample() {
    let s = read_ogg_opus(SAMPLE).unwrap();
    assert_eq!(s.channels, 2);
    assert!(s.pre_skip > 0);
    assert!(!s.packets.is_empty());
}
#[test] fn every_supported_candidate_decodes_the_full_length_and_not_silence() {
    // Unsupported 以外の候補は、長さが (final_granule - pre_skip) * 2 に一致し、
    // RMS が 0.01 より大きい。少なくとも1つの候補が Unsupported でないこと
}
#[test] fn snr_of_identical_signals_is_infinite_and_of_noise_is_low() { /* INFINITY と、< 10 dB */ }
#[test] fn rejects_non_opus_ogg_and_garbage() { /* 空、"OggS" だけ、OpusHead のない Ogg で Err、panic しない */ }
#[test] fn close_to_libopus_when_ffmpeg_is_available() {
    // ffmpeg がなければ eprintln! で理由を出して return。あれば各候補の snr_db を eprintln! する。
    // 合否は付けない（比較の結果はこの試作の答えであって、テストの失敗にはしない）
}
```

- [ ] **Step 3: 失敗を確認する**
  Run: `cargo test --manifest-path spikes/ios/Cargo.toml -p opus-probe`
  Expected: コンパイルエラー（`read_ogg_opus` などが未定義）

- [ ] **Step 4: lib と bin を実装する**

- [ ] **Step 5: テストが通ることと、bin の表を確かめる**
  Run: `cargo test --manifest-path spikes/ios/Cargo.toml -p opus-probe` → PASS
  Run: `cargo run --release --manifest-path spikes/ios/Cargo.toml -p opus-probe` → 候補ごとの表。報告に貼る

- [ ] **Step 6: CI を足す**
  `ios-spike.yml` の `linux` ジョブ（ubuntu-latest）：`apt-get install -y ffmpeg libasound2-dev`、`cargo fmt --check`（`--manifest-path spikes/ios/Cargo.toml --all`）、clippy `-D warnings`、`cargo test`、`cargo run --release -p opus-probe`（表をログに残す）。ルートの `ci.yml` は変えない。

- [ ] **Step 7: Commit** `spike(ios): workspace and Opus decoder comparison`

---

### Task 2: アプリの骨組み、表示とタッチの画面、レポート

**Files:**
- Create: `spikes/ios/app/Cargo.toml`（package `asteria-ios-spike`、`[lib] name = "asteria_ios_spike"`、`crate-type = ["staticlib", "rlib"]`）、`spikes/ios/app/src/lib.rs`、`spikes/ios/app/src/report.rs`、`spikes/ios/app/src/screens/mod.rs`、`spikes/ios/app/src/screens/display.rs`、`spikes/ios/app/src/screens/report.rs`、`spikes/ios/app/src/ios/mod.rs`、`spikes/ios/app/src/bin/desktop.rs`
- Modify: `spikes/ios/Cargo.toml`（members に `app`）、`.github/workflows/ios-spike.yml`（`ios` ジョブ）
- Test: `spikes/ios/app/src/report.rs` の `#[cfg(test)]`

**Interfaces:**
- Produces：
  - `pub fn run() -> eframe::Result`：`eframe::run_native("Asteria Spike", NativeOptions::default(), ...)`。起動時に `corundum_gui::fonts::install_system_cjk_fallback` を呼び、戻り値（ファミリー名か `None`）をレポートの `fonts.cjk` に入れる
  - `#[cfg(target_os = "ios")] #[unsafe(no_mangle)] pub extern "C" fn asteria_spike_main()`：`catch_unwind` の中で `run()` を呼び、エラーと panic は `eprintln!` で出す
  - `pub struct Report`（`BTreeMap<String, String>`）、`pub type SharedReport = Arc<Mutex<Report>>`、`fn set(&mut self, key: &str, value: impl Display)`、`fn to_text(&self) -> String`：1行目に `asteria-ios-spike <CARGO_PKG_VERSION> (<target_os>/<target_arch>)`、以降はキーの順に `key: value`
  - `pub enum Screen { Display, Audio, Photos, Opus, Report }`。corundum-gui の `Shell` に `Destination` として並べ、アイコンは corundum-gui の Lucide を使う（足りないものは `info` などで代用し、足りなかったことを Task 6 のイシューの候補として報告に書く）。Audio、Photos、Opus はこの Task では「未実装」とだけ表示する
  - `Screen::Report`：`to_text()` を等幅で表示し、「コピー」ボタンで `copy_report(&str)` を呼ぶ。`copy_report` は iOS では `ios::pasteboard::set_string`（UIPasteboard の `generalPasteboard().setString`）、それ以外では `ctx.copy_text`
- Display の画面（spec §8 の1）：
  - 毎フレーム、egui のセーフエリアの値（egui 0.36 の `SafeAreaInsets`。取り出し方は egui の docs で確かめる）、画面の矩形、`pixels_per_point` を表示し、レポートの `display.safe_area`、`display.screen_rect`、`display.ppp` に入れる
  - 日本語の見本の文（「日本語の表示：漢字・ひらがな・カタカナ」）
  - タッチの確認：押した回数を数えるボタン、指の位置と同時に触れている数を表示する領域、200 行のスクロールするリスト、テキスト入力欄（ソフトウェアキーボードが出るか、入力欄が隠れないか）
  - 確かめた結果をユーザーが記録できるよう、項目ごとに「OK」「NG」のトグルを置き、レポートの `check.display.<項目>` に入れる（項目：`safe_area`、`tap`、`scroll`、`keyboard`、`cjk`、`rotate`）

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn report_text_has_header_and_sorted_lines() {
    // set("b", 1); set("a", "x") → 2行目が "a: x"、3行目が "b: 1"。1行目は "asteria-ios-spike " で始まる
}
#[test] fn setting_a_key_twice_keeps_the_last_value() {}
```

- [ ] **Step 2〜4: 失敗を確認し、実装し、成功を確認する**
  Run: `cargo test --manifest-path spikes/ios/Cargo.toml -p asteria-ios-spike`
  Run: `cargo run --manifest-path spikes/ios/Cargo.toml -p asteria-ios-spike --bin desktop`：ウィンドウが開き、レールに5つの画面が並び、日本語が表示されることを確かめる（できなければ、起動ログを報告に書く）

- [ ] **Step 5: CI に iOS 向けの clippy を足す**
  `ios` ジョブ（macos-latest）：`rustup target add aarch64-apple-ios aarch64-apple-ios-sim`、`Swatinem/rust-cache@v2`（workspaces: `spikes/ios`）、両方の target で `cargo clippy --manifest-path spikes/ios/Cargo.toml --target <t> --all-targets -- -D warnings`

- [ ] **Step 6: Commit** `spike(ios): app shell, display and touch screen, report`

---

### Task 3: Xcode プロジェクトと、シミュレーターでの起動の CI

**Files:**
- Create: `spikes/ios/xcode/project.yml`、`spikes/ios/xcode/Sources/main.m`、`spikes/ios/xcode/build-rust.sh`、`spikes/ios/xcode/Local.xcconfig.example`、`spikes/ios/README.md`
- Modify: `spikes/ios/.gitignore`（`xcode/*.xcodeproj/`、`xcode/Local.xcconfig`、`xcode/build/`）、`.github/workflows/ios-spike.yml`

**Interfaces / 決まっている値:**
- ターゲット名とスキーム名 `AsteriaSpike`、`CFBundleDisplayName` は `Asteria Spike`、iOS の最小バージョンは 16.0、iPhone のみ（`TARGETED_DEVICE_FAMILY = 1`）
- バンドル ID と署名のチームは `Local.xcconfig`（gitignore）で上書きする。例のファイルの既定は `PRODUCT_BUNDLE_IDENTIFIER = io.github.sapphireturtle.asteria.spike`、`DEVELOPMENT_TEAM =`（空）。`project.yml` は `configFiles` でこれを読み、ファイルがなくても生成できるようにする（シミュレーターと CI は署名なし）
- `main.m`：`extern void asteria_spike_main(void); int main(int argc, char *argv[]) { asteria_spike_main(); return 0; }`
- Info.plist（`project.yml` の `info.properties` で生成）：
  - `UILaunchScreen`: `{}`（これがないと全画面にならない）
  - `UIBackgroundModes`: `[audio]`
  - `NSPhotoLibraryUsageDescription`: 「写真の一覧と原本の読み取りを試すために使います。」
  - `UISupportedInterfaceOrientations`: 縦と左右の横
  - `UIApplicationSupportsIndirectInputEvents`: `true`
- `build-rust.sh`（ビルド前の Run Script として呼ぶ）：`$PLATFORM_NAME` が `iphoneos` なら `aarch64-apple-ios`、`iphonesimulator` なら `aarch64-apple-ios-sim`。`$CONFIGURATION` が `Release` なら `--release`。`cargo build --manifest-path ../Cargo.toml -p asteria-ios-spike --lib --target <t>` を実行し、`libasteria_ios_spike.a` を `$BUILT_PRODUCTS_DIR/rust/` に置く。Xcode の環境では `PATH` に `~/.cargo/bin` を足す。スクリプトの入力として `../app/src`、`../opus-probe/src`、`../Cargo.lock` を、出力として `.a` を宣言する
- リンク：`LIBRARY_SEARCH_PATHS = $(BUILT_PRODUCTS_DIR)/rust`、`OTHER_LDFLAGS = -lasteria_ios_spike`。SDK のフレームワーク：UIKit、Foundation、Metal、QuartzCore、Photos、AVFAudio、MediaPlayer、AudioToolbox、CoreAudio、CoreText、CoreFoundation
- `README.md`（Mac での手順）：Xcode、`brew install xcodegen`、`rustup target add aarch64-apple-ios aarch64-apple-ios-sim`、`cp xcode/Local.xcconfig.example xcode/Local.xcconfig` してチームを書く、`cd spikes/ios/xcode && xcodegen generate && open AsteriaSpike.xcodeproj`、実機を選んで Run。結果の集め方（レポートの画面の「コピー」を押して、チェックリストの文書に貼る）

- [ ] **Step 1: ファイルを書く**

- [ ] **Step 2: CI でシミュレーターのビルドと起動を確かめる**
  `ios` ジョブに足す：`brew install xcodegen`、`xcodegen generate`、`xcodebuild -project AsteriaSpike.xcodeproj -scheme AsteriaSpike -sdk iphonesimulator -configuration Debug -derivedDataPath build CODE_SIGNING_ALLOWED=NO build`。続けて、`iPhone` で始まる利用可能なシミュレーターを起動し（corundum-gui の Mobile CI と同じ方法）、`xcrun simctl install`、`xcrun simctl launch --console-pty` をバックグラウンドで動かし、20 秒後に `xcrun simctl io booted screenshot screenshot.png`、アプリのプロセスがまだ生きていることを確認する（落ちていればジョブを失敗させ、コンソールの出力をログに残す）。スクリーンショットとコンソールの出力は `actions/upload-artifact` で残す

- [ ] **Step 3: PR の CI を待ち、スクリーンショットを見る**
  Expected: ビルドが通り、スクリーンショットにレールと日本語の見本が映っている。報告にアーティファクトの名前を書く

- [ ] **Step 4: Commit** `spike(ios): Xcode project and simulator launch in CI`

---

### Task 4: 音声：バックグラウンド再生とロック画面の操作

**Files:**
- Create: `spikes/ios/app/src/audio.rs`、`spikes/ios/app/src/screens/audio.rs`、`spikes/ios/app/src/ios/audio_session.rs`、`spikes/ios/app/src/ios/now_playing.rs`
- Modify: `spikes/ios/app/Cargo.toml`（`opus-probe = { path = "../opus-probe" }` を足す）、`spikes/ios/app/src/ios/mod.rs`、`spikes/ios/app/src/screens/mod.rs`
- Test: `spikes/ios/app/src/audio.rs` の `#[cfg(test)]`

**Interfaces:**
- Produces（どの OS でも動く部分、`audio.rs`）：
  - `pub enum Source { Tone, OpusSample }`：Tone は 440/554/659 Hz を 0.5 秒ずつ繰り返す音量 0.2 のアルペジオ（終わりがない）。OpusSample は `opus_probe::SAMPLE` を Task 1 で一番速かった候補で復号し、繰り返す
  - `pub enum Command { Play(Source), Pause, Resume, Toggle, Stop, InterruptionBegan, InterruptionEnded { should_resume: bool }, OldDeviceUnavailable }`
  - `pub enum PlayState { Stopped, Playing(Source), Paused { source: Source, by_interruption: bool } }`
  - `pub fn next_state(state: PlayState, cmd: Command) -> PlayState`：中断で止めたときだけ `by_interruption: true`。`InterruptionEnded { should_resume: true }` は `by_interruption` のときだけ再生に戻す。`OldDeviceUnavailable` は再生中なら `Paused { by_interruption: false }`
  - `pub struct Engine`：専用のスレッドで cpal の出力ストリームを持ち、`fn send(&self, cmd: Command)` と `fn state(&self) -> PlayState` を持つ。ストリームのエラーと出力サンプルレート（48 kHz 以外なら線形補間で変換する）をレポートの `audio.*` に入れる
- Produces（iOS、`ios/`）：
  - `audio_session::activate_playback() -> Result<(), String>`：`AVAudioSession.sharedInstance()` のカテゴリを `AVAudioSessionCategoryPlayback` にして有効化する。結果はレポートの `audio.session`
  - `audio_session::observe(tx: mpsc::Sender<Command>)`：`AVAudioSessionInterruptionNotification`（種類と `ShouldResume` のオプション）と `AVAudioSessionRouteChangeNotification`（理由が `OldDeviceUnavailable`）を `NSNotificationCenter` で受け、`Command` に変えて送る
  - `now_playing::register(tx: mpsc::Sender<Command>)`：`MPRemoteCommandCenter` の play、pause、togglePlayPause に handler を付けて `Command` を送り、`MPRemoteCommandHandlerStatus::Success` を返す
  - `now_playing::update(title: &str, playing: bool, elapsed_secs: f64)`：`MPNowPlayingInfoCenter.defaultCenter().setNowPlayingInfo`（タイトル、`MPNowPlayingInfoPropertyPlaybackRate`、`MPNowPlayingInfoPropertyElapsedPlaybackTime`）。状態が変わるたびと、再生中は 5 秒ごとに呼ぶ
  - 受け取った遠隔操作と中断は、回数と最後の時刻をレポートの `audio.remote.<種類>`、`audio.interruptions`、`audio.route_changes` に入れる
- 画面：Tone と OpusSample の再生ボタン、一時停止と停止、状態の表示、チェックリストのトグル（`check.audio.<項目>`：`locked_2min`、`lock_screen_controls`、`control_center`、`interruption_resume`、`unplug_pauses`、`background_foreground`）

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn interruption_pauses_and_resumes_only_when_allowed() {
    // Playing(Tone) + InterruptionBegan → Paused{by_interruption:true}
    // + InterruptionEnded{should_resume:false} → Paused のまま
    // Playing + InterruptionBegan + InterruptionEnded{should_resume:true} → Playing(Tone)
}
#[test] fn user_pause_is_not_resumed_by_interruption_end() {
    // Playing + Pause + InterruptionEnded{should_resume:true} → Paused{by_interruption:false}
}
#[test] fn unplugging_pauses_and_toggle_resumes() {}
#[test] fn toggle_and_stop_from_every_state() { /* Stopped で Toggle は Stopped のまま、など全組み合わせ */ }
#[test] fn tone_is_bounded_and_not_silent() { /* 1 秒分の全サンプルが |x| <= 0.2、RMS > 0.05 */ }
```

- [ ] **Step 2〜4: 失敗を確認し、実装し、成功を確認する**
  Run: `cargo test --manifest-path spikes/ios/Cargo.toml -p asteria-ios-spike`
  Run: デスクトップの bin で Tone が鳴ることを確かめる（音が出ない環境ならその旨を報告に書く）

- [ ] **Step 5: Commit** `spike(ios): background playback, interruptions, lock screen controls`

---

### Task 5: 写真：PhotoKit の列挙と原本の取得

**Files:**
- Create: `spikes/ios/app/src/photos.rs`、`spikes/ios/app/src/screens/photos.rs`、`spikes/ios/app/src/ios/photos.rs`
- Modify: `spikes/ios/app/Cargo.toml`、`spikes/ios/app/src/ios/mod.rs`、`spikes/ios/app/src/screens/mod.rs`
- Test: `spikes/ios/app/src/photos.rs` の `#[cfg(test)]`

**Interfaces:**
- Produces（どの OS でも動く部分、`photos.rs`）：
  - `pub enum Auth { NotDetermined, Restricted, Denied, Authorized, Limited }`、`fn describe(self) -> &'static str`（画面に出す日本語の説明。Denied と Restricted には「設定アプリで許可してください」を含める）
  - `pub enum ResourceKind { Photo, Video, PairedVideo, FullSizePhoto, AlternatePhoto, Other(i64) }`（PHAssetResourceType の値から）
  - `pub struct ResourceInfo { pub kind: ResourceKind, pub original_filename: String, pub uti: String }`
  - `pub struct AssetInfo { pub local_id: String, pub created: Option<String>, pub is_live: bool, pub resources: Vec<ResourceInfo> }`
  - `pub struct UploadPlan { pub primary: (ResourceInfo, String), pub paired_video: Option<(ResourceInfo, String)> }`：送るリソースと、送るときの名前
  - `pub fn plan_upload(asset: &AssetInfo) -> Option<UploadPlan>`：原本の写真か動画（`Photo` か `Video`。編集済みの `FullSizePhoto` ではなく原本）を primary にする。Live Photos は `PairedVideo` を組にし、その名前は primary のファイル名の拡張子より前の部分に、動画の拡張子を付けたもの（spec §5.3「同じベース名の組」）。primary がなければ `None`
  - `pub struct StreamHash`：`fn update(&mut self, chunk: &[u8])`、`fn finish(self) -> (u64, String)`（バイト数と `blake3:<hex>`）
- Produces（iOS、`ios/photos.rs`）：
  - `request_auth(tx)`：`PHPhotoLibrary::requestAuthorizationForAccessLevel_handler`（`PHAccessLevel::ReadWrite`）。結果を `Auth` で送る
  - `counts() -> (usize, usize, usize)`：画像、動画、Live Photos の数
  - `recent(limit: usize) -> Vec<AssetInfo>`：作成日の新しい順に `limit` 件（画面からは 30 件）
  - `fetch_original(local_id, resource_index, tx)`：`PHAssetResourceManager::defaultManager().requestDataForAssetResource_options_dataReceivedHandler_completionHandler` で、`networkAccessAllowed = true`。断片ごとに `StreamHash::update` に流し、進み具合（バイト数）と、終わったら（バイト数、ハッシュ、秒数、エラー）を送る
- 画面：許可の状態と「許可を求める」ボタン、件数、最近の 30 件（Live なら印、リソースの種類とファイル名、`plan_upload` の結果の名前）、各行の「原本を読む」、全部をまとめて読む「30 件を読む」（合計のバイト数と MB/s）。結果はレポートの `photos.auth`、`photos.counts`、`photos.sample.<n>`、`photos.fetch.<n>`、`photos.batch` に入れる。チェックリストのトグル（`check.photos.<項目>`：`denied_ok`、`limited_ok`、`heic_original`、`live_pair`、`video_4k`、`icloud_only`）

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test] fn live_photo_pairs_video_under_the_photo_base_name() {
    // Photo "IMG_0001.HEIC" + PairedVideo "IMG_0001.MOV" → ("IMG_0001.HEIC", "IMG_0001.MOV")
    // PairedVideo の名前が "FullSizeRender.mov" でも → "IMG_0001.mov"
}
#[test] fn edited_photo_still_sends_the_original() {
    // Photo "IMG_0002.JPG" + FullSizePhoto "FullSizeRender.jpg" → primary は IMG_0002.JPG
}
#[test] fn video_asset_has_no_pair_and_unknown_asset_is_none() {}
#[test] fn stream_hash_equals_one_shot_hash() {
    // 1 MiB を 1 B、4 KiB、不揃いの断片に分けて update しても、blake3::hash と一致する
}
#[test] fn denied_and_restricted_explain_how_to_allow() {}
```

- [ ] **Step 2〜4: 失敗を確認し、実装し、成功を確認する**
  Run: `cargo test --manifest-path spikes/ios/Cargo.toml -p asteria-ios-spike`
  デスクトップでは写真の画面は「iOS のみ」と表示する

- [ ] **Step 5: Commit** `spike(ios): PhotoKit enumeration and streamed original reads`

---

### Task 6: 端末での Opus の計測と、チェックリスト

**Files:**
- Create: `spikes/ios/app/src/screens/opus.rs`、`docs/superpowers/spikes/2026-10-23-ios-spike.md`
- Modify: `spikes/ios/app/src/screens/mod.rs`

**Interfaces:**
- Opus の画面：「計測する」で、別スレッドで全候補の `opus_probe::measure(c, &read_ogg_opus(SAMPLE)?, None)` を3回ずつ実行し、x_realtime の中央値、pcm_hash、error を表示して、レポートの `opus.<name>.x_realtime`、`opus.<name>.pcm_hash`、`opus.<name>.error` に入れる。release ビルドでないときは「Debug ビルドの値です」と表示し、レポートに `opus.build = debug` を入れる
- チェックリストの文書 `docs/superpowers/spikes/2026-10-23-ios-spike.md`：
  - 目的（spec §8 の4項目）と、判断の基準
    - egui：セーフエリアの内側に収まり、タップ、スクロール、キーボード、回転、バックグラウンドからの復帰が実用的に動く
    - 音声：ロックしたまま 2 分以上鳴り続け、ロック画面とコントロールセンターから一時停止と再開ができる。中断とイヤホンを抜いたときに止まる
    - Opus：libopus との SNR が 60 dB 以上、iPhone の release ビルドで 20 倍速以上、license が GPL-3.0 と両立する候補が1つ以上あれば、オフライン用の音声は Ogg Opus にする。なければ Ogg Vorbis（symphonia で復号できる）にする
    - 写真：HEIC の原本、Live Photos の組、4K 動画、iCloud にしかない写真を、メモリを増やさずに読める
  - 手順（`spikes/ios/README.md` へのリンク）と、項目ごとの確認方法。Review Focus の 2 と 4 を含める（ホーム画面に戻って復帰、ロックして解除、数 GB の動画、機内モードで iCloud の写真を読む）
  - 結果を貼る場所：Linux の `opus-probe` の表（Task 1）、CI のスクリーンショットのアーティファクト（Task 3）、端末のレポートの全文、気づいたこと
  - corundum-gui の気づき（タッチでの使い勝手、アイコンの不足、セーフエリア）を書く欄。後で sapphire-framework にイシューを立てる元にする

- [ ] **Step 1: 画面と文書を書く**
- [ ] **Step 2: テストと clippy を通す**
  Run: `cargo test --manifest-path spikes/ios/Cargo.toml` と、CI の iOS ジョブ
- [ ] **Step 3: Commit** `spike(ios): on-device Opus timing and the device checklist`

---

### Task 7: 端末での確認と結果の反映（ユーザーと controller）

subagent には任せない。Task 1〜6 を PR にしてマージした後に行う。

- [ ] **Step 1（ユーザー）:** `spikes/ios/README.md` の手順で実機にインストールし、チェックリストを進め、レポートの画面の「コピー」で全文を取り、会話に貼る
- [ ] **Step 2:** 結果を `docs/superpowers/spikes/2026-10-23-ios-spike.md` に書き込み、4項目それぞれの結論を書く
- [ ] **Step 3:** spec を更新する：§8 の各項目に結論を書き、オフライン用の音声形式（Opus か Vorbis、使うデコーダーの crate）を §5.1 と §5.4 に書く。詰まった点があれば §5.5 に書く
- [ ] **Step 4:** corundum-gui の問題と欲しいものを、sapphire-framework にイシューとして立てる（1件ずつ。再現の手順と、試作のどの画面で気づいたかを書く）
- [ ] **Step 5:** ロードマップ（Plan 1 の冒頭）に、Plan 4 以降で変わることがあれば書き足し、PR にしてマージする
