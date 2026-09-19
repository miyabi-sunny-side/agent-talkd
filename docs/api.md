# HTTP API

起動済みのagent-talkへ接続するクライアント向けの参照です。
起動とアクセス範囲は[README](../README.md)、環境設定は[設定と移行](operations.md)を参照してください。
まず`GET /api/hello`で接続を確認し、`GET /api/agents`から対象の識別子を取得します。

| 操作 | API |
|---|---|
| health / version | `GET /api/hello` |
| Herdr 内の対象一覧 | `GET /api/agents` |
| 対象 CLI の会話 | `GET /api/conversation?pane=<pane>&session=<session_id>` |
| 会話の前後ページ | 同 URL に `before=<cursor>` または `after=<cursor>`（排他） |
| 閲覧専用の現在画面 | `GET /api/screen?pane=<pane>&terminal=<terminal_id>`（任意で `session=<session_id>`） |
| 原文メッセージ入力 | `POST /api/messages` |

## 現在の画面

画面取得は最大256 KiBです。取得前後の端末・セッションの一致を確認します。
応答の識別子は`pane_id`、`terminal_id`、`session_id`（未登録なら`null`）です。
表示内容は`text`、形式は`format: "text"`、取得時刻は`captured_at`（epoch milliseconds）に入ります。

## メッセージの送信

送信は`Content-Type: application/json`を指定します。本文の例:

```json
{"pane_id":"w1:p3","session_id":"一覧の識別子","body":"原文"}
```

本文は最大 32 KiB、空白だけの本文と改行・タブ以外の制御文字を拒否します。成功応答は `{"status":"submitted"}`。エラーは `{"error":{"code":"...","message":"..."}}` です。CORS を開放せず、異なる Origin と cross-site fetch を拒否します。

## 会話のページ取得

会話レスポンスの `older_cursor` は過去ページ（先頭なら `null`）、`next_cursor` はそのページの続きの取得に使います。`has_more` は要求方向に続きがあること、`pending_tail` は書きかけの末尾待ち、`truncated` は読み取れない記録の省略を表します。カーソルはその native 履歴専用です。ファイルの置換・縮小等で `cursor_changed`（409）になったら、表示中の会話を保持して直近を開き直します。native 履歴の通常の追記を前提とし、独立した会話の保存・再送は行いません。
