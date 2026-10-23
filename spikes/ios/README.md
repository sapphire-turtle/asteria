# iOS spike

egui/eframe (wgpu) を iOS の実機で動かし、写真・音声・日本語表示などを確かめる試作。

## Mac での手順

1. Xcode を入れる。
2. `brew install xcodegen`
3. `rustup target add aarch64-apple-ios aarch64-apple-ios-sim`
4. `cp xcode/Local.xcconfig.example xcode/Local.xcconfig` して、`DEVELOPMENT_TEAM` に自分のチーム ID を書く（必要ならバンドル ID も変える）。このファイルは gitignore 済み。実機でなければ不要。
5. `cd spikes/ios/xcode && xcodegen generate && open AsteriaSpike.xcodeproj`
6. 実機を選んで Run（初回は端末側で開発者を信頼する）。

シミュレーターは arm64 のみ対応（Rust のライブラリが arm64 だけのため）。Apple Silicon の Mac が必要で、Intel Mac のシミュレーターでは動かない。

## 結果の集め方

アプリのレポートの画面で「コピー」を押し、チェックリストの文書に貼る。

## CI

`.github/workflows/ios-spike.yml` の `ios` ジョブが、署名なしでシミュレーター向けにビルドして起動し、スクリーンショットとコンソール出力をアーティファクト `ios-simulator` に残す。
