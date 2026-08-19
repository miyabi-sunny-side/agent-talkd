# agent-talkd

herdr 上の Claude/Codex などの対話エージェント間で、相談・レビュー・通知を
安全に取り次ぐ Rust 製メッセージブローカーです。デーモンが状態を一元管理し、CLI は Unix
ドメインソケット経由で操作を提供します。

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
install -m755 target/release/agent-talk-mcp ~/.local/bin/agent-talk-mcp
```

このcrateは`agent-talk`（CLI・デーモン）と`agent-talk-mcp`（stdio MCP server）の
2つのバイナリを作ります。リリースアーカイブには両方が入っており、同じreleaseから
取り出せば世代が揃います（`agent-talk-mcp --version` で確認できます）。
`agent-talk update`（self-update）が置き換えるのは`agent-talk`自身だけなので、
adapterはアーカイブ側から配置してください。

フロントエンドのビルドにはNode.js 24とnpmを使います。`client/dist`を先に生成すると、
続くCargoビルドがその静的ファイルを単一の`agent-talk`バイナリへ埋め込みます。
`build.rs`からnpmは起動しません。`client/dist`がない状態でもCargoビルド自体は成功しますが、
そのバイナリの静的ページは503を返します。

## CLI

`run`, `register`, `unregister`, `who`, `resolve`,
`send`, `read`, `update`, `ensure-daemon`, `daemon-status` を提供します。
`read-message`, `ack-message`, `list-peers` は MCP adapter が使う配管コマンドで、本文・
受領報告の結果・登録agentと未受領IDをそれぞれJSONで返します。送信側の
`send-message` は MCP adapter 専用の daemon RPC です。CLI からは必要なオプションを
渡せず `send-message optionsがありません` で失敗するので、CLI から送るときは `send`
を使ってください。この4つは未登録の pane から呼ぶと拒否されます。
移行注記: 旧 wire 名 (`read-v1` / `ack-v1` / `peers-v1` / `send-message-v1` /
`mailbox-list-v1`) は稼働中の旧 adapter を session 途中で壊さないための互換 alias
として受理されます。次の minor release で削除予定です。新しい adapter は canonical
名を先に送り、daemon が明示的に `unknown command` を返したときだけ旧名で1回
再試行します (接続失敗や timeout では再試行せず、send の二重配送を防ぎます)。
互換用の `gc`, `watch` は no-op です。デーモンが未起動なら CLI が herdr
socket 単位で自動起動し、既存デーモンの版が古ければ安全に交代します。
インストール済みのversionは `agent-talk --version` で確認できます。
各サブコマンドの使い方は `agent-talk <command> --help` で確認できます。

`resolve` は宛先を pane へ解決して pane id を1行で表示します。`--json` を付けると、
宛先の識別情報と、宛先が Claude Code のときは組み込み cross-session channel の宛先に
なる socket も返します。

```json
{"version":1,"label":"knowledge/claude","pane":"w2E:pA","runtime":"claude","pid":1481334,"uds":"/run/user/1000/cc-socks/1481334.sock"}
```

`pid` は herdr がその pane に検出している runtime と**同名の foreground process**
（`pane.process_info`）の PID で、`uds` は `$XDG_RUNTIME_DIR/cc-socks/<その PID>.sock` が
実在するときだけ入ります。pane の主 agent だけが foreground process に載るので、その
agent が spawn した子 agent の socket が同じ directory に並んでいても選ばれません。
両方とも毎回導出し、保存しません。**cc-socks を持つのは Claude Code だけ**なので、
`runtime` が `claude` でない pane では、同じ PID の socket が cc-socks に残っていても
導出しません。同名の process が 0 個か 2 個以上のとき、socket が無いとき、herdr が
`pane.process_info` に答えられない・壊れた応答を返す・数秒以内に答えないときは、どれも `null` です
（推測はしません）。探す directory は `AGENT_TALK_CC_SOCKS` で差し替えられます。

`run` は子プロセスの実行中だけ現在のpaneを登録し、終了時に登録解除します。
たとえば `_agent_talk_run codex codex "$@"` は次のように置き換えられます。

```sh
agent-talk run codex codex "$@"
```

外部連携は `--from` で許可された mailbox に送信し、`mailbox-list` で
read-only に取得できます。返信は agent pane 内で `agent-talk reply <id> 本文`
を実行します。mailbox event は consume されず、各 mailbox の最新500件を保持します。
pane なしは外部callerの誤用防止規約であり、実際のRPC境界は同一UIDのUnix
socketです。

`mailbox-list` の安定JSON schema:

```json
{"version":1,"mailbox":"mobile","events":[{"id":12,"created_at":"2026-07-21T11:00:00Z","mailbox":"mobile","source_label":"mobile","direction":"out","body":"依頼","skill":"deliver","target_name":"claude","target_pane":"w1:p1","reply_to":null}]}
```

`--after` は排他的ID、`--limit` は1〜500です。allowlistから外したmailboxの既存eventは
保持されますが閲覧できず、再許可すると再び取得できます。旧daemonは新しい
`reply`/`mailbox-list` commandを未知commandとして失敗させ、別commandへ降格しません。

`agent-talk update` は Linux x86_64 / macOS Apple Silicon の公開GitHub
Releaseだけを対象に、タグ固定assetとSHA-256を検証して更新します。ローカル版が
latest以上の場合はdowngradeせず、デーモンの版確認だけを行います。herdr が
無い環境ではCLI更新を完了し、daemonは `not applicable` と表示します。

## MCP server

`agent-talk-mcp` は agent が触る窓口の stdio MCP server です。herdr ごとに
1つ動いているデーモンへ、既存の Unix domain socket で接続します。公開する tool は
次の4つだけで、file 読み書き・任意 path 指定・subprocess 実行の tool はありません。

| tool | 引数 | 返り値 |
| --- | --- | --- |
| `list_peers` | なし | `{"version":1,"self":"w1:p4","pending_to_me":[2],"peers":[...]}`。各peerは`name`/`runtime`/`state`/`location`/`pane`/`cwd`/`queued`/`pending_from_me` |
| `send_message` | `to`, `body`, `no_reply?` | `{"version":1,"id":0,"path":"sent","to":"w1:p2","name":"claude"}`（`path` は `sent` か `queued`） |
| `read_message` | `id` | `{"version":1,"id":0,"from":"codex","from_full":"knowledge/codex","reply_to":"w1:p5","body":"..."}` |
| `ack_message` | `id` | `{"version":1,"id":0,"outcome":"acked"}`（未知IDは `no_pending_message`） |

どの返り値も `version: 1` の JSON です。デーモンの応答がこの形でなければ、adapter は
成功として扱わず tool error（`isError: true`）にします。

各 peer の `runtime` は、herdr がその pane に**今**検出している agent 種別
（`"claude"` / `"codex"` / `"grok"` / `"cursor"` など。検出 manifest は増減するので
固定の列挙ではありません）で、未検出なら `null` です。タブ名を付けた pane では
`name` から runtime を読めないため、`name` とは別に返します。使い道は読み取り専用の
判別だけです — 相手が `"claude"` のときだけ Claude Code 組み込みの cross-session
channel を選び、それ以外・`null`・`runtime` field 自体が無い場合（`runtime` を返さない
旧 daemon）は agent-talk へフォールバックします。`runtime` の追加で `version` は
上がりません（既存 field の名前と値は不変で、読み飛ばす client はそのまま動きます）。

送信側と宛先の両方が live で `"claude"` で、かつ宛先 pane の agent PID から Claude Code
cross-session socket (`$XDG_RUNTIME_DIR/cc-socks/<PID>.sock`) が引けるときは、`send_message` は
**配送せずに** `宛先 ... は Claude Code なので、組み込みの cross-session channel ... で
送ってください。宛先は uds:<path> です` と拒否します。そのまま組み込みの
`SendMessage` へ `uds:<path>` を渡してください。組み込み channel が使えないときは CLI の
`agent-talk send` が逃げ道です。3条件のどれかが欠けるとき（socket が引けない、herdr が
`pane.process_info` に答えられない、どちらかが claude でない）は従来どおり配送します。
この拒否では message は永続化されず、呼び鈴も未受領一覧も変わりません。CLI の
`send` / `send-v2` / `reply` と外部 mailbox 経路は影響を受けません。

呼び出し元の pane が未登録なら、4つとも
`この操作は登録済みのagent paneからのみ実行できます` で拒否されます。拒否は呼び鈴も
未受領一覧も変えません。CLI の `send` は未登録 pane からも従来どおり送れます。人間が
CLI から送る経路を閉じないためです。

`read_message` の `from` は送信時点の送信者名（bare 名）で、送信者が退出・改名しても
変わりません。`from_full` は同じく送信受理時点で捕捉した canonical full label
（`<workspace>/<name>`）で、同名の agent が別 workspace に居ても区別できます。
呼び鈴の差出人表記もこの full label です。workspace を捕捉していない送信者
（`human` / `system` / `--from` の外部送信元、および `from_full` 導入前の journal 由来の
message）では、`from_full` は `from` と同じ bare 名になります。読み出し時に現在の
peer 一覧や cwd から workspace を推測することはありません。

登録済みの pane からの送信で、その pane の登録内容と herdr の現在の状態が食い違って
いる間は、`送信元 pane ... と herdr の現在の状態が食い違うため送信できません` で
拒否されます（タブ名の変更や runtime の交代の直後に起こりえます）。拒否は呼び鈴も
未受領一覧も変えないので、同期が追いついてからそのまま再送してください。

`reply_to` は同じ名前の agent がその pane に今も登録されている場合だけ pane ID を返し、
それ以外は `null` です。`null` のときは `list_peers` で現在の宛先を選び直します。

呼び鈴を受けた側の手順は2段階です。

1. `read_message` で読む。読んだ時点で受領になり、本文は何度でも読める。
2. 作業する。返信が必要なら `send_message` で普通に送り返す。返信専用の tool は
   ありません。`ack_message` は互換の空操作です。

agent 間の message は peer からの**連絡**です。受け取った側がすでに持つ権限を増やしも
減らしもせず、着手を止める理由にもなりません。呼び鈴と brief も「連絡が届きました」と
提示します。人間の CLI caller と外部 mailbox（`--from`）からの送信だけは、入口に居るのが
user 本人なので従来どおり「依頼が届きました」で届きます。

接続先の socket は `XDG_RUNTIME_DIR`（無ければ `HOME`）から導出します。
`HERDR_SOCKET_PATH` があればデーモンと同じ規則でその herdr 用の path を、
無ければ既定 session の固定 path (`agent-talkd/herdr.sock`) を使います。
tool 引数や `AGENT_TALK_RPC_SOCKET` からは受け取らず、subprocess も
起動しません。

**環境変数の forward は不要です。** `HERDR_PANE_ID` があれば呼び出し元
pane として自己申告し、無ければ daemon が接続の peer PID から /proc の
祖先を遡り、herdr が agent 本体へ与えた `HERDR_PANE_ID` /
`HERDR_SOCKET_PATH` を読んで呼び出し元 pane を確立します (Linux)。
launcher が env を clear する agent (grok 等) も設定なしで使えます。
祖先に identity が見つからない・別の herdr session に属する場合は
fail closed で、`HERDR_PANE_ID` の明示 forward を案内します。named
session の herdr を使う場合だけ `HERDR_SOCKET_PATH` の forward が必要です。

未受領のIDは CLI の `who` でも両方向を確認できます。

```console
$ agent-talk who
claude     busy/working herdr main:1.4 (w1:p4)  /home/miyabi/projects/sunny-side/agent-talkd
codex      idle/idle   herdr main:1.5 (w1:p5)  /home/miyabi/projects/sunny-side/agent-talkd
pending-to-me: #0
```

状態列は `herdr の状態を idle/busy に正規化した値/herdr の生の観測`、backend列は
tmux併存期の表形式を維持した固定値`herdr`です。agent-talkd独自のBusy/Idle状態は
持ちません。

`pending-to-me` は自分宛で未受領のID（配達待ちqueueの分も含む）、
`pending-from-me <pane>` は自分が送って未受領のIDです。後者にも配達待ちqueueの分を
含みます。どちらの行も未受領が無ければ出ません。

## Observation & letters page

daemonはCLI用RPC socketに加え、agent registry、pane screen、外部mailbox履歴の
閲覧と、手紙の投函 (`POST /api/letters`、唯一の書き込みroute) を持つ埋め込み済み
SPAをHTTP-over-UDSで提供します。既定はUnix domain socketのみで、
`AGENT_TALK_HTTP_ADDR` を明示設定したときだけ同じrouteがTCPでも開きます。
TCP面の到達範囲はoperatorが所有するVPN/LAN境界で定め、processは認証を
追加しません (2026-08-05のuser指示による裁定)。

既定のHTTP socketは
`$XDG_RUNTIME_DIR/agent-talkd/<name>.http.sock`です。
`XDG_RUNTIME_DIR`が未設定なら
`~/.cache/agent-talkd/run/agent-talkd/<name>.http.sock`を使います。
`<name>`は既定のherdr socketなら`herdr`、named session
(`~/.config/herdr/sessions/<session>/herdr.sock`) なら`herdr-<session>`です。
既定のCLI用`<name>.sock`と同じstemを使います。
`AGENT_TALK_RPC_SOCKET=/path/custom.sock`でRPC socketを上書きした場合も、同じ親directoryの
`/path/custom.http.sock`へ追随します。

- `GET /api/hello`: 製品名とversionをJSONで返します。
- `GET /api/who`: 現在登録中のagent名、idle/busy状態、pane、session、location、cwdを
  JSONで返します。
- `GET /api/agents/<pane>/screen`: 登録中のpaneの現在の表示範囲を、plain textの
  `screen` fieldを持つJSONで返します。`w1:p1`のようなpane IDはURL上では
  `w1%3Ap1`のようにpercent encodeします。
- `GET /api/mailboxes`: `AGENT_TALK_ALLOWED_SOURCES`で現在許可されているmailbox名を返します。
- `GET /api/mailbox/<mailbox>?after=<id>&limit=<n>`: mailbox eventをID順に非consume取得します。
  `after`は排他、`limit`は1〜500で既定100です。JSON event schemaは
  `mailbox-list`と同一です。
- `GET /api/agents/<pane>/skills`: 対象 pane の agent に選べる skill 名一覧
  (HOME 上の installed skill ∩ `AGENT_TALK_ALLOWED_SKILLS`。skill 記法が無い
  runtime は空配列)。
- `POST /api/letters`: 唯一の書き込みroute。`{"source","target","body","skill"?}` のJSONを
  受け、CLIの `send --from` と同一の外部mailbox送信経路 (allowlist・resolve・
  journal-first・配達/requeue) へ流します。sourceは `AGENT_TALK_ALLOWED_SOURCES`
  (既定 `mobile`) が最終判定し、未許可は403。Content-Typeは `application/json`
  のみ (それ以外415)、本文上限1MiB (超過413)、CORS headerは返しません
  (他siteのbrowserからは投函できません)。成功は `{"version":1,"id","path","to","name"}`。
- 未知の`/api/`以下: JSONの404を返します。撤去済みの旧 `/v1/*` も同じJSON 404で
  明示的に拒否します (SPA entryへはfallbackさせません。200を返すと残存する旧
  updaterのhealth probeが旧APIを正常と誤認するためです)。
- GET以外: `POST /api/letters` を除き `Allow` 付きの405を返します。
- `GET /manifest.webmanifest`: web app manifest (`application/manifest+json`)。
  スマホのブラウザからホーム画面へ追加すると、`/app-icon-*.png` のアイコンで
  standalone表示のアプリとして起動します (service workerは持たないため、
  オフライン動作はしません)。
- その他のGET: 埋め込み静的ファイルを返し、未知の画面パスはSPA entryへfallbackします。
  静的ファイルを埋め込まずにビルドした場合はJSONの503を返します。

screen/mailbox path parameterのencoding不正、screen・mailboxesへのquery、mailboxの
未知・重複・範囲外queryは400です。decodeできてもpane IDやmailbox tokenの形式が不正、
またはpaneが未登録・mailboxが未許可なら404を返します。登録確認後にpaneの消滅を確認した
screen取得は410、herdr の read や broker の一時障害は503です。UIのScreenはdocumentが
表示中の間だけ2秒ごとに更新し、手動更新もできます。取得済みscreenがある状態で更新に
失敗した場合は、内容を薄く残して失敗状態を表示し、再取得を続けます。Lettersは自動poll
せず、更新操作で最後のevent IDより後を追加取得します。

UDS上のAPIは、たとえば次のように確認できます。

```sh
agent_talk_runtime="${XDG_RUNTIME_DIR:-$HOME/.cache/agent-talkd/run}"
curl --unix-socket \
  "$agent_talk_runtime/agent-talkd/herdr.http.sock" \
  http://localhost/api/who

curl --unix-socket \
  "$agent_talk_runtime/agent-talkd/herdr.http.sock" \
  http://localhost/api/agents/w1%3Ap1/screen

curl --unix-socket \
  "$agent_talk_runtime/agent-talkd/herdr.http.sock" \
  'http://localhost/api/mailbox/mobile?after=0&limit=10'
```

この例は`AGENT_TALK_RPC_SOCKET`を上書きしていない既定設定向けです。
HTTP socketへ接続できる同一UIDのcallerは、paneやmailboxごとの所有者確認なしに、すべての
登録paneの表示内容と現在許可されている全mailboxの履歴を読めます。peer UID検査は別のOS
userからの接続を拒む境界であり、同一UID内の人間を識別・認可するidentity gateでは
ありません。GET応答はscreen内容をlogへ記録せず、状態変更、journal追記、terminal入力を
行いません。状態を変えるのは `POST /api/letters` だけで、これは既存の外部mailbox
送信経路 (allowlist・journal-first) をそのまま通ります。

`send` は `#<id>` を返し、受信側は呼び鈴に表示された
`read_message <id>`（MCP。CLI では `agent-talk read <id>`）で本文を
取得します。受領は `read` / `read_message` が本文を返した時点です。本文は残り、
同じ ID を何度でも読み直せます。`ack-message` / `ack_message` は互換の空操作です。
配達済みのまま未読のメッセージには、daemonが受領催促の呼び鈴を送ります。
催促が出るのはherdrの観測がターンとターンの間（idle / done）のときだけです
（読むことを促し、5分間隔より詰めて連打しません。`working` への初回配達とは別です）。
宛先本人は呼び鈴を待たず、
`list_peers` の `pending_to_me`（queue 中を含む）から ID を得て `read_message` で
本文を pull できます。queue 中は**先頭から順に**だけ pull できます（FIFO）。
pull は journal の配達完了記録を書いて queue から外すので、
後から同じ ID の呼び鈴は鳴りません。他 pane 宛の本文は引き続き読めません。
本文、未配達queue、受領状態は既定で
`$XDG_STATE_HOME/agent-talkd/`（未設定時は `~/.local/state/agent-talkd/`）
のjournalに保存します。従来の `~/.cache/agent-talk/*.md` は新規作成せず、
既存ファイルも自動削除しません。

外部クライアントは `send` の宛先直後に送信元ラベルとスキルを指定できます。
オプションより後の本文が `--` で始まる場合は、その前に `--` を置きます。

```sh
printf '%s\n' '手紙の本文' | agent-talk send claude --from mobile --skill deliver
agent-talk send codex --skill deliver -- '--literal body'
agent-talk send claude --no-reply '確認してください。返信は不要です。'
```

登録中のagent paneは `--from` と `--skill` を指定できません。2つ目の例は未登録の
human caller向けです。外部クライアントは許可された `--from` と組み合わせて
`--skill` を指定します。

`--skill` は宛先の runtime (herdr の検出名) に応じ、Claudeでは `/deliver `、Codexでは
`$deliver ` のような固定呼び出しを呼び鈴の先頭へ付けます。本文は従来どおり
journalだけに保存され、端末への入力には含まれません。`--from` と `--skill` は
daemonでも検証され、未許可値や記法未設定のagent宛は配達せずエラーにします。
`--from` は同一ユーザーで動くローカルクライアントが自己申告する表示ラベルです。
認証済みの送信元情報ではないため、認可や監査の判断には使用しないでください。
`--no-reply` は agent 間の一方向連絡用で、返信不要の brief と呼び鈴を生成します。
重大な実害を防ぐ異議に限り、skill側の判断で1通だけ返信できます。`--from` との併用は
拒否され、外部mailboxの `reply <id>` 契約は変わりません。未対応の旧daemonへは
`send-v2` の未知commandとして失敗し、通常送信へ降格しません。

設定は daemon 起動時の環境変数 `AGENT_TALK_LOG_LEVEL`（既定 `info`）と
`AGENT_TALK_QUEUE_LIMIT`（pane ごとの通常メッセージ上限、既定 `1000`）
で指定します。追加の送信設定は次の環境変数で指定します。

- `AGENT_TALK_SKILL_SYNTAX`: runtime 検出名と記法の対応。形式は
  `claude=slash,codex=dollar`。この2件は既定値で、設定値は追加・上書きされます。
- `AGENT_TALK_ALLOWED_SKILLS`: 許可するスキル名のカンマ区切り。未設定時は
  文字種・長さ検証のみです。
- `AGENT_TALK_ALLOWED_SOURCES`: 許可する外部送信元ラベルのカンマ区切り。
  既定は `mobile` です。`human`、`system`、登録中のagent名は指定できません。

## pane 終了検知

daemon が 2 秒間隔の health tick で herdr の snapshot を読み、登録と照合します
(詳細は下の「herdr の登録は pull」)。herdr 自体の終了はこの health check が
検知し、一過性の失敗は1回だけ許容します。

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
cargo test --locked --test bridge -- --ignored
cargo build --locked --release
```

`tests/bridge.rs` の E2E は background daemon を実際に spawn するため、
通常のテスト実行では ignore されています。

## herdr backend

daemon は herdr socket から導出した RPC socket を 1 つ開きます。pane の中に
居るクライアントは自分の環境変数から同じ socket を導出して接続します。

| 環境変数 | 役割 |
|---|---|
| `AGENT_TALK_HERDR_SOCKET` | herdr socket を明示指定する。空文字なら無効 |
| `AGENT_TALK_HTTP_ADDR` | 設定すると HTTP 面を TCP でも待ち受ける（例 `127.0.0.1:8787`）。既定は無効 |
| `AGENT_TALK_CC_SOCKS` | Claude Code の cross-session socket を探す directory。既定は `$XDG_RUNTIME_DIR/cc-socks` |

herdr の pane の中に居る場合（`HERDR_PANE_ID` / `HERDR_SOCKET_PATH` がある）は
自動的に herdr socket を発見します。**socket file が存在するだけでは
有効になりません** — 停止済み・無関係な herdr を掴まないためです。

pane id は herdr が発行する **opaque な文字列**で、agent-talkd は文法を
検証しません (採番規則の推測が実採番より狭くて全停止した事故の再発防止)。
宛先文字列が registry の pane id に完全一致すれば pane 直指定、しなければ
名前/scope として解決します。

herdr の workspace には人間向けの **label** があり (`workspace.list`)、
表示 (`who` の location、`:5002` UI) と宛先解決の両方に使います。
label の無い workspace と旧来の `w2/codex` 形は workspace_id で引き続き
解決できます。`/`・`:`・空白を含む label は宛先構文と衝突するため採用せず、
workspace_id 表示へ fallback します (rename は次回取得で自動追従)。

agent の **name** (宛先と表示) は herdr のタブ label から導出します。宛先に
使える非数字の label があればそれ、無ければ runtime 検出名 (claude / codex
など) です — custom 名の無いタブは herdr が番号文字列を label に入れるため、
純数字 label も runtime 検出名へ fallback します。runtime は name とは別に
保持し、`--skill` の記法と installed skill 一覧の解決に使います — タブ名
`fable` の claude にも `/deliver` が届きます。

素の `codex` のような bare 名は自分と同じ workspace 内 (自分自身を除く) で
探し、workspace を暗黙にまたぎません。同名候補が2つ以上あるときは同一タブの
近接でも自動選択せず、候補の pane id を案内する曖昧エラーになります
(pane id 直指定なら届きます)。別 workspace へは `<scope>/<name>` を明示
します (tmux 併存期の正式名称 `herdr/<scope>/<name>` も互換 alias として
受理します)。

herdr への配送は、herdr が **idle / done / working と判定した pane に**
`agent.prompt` で agent 本人へ submit まで行います（agent が居ない
pane には herdr が拒否を返すため、素の shell へ呼び鈴が入ることはありません）。
`blocked` / `unknown` には一文字も送りません。`working` を配達可能にするのは、
長寿命の裏プロセスで herdr が idle/done に戻らない相手へ呼び鈴が滞留しない
ためです。done を配達可能にするのは、非表示 tab の完了バッジが user の巡回まで
配達を塞がないためです（done への配達は未閲覧バッジを消して新ターンを始めます）。
表示上の idle/busy は配達可否とは別で、`working` は引き続き `busy` と出ます。
配送が拒否されたメッセージは queue に残り、**宛先が配達可能である正の証拠が
次に得られた時点（2秒間隔の health tick）で同じ ID のまま自動再試行**
されます。`queued` は「捨てられた」ではなく「配達可能を待って自動配送される」
の意味です。

### herdr の読み出し RPC には 5 秒の期限がある

herdr への **読み出し専用 RPC**（`ping` / `pane.list` / `workspace.list` /
`tab.list` / `pane.get` / `pane.read` / `pane.process_info`）は **5 秒**で
諦めます。この値は正常応答の上限ではなく**異常検知の閾値**です — ローカルの
Unix domain socket が読み出しに 5 秒かかる時点で herdr 側が固まっており、
一方 2 秒だと 4 MiB 近い `pane.read` や高負荷時の正当に遅い応答を切りかねません。
期限を持たないと、herdr が黙り込んだときに daemon の単一 event loop がそこで
止まり、**静的ページは 200 のまま API だけが永久に返らなくなります**。

超過は method 名を含むエラーになり、agent 一覧を返す `GET /api/who` は
503 (`registry_unavailable`) を返します。retry も、前回の一覧への fallback も
しません — どちらも「生きている agent 一覧」を偽るためです。broker を経由しない
静的ページはこの間も 200 のままで、herdr が戻れば次の要求から一覧も戻ります。

**呼び鈴を送る `agent.prompt` には期限を付けません。** これは pane へ入力を
注入する mutating な RPC で、応答を失うと「herdr 側で実行済みかどうか」が
分かりません。期限でエラーにすると daemon が「送れなかった」と判断して
再配送し、呼び鈴の**二重配送**になりえます。

daemon の health tick（2 秒間隔）は、**未処理のものが同時に 1 件になるように
集約** します。event loop は 1 本で、herdr が黙った tick は期限ぶん止まるため、
tick を送り続けると check が処理速度より速く積もり、broker を経由する API が
その後ろで滞留してしまいます。捨てた tick は次の tick で取り戻せます（health tick は
状態を持ち越さない巡回です）。この集約のおかげで、herdr が黙り続けている間も
broker を経由する API は有限時間で応答します。同じ tick の中で一覧取得が既に
期限切れになっているときは、同じ一覧をもう一度引く受領催促も次の tick へ回します。

一覧取得の失敗と復旧は daemon の log（`$XDG_STATE_HOME/agent-talkd/agent-talkd.log`）
に記録します。記録するのは**落ち始めの WARN 1 行と復旧の INFO 1 行だけ**です —
snapshot は最短 2 秒間隔で回るため、失敗している間ずっと同じ行を吐くと
log が洪水になります。`/api/who` 経由の取得と定期 snapshot はこの状態を
共有するので、同じ障害が二重に記録されることもありません。

### herdr の登録は pull（hook 不要）

**daemon はsend・message RPCの受信時に即座に、待機中は2秒間隔のhealth tickで
herdrのsnapshotを読み、agentの載っているpaneを同期**します。grok CLIのように
登録 hook を持たない agent も、herdr の pane で起動するだけで数秒以内に
peer になります。

- pane の identity (タブ名由来の name / runtime 検出名) のどちらかが変わったら、
  旧登録を即座に外して新しい identity で引き継ぎます（herdr の観測は native な
  同一性情報のため、猶予は不要です）。
- 成功したsnapshotから消えたpaneは即座に登録を外し、未受領メッセージを
  送信元へ回収します。pen-cliなどによる高速なspace削除・再作成でも古い登録を
  次のsnapshotまで持ち越しません。
- snapshot の取得自体に失敗している間は判定を一切進めません
  （不完全な証拠で登録を消さないため）。
- herdr 側 pane への `unregister` は拒否します — pull が次の tick で登録し直す
  ため、手動解除は振動を生むだけで意味を持ちません。

hook を持たない agent の MCP tool 利用にも env forward は不要です —
identity は daemon が接続の peer PID から確立します (上の「MCP server」参照)。
`XDG_RUNTIME_DIR` が非標準な環境と named session だけが明示 forward を
必要とします。

### 既知の TODO

- herdr の `AttentionRequired`（固着 pane の検知と通知）は未実装

## 対応環境

- Linux x86_64
- macOS Apple Silicon
- herdr 0.7.5（protocol 17）で検証

Windows は herdr を前提にできないためサポート対象外です。

## ライセンス

[MIT License](./LICENSE)

設計上の判断と配送の不変条件は[設計資料](./docs/design.md)にまとめています。
