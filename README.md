# agent-talk

Herdr内のCodex / Claude Codeへブラウザから指示を送り、同じセッションの返答を読むアプリです。
スマホから送った原文もCLIの会話履歴に残るため、後で端末から作業を続けられます。

## 必要な環境

- agent-talkを動かすLinux x86_64またはmacOS arm64のマシン
- 同じマシンで動く[Herdr](https://herdr.dev/docs/install/)と、CodexまたはClaude Code
- CLIの会話履歴を読めるユーザー権限と、`herdr`を実行できる`PATH`
- 閲覧用ブラウザ。遠隔利用にはTailscaleや認証付きプロキシなどの接続環境

**アプリにログイン機能はなく、HTTPは全IPv4インターフェースで待ち受けます。**
起動前に、ファイアウォールやプロキシで利用者以外からの接続を制限してください。
Tailscaleやプロキシのアクセス設定は運用側で管理します。同じ権限のプロセスもAPIを利用できます。

## Herdrを準備する

1. [Herdrの導入手順](https://herdr.dev/docs/install/)に従ってインストールします。
2. 使うCLIのintegrationをインストールします。両方を使う場合の例です。

   ```sh
   herdr integration install codex
   herdr integration install claude
   herdr integration status
   ```

3. 作業ディレクトリで`herdr`を起動し、そのpane内で`codex`または`claude`を起動します。
   すでにHerdr内にいる場合は、新たにHerdrを起動する必要はありません。
4. 端末で最初の会話を開始します。CLIによっては、その後にnative sessionが登録されます。

詳しい初回操作は[Herdrの案内](https://herdr.dev/agent-guide.md)を参照してください。
agent-talkは、Herdrと同じユーザー・接続環境で動かします。

## agent-talkを起動する

[Releases](https://github.com/miyabi-sunny-side/agent-talkd/releases/latest)から、
環境に合うarchiveと同名の`.sha256`ファイルを同じディレクトリへダウンロードします。

| 環境 | archive |
|---|---|
| Linux x86_64 | `agent-talk-linux-x86_64.tar.gz` |
| macOS arm64 | `agent-talk-macos-aarch64.tar.gz` |

Linuxでの確認・展開・起動例です。

```sh
sha256sum -c agent-talk-linux-x86_64.tar.gz.sha256
tar -xzf agent-talk-linux-x86_64.tar.gz
./agent-talk --version
PORT=5002 ./agent-talk daemon
```

macOSでは`shasum -a 256 -c`で確認し、archive名をmacOS用へ置き換えます。
archiveには`agent-talk`と`LICENSE`が入り、ブラウザ画面はバイナリに含まれています。
同じPCでは<http://localhost:5002>、遠隔では用意したTailscaleやプロキシのURLを開きます。
終了する場合は、daemonを起動した端末でCtrl+Cを押してください。

## 最初のメッセージを送る

1. 一覧の作業先・CLI・状態を確認し、送信するセッションを開きます。
2. 「手紙」を開き、本文を入力して「送信」を押します。
3. 「会話」で返答を確認します。送信受付の表示は、CLIの作業完了を意味しません。

「会話」はCLIの保存済み履歴、「画面」はHerdrの現在の表示テキストです。
タブを切り替えても宛先と下書きは保たれます。「画面」からキー操作はできません。
「画像を追加」でスクリーンショットや写真（PNG・JPEG・WebP、20 MiBまで）を選ぶと、保存先のパスが本文に入ります。
説明を書き足して「送信」すると、CLIがそのパスから画像を読めます。画像は24時間後に自動で削除されます。
長い履歴は「古い会話」「新しい会話」で辿り、「最新へ戻る」で直近へ戻れます。

「セッション未登録」の場合は、integrationの状態と端末での最初の会話を確認してください。
承認待ち・終了・状態不明の宛先へは送信できません。Grokなど対応外のCLIは「画面」の閲覧のみです。
送信中に接続が切れた場合は、会話を確認してから再送してください。自動再送はありません。
履歴の取得条件や表示上限は[設定と移行](docs/operations.md#対応とセッションの接続)にあります。

## 設定・更新

待受ポートは`PORT`（既定`5002`）、ログのレベルは`LOG_LEVEL`（既定`info`）で指定します。
Herdrの接続設定、履歴の保存先、旧brokerからの移行は[設定と移行](docs/operations.md)を参照してください。
独自クライアント向けの[HTTP API](docs/api.md)もあります。

`./agent-talk update`は配布物のchecksumを確認して実行ファイルを更新します。
更新後はdaemonを再起動してください。サービスとして常駐させた場合は、そのサービスを再起動します。

## 開発

Rust 1.96 / Node 24 / npmを使います。ソースを取得してフロントから順にビルドします。

```sh
git clone https://github.com/miyabi-sunny-side/agent-talkd.git
cd agent-talkd
npm --prefix client ci
npm --prefix client run build
cargo build --locked --release
```

生成物は`target/release/agent-talk`です。フロントをビルドしない場合、UIは503を返します。
検証コマンドは[AGENTS.md](AGENTS.md)、UIの設計は[DESIGN.md](DESIGN.md)、
内部の構成は[docs/design.md](docs/design.md)を参照してください。
