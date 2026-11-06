<img src="docs/icons/asteria-photos.svg" alt="asteria photos" width="64" height="64"> <img src="docs/icons/asteria-music.svg" alt="asteria music" width="64" height="64">

# project-asteria

## インストール

[リリースのページ](https://github.com/sapphire-turtle/asteria/releases)の各リリースに、2つの desktop アプリのインストール用パッケージと、ウィンドウの無い2つのサーバーの単体のファイルがある。プレリリース（`v0.1.0-rc.1` など）にはその印が付く。

desktop アプリは、ウィンドウ、サーバーのコマンドライン（`asteria-photos` / `asteria-music`）、必要ならこのコンピューターのサーバーまでを1回のインストールでそろえる。使うシステム向けのファイルを落とす（名前に `photos` か `music` が入る）。

- Windows：`asteria-photos-desktop_<version>_x64-setup.exe` は自分のユーザーだけに入れ（`%LocalAppData%\Asteria Photos`、スタートメニューの項目つき）、`asteria-photos` を `Path` に入れる。アンインストールは設定 › アプリから。
- macOS：`asteria-photos-desktop_<version>_universal.dmg`。Apple シリコンと Intel の Mac に共通の1つのアプリで、アプリケーションフォルダーにドラッグする。設定 › 情報で `asteria-photos` を `PATH` に置ける。
- Linux：`asteria-photos-desktop_<version>_x86_64.AppImage`（64 ビットの Arm、Raspberry Pi などでは `_aarch64.AppImage`）。実行できるようにして（`chmod +x`）起動する。設定 › 情報で `asteria-photos` を `PATH` に置ける。

インストールしたアプリは1日ごとに新しいリリースを探してバックグラウンドで落とし、署名を確かめて、利用者が選んだとき（設定 › 情報）か次の起動時に適用する（リリースの `.sig` と `asteria-<app>-latest.json` はアプリが読むもの。`.app.tar.gz` は macOS のアプリの更新用）。

- Windows と macOS のパッケージには、サーバーが使う外部ツール（photos にだけ ffmpeg、Windows では libvips の `vipsthumbnail`。music には何も要らない）が入っている。ライセンスは `tools/LICENSES`（ffmpeg は GPLv3、libvips は LGPL）。Linux ではパッケージマネージャーで入れる（[このコンピューターのサーバー](#このコンピューターのサーバー)を参照）。
- Windows のインストーラーはまだ署名していないので、実行の前に SmartScreen が警告する（「詳細情報」›「実行」）。macOS のアプリは、リリースのワークフローに Developer ID があれば署名と公証つきになる。それまでは Gatekeeper が最初は開かない（システム設定 › プライバシーとセキュリティの「このまま開く」で許可する）。

ウィンドウの無いサーバーは `asteria-photos-headless-<platform>` と `asteria-music-headless-<platform>`（`linux-x86_64`、`linux-aarch64`、`macos-aarch64`、`windows-x86_64.exe`）。ファイル名を `asteria-photos`（または `asteria-music`。Windows では `.exe` を残す）に変え、実行できるようにして `PATH` に置く。`asteria-photos update` で最新のリリースに置き換わる（`--check` は有無を知らせるだけ、`-y` は確認しない）。置き換えた後は、動いている `serve` を起動し直す。プレリリース（rc）のバイナリには、`update` は後のプレリリースを勧めないので、それは手でダウンロードする。`cargo install` で入れたサーバーは `cargo install` で更新し、desktop アプリは自分で更新する。

## photos サーバーの起動

`asteria-photos` は、GUI のないホスト向けのウィンドウを持たないサーバー。`cargo install --path photos/headless` で入れる（サーバーのビルドには、同梱の libopus のために `cmake` が要る）。デスクトップアプリ（`asteria-photos-desktop`）も同じコマンドを実行できる。Windows では `cargo install` で入れたデスクトップアプリは GUI のプログラムなので、コマンドラインにはパッケージの `asteria-photos`（コンソール用の shim）か、ウィンドウの無いサーバーを使う。

```sh
# ライブラリ（既存でも可）と設定ファイルを作り、キー `owner` のトークンを一度だけ表示する。
asteria-photos init --library ~/Pictures --owner me
# 別の端末のためにキーを足し、そのトークンを一度だけ表示する。
asteria-photos api-key add laptop
# 起動する（Ctrl-C で安全に停止）。
asteria-photos serve
```

- 設定ファイルは、`--config <ファイル>` で別の場所を指さなければ `<設定ディレクトリ>/asteria-photos/server.toml`（どのコマンドも `--config` を取れる）。キーは `api-key list|retire|restore|rotate` で扱う。どのキーもライブラリの所有者として動く。
- `init` は既存の設定ファイルを `--force` なしでは上書きしない。標準出力に出るのはトークンだけ。
- 既定の待ち受けは `127.0.0.1:7810`（`init --listen` で設定、`serve --listen` で上書き）。
- キャッシュ（索引とサムネイル）は `$XDG_CACHE_HOME/asteria-photos/<library-id>/`（通常は `~/.cache/...`）にあり、消しても問題ない。評価やタグはライブラリ内のサイドカーが原本。
- サーバー自身の状態（操作のジャーナル、未完了のアップロード、サーバーのロック）は、ライブラリではなく `$XDG_DATA_HOME/asteria-photos/<library-id>/`（通常は `~/.local/share/...`）に置く。ライブラリの `.asteria/` には、ライブラリ自身のファイル（`library.toml`、`users.toml`、のちにゴミ箱と共有のプレイリスト）だけがある。
- キーはデータディレクトリのファイルなので、`api-key add` で足したキーは動いているサーバーを再起動せずに使える。`init --force` は設定ファイルを作り直し、既存のキーをすべて失効させる。
- トークンは平文（素の HTTP）で流れる。サーバーは LAN か Tailscale などの VPN の中だけで公開し、インターネットに直接出さない。
- 1つのホストで1つのライブラリを扱うサーバーは1つだけ。`serve` はデータディレクトリの `server.lock` をロックし、同じホストの2つ目はエラーで終了する。
- 設定ファイルに `[[token]]` がある以前の版からの更新：それを開いた最初のコマンドが、`[[token]]` を API キー `token-1`、`token-2`… として取り込み、設定ファイルをそれ抜きで書き直す（ファイル中のコメントは消える。ファイルが読み取り専用なら、サーバーは警告を出すので `[[token]]` を手で消す）。トークンはそのまま使えるが、ほかのユーザーに発行したトークンも所有者として動くようになる。
- 別々のホストのサーバーが、1つのライブラリを（NFS などで）共有してよい（本番と試験用のサーバーなど）。ジャーナル、キャッシュ、未完了のアップロードはサーバーごとに持つ。ファイルの監視はほかの NFS ホストからの変更に気づかないので、そうした変更は定期的な再スキャン（15分ごと）か `library.rescan` で反映される。アップロードの重複の確認は、そのサーバーがスキャンしたものしか知らない。同じホストで同じライブラリのサーバーを2つ動かすことはできない（キャッシュとデータディレクトリを共有してしまう）。
- よくある形式（JPEG・PNG・WebP・GIF・TIFF、RAW のプレビュー）のサムネイルはサーバー自身が作る。`vipsthumbnail`（libvips-tools）が要るのは HEIC・AVIF・一部のカメラの RAW・とても大きい画像だけで、動画には `ffmpeg` が必要（`PATH` から探す。`ASTERIA_VIPSTHUMBNAIL` と `ASTERIA_FFMPEG` で場所を指すこともできる）。ログは `RUST_LOG` で調整（既定 `info`）。

## music サーバーの起動

`asteria-music` は、ウィンドウを持たないサーバー（`cargo install --path music/headless`。ビルドには `cmake` が要る）。デスクトップアプリ（`asteria-music-desktop`）も同じコマンドを実行できる。Windows では `cargo install` で入れたデスクトップアプリは GUI のプログラムなので、コマンドラインにはパッケージの `asteria-music`（コンソール用の shim）か、ウィンドウの無いサーバーを使う。

```sh
# ライブラリ（既存でも可）と設定ファイルを作り、キー `owner` のトークンを一度だけ表示する。
asteria-music init --library ~/Music --owner me
# 別の端末のためにキーを足し、そのトークンを一度だけ表示する。
asteria-music api-key add laptop
# 起動する（Ctrl-C で安全に停止）。
asteria-music serve
```

- コマンド、設定ファイル、トークンの扱い、キャッシュとデータのディレクトリ（`asteria-photos` の代わりに `asteria-music`）、ロック、素の HTTP についての注意は、上の photos サーバーと同じ。既定の待ち受けは `127.0.0.1:7811`。
- ライブラリの音声ファイル（FLAC、MP3 など）を、アルバム、アーティスト、プレイリストの3つの軸で見られる。アルバムとアーティストはファイルのタグから作る。タグのないファイルも、ファイル名で一覧に出る。
- プレイリストは、ライブラリ内の `.m3u8` と `.m3u` のファイル。ファイルが移動したエントリは、プレイリストにコンテンツハッシュがあればそれで見つけ直す。見つからないエントリは「見つからない」として返す。
- タグとプレイリストは読むだけ。サーバーが音楽ファイルやプレイリストに書き込むことはない。
- アルバムのジャケットは、トラックに埋め込まれた画像、なければトラックのフォルダ、そこになければその上のアルバムのフォルダ（`CD1` のようなディスクのフォルダの場合）にある `cover`、`folder`、`front`、`album` という名前の画像（`.jpg`、`.jpeg`、`.png`、`.webp`）。
- ジャケットは、サーバーが外部ツールなしで作る。トラックのオフライン用の音声（Ogg Opus。原本が大きすぎる場所で聴くためのもの）も、サーバーが外部ツールなしで作る。

## MCP クライアントの接続

どちらのサーバーも Model Context Protocol に対応しているので、AI エージェントからライブラリを見て整理できる。

```sh
# Claude Code の場合。<URL> はサーバーのアドレス（例: http://127.0.0.1:7811）
claude mcp add --transport http asteria <URL>/mcp --header "Authorization: Bearer <token>"
```

- エンドポイントは `<サーバーの URL>/mcp`（Streamable HTTP）である。トークンはクライアントと同じ Bearer トークンで、エージェントはライブラリの所有者として動く。
- ツールは、ライブラリ・フォルダ・アイテムの参照、評価・タグ・説明を書き換える `media_update`、ファイルの移動・改名・ゴミ箱入れをする `organize_apply` とその状況・取り消し・履歴、サムネイルを見る `media_preview` である。music サーバーには `album_list`、`album_get`、`artist_list`、`artist_get`、`playlist_list`、`playlist_get` が加わる。
- エージェントの変更はジャーナルに `agent` として残り、`organize_undo` でファイル操作のバッチごと取り消せる。メタデータの編集（`media_update`）はバッチではなく、取り消せない。

## photos クライアントの起動

```sh
# Linux では eframe のためにいくつかのシステムパッケージが要る（Debian/Ubuntu の名前）。
sudo apt install libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev libgl1-mesa-dev
cargo run -p asteria-photos-desktop --release
# 引数を付けると、同じバイナリがサーバーの CLI になる（`asteria-photos serve` と同じ）。
cargo run -p asteria-photos-desktop --release -- serve
```

- クライアントは `asteria-photos serve` が配信するライブラリを見るもの。サムネイルはサーバーが作るので、libvips や ffmpeg は要らない。
- 初回はサーバーが未登録なので、ウィンドウがライブラリの場所を尋ねる：このコンピューター（[このコンピューターのサーバー](#このコンピューターのサーバー)）、ほかのサーバー、あとで。ほかのサーバー（またはあとで）を選ぶと、設定が「サーバー」ページで追加フォームを開いた状態になる。名前、アドレス（`http://192.168.1.10:7810` など）、`init`（または `api-key add`）が表示したアクセストークンを入れ、「接続を確かめる」で確かめてから保存する。左のレールで「ライブラリ」を開くと写真が見える。
- サーバーは複数登録できる。ステータスバー（右下。幅が狭いときはアイコンだけ）に使用中のサーバーと接続の状態が出る。メニューから別のサーバーを選ぶと切り替わり、「サーバーを管理…」（設定 › サーバー）では追加、編集、削除ができる。使用中のサーバーは次回の起動でも覚えていて、切り替えるとライブラリのルートから表示し直す。
- サーバーにつながらない、トークンが通らない、バージョンが合わない、といったときは、ライブラリに理由と「もう一度」ボタンが出る。オフライン用のキャッシュはまだない。
- サーバーとトークンは、アプリの設定ディレクトリの `servers.toml` に保存される（Linux は `~/.config/asteria-photos/`、macOS は `~/Library/Application Support/asteria-photos/`、Windows は `%LOCALAPPDATA%\asteria-photos\`。以前の版が `%APPDATA%` に置いた設定は、最初の起動でここへ移る）。Unix ではこのファイルを自分だけが読める権限（0600）にする。トークンは OS のキーチェーンには入れない。取得したサムネイルはアプリのキャッシュディレクトリ（Linux は `~/.cache/asteria-photos/`）に置かれ、消しても問題ない。
- 写真をクリックで選ぶと、右にプレビューと詳細が出る。グリッドでは矢印キーで選択が動き、Enter（またはダブルクリック）で選んだ写真が全画面表示になる。全画面では左右キーで同じフォルダ内を前後に移動し、Esc で閉じる。
- 詳細ペインでは、評価（星をクリック。同じ星をもう一度押すと解除）、カラーラベル、キーワードと階層タグ、説明（入力欄からフォーカスが外れると保存）を編集できる。編集はサーバーがライブラリのサイドカーに書き込む。サーバーがサイドカーを読めない写真は、理由つきで読み取り専用になる。

## music クライアントの起動

```sh
# Linux では eframe のためにいくつかのシステムパッケージと、音の出力のための ALSA が要る（Debian/Ubuntu の名前）。
sudo apt install libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev libgl1-mesa-dev libasound2-dev
cargo run -p asteria-music-desktop --release
# 引数を付けると、同じバイナリがサーバーの CLI になる（`asteria-music serve` と同じ）。
cargo run -p asteria-music-desktop --release -- serve
```

- クライアントは `asteria-music serve` が配信するライブラリを再生するもの。カバーと低音質のコピーはサーバーが作るので、libvips や ffmpeg は要らない。
- 初回は photos クライアントと同じく、ウィンドウがライブラリの場所を尋ねる。ほかのサーバー（またはあとで）を選ぶと、設定が「サーバー」ページで追加フォームを開いた状態になる。名前、アドレス（`http://192.168.1.10:7811` など）、`init`（または `api-key add`）が表示したアクセストークンを入れ、「接続を確かめる」で確かめてから保存する。サーバーの登録、切り替え、保存のしかたは photos クライアントと同じ（`servers.toml`。`asteria-photos` の代わりに `asteria-music` の下）。
- アルバム、アーティスト、プレイリストを選ぶと曲が並び、再生できる。再生はファイルを取り終える前に始まり、まだ届いていない位置へのシークは、その部分が届くまで待つ。
- 「低音質で再生する」はアプリ全体で一つの設定で、音量といっしょにアプリの設定ディレクトリの `playback.toml` に保存される。オンにすると、サーバーが曲を低ビットレートの Ogg Opus（オフライン用のコピー）に変換し、クライアントはそれを再生する。回線が遅いときのためのもの。曲を初めて再生するときは、サーバーが変換するあいだ「準備中」と出ることがある。変更は次に開く曲から効く。
- まだないもの：フォルダでの表示（#20）、評価（#19）、シャッフルとリピート（#17）、ギャップレス再生（#18）、メディアキー（`sapphire-turtle/corundum-framework#50`）。

## このコンピューターのサーバー

desktop アプリは、このコンピューターのフォルダをライブラリにしたサーバーを用意できる。1台だけでほかに何も要らない。それでもウィンドウはサーバーにつなぐだけで、サーバーはバックグラウンドで動く `asteria-photos serve`（または `asteria-music serve`）。

- 初回、サーバーが未登録なら「このコンピューター」を選び、ライブラリのフォルダを選ぶ（既にライブラリのフォルダはそのまま使う）。アプリはサーバーの設定ファイルを既定の場所に書き、名前 `desktop` の API キーを作り、サーバーを「このコンピューター」（`http://127.0.0.1:7810`、music は `:7811`）として登録して選ぶ。続けて、サーバーをバックグラウンドで動かし続けるかを尋ねる（Linux は systemd のユーザーユニット、macOS は LaunchAgent、Windows はタスクスケジューラーのログオン時のタスク）。断ったときは `asteria-photos serve` で自分で起動する。最後に、見つかった外部ツールを出す。
- 設定 › ローカルサーバーでは、ライブラリと待ち受けアドレスの表示、バックグラウンドのサービスの登録・起動・停止・削除、ライブラリの API キーの管理（スマートフォンやほかのコンピューターごとに足し、使わなくなったものは外す）、外部ツールの有無の表示ができる。ローカルサーバーがまだ無いときは、「ローカルサーバーを用意する」ボタンで初回の手順をもう一度たどれる。
- サムネイルの多くはサーバー自身が作り、外部ツールは、サーバーが読めないもの（HEIC・AVIF・一部のカメラの RAW・非常に大きな画像）と動画の分を補う。無くてもサーバーは動く。サーバーが動くマシンに入れる：
  - `vipsthumbnail`（libvips に含まれる）：サーバー自身ではデコードできない HEIC・AVIF・一部のカメラの RAW の写真のサムネイル（多くは macOS のシステムの `sips` でも代わりに作れる）。Debian/Ubuntu は `sudo apt install libvips-tools`（ほかのディストリビューションも似た名前のパッケージ）、macOS は `brew install vips`、Windows は github.com/libvips/build-win64-mxe/releases の libvips を入れ、その `bin` フォルダを `PATH` に足す。
  - `ffmpeg`：photos の動画のサムネイル（music は使わない。オフライン用のコピーは自分で作る）。`sudo apt install ffmpeg`、`brew install ffmpeg`、Windows は ffmpeg.org/download.html から入れ、その `bin` フォルダを `PATH` に足す。
  - desktop アプリの Windows と macOS のパッケージには、そのアプリが使うものがはじめから入っている（photos：macOS は ffmpeg で、`vipsthumbnail` の代わりにシステムの `sips` を使う。Windows は両方。music：何もない）。
  - または `ASTERIA_VIPSTHUMBNAIL` / `ASTERIA_FFMPEG` にプログラムの場所を入れる。入れなければパッケージが置く場所（Windows では実行ファイルの横の `tools` と `vips\bin`、macOS ではアプリの `Contents/Resources`）、`PATH`、続けて `/opt/homebrew/bin` と `/usr/local/bin`（macOS）または `/usr/local/bin`（Linux）を探す。ページの「もう一度確かめる」で探し直す。

## photos クライアントからのアップロード

- デスクトップの photos ウィンドウに、ファイルやフォルダをドラッグ＆ドロップする（重ねている間は「Inbox に送る」と出る）。サーバーにつながっていないと送れない。進み具合は「アップロード」画面とステータスバーの件数で見られる。
- ファイルはライブラリの `Inbox/` の下に置かれる。フォルダは中の構造を保って `Inbox/<フォルダ名>/…` に、ファイルは `Inbox/` の直下に置く。同じフォルダで、最後の拡張子を除いた名前が同じファイル（`IMG_0001.HEIC` と `IMG_0001.MOV`、`IMG_0001.HEIC` とその `IMG_0001.HEIC.xmp`）は組にして一緒に送る。`Screenshot … 10.15.30.png` と `Screenshot … 10.42.07.png` は別々の組になる。`Inbox/` で名前が重なったら、組の全ファイルの拡張子の前に同じ番号を付ける（`IMG_0001 (2).HEIC`、`IMG_0001 (2).MOV`）。
- 送るのは写真、動画、その XMP サイドカーだけ。それ以外と隠しファイルは送らず、件数を出す（「直近のドロップのうち N 件は対象外でした」）。Lightroom 式の `IMG_0001.xmp` は、拡張子を除いた名前が `IMG_0001` のメディアが1つだけならそのサイドカーとして送り、複数に当てはまるときは送らずに警告を出す。サーバーが読めない XMP は捨てられ、組のほかのファイルは置かれる。その行には「完了（警告 N 件）」と出る（理由はマウスを重ねると出る）。
- ライブラリに既にあるファイルは送らず、「ライブラリにあります」のまま、「保存」か「破棄」を選ぶまで待つ（「すべて保存」「すべて破棄」もある）。ほかのファイルはその間も送られる。再録盤のように、別の写真や曲が同じ payload を持つことがあるので、自動では捨てない。失敗したものは「再試行」、1件の送信は「取り消し」で止められる。
- 中断した送信は、続きから再開する。アプリを閉じて開き直したあとも同じで、送信キューはライブラリとサーバープロファイルの組ごとにアプリのデータディレクトリへ保存される。確定の応答が届かなかったとき（タイムアウト、接続断、アプリを閉じた）は、送り直さずに確定を問い合わせ直す。クライアントが待つのをやめても、サーバーは確定を最後まで進める。ローカルのファイルが変わっていたら最初から送り直す。サーバーにつながらないときは待って、あとで再試行する。
- サーバー側で、7日間データが届かなかった未完了のセッションは消える。その場合、クライアントはそのファイルを最初から送り直す。
- サーバーが古くてアップロードに対応していなければ、「アップロード」画面にそう出て、何も送らない。
- 上限は、1ファイル 20 GiB。ユーザーごとの未完了のセッションは合計 64 GiB、1000 個まで（超えると QUOTA エラー）。
