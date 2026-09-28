# 設定と移行

通常の導入は[README](../README.md)を参照してください。既存環境の設定確認と移行のための文書です。

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

実装の参照先: [設定読込](../src/config.rs)、[コマンド振り分け](../src/main.rs)、[Herdr CLI 呼び出し](../src/herdr.rs)、[履歴読込](../src/history.rs)、[更新処理](../src/update.rs)。

## 対応とセッションの接続

Codex と Claude Code に対応します。Herdr の公式 integration hook が報告する native session ID / path と、実行中プロセスを照合して送信先を決めます。Grok などは一覧に表示しますが、入力と履歴は未対応です。

SessionStart hook が最初のプロンプト後に動く CLI では、端末で最初の会話を開始するまで「セッション未登録」と表示します。未登録、承認待ち、状態不明、終了、再起動した対象へは送信しません。対象が変わったら下書きを保持して理由を表示します。

Herdr が原文入力を受け付けても、CLI の処理完了を意味しません。実際の出力は会話に表示されます。送信途中の接続切断で受付が確認できない場合は、会話を確認してから再送してください。自動再送や永続キューはありません。

履歴は各CLIのJSONLからuser / assistantのテキストを表示します。
保存先はCodexの`~/.codex/sessions`、Claudeの`~/.claude/projects`です。tool の内部応答や推論内容は表示しません。

手紙に添えた画像は`~/.cache/agent-talk/images`（所有者のみ読み書き可）に保存し、保存から24時間後にdaemonが削除します。
削除はdaemonの起動時と10分ごとに行い、このディレクトリの`image-`で始まる通常ファイルだけを対象にします。
同じホストの同じユーザーで動くCodex / Claude Codeが、本文のパスから読み取れます。

直近から開き、「古い会話」「新しい会話」で長い履歴をページ単位に辿れます。各取得は最大2 MiB、最大500メッセージです。新着は差分取得します。

過去を読んでいる間の更新や再接続で読み位置を保持し、「最新へ戻る」で直近末尾へ戻れます。

表示は最大 1000 件 / 4 MiB の本文までとし、上限では既存の表示を保持してページ移動を案内します。
巨大な単一記録や壊れた記録の省略は画面に示します。ページ再読込は同じセッションの直近から再開します。
履歴が利用できない場合は端末画面を代替の報告として扱わずエラーを表示します。

Herdr 0.8.2では、session identityの比較とprompt送信を一つの操作で行えません。
照合直後に同じpaneのプロセスが入れ替わる短い競合窓は残ります。送信直前の session / foreground process 照合と Herdr の blocked 判定を使い、名前による曖昧な宛先解決を避けます。

「会話」は native 履歴を読む機能、「画面」は Herdr の端末表示を定期取得して出先から様子を確認する機能です。取得元と更新は独立しており、会話履歴を取得できなくても「画面」を開けます。詳しくは [会話履歴と端末画面の取得責務](history-ownership.md) を参照してください。

## 旧 broker からの移行

エージェント間通信、`agent-talk-mcp`、peer API、UDS broker、mailbox、journal 中継、ack、呼び鈴を廃止しました。旧 MCP 登録と関連 helper / 起動設定を削除し、通常の Herdr integration hook を配布してください。旧構成を配布したサービス定義やCLI設定から解除します。新規導入ではこの移行は不要です。

旧 journal は自動削除・再生・移行しません。過去の非実行記録として必要なら保存できます。新しい指示は既存 CLI の履歴だけに残ります。旧 `*.sock` と `*.http.sock` は旧 daemon 停止後に運用側で片付けられます。新 daemon は broker socket や状態ディレクトリを作りません。

