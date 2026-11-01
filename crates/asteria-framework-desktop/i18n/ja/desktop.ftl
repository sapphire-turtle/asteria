# asteria-framework-desktop の文字列。キーは `asteria-desktop-<area>-<what>`。

# このコンピューターのサーバー。サーバーの一覧での名前。
asteria-desktop-this-computer = このコンピューター

# 初回起動：ライブラリの場所。
asteria-desktop-where-title = ライブラリはどこにありますか？
asteria-desktop-where-this-computer-note = このコンピューターのフォルダを選びます。ここでサーバーが動き、アプリはそれにつなぎます。
asteria-desktop-where-other = ほかのサーバー
asteria-desktop-where-other-note = ライブラリはほかのコンピューターにあります。「サーバー」でそのサーバーを登録します。
asteria-desktop-where-later = あとで
asteria-desktop-where-later-note = あとで設定から用意します。

# 初回起動：このコンピューターのサーバーを用意する。
asteria-desktop-setup-title = このコンピューターにライブラリを用意する
asteria-desktop-setup-intro = ライブラリのフォルダを選んでください。既にライブラリならそのまま使い、そうでなければライブラリにします。
asteria-desktop-setup-choose = フォルダを選ぶ…
asteria-desktop-setup-picker-title = ライブラリのフォルダ
asteria-desktop-setup-running = { $path } を用意しています…
asteria-desktop-setup-done = { $path } のライブラリを用意しました。アプリは { $url } でつなぎます。
asteria-desktop-setup-failed = ライブラリを用意できませんでした：{ $error }
asteria-desktop-setup-stopped = 用意の途中で止まりました
asteria-desktop-setup-try-again = もう一度
asteria-desktop-setup-other-folder = ほかのフォルダを選ぶ

# 初回起動：バックグラウンドのサービス。
asteria-desktop-service-question = サーバーをバックグラウンドで動かし続けますか？
asteria-desktop-service-reason = アプリはサーバーを通してライブラリを見せます。バックグラウンドで動かすと、ログインしたときに起動し、ライブラリを見張り続けます。
asteria-desktop-not-running = サーバーは動いていません。起動するには、次を実行します：

# 外部ツール。
asteria-desktop-tools-title = 外部ツール
asteria-desktop-tools-intro = サーバーはこれらのプログラムでサムネイルや小さい版を作ります。無くてもサーバーは動きますが、それらは作られません。
asteria-desktop-tools-vips-purpose = 画像のサムネイル。
asteria-desktop-tools-ffmpeg-purpose = 動画のサムネイルと、オフライン用の音楽の小さい版。
asteria-desktop-tools-found = 見つかりました：{ $path }
asteria-desktop-tools-missing = 見つかりません。
asteria-desktop-tools-linux = ディストリビューションのパッケージマネージャーで、パッケージ { $package } を入れてください。
asteria-desktop-tools-macos = Homebrew で入れてください：brew install { $package }
asteria-desktop-tools-windows-vips = Windows 版の libvips（github.com/libvips/build-win64-mxe/releases）をダウンロードし、その bin フォルダを PATH に足してください。
asteria-desktop-tools-windows-ffmpeg = Windows 版の FFmpeg（ffmpeg.org/download.html）をダウンロードし、その bin フォルダを PATH に足してください。
asteria-desktop-tools-env = または { $var } にプログラムのパスを設定してください。
asteria-desktop-tools-check-again = もう一度確かめる

# 設定の「ローカルサーバー」ページ。
asteria-desktop-page-label = ローカルサーバー
asteria-desktop-page-not-set-up = まだローカルサーバーを用意していません。
asteria-desktop-page-set-up = ローカルサーバーを用意する
asteria-desktop-page-unreadable = ローカルサーバーの設定ファイルを読めませんでした：{ $error }
asteria-desktop-page-library = ライブラリ
asteria-desktop-page-listen = 待ち受けアドレス
asteria-desktop-page-config = 設定ファイル
asteria-desktop-page-listen-note = ほかの機器からの接続を受けるには、設定ファイルの listen を変えてください。
asteria-desktop-page-service = バックグラウンドのサーバー
asteria-desktop-page-service-note = バックグラウンドのサービスを使わないときは、自分でサーバーを起動します：
asteria-desktop-page-keys = API キー
asteria-desktop-page-keys-failed = API キーを開けませんでした：{ $error }
