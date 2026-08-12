# 0002. メッセージは受領報告で消す

- Status: accepted
- Independent review: codex (`%6`) — 再判定で **A〜F すべて PASS**
- Date: 2026-08-01
- 関連: [0001](0001-conversation-broker-scope.md)。MCP と同時に導入する
- Amendment (2026-08-12): 宛先本人の **pull 配達**を許可。`pending_to_me` は queue 中を含む。
  未配達の `read` は journal `Complete` で durable 配達完了してから本文を返す。
  未配達の `ack` は拒否（先に read）。他 pane 拒否は維持。詳細は下記「Amendment」。

## 要約（決裁者はここだけ読めば足りる）

**受け取った側が「読んだよ、届いたよ」と報告するまでメッセージは残る。報告したら消える。**

手順は3段階だけ（主経路は push 呼び鈴）。

1. 呼び鈴が鳴る（または `pending_to_me` で自己発見する） → **読む**（何度でも読める）
2. **作業に入る前に受領報告を送る** → ここで消える
3. 作業する。返信が必要なら**普通の送信機能**で新しい手紙を送り返す

| 問い | 答え |
|---|---|
| いま何が起きているか | 一度 `read` すると、次の journal 圧縮で**本文が完全に消える** |
| それで何が困るか | 読む前や読んだ直後に中断されると本文ごと失われる。実例: `#714` を後から読もうとして「見つかりません」になり要約を再送した |
| どう変えるか | **受領報告が来るまで消さない**。報告は agent が明示的に送る |
| agent は何を覚えるか | 「届いたら読んで、作業前に受領報告」。主 wake は呼び鈴。必要なら `list_peers` → pull read |
| 報告を忘れたら | メッセージは残る。`list_peers` が**両側から未受領を見せる**ので、受け手は自分で読み直せて、送り手も気づける |
| 「届いたか分からない」問題は | `list_peers` が**自分が送って未受領の ID**（`pending_from_me`）と**自分宛で未受領の ID**（`pending_to_me`）の両方を返す |
| 呼び鈴の前に読めるか | **宛先本人は読める**（pull）。pull は `Complete` を書いて queue から外すので後から同 ID の呼び鈴は鳴らない。他 pane は拒否。未配達の ack は拒否 |
| 返信は | 専用の仕組みを作らない。**普通の送信**で返す |
| 30分などの時間制限は | **不要**。前案の TTL は破棄 |
| user から見て何が変わるか | 中断しても本文が残る。自分の送信が相手に届いたか ID 単位で分かる |
| user がいま決めることは | **無い** |
| 次の一歩 | MCP の Phase 1 と同時に実装 |

**なぜ時間ではなく報告か**: 30分という数字に根拠がない。短ければ失われ、長ければ溜まる。
「受け取ったと言われたから消す」なら、必要な間だけ正確に残り、しかも**送達確認が副産物として
手に入る**。

## Decision

メッセージの削除条件を「次の checkpoint」から「**受信側からの明示的な受領報告**」へ変更する。

- `read` は**一切消さない**。受領報告が来るまで何度でも読める
- 受領報告は agent が明示的に送る。MCP tool `ack_message(id)` を追加する
  （[0001](0001-conversation-broker-scope.md) の tool は 3 → **4** になる）
- 返信は専用機構を作らず `send_message` で行う（[0001](0001-conversation-broker-scope.md) の
  `reply_message` 不採用の方針を維持）
- `list_peers` に**両方向の未受領 ID 一覧**を含める
  （`pending_to_me` = 自分宛の未受領。**queue 中を含む**、`pending_from_me` = 自分が送って未受領）
- `read_message` は宛先本人なら配達前でも許可し、そのとき **pull 配達**（`Complete`）する
- `ack_message` は**配達完了前を拒否**する（先に read）

## Decision owner

user（miyabi）。

## Authority evidence

user 原文（2026-08-01、本 repo の会話ペイン）:

> 読了未完了のメッセージが残るという議論は、私の想定と違います。
> まず通知を受け取ったら、それが「作業後返信してください」「返信不要です」問わず、
> 作業に入る前に「手紙読んだよ、届いたよ」という報告をagent-talkに対して送信する。
> そして作業後、必要に改めて返信用の手紙を作成し、普通の送信機能で相手に送り返す。
> これにしましょう。

先行する原文（同日）:

> メッセージを一度読んだらもう読めないというのがシビア過ぎるんじゃないかと思いました。
> スパイ映画のカセットテープかよ。

> そもそも届いているのか届いてないのかよくわからんって問題も同時に解決しますしね。

## 現状の正確な挙動（実装で確認済み）

1. `read` は `stored.consumed = true` を立てる（`src/state.rs:225`）。この時点ではまだ読める
2. `checkpoint_if_needed` は**リクエストのたびに**呼ばれる（`src/daemon.rs:814`）
3. 発火条件は journal の**総レコード数 ≥ 256**（`src/journal.rs:119`）
4. 圧縮時、`consumed && !queued` は新 journal に書かれず捨てられ、
   `prune_consumed_not_queued()` でメモリからも消える（`src/journal.rs:150,196`）

つまり**読んだ瞬間ではなく次の圧縮で消える**。圧縮は頻繁なので体感は「読んだら消える」。

## 設計

### 状態

削除の判定に必要なのは**受領したか否かの1軸だけ**。

| 状態 | 意味 |
|---|---|
| `Pending` | 未受領。**読まれていても残る** |
| `Acked` | 受領報告済み。削除対象（queue 中なら本文は保持、可視性からは除外） |

`read` は状態を変えない。「読んだが未完了」という中間状態を**作らない**。
前案が持ち込んだターン単位の読了集合も、それに伴う複雑さも不要になる。

**読んだかどうかは記録しない。** 削除判定に使わないうえ、記録すると
「未読の Pending」と「読了済みの Pending」を区別する分岐が復活する。判定軸は受領の1つだけ。

### `ack_message` の契約

| 対象の状態 | 動作 |
|---|---|
| 呼び出し元宛の `Pending`（配達完了済み） | 宛先検査 → append + fsync → `Acked`。以後 `read` は not-found |
| **queue 中／配達未完了** | **拒否**。下記の理由により、配達が完了するまで ack させない |
| 他 pane 宛の `Pending` | **拒否**（既存の `read` と同じ宛先検査） |
| 存在しない ID | **mutation なしで冪等成功**（`no_pending_message` 等の outcome を返す） |

### `read_message` の所有者 pull（Amendment 2026-08-12）

当初は「配達未完了の read/ack を拒否し、`pending_to_me` は配達完了済みだけ」とした。
これは push 呼び鈴前の先読みと、配達前 ack による「後から呼び鈴だけが届く」害を
防ぐためだった。

運用上、**宛先本人**が busy 中に溜まった自分宛を、呼び鈴を待たず自己発見・読む
必要があることが分かった。他 pane への秘匿は宛先検査で足り、本人に本文を秘匿する
理由はない。

| 対象 | 契約（改正後） |
|---|---|
| `read_message`（宛先本人・未配達） | **queue 先頭のときだけ** journal に `Complete` を append+fsync → queue から外し `delivered=true` → 本文を返す（`agent.prompt` なし）。後続の先取りは拒否。fsync 失敗時は本文を返さず queue を進めない |
| `read_message`（他 pane） | **拒否**（従来どおり） |
| `ack_message`（未配達） | **拒否**（先に read。本文を見ずに消さない／旧害の再発防止） |
| `pending_to_me` | 自分宛の**全未受領**（queue 中を含む） |
| `pending_from_me` | queue 中を含む**全未受領**（送り手は「まだ届いていない」も知りたい） |
| queue 中の `Acked` 化 | pull 後は通常 ack。未 pull の明示 ack は不可。内部 terminal 処理は従来どおり |

**なぜ「存在しない ID」を成功にするか**: 応答が失われた後の再送を安全にするため。
`Acked` は checkpoint で prune され所有情報ごと消えるので、再送時に
「既に受領済み」と「そもそも存在しない」を区別できない。区別を保つには
宛先付き tombstone を永続保持する必要があり、削除を目的とする本決定と矛盾する。
mutation を伴わないので、他 pane の `Pending` を消す危険もない。

**なぜ queue 中の ack を拒否するか（維持）**: 配達前に ack されると、
送り手の未受領一覧からは消えるのに呼び鈴だけ後から届き、`read` が not-found になる
害がある。pull read が先に `Complete` すればこの害は起きない。ack 単独の pull は
本文を見ずに消せるため許可しない。

`ack_message` は**pane を引数に取らない**。呼び出し元 pane は adapter が導出する
（[0001](0001-conversation-broker-scope.md)）。登録済み caller pane・target pane・
現在の target name を検証してから ack する。
**journal の append + fsync が成功する前に、メモリと可視性を `Acked` へ進めない。**

### 報告忘れの扱い

agent が `ack_message` を忘れると、メッセージは `Pending` のまま残る。これは**安全側**である
（誤削除より残る方が良い）。ただし「自動的に直る」わけではない。

`pending_from_me` は**送り手にしか見えない**。受け手が中断・再起動して ID を失うと、
本文は残っているのに**受け手自身が再発見できない**。送り手が気づいて再通知しない限り
放置される。

したがって `list_peers` は**両方向**を返す。

| 欄 | 内容 |
|---|---|
| `pending_to_me`（top-level） | 呼び出し元宛で未受領の ID 一覧（**queue 中を含む**）。**本文は含めない** |
| `pending_from_me`（peer ごと） | 呼び出し元が送って未受領の ID 一覧。**queue 中も含む** |

これにより受け手は再起動後も `list_peers` → `read_message` → `ack_message` を再開できる。
server instructions に「呼び鈴を読んだら作業前に `ack_message` を呼ぶ」を明記する。

### pane 消滅時の掃除

現実装では、pane が消えても**読了済みメッセージは掃除されない**。実装確認済み:

- `src/state.rs:275` の `messages_for_target` は `!stored.consumed` で絞る
- `src/daemon.rs:1530` の `remove_agent` はその集合だけを `notify_failures` へ渡す
- `src/state.rs:92` の `remove` は agent エントリを消すだけで messages を消さない

新状態では「読まれたが未受領」が長く残りうるため、掃除契約を明示する。

**読了を記録しないので「未読の Pending」は判定できない。** よって掃除と失敗通知は
**「未受領の `Pending` 全件」**に対して定義する。

- 失敗通知の対象は**未受領の `Pending` 全件**（読了済みかどうかを問わない）
- 通知文は「配達されなかった」ではなく
  **「受領報告されないまま宛先が退出した」**とする。
  読んだが ack 前に落ちた場合も未受領として扱う、という **ack を正とする意味論**
- pane 除去時、**通知の完了後に残る `Pending` を terminal `Acked` にする**
- journal replay でも `Remove` 後に同じ最終状態へ収束する
- 通知の durability を壊さない。**途中失敗では remove しない**

### `read` 以外の Consumed を巻き込まない

現行の `Record::Consumed` は read 専用ではない。production の生成箇所は3つ:

| 箇所 | 意味 | 新形式での扱い |
|---|---|---|
| `daemon.rs:1304`（`read`） | 読んだ | **記録しない**（read は削除に影響しない） |
| `daemon.rs:1252` | 宛先退出により配達不能 | `Acked`（terminal tombstone） |
| `daemon.rs:1656` | 配達失敗を送信者へ通知済み | `Acked`（terminal tombstone） |

terminal tombstone は従来どおり**即座に削除対象**であり、受領報告待ちにはしない。

### journal 形式と旧データの移行

`Record::Consumed { id }` を **`Record::Acked { id }`** として扱う。
variant 名を変えると旧 journal を読めなくなるため、**tag は `Consumed` のまま維持**し、
意味だけを「受領済み = 削除対象」に読み替える。

旧 journal の `Consumed` レコードは「読んだので次の圧縮で削除対象」の意味だったので、
**新意味の `Acked` と一致する**。追加フィールドも custom default も不要で、移行処理はいらない。

前案が必要としていた `done: bool` と `#[serde(default = "default_true")]` は**不要になった**。

### 送達可視性

- 集約件数だけでは、送信者が**自分の**メッセージの受領を判定できない
  （他送信者の増減に紛れるため）。ID 一覧で返す
- 本文も、他送信者の ID も開示しない
- `send_message` が返した ID が `pending_from_me` にあれば未受領、
  無ければ受領済みまたは terminal
- 相手の総 pending 件数は workload 表示として併記してよいが、**受領の根拠にはしない**

### checkpoint 発火条件（併せて修正する既存バグ）

現在は総レコード数で判定し、圧縮後に `self.records = count` を代入する。
**圧縮後の snapshot 自体が256レコード以上なら、追記ゼロでも毎リクエストで
全書き換え + `sync_all` + rename + 親 fsync が走り続ける。**
配達済み未読は概ね2レコードなので約128件で閾値に達し、現行でも起こりうる既存バグ。

判定を「**前回 checkpoint 以降に追記したレコード数**」へ変える。

### 再起動を跨いで追記数を復元する

カウンタを checkpoint 後 0 とし `Journal::open` で 0 初期化すると、
**256追記未満ごとに再起動する環境では永久に checkpoint されず journal が単調増加する**。
逆に open 時へ総数を入れると、大きい snapshot では再起動直後に毎回不要な圧縮が走る。

新 variant は足さず、**既存の `Record::Sequence` を checkpoint 境界マーカーとして再利用**する。

- checkpoint 出力では `Sequence` を**先頭ではなく末尾**に書く
- `Journal::open` のカウンタは通常レコードで +1、`Sequence` で **0 にリセット**
- 旧 journal は `Sequence` が先頭または不在なので、初回だけ保守的に多く数えて1回圧縮し、
  以後は正確になる
- 旧 daemon も `Sequence` 自体は既知なので rollback 互換性が高い
- **checkpoint 成功時のみ** 0 にする。失敗時はリセットしない

ID 再利用防止は保たれる。`restore_next_id` は `self.next_id.max(next_id)`、
`restore_message` も `max(message.id + 1)` であることを実装で確認済み。

### queue の優先

`Acked` でも queue 中なら本文を durable に保持する。ただし可視性と未受領一覧からは除外し、
queue から外れた後に checkpoint で prune する。
保持と可視性を同じ1つの真偽値で判定しないこと。

## Rejected alternatives

| 案 | 却下理由 |
|---|---|
| **時間ベース TTL（30分）** | 数字に根拠がない。短ければ失われ長ければ溜まる。削除を checkpoint 任せにすると低トラフィック時は30分を過ぎても読めてしまい契約を満たせない |
| **ターン終了を暗黙の受領とする** | 「読んだが未完了」という中間状態が長く残り、ターン単位の読了集合・busy 由来の区別・再起動時の保守的復旧といった機構が必要になる。user の想定（作業前に明示報告）とも異なる |
| **`read` した時点で即削除** | 現状と同じ。読んだ直後の中断で本文が失われる（`#714` の実例） |
| **読んでも一切消さない** | journal が単調増加し圧縮の意味が失われる |
| **集約 pending 件数だけで送達可視性を出す** | 送信者が自分の ID の受領を判定できない |
| **未受領一覧を送り手側（`pending_from_me`）だけにする** | 受け手が中断・再起動して ID を失うと、本文が残っていても**自分で再発見できない**。送り手が気づいて再通知しない限り放置される |
| **返信用の専用 tool を作る** | 返信は普通の送信で足りる（user 指示）。tool を増やさない |

## Non-goals

- 未読メッセージの保持規則の変更（現状どおり無期限）
- 外部 mailbox の retention（[0001](0001-conversation-broker-scope.md) で削除決定済み）
- 過去に失われたメッセージの復元（不可能）
- 既読・未読の履歴閲覧 UI（[0001](0001-conversation-broker-scope.md) の非目標）
- 送信者への能動的な受領通知（呼び鈴の増加になる。`list_peers` の観測で足りる）
- 本文や他送信者の ID の開示
- 受領報告の自動化（agent の明示的な行為とする。user 指示）

## Premises

1. agent は server instructions に従い、読了後・作業前に `ack_message` を呼ぶ。
   忘れても誤削除は起きず、`pending_to_me` / `pending_from_me` により**両側から観測できる**。
   ただし**自動的に解消はしない**。受け手が `pending_to_me` を見に行くか、
   送り手が気づいて再送するまで残る
2. pane 消滅時、本決定で追加する掃除契約により `Pending` が terminal 化される
   （**現実装では成立していないため、本決定で契約として追加する**）

## Reopen triggers

- 受領報告の忘れが実測で常態化し、未受領が溜まり続けた（premise 1 が偽）
- journal のサイズや圧縮頻度が実測で問題になった
- 権限範囲外の変更が必要になった

## Verification

**受領報告と保持**

- **配達完了済み** message の再 `read_message` は本文を返し、journal が増えないこと
  （未配達への初回 pull は `Complete` を1回追記する）
- **`read` 後に checkpoint を跨いでも読めること**（変更前はここで消えていた）
- `ack_message` 後に `read` が not-found を返すこと
- **checkpoint / prune の後に ack を再送しても、mutation なしで冪等成功すること**
  （`no_pending_message` 相当。他 pane の `Pending` を消さないこと）
- **`pending_to_me` に queue 中の ID が現れること**（所有者 pull の自己発見）
- **queue 先頭の所有者 `read_message` は pull 成功し、後続 ID の先取りは journal/state 不変で拒否されること**
- **未配達の `ack_message` が拒否されること**（先に read。本文を見ずに消さない）
- **宛先本人の未配達 pull 後、後続の同じ ID の呼び鈴が無いこと**
- `pending_from_me` には queue 中の ID が現れること
- queue 中のメッセージを `Acked` にできるのは内部の terminal 処理だけであること
  （未 pull の明示 ack は不可。pull 後は通常 ack）
- **他 pane 宛の ID を ack できないこと**（既存の宛先検査と同じ境界）
- `ack_message` が pane を引数に取らず、caller pane を adapter 側から導出していること
- **`pending_to_me` により、受け手が再起動後に未受領 ID を再発見し read → ack を再開できること**
- `pending_to_me` / `pending_from_me` が本文を含まないこと
- ack の journal 追記が失敗したとき `Acked` にせず、再読と再 ack が可能なこと（fail closed）
- 未受領のまま中断・再起動しても本文が残ること

**pane 消滅**

- 未受領メッセージを持つ pane が退出 → 失敗通知は重複生成せず、メッセージは削除対象になること
- **読了済みだが未受領のまま pane が退出した場合も「受領報告されないまま退出」として通知されること**
- 通知文が「配達されなかった」ではなく受領報告の欠如を表す文言であること
- daemon 再起動後も復活しないこと
- 通知の途中失敗では remove しないこと

**送達可視性**

- 複数送信者が同時に増減する状況で、呼び出し元が**自分の ID だけ**正しく追跡できること
- 本文と他送信者の ID が露出しないこと

**既存契約の非回帰**

- 未読メッセージが checkpoint を跨いで保持されること
- **旧 journal の `Consumed` レコードが、新意味の `Acked`（即削除対象）として replay されること**
- wire tag が `Consumed` のままであること（新 variant を足すと旧 daemon が読めなくなる）。
  コード上の名称と製品用語 `Acked` のずれをコメントとテストで固定すること
- **queue 中なら `Acked` でも本文が保持され、再起動を跨いでも残ること**
- queue から外れた後に checkpoint で prune されること
- terminal tombstone（配達不能・失敗通知後）が従来どおり即座に削除対象になること
- checkpoint → 255追記 → 再起動 → 1追記で checkpoint が走ること
- checkpoint 直後の再起動では、リクエストだけでは checkpoint が走らないこと
- 旧形式（`Sequence` 先頭または不在）を open しても壊れず、1回の圧縮で新境界へ移行すること
- checkpoint 失敗時に pending カウントを失わないこと
- 保持中のメッセージが多い状態で checkpoint が毎リクエスト走らないこと
- **ID 再利用防止**: `Sequence` を末尾へ移しても `next_id` が後退しないこと

**Optional**

- 実運用での journal サイズと未受領残存数の観測

## Supersedes / updates

| path | provenance | current role | 扱い |
|---|---|---|---|
| 前案（時間 TTL 版・ターン終了 ack 版） | agent-origin | decision evidence | **破棄**（未コミットのため差し替え済み） |
| [0001](0001-conversation-broker-scope.md) の tool 一覧 | agent-origin | decision evidence | **`ack_message` を追加し 3 → 4 tool** |
| [0001](0001-conversation-broker-scope.md) の `list_peers` 仕様 | agent-origin | decision evidence | `pending_to_me`（配達完了済みのみ）と `pending_from_me`（queue 中含む）を追加 |
| `src/journal.rs`（保持判定・発火条件・`Sequence` 位置） | agent-origin | implementation artifact | 変更対象 |
| `src/state.rs`（`Pending`/`Acked`・`messages_for_target`・`remove`・prune） | agent-origin | implementation artifact | 変更対象 |
| `src/daemon.rs`（`read` の非破壊化・ack 経路・`remove_agent`・terminal tombstone） | agent-origin | implementation artifact | 変更対象 |
| `src/daemon.rs` の not-found 文言 | agent-origin | implementation artifact | 「checkpoint 済みの可能性があります」は不正確になるため更新 |
| `docs/design.md` / `README.md`（read は checkpoint まで非破壊、の記述） | agent-origin | descriptive state | exact-conformance update |

`AGENTS.md` の durability 不変条件（送達前 fsync、再起動後の復旧、ID 再利用禁止）は
**変更しない**。本決定は削除の契機だけを変え、耐久性の契約には触れない。

## Readiness

| 条件 | 判定 | 根拠 |
|---|---|---|
| A. Authority closure | **PASS** | user が手順（読む → 作業前に受領報告 → 作業 → 普通の送信で返信）を明示 |
| B. Conflict closure | **PASS** | `AGENTS.md` の durability 不変条件と非競合。[0001](0001-conversation-broker-scope.md) の tool 数変更を更新対象に列挙 |
| C. Product closure | **PASS** | TTL・ターン終了 ack・即削除・無期限保持・集約件数・返信専用 tool を却下理由つきで棄却 |
| D. Risk closure | **PASS** | 報告忘れは両方向の可視化で所有（自動解消はしないと明記）。pane 消滅時の掃除を実装契約として追加 |
| E. Verification closure | **PASS** | 非破壊 read、prune 後の ack 冪等性、queue 中 ack 拒否、宛先境界、fail closed、pane 消滅時の通知語義、両方向可視性を mandatory 化 |
| F. Independent executability | **PASS** | レビュアーが再判定で PASS と判定（会話履歴なしで変更対象・非目標・検証・権限範囲を復元でき、blocking question なし） |

**独立レビュアー（codex, `%6`）の再判定: A〜F すべて PASS。**

判定に至るまでに指摘され、反映した点:

- **「自己修正的」は過大主張だった。** `pending_from_me` は送り手にしか見えず、
  受け手が中断・再起動して ID を失うと本文が残っていても再発見できない。
  `pending_to_me` を追加し、記述も「自動解消はしない」に訂正した
- **checkpoint 後の ack 再送**: `Acked` は prune されるため「既に受領済み」と
  「存在しない」を区別できない。未知 ID は mutation なしの冪等成功と契約化した
- **queue 中の ack**: ID を推測して配達前に ack されると、送り手からは消えるのに
  呼び鈴だけ届き `read` が not-found になる。配達未完了の ack を拒否と固定した
- **pane 掃除の語義**: 読了を記録しない以上「未読の Pending」は判定できない。
  対象を「未受領の Pending 全件」と定義し、通知文も受領報告の欠如を表す文言へ変更した
- **wire tag**: `Consumed` 維持は妥当と確認。コード名と製品用語のずれを固定する指示を追加
