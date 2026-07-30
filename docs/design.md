# 設計

agent-talkd は、tmux 上の対話エージェントへ作業中の入力を割り込ませずに
メッセージを渡すための小さなブローカーです。

## プロセス構成

- `agent-talk daemon` は tmux サーバーごとに1プロセス起動し、登録・busy状態・
  queueを単一イベントループで所有します。
- `agent-talk` の各CLIコマンドは、tmux socket名ごとのUnix domain socketを
  通じてdaemonへ1要求を送ります。daemonがなければ競合を避けて自動起動します。
- daemonは同じイベントループへ接続する第二のUnix domain socketで、read-onlyな
  HTTP adapterも提供します。HTTP接続の処理は個別taskで行いますが、`GET /v1/who`は
  eventを主ループへ送り、daemon memoryとlive tmux paneをそこで照合します。registryを
  別の状態storeへ複製しないため、CLI配送との状態競合を増やしません。
- `ensure-daemon` は同じtmux socketだけを対象に、実行中daemonの版を確認します。
  同版なら何もせず、旧版は graceful shutdown を先に試し、旧RPCしかない場合だけ
  Unix socket peer の UID/PID に限定して停止します。複数tmux serverを探索したり、
  名前による全体 kill を行ったりしません。
- `internal-daemon-status` と `internal-daemon-shutdown` は lifecycle 管理用のRPCです。
  同じUIDのsocket利用者だけが呼べる既存のinternal RPCと同じ境界で、通常の利用者向け
  コマンドではありません。
- `update` は GitHub の公開 latest release をタグ固定で取得し、公開SHA-256の検証と
  安全なtar展開を完了するまで現行実体を変更しません。同一ディレクトリでfsync済みの
  stagingをatomic renameし、置換後の新binary自身で `ensure-daemon` を実行します。
  GitHubへのHTTPSと同一チャネルのchecksumが供給チェーンの境界であり、checksumは
  破損・部分ダウンロード検出を担います。
- pane消滅はglobal hookをwake-upとして、tmuxの状態確定を短時間待ってからlive pane
  一覧と照合します。tmuxサーバーの終了・再起動はserver PIDを使った2秒間隔のhealth
  checkで検知し、一過性の実行失敗は1回だけ許容します。監視用のtmux sessionやcontrol
  mode clientは作成しません。hookにはdaemonのRPC socket絶対パスを渡し、tmux serverと
  CLIの環境変数が異なっても同じdaemonへ接続します。

## Status pageとHTTP-over-UDS

HTTP adapterは同一ホスト・同一tmux serverという境界を維持するため、TCP listenerを
作りません。既定のsocketはruntime directoryの
`agent-talkd/<tmux-socket-name>.http.sock`で、CLI用
`agent-talkd/<tmux-socket-name>.sock`とstemを共有します。tmux socketのbasenameは
英数字・`-`・`_`以外を`_`へ正規化します。`XDG_RUNTIME_DIR`がなければ
`~/.cache/agent-talkd/run`をruntime directoryとします。RPC socketを環境変数で
上書きした場合も、その実効RPC pathの親directoryとfile stemから
`<stem>.http.sock`を導出します。二つのsocketを別の信頼境界へ分離しません。

daemon起動時はsocket親directoryを作成して0700へ固定し、HTTP接続accept後にも
peer credentialのUIDがdaemonのeffective UIDと一致することを要求します。前者は
別UIDがsocketへ到達する経路をdirectoryで制限し、後者は既存directoryのmodeや
環境差に依存しない接続単位の検査です。どちらかを外すと、registryのpane情報やcwdを
同一UID外へ露出させる可能性があります。これは同じUID内のクライアントを相互認証する
仕組みではありません。

routeは次の順で分類します。

1. GET以外は、pathにかかわらず`Allow: GET`付きのJSON 405にする。
2. `GET /v1/hello`は製品名とversion、`GET /v1/who`はregistry snapshotをJSONで返す。
3. 未知の`/v1/`以下は静的fallbackへ流さずJSON 404にする。
4. その他のGETは埋め込みassetを返し、該当assetがなければ`/index.html`へfallbackする。

HTTP APIは状態変更routeを持ちません。screen capture、letter送受信、busy recoveryも
この段階のadapterには含めません。ブラウザはUDSを直接開けないため、status pageを
ブラウザで利用するtransport bridgeも別scopeです。

起動時はstaleなHTTP socketを除去してからbindします。daemonのイベントループ終了時は
RPC socketとHTTP socketの両方を除去します。bind途中で失敗した場合にHTTPだけを提供する、
またはRPCだけで継続する縮退modeは設けず、daemon全体を起動失敗にします。

## Frontendの埋め込みと更新

status clientはNode.js 24、strict TypeScript、Svelte 5、Viteで構築します。npm依存関係は
`client/package-lock.json`で固定し、CIとreleaseは`npm ci`、format/type/test、frontend
buildをRust buildより先に実行します。Cargoの`build.rs`はnpmを起動せず、既に存在する
`client/dist`を列挙して`include_bytes!`用の表を生成します。これによりRustだけのbuildは
frontend toolchainなしでも成功し、assetがないバイナリはAPIを維持したまま静的routeだけ
`static_assets_unavailable`の503を返します。assetが必要な配布物は、順序を逆にすると
動作するstatus pageを含まないため、frontend-firstのbuild順がrelease不変条件です。

配布時は静的assetも既存の単一`agent-talk`バイナリに含まれます。したがってupdaterの
タグ固定asset、checksum検証、atomic replacementという既存境界がUIにもそのまま及び、
UI用の別install先・別version・別更新チャネルを作りません。

## 状態と配送

daemonのメモリを稼働中の唯一の真実とし、`@agent` と `@agent_state` は
既存hookとの互換性を保つ表示用ミラーです。daemon起動時は`@agent`だけを
登録復旧のヒントとして読み、stateは必ずidleに倒します。stale busyには
自己修復の機会がなく配達が固着する一方、実際にbusyなら直後のbusy hookが
復元するためです。`@agent_state`を状態の真実として読み戻しません。

配送入口は1つです。

1. 依頼ヘッダと本文をID付きでjournalへ永続化します。
2. 宛先がidleなら、`@agent_state`をbusyにして`agent-talk read <id>`を
   案内する呼び鈴を入力し、0.3秒後にEnterを送ります。スキル指定時も端末へ
   入るのは、daemonが検証・生成したスキルトークンと固定の呼び鈴だけです。
3. 宛先がbusyなら、配送待ちqueueへ入れてから`queued (busy)`を返します。
4. `turn-end`は宛先をidleにし、queue先頭を1件だけ配送してbusyへ戻します。
5. `read`は本文を返してConsumedを追記しますが、その場では本文を破壊せず、
   checkpointまでは再取得できます。
6. 未readのまま宛先が消滅した場合、元本文を含む配達失敗通知を送信元用の
   新しいメッセージとして作成します。

## 永続化の不変条件

本文とqueueを保持するjournalはJSON Linesのappend-only形式で、tmux
socket名ごとに分離します。

- `sent`または`queued (busy)`を返す前に本文のappendと`fsync`を完了する。
- journal書き込みに失敗したメッセージを配達済み・queuedとして報告しない。
- daemon再起動時に未read本文と未配達queueを復元する。
- Consumed済みかつ配送待ちqueueにない本文はcheckpointで圧縮消滅させる。
  queue内で先にreadされた本文は、後続の`turn-end`配送まで保持する。
- checkpoint後もメッセージIDのhigh-water markを保持し、IDを再利用しない。
- pane IDが再利用されても、起動時に`@agent`の登録名を照合して誤配しない。
- 本文は1MiBを上限とし、journalの無制限な単発肥大を防ぐ。

単一イベントループにCLI要求とtmuxイベントを合流させることで、busy判定と
queue投入のlost wake-up、同一メッセージの同時二重配送を構造的に防ぎます。

## 外部送信元とスキル呼び出し

`send` の外部送信元ラベルは表示専用で、返信・配達失敗通知に使う内部sender
identityとは分離します。登録agentからのラベル上書きと予約名への偽装は拒否します。
登録agentからのskill指定も拒否し、skillを伴うpeer依頼をuser authorityとして
扱わせません。未登録のhuman callerと、許可された外部送信元からのskill指定は維持します。
ただしRPCのpane情報を含め、同一ユーザーで動くクライアントからの自己申告です。
送信元の真正性を保証する認証境界ではなく、認可や監査には使用しません。

`send --no-reply` は agent 間の一方向連絡を表す送信オプションです。journal/stateの
schemaは変更せず、既存Messageに生成済みのbrief/bellを保存します。既定の送信文言は
byte単位で維持し、no-reply時だけ「返信は不要」と明示します。完全な返信禁止ではなく、
重大な実害を防ぐ異議のみ1通可という運用判断はskill側が担います。外部mailboxの
`--from`との併用は拒否します。

## External mailbox

`send --from <label>` は通常のpane配達と同じIDを持つ `direction=out` eventを
journalへ追加します。返信は配達先paneで `agent-talk reply <id>` を実行し、現在の
pane IDと登録agent名が元eventと一致する場合だけ `direction=in` eventを追記します。
返信はtmuxへ打鍵しません。paneを同名agentが再登録した場合は後継paneとして返信を
許可する既知の挙動です。

`mailbox-list-v1` は非consumeのversioned JSON APIで、ID順・`--after`排他・limit最大500、
mailboxごとの最新500件retentionです。保存時刻はepoch秒、出力時にRFC3339へ変換します。
`TMUX_PANE`なしは外部callerの規約であり、セキュリティ境界は同一UIDのRPC socketです。
allowlistから外したmailboxのeventは削除せず閲覧だけ拒否します。新しいjournal variantを
含むファイルは旧binaryへdowngrade非対応です。外部event記録後のjournal障害では、配達
されなかった `direction=out` event が履歴に残る場合がありますが、pane配達は取り消されます。

スキル名は小文字ASCII英数字と `:`、`_`、`-` に限定し、64 bytesを上限とします。
agentごとの記法は自由文字列ではなく `slash` または `dollar` として設定し、daemonが
固定prefixを生成します。オプション付き送信は内部 `send-v2` protocolを使用するため、
未対応の旧daemonでは通常送信へ降格せず `unknown command` で失敗します。

## 既知の境界

人間がTUIへ入力してからbusy hookが発火するまでの短い区間は、tmuxの入力方式を
変えない限り完全には閉じられません。この区間の配送はbest-effortです。
通信範囲は同一ホストのtmuxサーバー内に限定し、ネットワーク越しの配送と
Windowsは対象外です。

旧RPCにはquiesce機能がないため、旧daemonの交代直前に配達記録が進行中だった場合、
 新daemonがjournal上の同じ未読IDを再配達する可能性があります。journalの各記録は
 fsync済みで、影響は呼び鈴が一時的に二重になる境界に限定されます。
