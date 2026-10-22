# project-asteria

## ギャラリーサーバーの起動

```sh
# ライブラリ（既存でも可）と設定ファイルを作り、所有者のトークンを一度だけ表示する。
asteria-gallery-server init --library ~/Pictures --owner me --config ~/.config/asteria/gallery.toml
# <library>/.asteria/users.toml にいる別のユーザーのトークンを追加する。
asteria-gallery-server token add --config ~/.config/asteria/gallery.toml --user <USER_ID>
# 起動する（Ctrl-C で安全に停止）。
asteria-gallery-server serve --config ~/.config/asteria/gallery.toml
```

- `init` は既存の設定ファイルを `--force` なしでは上書きしない。標準出力に出るのはトークンだけ。
- 既定の待ち受けは `127.0.0.1:7810`（`init --listen` で設定、`serve --listen` で上書き）。
- キャッシュ（索引とサムネイル）は `$XDG_CACHE_HOME/asteria-gallery/<library-id>/`（通常は `~/.cache/...`）にあり、消しても問題ない。評価やタグはライブラリ内のサイドカーが原本。
- サムネイルには `vipsthumbnail`（libvips-tools）、動画には `ffmpeg` が必要。ログは `RUST_LOG` で調整（既定 `info`）。
