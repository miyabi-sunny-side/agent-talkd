# 設計

agent-talkd は、herdr 上の対話エージェントへ作業中の入力を割り込ませずに
メッセージを渡すための小さなブローカーです。

## プロセス構成

- `agent-talk daemon` は herdr ごとに1プロセス起動し、登録・busy状態・
  queueを単一イベントループで所有します。
- `agent-talk` の各CLIコマンドは、herdr socket名から導出したUnix domain socketを
  通じてdaemonへ1要求を送ります。daemonがなければ競合を避けて自動起動します。
- `agent-talk-mcp` はagentが触る窓口のstdio MCP serverです。同じUnix domain socketで
  daemonへ接続するだけで、自分では状態を持たず、daemonも起動しません。MCP serverは
  sessionごとにspawnされて一緒に死ぬため、herdr監視・queue・journal・跨pane配達を
  持てません。常駐プロセスはこれらの所有者として残ります。
- daemonは同じイベントループへ接続する第二のUnix domain socketで、read-onlyな
  HTTP adapterも提供します。registry、screen、許可済みmailboxの要求はeventを主ループへ
  送り、daemon memoryとlive herdr paneをそこで照合します。registryやmailboxを別の
  状態storeへ複製しないため、CLI配送との状態競合を増やしません。
- `ensure-daemon` は同じherdr socketだけを対象に、実行中daemonの版を確認します。
  同版なら何もせず、旧版は graceful shutdown を先に試し、旧RPCしかない場合だけ
  Unix socket peer の UID/PID に限定して停止します。名前による全体 kill は
  行いません。
- `internal-daemon-status` と `internal-daemon-shutdown` は lifecycle 管理用のRPCです。
  同じUIDのsocket利用者だけが呼べる既存のinternal RPCと同じ境界で、通常の利用者向け
  コマンドではありません。
- `update` は GitHub の公開 latest release をタグ固定で取得し、公開SHA-256の検証と
  安全なtar展開を完了するまで現行実体を変更しません。同一ディレクトリでfsync済みの
  stagingをatomic renameし、置換後の新binary自身で `ensure-daemon` を実行します。
  GitHubへのHTTPSと同一チャネルのchecksumが供給チェーンの境界であり、checksumは
  破損・部分ダウンロード検出を担います。
- pane消滅は2秒間隔のhealth tickがherdrのsnapshotと登録を照合して検知します
  (詳細は「状態と配送」のpull規則)。herdr自体の終了もこのhealth checkが検知し、
  一過性の実行失敗は1回だけ許容します。

## Status pageとHTTP-over-UDS

HTTP adapterは同一ホスト・同一herdrという境界を既定とし、
`AGENT_TALK_HTTP_ADDR`を明示したときだけTCPでも開きます。既定のsocketは
runtime directoryの`agent-talkd/<name>.http.sock`で、CLI用
`agent-talkd/<name>.sock`とstemを共有します。`<name>`は既定のherdr socketなら
`herdr`、named sessionなら`herdr-<session>`です。`XDG_RUNTIME_DIR`がなければ
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

1. GET以外は、`POST /api/letters` を唯一の例外として `Allow` 付きのJSON 405にする。
2. `GET /api/hello`は製品名とversion、`GET /api/who`はregistry snapshotをJSONで返す。
3. `GET /api/agents/<pane>/screen`はstrictにpercent decodeしたsegmentを
   opaqueなpane idとして扱い、登録paneだけをherdrの`pane.read`へ渡して
   現在の表示範囲をJSON内のplain-text文字列で返す。pane idの文法検証はせず、
   登録確認をherdr APIより先に行う。
4. `GET /api/mailboxes`は現在のallowlist、`GET /api/mailbox/<mailbox>`は既存の
   `mailbox-list`と同じ非consume event viewを返す。mailbox tokenとallowlist、
   `after`、`limit`の検査は既存primitiveと共有する。
5. `POST /api/letters`は唯一の書き込みrouteで、既存の外部mailbox送信経路
   (`send --from` と同一のallowlist・resolve・journal-first・配達/requeue) を
   そのまま通す。独自の送信実装を持たない。JSON以外のContent-Typeを415で
   弾きCORS headerを返さないことで、browserのcross-site simple requestを
   遮断する。無認証TCPでの投函は2026-08-05のuser指示
   (「手紙を出す機能を復活させる」) を直接のauthorityとし、到達範囲は
   operator所有のVPN/LAN境界、TCPは既定off、processへのauth追加はしない。
6. 未知の`/api/`以下は静的fallbackへ流さずJSON 404にする。
7. その他のGETは埋め込みassetを返し、該当assetがなければ`/index.html`へfallbackする。

失敗応答は`{"error":"<code>"}`です。statusはcallerが再試行と選択修正を区別できるよう
次の意味に固定します。

| HTTP | 意味 | 主なcode |
| --- | --- | --- |
| 400 | path/queryを修正しない限り成功しない | `invalid_path_parameter`, `invalid_query`, `duplicate_query_parameter`, `invalid_after`, `invalid_limit` |
| 404 | API、登録agent、または許可済みmailboxとして存在しない | `not_found`, `agent_not_found`, `mailbox_not_found` |
| 410 | 登録確認後にlive paneの消滅を確認した | `pane_unavailable` |
| 503 | broker、herdr、capture、registry、または静的assetが一時的・実行時に利用できない | `broker_unavailable`, `backend_unavailable`, `capture_unavailable`, `registry_unavailable`, `static_assets_unavailable` |

HTTP APIの状態変更routeは `POST /api/letters` ただ1つで、それ以外は読み取り専用の
ままである。letterの拒否は403 (source未許可)・404 (宛先不在)・413 (本文超過)・
415 (JSON以外)・400 (その他) を返し、拒否時はjournalにもstateにも変化を残さない。
screen captureはherdrの`pane.read`に限定し、
subprocessを起動しません。screen/mailbox path parameterのencoding不正、screen・
mailboxesへのquery、mailboxの未知・重複・範囲外queryは400です。mailbox tokenの
形式が不正、またはpaneが未登録・mailboxが未許可なら404です (pane idは文法検証
しません — opaqueな文字列として登録の有無だけを見ます)。登録確認後に
paneが消えた場合だけ410とし、herdr一覧・readまたはbrokerの一時障害は503にして、消滅と
観測不能を混同しません。capture payloadは1 MiBを上限とし、screen内容をlogへ記録しません。
mailbox履歴はreadしてもconsumeせず、journal追記、delivery state遷移、doorbell、checkpointへ
影響を与えません。

0700 directoryとpeer UID検査は別UIDからの開示を防ぎますが、同一UID内のpane所有者や
人間を区別しません。そのためHTTP socketへ到達できる同一UID callerには、全登録paneの
screenと、現在allowlistにある全mailbox eventを開示する設計です。この開示範囲を
pane/mailbox単位の認可と誤解してはなりません。busy recoveryは引き続き
human identity gateを備えた入口と同時に導入する将来課題である。letter送信は
2026-08-05のuser指示 (「手紙を出す機能を復活させる」) により、identity gateを
待たず `POST /api/letters` として導入済み — 認可はprocessに足さず、TCP面は
`AGENT_TALK_HTTP_ADDR` を明示設定したときだけ開き (既定off)、その到達範囲は
operator所有のVPN/LAN境界が定める。processはJSON-only Content-Typeを強制し
CORS headerを返さないことで、browserのcross-site simple requestだけを遮断する。

起動時はstaleなHTTP socketを除去してからbindします。daemonのイベントループ終了時は
RPC socketとHTTP socketの両方を除去します。bind途中で失敗した場合にHTTPだけを提供する、
またはRPCだけで継続する縮退modeは設けず、daemon全体を起動失敗にします。

## Frontendの埋め込みと更新

observation clientはNode.js 24、strict TypeScript、Svelte 5、Viteで構築します。npm依存関係は
`client/package-lock.json`で固定し、CIとreleaseは`npm ci`、format/type/test、frontend
buildをRust buildより先に実行します。Cargoの`build.rs`はnpmを起動せず、既に存在する
`client/dist`を列挙して`include_bytes!`用の表を生成します。これによりRustだけのbuildは
frontend toolchainなしでも成功し、assetがないバイナリはAPIを維持したまま静的routeだけ
`static_assets_unavailable`の503を返します。assetが必要な配布物は、順序を逆にすると
動作するstatus pageを含まないため、frontend-firstのbuild順がrelease不変条件です。

配布時は静的assetも既存の単一`agent-talk`バイナリに含まれます。したがってupdaterの
タグ固定asset、checksum検証、atomic replacementという既存境界がUIにもそのまま及び、
UI用の別install先・別version・別更新チャネルを作りません。

Screen viewはdocumentがvisibleの間だけ2秒後のself-schedulingでcaptureを更新し、hidden時は
timerを止め、visibleへ戻った時点で直ちに再取得します。手動更新とpollが競合した場合は
古い応答を捨てます。初回失敗はretry stateを表示し、過去のscreenがある更新失敗ではその
plain textをdimmed表示のまま保持して次のpollを続けます。Letters viewは自動pollせず、
mailbox選択時に履歴をresetし、手動更新では末尾IDを排他的`after` cursorとして最大100件を
追加します。mailbox切替前の遅い応答は捨て、本文はscreenと同様にSvelteのtext bindingで
表示してmarkupとして解釈しません。UIは個別のHTTP error codeを表示せず、ScreenとLettersの
各viewで共通の取得失敗表示とretryへまとめます。status codeの区別はAPI caller向けです。

## 状態と配送

pane idはherdrが発行する**opaqueな文字列**で、brokerは文法を定義しない —
採番規則の推測が実採番より狭くて配達とMCPが全停止した事故（65c83bbで拡張）の
構造的な再発防止である。宛先文字列がregistryのpane idに**完全一致**すれば
pane直指定として最優先で解決し、しなければ`scope/name`文法の名前として解釈
する（bare名は近接解決。tmux併存期の正式名称`herdr/scope/name`は互換alias）。

paneの表示・解決上のsession名は、herdr自身が持つworkspace **label**
（`workspace.list`）を使い、labelが無い・宛先構文と衝突する場合は
workspace_idへfallbackする。workspace_idは互換aliasとして解決だけに残す。
brokerはlabelをread-onlyで消費し、`workspace.rename`を呼ばない。

daemonのメモリを稼働中の唯一の真実とします。登録はdaemon側のpullです —
health tickごとにsnapshotを読み、agentの載っているpaneを冪等に登録します。
互換の`register` commandはherdrの検出と一致する名前だけを受理し（不一致・
agent不在・snapshot取得不能は拒否）、daemon起動時はsnapshotの取得に成功する
まで要求を受け付けません。
herdrのpane一覧はagent列をnativeに持つため、hookを挟むよりdaemonが観測する
ほうが正確で、登録hookを持たないagent（grok CLI等）もそのままpeerになれます。
pull側の規則:

- 同じpaneのagent名が変わったら旧登録を即座に外して引き継ぐ（native identityに
  猶予は不要。旧登録の残骸は誤配先になる）。
- snapshotから消えたpaneは即evictせず**suspect**にする — 配送はqueueに留め、
  当人からのRPCも拒否し、**2回連続の欠落で初めて**登録を外して未受領を回収する。
  1回の欠落はherdrの検出ラグと区別できないため、その時点で配送やevictを行うと
  実在する宛先を誤って失う。
- snapshot取得に失敗した間は判定を進めない（不完全な証拠で消さない）。
- daemon起動時は要求の受付前にも1回同期する。journalが復元した古いidentityが
  最初のtickまでaddressableだと、旧名宛の呼び鈴をpaneの新しい占有者へ送る
  誤配窓（最大2秒）ができる。
- 新規メッセージの上限判定は、dispatchのqueue行き条件と同じpredicate
  （busy・queue残留・suspect）を共有する。busyだけを見ると、suspectの凍結中に
  queueが上限を素通りして無制限に伸びる。
- 手動`unregister`は拒否する。pullが次tickで登録し直すため、
  受理すると解除→再登録の振動になるだけで、意図した効果を持たない。

配送入口は1つです。

1. 依頼ヘッダと本文をID付きでjournalへ永続化します。
2. 宛先がbroker上idle（配送待ちでない）なら、`read_message <id>`と
   `ack_message`を案内する呼び鈴をherdrの`agent.prompt`でagent本人へ
   submitします。herdrが**積極的にidleまたはdoneと判定したpaneにだけ**送り、
   `working`/`blocked`/`unknown`には一文字も送りません（doneは完了出力の
   未閲覧バッジで、配達可能にしないと非表示tab宛がuserの巡回まで滞留する。
   doneへの配達は未閲覧バッジを消して新ターンを始める）。スキル指定時も
   端末へ入るのは、daemonが検証・生成したスキルトークンと固定の呼び鈴だけです。
3. 宛先がbusyなら、配送待ちqueueへ入れてから`queued (busy)`を返します。
   **queueが空でない間は宛先がidleでも新規メッセージを直接配達しません**
   （配達失敗でrequeueされた古いメッセージを新規が追い越すFIFOの破れの防止）。
4. `turn-end`は宛先をidleにし、queue先頭を1件だけ配送してbusyへ戻します。
   加えて、**idleのままqueueが残っているpaneは2秒間隔のhealth tickが先頭を
   1件ずつ再配送**します。turn-endの一瞬はherdrの画面検出がまだworkingを
   返すことがあり、その1回の失敗だけを配送契機にするとメッセージが滞留する
   ためです。配送の安全境界（busyへ送らない、herdrの配達可能ガード =
   idle/done）は不変で、検出が追いついた次のtickで同じIDがFIFOのまま流れます。
5. `read`は本文を返し、読了だけを記録します（memoryのみ）。受領報告が来るまで
   何度でも読めます。
6. `ack`は受領報告をjournalへ追記・fsyncしてから、そのメッセージを削除対象に
   します。以後の`read`はnot-foundです。
7. 未受領のまま宛先が消滅した場合、元本文を含む未受領通知を送信元用の
   新しいメッセージとして作成します。通知は送信元paneごとに1通へ集約し、
   回収した全メッセージのIDと本文を含めます（呼び鈴も送信元あたり1回）。
8. 配達済みのまま受領報告が1分間ないメッセージには、受領催促の呼び鈴を
   送ります。催促が出るのはbroker状態がidleで、かつherdrの観測が配達可能
   （idle/done）のときだけです。読了済みならack、未読なら読むことを促し、
   同じメッセージへの催促は5分間隔より詰めません。busy中は撃たず、催促の
   状態はmemoryのみで再起動後は配達時刻から数え直します。

## 永続化の不変条件

本文とqueueを保持するjournalはJSON Linesのappend-only形式で、herdr
socket名（`herdr` / `herdr-<session>`）ごとに分離します。tmux併存期の
journal（tmux socket名で命名）がちょうど1つ残っていて新名の journalが
無い場合は、daemon起動時にrenameで一回だけ引き継ぎ、未受領messageと
採番済みIDを失いません。候補が複数・列挙失敗・rename失敗では推測も
新規開始もせず起動を失敗させます（新しい空journalはIDを再利用して
しまうため、手動でのrenameを求めます）。

- `sent`または`queued (busy)`を返す前に本文のappendと`fsync`を完了する。
- journal書き込みに失敗したメッセージを配達済み・queuedとして報告しない。
- daemon再起動時に未受領本文と未配達queueを復元する。
- 受領報告済み（Acked）かつ配送待ちqueueにない本文だけをcheckpointで圧縮消滅させる。
  読んだだけの本文は消さない。queue内の本文はAckedでも、後続の`turn-end`配送まで
  保持する。保持と可視性を同じ1つの真偽値で判定しない。
- checkpointの発火は、総レコード数ではなく**前回checkpoint以降に追記した
  レコード数**（256件）で判定する。総数で判定すると、圧縮後のsnapshot自体が
  閾値以上になった時点で、追記がゼロでも毎リクエストで全書き換え・`sync_all`・
  rename・親fsyncが走り続ける。
- 圧縮出力では`sequence`レコードを**末尾**に書く。`Journal::open`は通常レコードで
  カウンタを+1し、`sequence`で0へ戻すため、これが再起動を跨いだ追記数の復元境界に
  なる。checkpointが成功したときだけカウンタを0にする。
- checkpoint後もメッセージIDのhigh-water markを保持し、IDを再利用しない。
- pane IDが再利用されても、起動時と各tickでherdrのnative identityを照合して誤配しない。
- 本文は1MiBを上限とし、journalの無制限な単発肥大を防ぐ。

単一イベントループにCLI要求とhealth tickを合流させることで、busy判定と
queue投入のlost wake-up、同一メッセージの同時二重配送を構造的に防ぎます。

## 受領報告と保持

メッセージの削除条件は「受信側からの明示的な受領報告（ack）」ただ1つです。状態も
`Pending`（未受領）と`Acked`（受領報告済み＝削除対象）の1軸しかありません。
**読了は削除・掃除・失敗通知の判定軸にしません。** 読了を判定軸にすると「未読の
Pending」と「読了済みのPending」を区別する分岐が全経路に復活し、判定軸が2つに
増えるためです（0002の「読了を記録しない」はこの趣旨）。読了は**受領催促の文言を
選ぶためだけ**にmemoryへ記録し、journalに残さず、上記のどの判定にも使いません。
restartで未読へ戻っても、催促文言が保守側（「未読なら読んでくれ」）に倒れるだけです。

daemonは`read`と`ack`で同じ宛先・配達状態の検査を使います。

| 対象 | `read` / `read-message` | `ack-message` |
| --- | --- | --- |
| 呼び出し元pane宛・配達完了済み・未受領 | 本文を返し、読了だけをmemoryに記録（受領・削除の状態は変えない） | append＋fsyncの後に`Acked`。`outcome: acked` |
| 配達未完了（queue中） | 拒否 | 拒否 |
| 他pane宛 | 拒否 | 拒否 |
| 存在しない、または受領報告済み | not-found | mutationなしで冪等成功（`outcome: no_pending_message`） |

この表はMCP adapter用RPC（`read-message` / `ack-message`）では、呼び出し元paneが登録済みagentである
場合にだけ適用されます。未登録paneはこの分岐へ入る前に拒否されます（「MCP adapter」を
参照）。

配達未完了を拒否するのは、IDを推測して呼び鈴の前に本文を読ませないためです。これを
許すと、送り手の未受領一覧からは消えたのに呼び鈴だけ後から届き、その時点の`read`が
not-foundになります。

存在しないIDをエラーではなく冪等成功にするのは、応答が失われた後の再送を安全に
するためです。`Acked`はcheckpointで所有情報ごとpruneされるので、再送時に
「既に受領済み」と「そもそも存在しない」を区別できません。区別を保つには宛先付きの
tombstoneを永続保持する必要があり、削除を目的とする本契約と矛盾します。mutationを
伴わないため、他paneの`Pending`を消す危険もありません。

未受領IDは両方向から観測できます。`pending_to_me`（`who`では`pending-to-me`）は
呼び出し元pane宛で**配達完了済み**かつ未受領のID、`pending_from_me`（`who`では
`pending-from-me <pane>`）は呼び出し元が送って未受領のIDで、**queue中も含みます**。
どちらも本文と他送信者のIDは含みません。送り手側だけにすると、受け手が中断・再起動して
IDを失ったとき、本文が残っていても受け手自身が再発見できなくなります。

受領報告を忘れてもメッセージは残ります（誤削除より安全側）。自動では削除しま
せんが、放置もしません: 配達済みのまま受領報告が1分間ないメッセージには、宛先が
broker状態がidleで、かつherdrの観測が配達可能（idle/done）のときだけdaemonが
受領催促の呼び鈴を送ります。読了済みならack、未読なら読むことを促し、pane単位で
1回の呼び鈴に集約し、同じメッセージへは5分間隔より詰めません。busy中は撃たず、催促は新しいメッセージを作りません（催促自体が受領報告の
対象になる再帰を避けるため）。催促タイマーはmemoryのみで、restart後は配達時刻
から数え直します。

### journalのwire tagは`consumed`のまま

製品用語の`Acked`に対応するjournalレコードのtagは`consumed`のままです。variant名を
変えると旧daemonが既存journalを読めなくなるため、tagを維持して意味だけ
「読了＝次の圧縮で削除」から「受領済み＝削除対象」へ読み替えています。旧journalの
`consumed`レコードは新しい意味と一致するので、移行処理も既定値も不要です。コード上の
`StoredMessage::acked`と`Record::Consumed`という名前のずれはこの互換のためです。

`sequence`が先頭にあるか存在しない旧journalは、開いた時点の追記数を保守的に多く
数えます。その結果1回だけ余分に圧縮し、以後は末尾`sequence`により正確になります。

### pane消滅時の掃除

読了は削除判定に使わないため、掃除と失敗通知の対象は**未受領の`Pending`全件**とし、
通知も「配達されなかった」ではなく「受領報告されないまま宛先が退出した」を表す
文言にします。読んだがack前に落ちた場合も未受領として扱う、ackを正とする意味論です。
通知は送信元paneごとに1通へ集約し、回収した全メッセージのIDと本文を含めます
（呼び鈴も送信元あたり1回）。集約通知に収録する元本文の合計は送信本文と同じ1MiBを
上限とし、超過する分はIDを残して本文を明示的に省略します（journalの単発肥大の防止は
集約後も維持されます）。通知の永続化と回収した全`Pending`のterminal化は同一の
journal appendで行い、journal replayでも`remove`後に同じ最終状態へ収束させます。
通知の途中で失敗した場合はremoveせず、耐久性を壊しません。

宛先が既に退出していて配達できなかったメッセージと、送信者向けの未受領通知を作り
終えた元メッセージは、terminalなtombstoneとして従来どおり受領報告を待たずに即削除
対象にします。これらには受け取る主体が存在せず、ackする者がいないためです。

## MCP adapter

`agent-talk-mcp`はstdio JSON-RPCのMCP serverとして、daemonの既存RPC socketへ接続
します。新しいsocketも新しいprotocolも作りません。agentへ公開する面はここのtool 4個
だけで、agentがCLIの使い方を読む必要をなくすことが目的です。MCPは道具の一覧と引数
仕様を毎ターン自動でcontextへ渡すため、調べる対象そのものが消えます。

| tool | daemonのコマンド |
| --- | --- |
| `list_peers` | `list-peers` |
| `send_message` | `send-message` |
| `read_message` | `read-message` |
| `ack_message` | `ack-message` |

旧wire名（`peers-v1` / `send-message-v1` / `read-v1` / `ack-v1` / `mailbox-list-v1`）は
稼働中の旧adapterをsession途中で壊さないための互換aliasとして残り、次のminorで
削除されます。逆方向のskew（新adapter→旧daemon）は、daemonが明示的に
`unknown command`を返したときだけadapterが旧名で1回再試行して吸収します。
接続失敗・timeout・壊れた応答では再試行しません（sendの二重配送の防止）。

`send_message`がCLIと同じ`send-v2`ではなく専用の`send-message`を使うのは、成功応答の
形が違うためです。CLIは人間向けテキスト（`sent -> w1:p2 (claude): #0`）を返し、MCPは
versioned JSON（`{"version":1,"id":0,"path":"sent","to":"w1:p2","name":"claude"}`）を
返します。1つのコマンドに2つの応答形を持たせると、どちらを返すかが呼び出し元の申告に
依存し、agentが人間向けテキストを構造化応答と取り違える余地が残ります。

`skill` / `from` / `pane` はtoolのschemaに存在しません。存在しない引数は誤用も偽装も
できません。呼び出し元identityは、spawn時の`HERDR_PANE_ID`があればadapterが申告し、
無ければ**daemonが接続のSO_PEERCREDのPIDから/procの祖先を遡り、herdrがagent本体へ
与えた`HERDR_PANE_ID`/`HERDR_SOCKET_PATH`の2 keyだけを読んで確立**します (Linux)。
cwdやコマンド名からの推測はしません（同種agentが同じdirectoryに2つ居ると誤配する）。
祖先が別のherdr sessionに属する場合・identityが見つからない場合はfail closedで、
明示forwardを案内します。wire上の`peer_pid`はserde skipで、clientの自己申告では
偽装できません。いずれのidentityもrouting metadataであって認証境界ではなく、実際の
境界はdaemon側の同一UID UDSと未登録paneの拒否です。

MCP serverはagentのexec sandboxの外で起動されるため、agent自身のshellより広い権限を
持ちます。そのためtool surfaceを会話だけに限定し、file読み書き・任意path指定・
subprocess実行・shell経由の呼び出しをtoolにも実装にも持ち込みません。1つでも持たせると、
agentが自分のsandboxを迂回する経路になります。

initializeで返すserver instructionsは短い操作契約に限定し、「呼び鈴を受けたら
`read_message`で読み、作業に入る前に`ack_message`で受領報告する」を含めます。toolが
context にあるだけでは横展開は起きない一方、判断そのものを縛る大きな文にすると
skillを消した意味が失われます。

### 未登録paneからの呼び出しは状態を見る前に拒否する

`send-message` / `read-message` / `ack-message` / `list-peers`の4つは、呼び出し元paneが現在
登録済みのagentであることを最初に確認し、満たさなければmessageの状態分岐へ進む前に
拒否します。判定を分岐より前に置くのは、未登録callerへ「存在しない」「他pane宛」
「配達未完了」の区別を返さないためです。区別が漏れると、未登録paneからID空間を
走査してmessageの存在と配達状況を観測できます。拒否はjournal追記・呼び鈴・
未受領一覧のいずれも変えません。

legacyの`send` / `send-v2`は従来どおり未登録paneからも受理します。これは人間がCLIから
送る唯一の経路であり、閉じるとhuman callerが送れなくなります。MCP側の4つだけを閉じる
のは、そこがagentの経路であり、登録に失敗したagentがhuman callerを騙って送れては
ならないためです。

### `read_message`が返すidentityは送信時点で捕捉したもの

`from`は送信時点で捕捉した送信者名（`Message::sender_name`）で、読み出し時にregistryを
引き直しません。`reply_to`は、捕捉時と同じ名前のagentがその paneに今も登録されている
場合だけpane IDを返し、それ以外は`null`です。送信者の退出・改名と、pane IDが別のagentへ
再利用された場合が`null`に当たります。

`from`を現在のregistryから引き直すと、pane再利用後に別人の名前で本文が提示されます。
逆に`reply_to`を捕捉したpane IDのまま返すと、そのpaneの新しい住人へ返信を誤配します。
表示用の名前と実際に配達へ使う宛先を別の規則で決めるのはこのためです。旧journal由来で
`sender_name`を持たないmessageは、`from`が生のsender（pane IDや`human`などのラベル）に
なり、`reply_to`は常に`null`になります。

### 応答の解釈を暗黙に劣化させない

4つのRPCはいずれもversioned JSONを返す契約なので、adapterは成功応答（`code == 0`）でも
`version`が1のJSON objectでなければ`isError: true`にし、
`agent-talkd の <command> 応答を解釈できません (<理由>)`を返します。人間向けテキストを
そのままtool結果として通すと、agentは送信が成立したかどうかを文面から推測することになり、
契約の違うdaemonへ接続していることにも気づけません。

### 起動時のcontract

接続先はspawn時の環境から純粋に導出します。`Config::discover`は呼ばず、
subprocessも起動せず、tool引数や`AGENT_TALK_RPC_SOCKET`のような任意の環境変数からも
接続先を受け取りません。

| 入力 | 扱い |
| --- | --- |
| `HERDR_SOCKET_PATH` | 任意。あれば絶対pathを検証してその herdr 用の socket 名を導出、無ければ既定 session の固定名 `herdr` |
| `HERDR_PANE_ID` | 任意。あれば opaque な id としてそのまま申告 (文法検証しない)、無ければ daemon の peer PID 解決に委ねる |
| `XDG_RUNTIME_DIR` | 任意。絶対pathならruntime rootに使う |
| `HOME` | `XDG_RUNTIME_DIR`欠落時のみ必須。`$HOME/.cache/agent-talkd/run`へfallback |

「設定されているのに壊れている」入力とruntime rootの欠落だけがfail closedです。
接続後はpeer UIDが自分のeffective UIDと一致することを確認し、daemon側の
same-UID境界と対称にします。

RPC socket pathはbasenameだけが`HERDR_SOCKET_PATH`由来（無ければ`herdr`）で、
**rootは`XDG_RUNTIME_DIR`**です。daemonがruntime directoryを使っている環境でMCP側にだけ`XDG_RUNTIME_DIR`が
渡らないと、MCPだけがHOME fallbackを導出し、実在しないsocketを掴んで必ず失敗します。
このときinitializeとtools/listは成功し、tool呼び出しだけが
`agent-talkd に接続できません (<path>)` というtool errorになります。掴んだpathが
daemonのRPC socketと違っていないかを、まずこのメッセージで確認します。

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
返信は端末へ打鍵しません。paneを同名agentが再登録した場合は後継paneとして返信を
許可する既知の挙動です。

`mailbox-list` は非consumeのversioned JSON APIで、ID順・`--after`排他・limit最大500、
mailboxごとの最新500件retentionです。保存時刻はepoch秒、出力時にRFC3339へ変換します。
paneなしは外部callerの規約であり、セキュリティ境界は同一UIDのRPC socketです。
allowlistから外したmailboxのeventは削除せず閲覧だけ拒否します。新しいjournal variantを
含むファイルは旧binaryへdowngrade非対応です。外部event記録後のjournal障害では、配達
されなかった `direction=out` event が履歴に残る場合がありますが、pane配達は取り消されます。

スキル名は小文字ASCII英数字と `:`、`_`、`-` に限定し、64 bytesを上限とします。
agentごとの記法は自由文字列ではなく `slash` または `dollar` として設定し、daemonが
固定prefixを生成します。オプション付き送信は内部 `send-v2` protocolを使用するため、
未対応の旧daemonでは通常送信へ降格せず `unknown command` で失敗します。

## 既知の境界

herdrの画面検出には原理的なラグがあり、状態取得と送信の間のraceは
herdr側にatomicなAPIが無い限り完全には閉じられません。この区間の配送は
best-effortです。UDS面の通信範囲は同一ホストのherdr内に限定し、TCP面は
operator所有のVPN/LAN境界の内側だけを想定します。Windowsは対象外です。

旧RPCにはquiesce機能がないため、旧daemonの交代直前に配達記録が進行中だった場合、
 新daemonがjournal上の同じ未読IDを再配達する可能性があります。journalの各記録は
 fsync済みで、影響は呼び鈴が一時的に二重になる境界に限定されます。
