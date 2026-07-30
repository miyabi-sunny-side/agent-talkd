# agent-talkd

tmux 上の Claude/Codex などの対話エージェント間で、安全に依頼を受け渡す
Rust 製メッセージブローカーです。デーモンが状態を一元管理し、CLI は Unix
ドメインソケット経由で既存の hooks と互換な操作を提供します。

## インストール

Linux x86_64 と macOS Apple Silicon のビルド済みバイナリを
[GitHub Releases](https://github.com/miyabi-sunny-side/agent-talkd/releases)
で配布しています。対応する `.sha256` ファイルで整合性を確認できます。

```sh
(
set -eu
mkdir -p ~/.local/bin

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) archive=agent-talk-linux-x86_64.tar.gz ;;
  Darwin-arm64) archive=agent-talk-macos-aarch64.tar.gz ;;
  *) echo "unsupported platform" >&2; exit 1 ;;
esac

tmpdir="$(mktemp -d)"
if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
  gh release download --repo miyabi-sunny-side/agent-talkd \
    --pattern "$archive" --pattern "$archive.sha256" --dir "$tmpdir"
else
  curl -fL "https://github.com/miyabi-sunny-side/agent-talkd/releases/latest/download/$archive" \
    -o "$tmpdir/$archive"
  curl -fL "https://github.com/miyabi-sunny-side/agent-talkd/releases/latest/download/$archive.sha256" \
    -o "$tmpdir/$archive.sha256"
fi

if [ "$(uname -s)" = Darwin ]; then
  (cd "$tmpdir" && shasum -a 256 -c "$archive.sha256")
else
  (cd "$tmpdir" && sha256sum -c "$archive.sha256")
fi
tar -xzf "$tmpdir/$archive" -C "$tmpdir"
install -m755 "$tmpdir/agent-talk" ~/.local/bin/agent-talk
rm -rf "$tmpdir"
)
```

ソースからビルドする場合:

```sh
git clone https://github.com/miyabi-sunny-side/agent-talkd.git
cd agent-talkd
npm --prefix client ci
npm --prefix client run build
cargo build --release --locked
mkdir -p ~/.local/bin
install -m755 target/release/agent-talk ~/.local/bin/agent-talk
```

フロントエンドのビルドにはNode.js 24とnpmを使います。`client/dist`を先に生成すると、
続くCargoビルドがその静的ファイルを単一の`agent-talk`バイナリへ埋め込みます。
`build.rs`からnpmは起動しません。`client/dist`がない状態でもCargoビルド自体は成功しますが、
そのバイナリの静的ページは503を返します。

## tmuxプラグイン

[TPM](https://github.com/tmux-plugins/tpm)を使う場合、`.tmux.conf` に次を追加します。

```tmux
set -g @plugin 'miyabi-sunny-side/agent-talkd'
```

`prefix + I` でプラグインを取得します。プラグインはバイナリを自動ビルド・
ダウンロードしないため、先に `~/.local/bin/agent-talk` を配置してください。

## CLI

`run`, `register`, `unregister`, `busy`, `idle`, `turn-end`, `who`, `resolve`,
`send`, `read`, `update`, `ensure-daemon`, `daemon-status` を提供します。
互換用の `gc`, `watch` は no-op です。デーモンが未起動なら CLI が tmux
サーバー単位で自動起動し、既存デーモンの版が古ければ安全に交代します。
インストール済みのversionは `agent-talk --version` で確認できます。
各サブコマンドの使い方は `agent-talk <command> --help` で確認できます。

`run` は子プロセスの実行中だけ現在のpaneを登録し、終了時に登録解除します。
たとえば `_agent_talk_run codex codex "$@"` は次のように置き換えられます。

```sh
agent-talk run codex codex "$@"
```

外部連携は `--from` で許可された mailbox に送信し、`mailbox-list-v1` で
read-only に取得できます。返信は agent pane 内で `agent-talk reply <id> 本文`
を実行します。mailbox event は consume されず、各 mailbox の最新500件を保持します。
`TMUX_PANE` なしは外部callerの誤用防止規約であり、実際のRPC境界は同一UIDのUnix
socketです。

`mailbox-list-v1` の安定JSON schema:

```json
{"version":1,"mailbox":"mobile","events":[{"id":12,"created_at":"2026-07-21T11:00:00Z","mailbox":"mobile","source_label":"mobile","direction":"out","body":"依頼","skill":"deliver","target_name":"claude","target_pane":"%1","reply_to":null}]}
```

`--after` は排他的ID、`--limit` は1〜500です。allowlistから外したmailboxの既存eventは
保持されますが閲覧できず、再許可すると再び取得できます。旧daemonは新しい
`reply`/`mailbox-list-v1` commandを未知commandとして失敗させ、別commandへ降格しません。

`agent-talk update` は Linux x86_64 / macOS Apple Silicon の公開GitHub
Releaseだけを対象に、タグ固定assetとSHA-256を検証して更新します。ローカル版が
latest以上の場合はdowngradeせず、デーモンの版確認だけを行います。tmux serverが
無い環境ではCLI更新を完了し、daemonは `not applicable` と表示します。

## Read-only observation page

daemonはCLI用RPC socketに加え、read-onlyなagent registry、pane screen、外部mailbox
履歴と埋め込み済みSPAを
HTTP-over-UDSで提供します。これはTCP listenerではなくUnix domain socketなので、
通常のブラウザから直接開くことはできません。TCP proxyやtsnet連携は現時点では
含まれません。

既定のHTTP socketは
`$XDG_RUNTIME_DIR/agent-talkd/<tmux-socket-name>.http.sock`です。
`XDG_RUNTIME_DIR`が未設定なら
`~/.cache/agent-talkd/run/agent-talkd/<tmux-socket-name>.http.sock`を使います。
`<tmux-socket-name>`はtmux socketのbasenameを取り、英数字・`-`・`_`以外を
`_`へ置換した値です。既定のCLI用`<tmux-socket-name>.sock`と同じstemを使います。
`AGENT_TALK_RPC_SOCKET=/path/custom.sock`でRPC socketを上書きした場合も、同じ親directoryの
`/path/custom.http.sock`へ追随します。

- `GET /v1/hello`: 製品名とversionをJSONで返します。
- `GET /v1/who`: 現在登録中のagent名、idle/busy状態、pane、session、location、cwdを
  JSONで返します。
- `GET /v1/agents/<pane>/screen`: 登録中のpaneの現在の表示範囲を、plain textの
  `screen` fieldを持つJSONで返します。`%1`のようなpane IDはURL上では
  `%251`のようにpercent encodeします。
- `GET /v1/mailboxes`: `@agent_talkd_allowed_sources`で現在許可されているmailbox名を返します。
- `GET /v1/mailbox/<mailbox>?after=<id>&limit=<n>`: mailbox eventをID順に非consume取得します。
  `after`は排他、`limit`は1〜500で既定100です。JSON event schemaは
  `mailbox-list-v1`と同一です。
- 未知の`/v1/`以下: JSONの404を返します。
- GET以外: `Allow: GET`付きの405を返します。
- その他のGET: 埋め込み静的ファイルを返し、未知の画面パスはSPA entryへfallbackします。
  静的ファイルを埋め込まずにビルドした場合はJSONの503を返します。

screen/mailbox path parameterのencoding不正、screen・mailboxesへのquery、mailboxの
未知・重複・範囲外queryは400です。decodeできてもpane IDやmailbox tokenの形式が不正、
またはpaneが未登録・mailboxが未許可なら404を返します。登録確認後にpaneの消滅を確認した
screen取得は410、tmux captureやbrokerの一時障害は503です。UIのScreenはdocumentが
表示中の間だけ2秒ごとに更新し、手動更新もできます。取得済みscreenがある状態で更新に
失敗した場合は、内容を薄く残して失敗状態を表示し、再取得を続けます。Lettersは自動poll
せず、更新操作で最後のevent IDより後を追加取得します。

UDS上のAPIは、たとえば次のように確認できます。

```sh
agent_talk_runtime="${XDG_RUNTIME_DIR:-$HOME/.cache/agent-talkd/run}"
agent_talk_tmux_name="$(tmux display-message -p '#{socket_path}' \
  | sed 's|.*/||; s/[^A-Za-z0-9_-]/_/g')"
curl --unix-socket \
  "$agent_talk_runtime/agent-talkd/$agent_talk_tmux_name.http.sock" \
  http://localhost/v1/who

curl --unix-socket \
  "$agent_talk_runtime/agent-talkd/$agent_talk_tmux_name.http.sock" \
  http://localhost/v1/agents/%251/screen

curl --unix-socket \
  "$agent_talk_runtime/agent-talkd/$agent_talk_tmux_name.http.sock" \
  'http://localhost/v1/mailbox/mobile?after=0&limit=10'
```

この例は`AGENT_TALK_RPC_SOCKET`を上書きしていない既定設定向けです。
HTTP socketへ接続できる同一UIDのcallerは、paneやmailboxごとの所有者確認なしに、すべての
登録paneの表示内容と現在許可されている全mailboxの履歴を読めます。peer UID検査は別のOS
userからの接続を拒む境界であり、同一UID内の人間を識別・認可するidentity gateでは
ありません。API応答はscreen内容をlogへ記録せず、状態変更、journal追記、terminal入力を
行いません。letter送信とbusy recoveryは人間の権限を使う変更・割り込み操作なので、
同一UIDだけを根拠に追加せず、Port-3のhuman identity gateと同時に導入します。

`send` は `#<id>` を返し、受信側は呼び鈴に表示された
`agent-talk read <id>` で依頼本文を取得します。`read` はcheckpointまでは
繰り返し実行できます。本文、未配達queue、状態遷移は既定で
`$XDG_STATE_HOME/agent-talkd/`（未設定時は `~/.local/state/agent-talkd/`）
のjournalに保存します。従来の `~/.cache/agent-talk/*.md` は新規作成せず、
既存ファイルも自動削除しません。

外部クライアントは `send` の宛先直後に送信元ラベルとスキルを指定できます。
オプションより後の本文が `--` で始まる場合は、その前に `--` を置きます。

```sh
printf '%s\n' '依頼本文' | agent-talk send claude --from mobile --skill deliver
agent-talk send codex --skill deliver -- '--literal body'
agent-talk send claude --no-reply '確認してください。返信は不要です。'
```

登録中のagent paneは `--from` と `--skill` を指定できません。2つ目の例は未登録の
human caller向けです。外部クライアントは許可された `--from` と組み合わせて
`--skill` を指定します。

`--skill` は宛先のagent種別に応じ、Claudeでは `/deliver `、Codexでは
`$deliver ` のような固定呼び出しを呼び鈴の先頭へ付けます。依頼本文は従来どおり
journalだけに保存され、tmuxへの入力には含まれません。`--from` と `--skill` は
daemonでも検証され、未許可値や記法未設定のagent宛は配達せずエラーにします。
`--from` は同一ユーザーで動くローカルクライアントが自己申告する表示ラベルです。
認証済みの送信元情報ではないため、認可や監査の判断には使用しないでください。
`--no-reply` は agent 間の一方向連絡用で、返信不要の brief と呼び鈴を生成します。
重大な実害を防ぐ異議に限り、skill側の判断で1通だけ返信できます。`--from` との併用は
拒否され、外部mailboxの `reply <id>` 契約は変わりません。未対応の旧daemonへは
`send-v2` の未知commandとして失敗し、通常送信へ降格しません。

設定は tmux の `@agent_talkd_log_level`（既定 `info`）と
`@agent_talkd_queue_limit`（pane ごとの通常メッセージ上限、既定 `1000`）
で指定します。追加の送信設定は次のglobal optionで指定します。

- `@agent_talkd_skill_syntax`: agent名と記法の対応。形式は
  `claude=slash,codex=dollar`。この2件は既定値で、設定値は追加・上書きされます。
- `@agent_talkd_allowed_skills`: 許可するスキル名のカンマ区切り。未設定時は
  文字種・長さ検証のみです。
- `@agent_talkd_allowed_sources`: 許可する外部送信元ラベルのカンマ区切り。
  既定は `mobile` です。`human`、`system`、登録中のagent名は指定できません。

## pane 終了検知

global `pane-exited[987]` および kill 系 hook を wake-up として使用し、daemon が
live pane 一覧と照合します。tmux サーバーの終了・再起動はserver PIDを使った2秒間隔の
health checkで検知し、一過性の実行失敗は1回だけ許容します。監視専用のtmux sessionは
作成しません。

## テスト

```sh
npm --prefix client ci
npm --prefix client run format:check
npm --prefix client run check
npm --prefix client test
npm --prefix client run build
cargo fmt -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
cargo test --locked --test tmux_integration -- --ignored
cargo build --locked --release
```

統合テストは隔離した実 tmux サーバーを作成するため、通常のテスト実行では
ignore されています。

## 対応環境

- Linux x86_64
- macOS Apple Silicon
- tmux 3.4以上（3.4 / 3.6bで検証）

Windowsはtmuxを前提とするためサポート対象外です。

## ライセンス

[MIT License](./LICENSE)

設計上の判断と配送の不変条件は[設計資料](./docs/design.md)にまとめています。
