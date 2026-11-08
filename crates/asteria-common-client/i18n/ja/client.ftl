# asteria-common-client の文字列。キーは `asteria-common-client-<area>-<what>`。

# サムネイルのグリッド。
asteria-common-client-grid-failed = サムネイルを読み込めませんでした: { $reason }
asteria-common-client-grid-video = 動画
asteria-common-client-grid-read-only = 読み取り専用: { $reason }
asteria-common-client-grid-rating = 5段階中 { $stars }

# サーバー設定のページ。
asteria-common-client-servers-title = サーバー
asteria-common-client-servers-empty = サーバーがまだありません。追加してください。
asteria-common-client-servers-in-use = 使用中
asteria-common-client-servers-use = このサーバーを使う
asteria-common-client-servers-add = サーバーを追加
asteria-common-client-servers-edit = 編集
asteria-common-client-servers-delete = 削除
asteria-common-client-servers-delete-confirm = 「{ $name }」を削除しますか？
asteria-common-client-servers-delete-yes = 削除する
asteria-common-client-servers-cancel = キャンセル
asteria-common-client-servers-form-add = 新しいサーバー
asteria-common-client-servers-form-edit = サーバーを編集
asteria-common-client-servers-name = 名前
asteria-common-client-servers-url = アドレス
asteria-common-client-servers-token = アクセストークン
asteria-common-client-servers-token-show = 表示
asteria-common-client-servers-token-hide = 隠す
asteria-common-client-servers-save = 保存
asteria-common-client-servers-test = 接続を確かめる
asteria-common-client-servers-testing = 接続しています…
asteria-common-client-servers-test-ok = 接続できました。{ $count } 件、オーナー: { $owner }
asteria-common-client-servers-test-failed = 接続できませんでした: { $reason }
asteria-common-client-servers-error-name = 名前を入力してください。
asteria-common-client-servers-error-url = http:// か https:// で始まるアドレスを入力してください（例: http://192.168.1.10:7810）。
asteria-common-client-servers-error-unreachable = サーバーに接続できません（{ $detail }）
asteria-common-client-servers-error-unauthorized = サーバーがアクセストークンを受け付けませんでした
asteria-common-client-servers-error-app-mismatch = このサーバーは別のアプリ用です
asteria-common-client-servers-error-api-mismatch = サーバーとアプリのバージョンが合いません（アプリ {$client}、サーバー {$server}）
asteria-common-client-servers-error-busy = サーバーが混み合っています（{ $seconds } 秒後に再試行）
asteria-common-client-servers-error-unprocessable = サーバーはこのファイルを処理できません
asteria-common-client-servers-error-not-found = サーバーにもうありません
asteria-common-client-servers-error-library-changed = サーバーのライブラリが別のものに変わりました
asteria-common-client-servers-error-server = サーバーがエラーを返しました
asteria-common-client-servers-error-protocol = サーバーの応答を読み取れません
asteria-common-client-servers-error-local = この端末で問題が起きました
asteria-common-client-servers-error-not-saved = オフライン用に保存していません
asteria-common-client-servers-error-offline = オフラインでは編集できません

# The server switcher in the status bar.
asteria-common-client-status-none = サーバーなし
asteria-common-client-status-connected = 接続済み
asteria-common-client-status-connecting = 接続中
asteria-common-client-status-failed = 接続できません：{ $reason }
asteria-common-client-status-degraded = 応答がありません：{ $reason }
asteria-common-client-status-offline = オフライン（保存したものを表示中）
asteria-common-client-status-manage = サーバーを管理…
asteria-common-client-status-reconnect = 再接続
asteria-common-client-status-hover = { $name }：{ $status }
