# 設計

agent-talkd は、tmux 上の対話エージェントへ作業中の入力を割り込ませずに
メッセージを渡すための小さなブローカーです。

## プロセス構成

- `agent-talk daemon` は tmux サーバーごとに1プロセス起動し、登録・busy状態・
  queueを単一イベントループで所有します。
- `agent-talk` の各CLIコマンドは、tmux socket名ごとのUnix domain socketを
  通じてdaemonへ1要求を送ります。daemonがなければ競合を避けて自動起動します。
- tmux control mode接続はtmuxサーバーの終了検知に使います。pane消滅はtmuxの
  通知だけでは全sessionを網羅できないため、global hookをwake-upとして
  tmuxの状態確定を短時間待ってからlive pane一覧と照合します。control mode
  だけが切断された場合はdaemonを維持し、低頻度のhealth checkでtmuxサーバー
  終了を検知します。hookにはdaemonのRPC socket絶対パスを渡し、tmux serverと
  CLIの環境変数が異なっても同じdaemonへ接続します。

## 状態と配送

daemonのメモリを稼働中の唯一の真実とし、`@agent` と `@agent_state` は
既存hookとの互換性を保つ表示用ミラーです。daemon起動時は`@agent`だけを
登録復旧のヒントとして読み、stateは必ずidleに倒します。stale busyには
自己修復の機会がなく配達が固着する一方、実際にbusyなら直後のbusy hookが
復元するためです。`@agent_state`を状態の真実として読み戻しません。

配送入口は1つです。

1. 依頼ヘッダと本文をID付きでjournalへ永続化します。
2. 宛先がidleなら、`@agent_state`をbusyにして`agent-talk read <id>`を
   案内する呼び鈴を入力し、0.3秒後にEnterを送ります。
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

## 既知の境界

人間がTUIへ入力してからbusy hookが発火するまでの短い区間は、tmuxの入力方式を
変えない限り完全には閉じられません。この区間の配送はbest-effortです。
通信範囲は同一ホストのtmuxサーバー内に限定し、ネットワーク越しの配送と
Windowsは対象外です。
