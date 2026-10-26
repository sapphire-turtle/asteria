# asteria-photos の文字列。キーは `asteria-photos-<area>-<what>`。

# レール
asteria-photos-screen-library = ライブラリ
asteria-photos-screen-settings = 設定

# ライブラリの画面
asteria-photos-library-root = ライブラリ
asteria-photos-library-scanning = スキャン中（{ $count } 件）
asteria-photos-library-loading = 読み込み中…
asteria-photos-library-empty = このフォルダは空です。
asteria-photos-library-failed = このフォルダを読み込めません：{ $reason }
asteria-photos-library-retry = もう一度
asteria-photos-library-connecting = { $name } に接続しています…
asteria-photos-library-connect-failed = { $name } に接続できません：{ $reason }
asteria-photos-library-no-servers = 写真を見るには、サーバーを追加してください。
asteria-photos-library-no-active = 写真を見るサーバーを選んでください。
asteria-photos-library-open-servers = サーバーの設定

# サーバーの切り替え（ステータスバー、狭い画面では下のバー）
asteria-photos-status-none = サーバーなし
asteria-photos-status-connected = 接続済み
asteria-photos-status-connecting = 接続中
asteria-photos-status-failed = 接続できません：{ $reason }
asteria-photos-status-degraded = 応答がありません：{ $reason }
asteria-photos-status-manage = サーバーを管理…
asteria-photos-status-hover = { $name }：{ $status }

# サーバーの一覧の保存
asteria-photos-save-failed = サーバーの一覧を保存できません：{ $reason }
asteria-photos-load-failed = サーバーの一覧を読めないため、変更は保存されません：{ $reason }

# 詳細ペイン（ライブラリの横のサイドペイン）
asteria-photos-detail-none = 写真を選ぶと、ここに詳細が表示されます。
asteria-photos-detail-other-screen = ライブラリを見ているあいだ、選んだ写真の詳細がここに表示されます。
asteria-photos-detail-open = 全画面で見る
asteria-photos-detail-folder = フォルダ
asteria-photos-detail-size = サイズ
asteria-photos-detail-modified = 更新日時
asteria-photos-detail-video = 動画です。静止画だけを表示しています。
asteria-photos-detail-preview-failed = プレビューを読み込めませんでした：{ $reason }
asteria-photos-detail-read-only = この項目は編集できません。
asteria-photos-detail-read-only-reason = 理由：{ $reason }
asteria-photos-detail-busy = サーバーが別の変更を処理中だったため、保存できませんでした。もう一度お試しください。
asteria-photos-detail-save-failed = 変更を保存できませんでした：{ $reason }
asteria-photos-detail-saving = 保存中…
asteria-photos-detail-rating = 評価
asteria-photos-detail-rating-none = 評価なし
asteria-photos-detail-rating-set = 星 { $stars } つにする
asteria-photos-detail-rating-clear = 評価を消す
asteria-photos-detail-label = カラーラベル
asteria-photos-detail-label-none = なし
asteria-photos-detail-label-red = 赤
asteria-photos-detail-label-yellow = 黄
asteria-photos-detail-label-green = 緑
asteria-photos-detail-label-blue = 青
asteria-photos-detail-label-purple = 紫
asteria-photos-detail-subjects = キーワード
asteria-photos-detail-subjects-add = キーワードを追加
asteria-photos-detail-hierarchical = 階層タグ
asteria-photos-detail-hierarchical-add = タグを追加（例：場所|日本）
asteria-photos-detail-tags-empty = なし
asteria-photos-detail-tag-add = 追加
asteria-photos-detail-tag-remove = 「{ $tag }」を外す
asteria-photos-detail-description = 説明
asteria-photos-detail-description-hint = 欄から離れると保存されます
asteria-photos-detail-description-empty = 説明なし

# 全画面プレビュー
asteria-photos-viewer-close = 閉じる（Esc）
asteria-photos-viewer-previous = 前へ（←）
asteria-photos-viewer-next = 次へ（→）
asteria-photos-viewer-details = 詳細
asteria-photos-viewer-position = { $index } / { $total }

# アップロードの画面
asteria-photos-screen-uploads = アップロード
asteria-photos-uploads-unavailable = ファイルを送るには、サーバーにつないでください。
asteria-photos-uploads-empty = 写真やフォルダをウィンドウにドロップすると、サーバーの Inbox に送ります。
asteria-photos-uploads-starting = アップロードのキューを開いています…
asteria-photos-uploads-waiting = サーバーが応答しないため、送信を止めています。あとでもう一度試します：{ $reason }
asteria-photos-uploads-stopped = 送信を止めました：{ $reason }
asteria-photos-uploads-queue-unreadable = アップロードのキューを読めないので、何も送りません：{ $reason }
asteria-photos-uploads-clear-finished = 完了を消す
asteria-photos-uploads-in-library = { $count } 件がライブラリにあります
asteria-photos-uploads-keep-all = すべて保存
asteria-photos-uploads-discard-all = すべて破棄
asteria-photos-uploads-keep = 保存
asteria-photos-uploads-discard = 破棄
asteria-photos-uploads-retry = 再試行
asteria-photos-uploads-cancel = 取り消し
asteria-photos-uploads-existing = ライブラリにあるファイル：
asteria-photos-uploads-excluded = 直近のドロップのうち { $count } 件は対象外でした（隠しファイル、または写真・動画ではないもの）。
asteria-photos-uploads-warn-sidecar = 複数のファイルに当てはまるため送りません：{ $path }
asteria-photos-uploads-warn-unreadable = { $path } を読めません：{ $reason }
asteria-photos-uploads-state-hashing = 読み込み中
asteria-photos-uploads-state-checking = ライブラリを確認中
asteria-photos-uploads-state-awaiting = ライブラリにあります
asteria-photos-uploads-state-uploading = 送信中
asteria-photos-uploads-state-committing = 保存中
asteria-photos-uploads-state-done = 完了
asteria-photos-uploads-state-discarded = 破棄しました
asteria-photos-uploads-state-failed = 失敗：{ $reason }

# ウィンドウへのドロップと、ステータスバーの件数
asteria-photos-drop-overlay = Inbox に送る
asteria-photos-drop-refused = サーバーにつながっていないので送れません
asteria-photos-status-uploads = ↑ { $done } / { $total }
asteria-photos-status-uploads-awaiting = ↑ { $done } / { $total }・判断待ち { $count }
asteria-photos-status-uploads-hover = アップロードを開く
