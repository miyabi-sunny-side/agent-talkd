# agent-talk

Tailscale 越しに Herdr の Codex / Claude Code セッションへメッセージを送り、返答・報告を読むための小さなブラウザ画面です。スマホから送った原文は選んだ CLI の会話に残るので、帰宅後そのセッションで作業を続けられます。

一覧で作業先・CLI・状態を確認し、対象を開いて手紙を送ります。返答は各 CLI が保存する会話履歴から Markdown で表示します。利用者の文面とコードの文字・空白は保持します。「会話」「画面」を切り替えても同じ宛先へ手紙を送れ、下書きも保たれます。短いタブ名や作業ディレクトリで宛先を区別できます。端末画面を操作するための記号キーや Ctrl / Shift 操作はありません。

「画面」は Herdr CLI の `pane read --source visible --format text` が返す現在の表示テキストです。ピクセル単位の窓画像ではなく、色・カーソル・端末の装飾は再現しません。行と空白を保持し、縦横にスクロールできます。取得成功時刻を表示し、切断・終了・入替・取得失敗時は以前の表示であることを示します。表示中だけ2秒ごとに取得し、別タブや非表示では停止します。送信できない未登録・対応外 CLI・承認待ちの画面も閲覧できます。画面の常駐記録やキー転送は行いません。

## 起動と配布

Linux x86_64 / macOS arm64 向け release archive は `agent-talk` と `LICENSE` を含みます。Svelte 画面はバイナリに埋め込みます。

```sh
PORT=5002 agent-talk daemon
```

daemon 起動時に HTTP を待ち受けます。sandbox の通常経路は Tailscale HTTPS です。アクセス範囲は Tailscale やプロキシなど配布側の構成が管理します。同権限のプロセスが API を使うことまで禁止する認証機構はありません。

CLI は `daemon`、`update`、`--help`、`--version` のみです。`update` は checksum を検証して実行ファイルを更新し、稼働サービスの再起動は運用側で行います。

## 環境変数

### アプリ設定

次の設定は `daemon` 起動時に読みます。待受アドレスは `0.0.0.0` 固定です。

| 変数 | 用途 | 必須・既定値 | 値の扱い |
|---|---|---|---|
| `PORT` | HTTP 待受ポート | 任意、未設定時 `5002` | ASCII 数字だけの `1`〜`65535`。空文字、空白・符号付き、範囲外、非 UTF-8 はエラーで起動停止。 |
| `LOG_LEVEL` | stderr に出すログのレベル | 任意、未設定時 `info` | 小文字の `off` / `error` / `warn` / `info` / `debug` / `trace`。空文字、空白付き、大文字、モジュール別フィルター、非 UTF-8 など不正値は `info`。 |

旧 `AGENT_TALK_HTTP_ADDR` と `AGENT_TALK_HERDR_SOCKET` は参照しません。ログ設定も `AGENT_TALK_LOG_LEVEL` や `RUST_LOG` は参照せず、`LOG_LEVEL` のみです。廃止した設定への互換 fallback はありません。

### OS・CLI の実行環境

| 変数 | 用途 | 必須・既定値 | 未設定・不正値の扱い |
|---|---|---|---|
| `HOME` | daemon が読む native 会話履歴の基点（`$HOME/.codex/sessions`、`$HOME/.claude/projects`） | daemon では必須、アプリの既定値なし | 未設定は起動エラー。空文字・相対パス・存在しないパスは起動時に検証せず、履歴取得時に解決・読み取りできなければエラー。 |
| `PATH` | daemon の `herdr`、update の `curl` の実行ファイル探索 | 各 CLI を実行できる環境が必要、アプリの既定値なし | OS の探索規則に従う。見つからない・実行できない場合は CLI 呼び出し時にエラー（daemon 起動時の検査はなし）。 |
| `TMPDIR` | update のダウンロード・展開用一時ディレクトリ（`tempfile` 経由） | 任意、未設定時は OS の一時領域 | [Rust の `temp_dir`](https://doc.rust-lang.org/std/env/fn.temp_dir.html) に従う。指定先へ一時ディレクトリを作成できなければ update はエラー。 |

agent-talk の `update` / `--help` / `--version` はアプリの設定読込を行いません。`herdr` と `curl` は起動元の環境を継承し、各 CLI 自身の設定はそれぞれが解釈します。

### Herdr が所有する設定

接続先の解決と RPC 通信は Herdr CLI が所有します。Herdr の接続に環境変数が必要なら、agent-talk を起動するサービスの環境に渡してください。既定値と不正値の扱いは利用する Herdr の仕様に従い、agent-talk では独自に解釈・上書きしません。

実装の参照先: [設定読込](src/config.rs)、[コマンド振り分け](src/main.rs)、[Herdr CLI 呼び出し](src/herdr.rs)、[履歴読込](src/history.rs)、[更新処理](src/update.rs)。

## 対応とセッションの接続

Codex と Claude Code に対応します。Herdr の公式 integration hook が報告する native session ID / path と、実行中プロセスを照合して送信先を決めます。Grok などは一覧に表示しますが、入力と履歴は未対応です。

SessionStart hook が最初のプロンプト後に動く CLI では、端末で最初の会話を開始するまで「セッション未登録」と表示します。未登録、承認待ち、状態不明、終了、再起動した対象へは送信しません。対象が変わったら下書きを保持して理由を表示します。

Herdr が原文入力を受け付けても、CLI の処理完了を意味しません。実際の出力は会話に表示されます。送信途中の接続切断で受付が確認できない場合は、会話を確認してから再送してください。自動再送や永続キューはありません。

履歴は Codex の `~/.codex/sessions` と Claude の `~/.claude/projects` にある JSONL を読み、user / assistant のテキストを表示します。tool の内部応答や推論内容は表示しません。直近から開き、「古い会話」「新しい会話」で長い履歴をページ単位に辿れます。各取得は最大 2 MiB / 500 メッセージで、新着は差分取得します。過去を読んでいる間の更新や再接続で読み位置を保持し、「最新へ戻る」で直近末尾へ戻れます。表示は最大 1000 件 / 4 MiB の本文までとし、上限では既存の表示を保持してページ移動を案内します。巨大な単一記録や壊れた記録の省略は画面に示します。ページ再読込は同じセッションの直近から再開します。履歴が利用できない場合は端末画面を代替の報告として扱わずエラーを表示します。

確認した Herdr 0.8.2 の CLI は session identity を原子的に比較して prompt を送る操作を持たないため、照合直後に同じ pane のプロセスが入れ替わる短い競合窓は残ります。送信直前の session / foreground process 照合と Herdr の blocked 判定を使い、名前による曖昧な宛先解決を避けます。

履歴取得を Herdr へ集約するための [必要契約と不足機能](docs/history-ownership.md) を整理しています。Herdr 0.8.2 は native session の参照を提供しますが、会話ページの取得 API はありません。実移譲と agent-talk の HOME 依存解消は未実装で、依存機能が提供されるまでは上記の会話表示を維持します。

## HTTP API

| 操作 | API |
|---|---|
| health / version | `GET /api/hello` |
| Herdr 内の対象一覧 | `GET /api/agents` |
| 対象 CLI の会話 | `GET /api/conversation?pane=<pane>&session=<session_id>` |
| 会話の前後ページ | 同 URL に `before=<cursor>` または `after=<cursor>`（排他） |
| 閲覧専用の現在画面 | `GET /api/screen?pane=<pane>&terminal=<terminal_id>`（任意で `session=<session_id>`） |
| 原文メッセージ入力 | `POST /api/messages` |

画面取得は最大256 KiBです。取得前後の端末・セッションの一致を確認し、`pane_id`、`terminal_id`、`session_id`（未登録なら `null`）、`text`、`format: "text"`、`captured_at`（epoch milliseconds）を返します。

送信は `Content-Type: application/json` で `{"pane_id":"w1:p3","session_id":"一覧の識別子","body":"原文"}` を渡します。本文は最大 32 KiB、空白だけの本文と改行・タブ以外の制御文字を拒否します。成功応答は `{"status":"submitted"}`。エラーは `{"error":{"code":"...","message":"..."}}` です。CORS を開放せず、異なる Origin と cross-site fetch を拒否します。

会話レスポンスの `older_cursor` は過去ページ（先頭なら `null`）、`next_cursor` はそのページの続きの取得に使います。`has_more` は要求方向に続きがあること、`pending_tail` は書きかけの末尾待ち、`truncated` は読み取れない記録の省略を表します。カーソルはその native 履歴専用です。ファイルの置換・縮小等で `cursor_changed`（409）になったら、表示中の会話を保持して直近を開き直します。native 履歴の通常の追記を前提とし、独立した会話の保存・再送は行いません。

## 旧 broker からの移行

エージェント間通信、`agent-talk-mcp`、peer API、UDS broker、mailbox、journal 中継、ack、呼び鈴を廃止しました。旧 MCP 登録と関連 helper / 起動設定を削除し、通常の Herdr integration hook を配布してください。sandbox-server と dotfiles の通常 installer がこの構成を所有します。

旧 journal は自動削除・再生・移行しません。過去の非実行記録として必要なら保存できます。新しい指示は既存 CLI の履歴だけに残ります。旧 `*.sock` と `*.http.sock` は旧 daemon 停止後に運用側で片付けられます。新 daemon は broker socket や状態ディレクトリを作りません。

## 開発

Rust 1.96 / Node 24 / npm を使います。検証コマンドは [AGENTS.md](AGENTS.md)、UI 契約は [DESIGN.md](DESIGN.md)、内部境界は [docs/design.md](docs/design.md) を参照してください。CLI adapter の隔離テストは Python 3 の stub を使います。実機テストでは専用 Herdr pane と検証用 HTTP port を使用します。
