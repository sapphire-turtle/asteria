# project-asteria

## photos サーバーの起動

```sh
# ライブラリ（既存でも可）と設定ファイルを作り、所有者のトークンを一度だけ表示する。
asteria-photos-server init --library ~/Pictures --owner me --config ~/.config/asteria/photos.toml
# <library>/.asteria/users.toml にいる別のユーザーのトークンを追加する。
asteria-photos-server token add --config ~/.config/asteria/photos.toml --user <USER_ID>
# 起動する（Ctrl-C で安全に停止）。
asteria-photos-server serve --config ~/.config/asteria/photos.toml
```

- `init` は既存の設定ファイルを `--force` なしでは上書きしない。標準出力に出るのはトークンだけ。
- 既定の待ち受けは `127.0.0.1:7810`（`init --listen` で設定、`serve --listen` で上書き）。
- キャッシュ（索引とサムネイル）は `$XDG_CACHE_HOME/asteria-photos/<library-id>/`（通常は `~/.cache/...`）にあり、消しても問題ない。評価やタグはライブラリ内のサイドカーが原本。
- `token add` は設定ファイルを書き換えるだけ。動いているサーバーには再起動で反映される。`init --force` は設定ファイルを作り直すので、既存のトークンはすべて置き換わる。
- トークンは平文（素の HTTP）で流れる。サーバーは LAN か Tailscale などの VPN の中だけで公開し、インターネットに直接出さない。
- 1つのライブラリを扱うサーバーは1つだけ。`serve` は `<library>/.asteria/server.lock` をロックし、2つ目はエラーで終了する。
- サムネイルには `vipsthumbnail`（libvips-tools）、動画には `ffmpeg` が必要。ログは `RUST_LOG` で調整（既定 `info`）。
