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
  live pane一覧と照合します。

## 状態と配送

daemonのメモリを唯一の真実とし、`@agent` と `@agent_state` は既存hookとの
互換性を保つ表示用ミラーです。ミラーの読み戻しはdaemon起動時の復旧に限り、
通常運転中の状態を上書きしません。

配送入口は1つです。

1. 宛先がidleなら、`@agent_state`をbusyにして呼び鈴を入力し、0.3秒後に
   Enterを送ります。
2. 宛先がbusyなら、journalへ永続化してから`queued (busy)`を返します。
3. `turn-end`は宛先をidleにし、queue先頭を1件だけ配送してbusyへ戻します。
4. 宛先が消滅した場合、未配達依頼ごとに送信元へ配達失敗を通知します。

## 永続化の不変条件

queue journalはJSON Linesのappend-only形式で、tmux socket名ごとに分離します。

- `queued (busy)`を返す前にappendと`fsync`を完了する。
- journal書き込みに失敗したメッセージをqueuedとして報告しない。
- daemon再起動時に未配達queueを復元する。
- 配達済み・失敗処理済みレコードはtombstoneで確定し、checkpointで圧縮する。
- pane IDが再利用されても、起動時に`@agent`の登録名を照合して誤配しない。

単一イベントループにCLI要求とtmuxイベントを合流させることで、busy判定と
queue投入のlost wake-up、同一メッセージの同時二重配送を構造的に防ぎます。

## 既知の境界

人間がTUIへ入力してからbusy hookが発火するまでの短い区間は、tmuxの入力方式を
変えない限り完全には閉じられません。この区間の配送はbest-effortです。
通信範囲は同一ホストのtmuxサーバー内に限定し、ネットワーク越しの配送と
Windowsは対象外です。
