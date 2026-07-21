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
cargo build --release --locked
mkdir -p ~/.local/bin
install -m755 target/release/agent-talk ~/.local/bin/agent-talk
```

## tmuxプラグイン

[TPM](https://github.com/tmux-plugins/tpm)を使う場合、`.tmux.conf` に次を追加します。

```tmux
set -g @plugin 'miyabi-sunny-side/agent-talkd'
```

`prefix + I` でプラグインを取得します。プラグインはバイナリを自動ビルド・
ダウンロードしないため、先に `~/.local/bin/agent-talk` を配置してください。

## CLI

`register`, `unregister`, `busy`, `idle`, `turn-end`, `who`, `resolve`,
`send`, `read`, `update`, `ensure-daemon`, `daemon-status` を提供します。
互換用の `gc`, `watch` は no-op です。デーモンが未起動なら CLI が tmux
サーバー単位で自動起動し、既存デーモンの版が古ければ安全に交代します。
インストール済みのversionは `agent-talk --version` で確認できます。

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
```

`--skill` は宛先のagent種別に応じ、Claudeでは `/deliver `、Codexでは
`$deliver ` のような固定呼び出しを呼び鈴の先頭へ付けます。依頼本文は従来どおり
journalだけに保存され、tmuxへの入力には含まれません。`--from` と `--skill` は
daemonでも検証され、未許可値や記法未設定のagent宛は配達せずエラーにします。
`--from` は同一ユーザーで動くローカルクライアントが自己申告する表示ラベルです。
認証済みの送信元情報ではないため、認可や監査の判断には使用しないでください。

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

tmux control mode は専用 `_agent_talkd` セッションへ接続し、tmux サーバーの
終了を検知します。tmux 3.6b では別セッションの pane 終了通知が control
client に届かないため、global `pane-exited[987]` および kill 系 hook を
wake-up として併用し、daemon が live pane 一覧と照合します。

## テスト

```sh
cargo test
cargo test --test tmux_integration -- --ignored
cargo clippy --all-targets -- -D warnings
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
