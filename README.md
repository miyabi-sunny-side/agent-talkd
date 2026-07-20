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
`send` を提供します。互換用の `gc`, `watch` は no-op です。デーモンが未起動
なら CLI が tmux サーバー単位で自動起動します。

状態遷移ログと queue journal は既定で
`$XDG_STATE_HOME/agent-talkd/`（未設定時は `~/.local/state/agent-talkd/`）
に保存します。

設定は tmux の `@agent_talkd_log_level`（既定 `info`）と
`@agent_talkd_queue_limit`（pane ごとの通常メッセージ上限、既定 `1000`）
で指定します。

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
