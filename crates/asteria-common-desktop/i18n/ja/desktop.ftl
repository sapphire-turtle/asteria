# asteria-common-desktop の文字列。キーは `asteria-common-desktop-<area>-<what>`。

# このコンピューターのサーバー。サーバーの一覧での名前。
asteria-common-desktop-this-computer = このコンピューター

# 初回起動：ライブラリの場所。
asteria-common-desktop-where-title = ライブラリはどこにありますか？
asteria-common-desktop-where-this-computer-note = このコンピューターのフォルダを選びます。ここでサーバーが動き、アプリはそれにつなぎます。
asteria-common-desktop-where-other = ほかのサーバー
asteria-common-desktop-where-other-note = ライブラリはほかのコンピューターにあります。「サーバー」でそのサーバーを登録します。
asteria-common-desktop-where-later = あとで
asteria-common-desktop-where-later-note = あとで設定から用意します。

# 初回起動：このコンピューターのサーバーを用意する。
asteria-common-desktop-setup-title = このコンピューターにライブラリを用意する
asteria-common-desktop-setup-intro = ライブラリのフォルダを選んでください。既にライブラリならそのまま使い、そうでなければライブラリにします。
asteria-common-desktop-setup-owner = あなたのユーザー名
asteria-common-desktop-setup-owner-note = ここで作るライブラリの持ち主です。アプリでの操作はこのユーザーとして記録されます。
asteria-common-desktop-setup-owner-invalid = 半角英数字と - _ で 1〜{ $max } 文字にしてください。
asteria-common-desktop-setup-existing-owner = このフォルダは既にライブラリだったので、持ち主 { $owner } のままです。
asteria-common-desktop-setup-choose = フォルダを選ぶ…
asteria-common-desktop-setup-picker-title = ライブラリのフォルダ
asteria-common-desktop-setup-running = { $path } を用意しています…
asteria-common-desktop-setup-done = { $path } のライブラリを用意しました（持ち主：{ $owner }）。アプリは { $url } でつなぎます。
asteria-common-desktop-setup-failed = ライブラリを用意できませんでした：{ $error }
asteria-common-desktop-setup-stopped = 用意の途中で止まりました
asteria-common-desktop-setup-try-again = もう一度
asteria-common-desktop-setup-other-folder = ほかのフォルダを選ぶ

# 初回起動：バックグラウンドのサービス。
asteria-common-desktop-service-question = サーバーをバックグラウンドで動かし続けますか？
asteria-common-desktop-service-reason = アプリはサーバーを通してライブラリを見せます。バックグラウンドで動かすと、ログインしたときに起動し、ライブラリを見張り続けます。
asteria-common-desktop-not-running = サーバーは動いていません。起動するには、次を実行します：

# 外部ツール。
asteria-common-desktop-tools-title = 外部ツール
asteria-common-desktop-tools-intro = サムネイルの多くはサーバー自身が作ります。これらのプログラムは、サーバーが読めないもの（HEIC・AVIF・一部のカメラの RAW・非常に大きな画像）と動画の分を補います。無くてもサーバーは動きます。
asteria-common-desktop-tools-vips-purpose = HEIC・AVIF と一部のカメラの RAW のサムネイル。
asteria-common-desktop-tools-ffmpeg-purpose = 動画のサムネイル。
asteria-common-desktop-tools-found = 見つかりました：{ $path }
asteria-common-desktop-tools-missing = 見つかりません。
asteria-common-desktop-tools-sips = 入っていません。それらの画像は macOS に入っている sips で作ります。
asteria-common-desktop-tools-linux = ディストリビューションのパッケージマネージャーで、パッケージ { $package } を入れてください。
asteria-common-desktop-tools-macos = Homebrew で入れてください：brew install { $package }
asteria-common-desktop-tools-windows-vips = Windows 版の libvips（github.com/libvips/build-win64-mxe/releases）をダウンロードし、その bin フォルダを PATH に足してください。
asteria-common-desktop-tools-windows-ffmpeg = Windows 版の FFmpeg（ffmpeg.org/download.html）をダウンロードし、その bin フォルダを PATH に足してください。
asteria-common-desktop-tools-env = または { $var } にプログラムのパスを設定してください。
asteria-common-desktop-tools-none = このアプリのサーバーは外部ツールを使いません。
asteria-common-desktop-tools-check-again = もう一度確かめる

# 設定の「ローカルサーバー」ページ。
asteria-common-desktop-page-label = ローカルサーバー
asteria-common-desktop-page-not-set-up = まだローカルサーバーを用意していません。
asteria-common-desktop-page-set-up = ローカルサーバーを用意する
asteria-common-desktop-page-unreadable = ローカルサーバーの設定ファイルを読めませんでした：{ $error }
asteria-common-desktop-page-library = ライブラリ
asteria-common-desktop-page-listen = 待ち受けアドレス
asteria-common-desktop-page-config = 設定ファイル
asteria-common-desktop-page-listen-note = ほかの機器からの接続を受けるには、設定ファイルの listen を変えてください。
asteria-common-desktop-page-service = バックグラウンドのサーバー
asteria-common-desktop-page-service-note = バックグラウンドのサービスを使わないときは、自分でサーバーを起動します：
asteria-common-desktop-page-keys = API キー
asteria-common-desktop-page-keys-failed = API キーを開けませんでした：{ $error }

# 設定の「情報」ページ。
asteria-common-desktop-about = 情報
asteria-common-desktop-about-version = バージョン { $version }
asteria-common-desktop-about-licence-ffmpeg-vips = Asteria は GPL-3.0-or-later のフリーソフトウェアです。Windows と macOS のパッケージには ffmpeg（GPLv3）、Windows にはさらに libvips（LGPL）が入っています。それぞれのライセンスとソースの場所は、プログラムの横（macOS ではアプリの Contents/Resources）の tools/LICENSES にあります。
asteria-common-desktop-about-licence-vips = Asteria は GPL-3.0-or-later のフリーソフトウェアです。Windows のパッケージには libvips（LGPL）が入っています。そのライセンスとソースの場所は、プログラムの横の tools/LICENSES にあります。
asteria-common-desktop-about-licence-none = Asteria は GPL-3.0-or-later のフリーソフトウェアです。
