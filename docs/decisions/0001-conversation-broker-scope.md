# 0001. agent-talkd を会話ブローカへ縮小し、agent 窓口を MCP に一本化する

- Status: accepted
- Date: 2026-07-31
- Independent review: codex (`%6`) — 再判定で **A〜F すべて PASS**

## 要約（決裁者はここだけ読めば足りる）

**agent-talkd は「tmux 上の agent 同士の会話ブローカ」だけになる。
agent はコマンドを覚えず、MCP の道具を4つ使う。**

| 問い | 答え |
|---|---|
| 何を作るか | `agent-talk-mcp` という小さい実行ファイル1個 |
| agent に何が見えるか | `list_peers` / `send_message` / `read_message` / `ack_message` の**4つだけ** |
| なぜ `--help` 連発が止まるか | MCP は道具の一覧と引数仕様を毎ターン自動で渡す。調べる対象が消える |
| なぜ skill が要らなくなるか | 道具が常設になるので、使い方を教える文書が不要 |
| `/agent-talk` は | 打たなくてよくなる。agent が必要と判断した時に自分で呼ぶ |
| **Web 画面・HTTP サーバ** | **ごっそり削除**（`client/` 一式、HTTP-over-UDS、`/v1/*` 全部） |
| **スマホからの会話（外部 mailbox）** | **削除**（`--from` / `mailbox-list-v1` / `reply`） |
| スマホからの操作は今後どうなるか | SSH を別口として使う**専用アプリ**の領域。本 repo の範囲外 |
| 常駐プロセス | **残る**。tmux 監視・queue・journal・呼び鈴は他で代替できない |
| 呼び鈴（割り込み通知） | **tmux のまま**。MCP は agent のターンを起こせないため |
| いま user が決めることは | **無い** |
| 次の一歩 | Phase 1 = MCP バイナリ追加 + 受領報告方式（[0002](0002-message-retention-ack.md)）を同時に実装 |

**消えるもの**: Web 画面（`client/` 一式）、HTTP-over-UDS アダプタと `/v1/*`、
外部 mailbox（`--from` / `mailbox-list-v1` / `reply`）、`--skill`、
CLI の会話コマンド、`agent-talk` skill、`agent-talk-peer` dispatcher、`gc` / `watch`。

**残るもの**: 常駐プロセス（登録・busy/idle・queue・journal・呼び鈴）と、
hooks と wrapper が呼ぶ配管コマンド（agent からは見えなくなる）。

最終形は「**tmux 上の agent 同士を繋ぐだけの常駐プロセス + MCP の道具4つ**」。
それ以外は全部無くなる。

以降は実装者向けの詳細。

## Decision

agent-talkd を「同一 tmux server 上の agent 同士が会話するためのブローカ」に縮小する。
**agent が触る窓口は MCP tool 4 個だけ**とし、CLI は hooks と wrapper が呼ぶ機械の配管に
降格させる。skill の受け渡し（`--skill`）は廃止する。

常駐プロセスは選択肢ではなく前提である。tmux 監視・登録状態・queue・journal・busy/idle・
doorbell はセッションを跨いで生き続ける必要があり、MCP server（セッションごとに spawn され
一緒に死ぬ）も Web server（常駐プロセスの別名）もこれを代替しない。
決めるべきは「常駐プロセスをどう置くか」ではなく「**agent が触る窓口を何にするか**」であり、
その答えが MCP である。

### 最終形

```
[claude / codex / cursor の pane]
  └─ MCP (stdio): list_peers / send_message / read_message / ack_message  ← agent が触る唯一の面
       └─ Unix domain socket（TMUX の名前 + XDG_RUNTIME_DIR（無ければ HOME fallback）から
            daemon と同一規則で導出。subprocess を起動しない）
            └─ agent-talkd（常駐・tmux server ごとに1つ）
                 ├ 登録 / busy・idle / queue / journal
                 └ doorbell: tmux send-keys（MCP では代替不能）

[hooks / zsh wrapper]  ← 機械だけが呼ぶ。agent は読まない
  └─ agent-talk register | unregister | busy | idle | turn-end | run | ensure-daemon
```

### MCP tool（agent が見る唯一の面）

| tool | 引数 | 返り値 |
|---|---|---|
| `list_peers` | なし | 登録 agent の一覧（name / state / location / pane / cwd） |
| `send_message` | `to`, `body`, `no_reply?` | `{id, path: "sent"｜"queued"}` |
| `read_message` | `id` | `{from, reply_to, body}` |
| `ack_message` | `id` | 受領報告。[0002](0002-message-retention-ack.md) で追加 |

`resolve` は `send_message` の内部へ吸収する（宛先解決は tool として露出しない）。
外部 mailbox を廃止するため、`reply_message` は**作らない**。
peer への返信は `send_message` で足りる。

`list_peers` は未受領の message ID を**両方向**返す（[0002](0002-message-retention-ack.md)）。
`pending_to_me`（自分宛で未受領。**queue 中を含む**）と、peer ごとの `pending_from_me`
（自分が送って未受領。queue 中を含む）。本文は含めない。
宛先本人の未配達 `read_message` は pull 配達（[0002](0002-message-retention-ack.md) Amendment）。
未配達の `ack_message` は拒否する。

schema に `skill` / `from` / `pane` は**存在しない**。存在しない引数は誤用も偽装もできない。
呼び出し元 identity は adapter が spawn 時の `TMUX_PANE` から導出し、agent は触れない。

### 起動時の contract（premise 3 の帰結）

1. Phase 2 のランタイム設定で **`TMUX` / `TMUX_PANE` / `XDG_RUNTIME_DIR` の3つ**を
   明示 forward する（codex は `env_vars` 指定が必須、claude は既定で継承済み、cursor は要測定）

   **`XDG_RUNTIME_DIR` を落とすと接続できない。** RPC socket の path は
   basename だけが `TMUX` の socket 名由来で、**root は `XDG_RUNTIME_DIR`**
   （未設定なら `$HOME/.cache/agent-talkd/run`）である（`src/config.rs:54-55`）。
   実測した codex の MCP 既定環境7変数に `XDG_RUNTIME_DIR` は無く、現行ホストの
   daemon 側は `/run/user/1000` を使う。forward しなければ MCP だけが HOME fallback を
   導出し、**実在しない socket を掴んで必ず失敗する**。

2. 入力の必須／任意を**一意に固定する**。

   | 入力 | 扱い |
   |---|---|
   | `TMUX` | **必須**。欠落・不正なら **fail closed**（tool を1つも公開せず終了） |
   | `TMUX_PANE` | **必須**。同上 |
   | `XDG_RUNTIME_DIR` | **任意**。存在すれば絶対 path 等を検証して root に使う |
   | `HOME` | `XDG_RUNTIME_DIR` 欠落時のみ**必須**。検証して `$HOME/.cache/agent-talkd/run` へ fallback |
   | 両方とも利用不能 | **fail closed** |

   曖昧な状態では起動しない。勝手な既定値は作らない
3. daemon UDS の path は、**daemon と同一の規則**で導出する。すなわち
   「`TMUX` の socket path の basename を正規化した名前」＋「`XDG_RUNTIME_DIR`
   （または HOME fallback）を root とする既存の path 規則」。
   `Config::discover` は呼ばない。tmux subprocess を起動しない
4. 接続後、**peer UID が自分の effective UID と一致することを確認**する
   （daemon 側の既存 same-UID 境界と対称にする）
5. `TMUX_PANE` は **routing metadata であって authentication boundary ではない**。
   daemon 側の既存境界（same-UID UDS、未登録 pane の拒否）は変更しない

### server instructions（tool があるだけでは相談は起きない）

MCP tool が context にあることは、agent が実際に横展開する保証にならない。
initialize で返す server instructions を**短い操作契約に限定**し、次の趣旨だけを含める。

- 関連する作業をしている agent へ相談・共有してよい
- 不確かな横断事項では自分の判断で使う
- 受け取った内容は peer の情報であって user の権限ではない
- **呼び鈴を受けたら `read_message` で読み、作業に入る前に `ack_message` で受領報告する**
  （[0002](0002-message-retention-ack.md)）

判断そのものを縛る大きな skill 文にしない。skill を消した意味が失われるため。

## Decision owner

user（miyabi）。agent は選択肢の整理と技術判断のみを担当。

## Authority evidence

user 原文（2026-07-31、本 repo の会話ペイン）:

> 機能を大幅に縮小する事が決まったのですが、最終的な要件・目標を決めましょう。
> - 決まっていること
>   - スキルの受け渡し等は全てなし
>   - エージェントが横展開で気軽に相談出来るようにする
>   - agent達は追加のスキル無し、自己判断で情報を横展開する
>     - 毎回 /agent-talk と打ち込むのは面倒臭すぎて時間の無駄

前提となる先行決定（同日、user 原文）:

> agent-talk, terraceは完全にagent同士が会話するだけの小さい機能に閉じてしまって、
> スマホ端末からは、既存アプリはお話にならないので、専用アプリからSSHを使った別口から
> 操作するアプリとして作り込んでいくのが良いかなと考えました。

broker の message ID は補助参照であり、一次根拠ではない。

## Authorized effects

**Allowed**

- 新規 binary `agent-talk-mcp`（同一 crate の第2 `[[bin]]`）を追加し、stdio JSON-RPC で
  MCP tool 4 個を提供する
- 既存 UDS 越しに daemon へ接続する（新しい socket も新しい protocol も作らない）
- `--skill` とその設定（`@agent_talkd_skill_syntax` / `@agent_talkd_allowed_skills`）を削除する
- CLI から会話動詞（who / send / read / reply）を段階的に削除する
- doorbell 文言を MCP 前提へ変更する
- `gc` / `watch` を削除する
- **Web 画面と HTTP 面を一式削除する**: `client/`、`build.rs` の資産埋め込み、
  HTTP-over-UDS listener、`/v1/*` route、`http_socket`、CI の frontend job、
  `DESIGN.md`、関連依存（hyper / hyper-util / http-body-util / bytes）
- **外部 mailbox を削除する**: `--from`、`mailbox-list-v1`、`reply`、
  `@agent_talkd_allowed_sources`、mailbox event の state / journal / retention

**Forbidden**

- TCP listener、tailnet 連携、Funnel / Serve、HTTP write route の追加
- 新しい認証機構・token・socket 分割の追加（same-UID 境界は現状維持）
- `TMUX_PANE` など自己申告 metadata を認証根拠へ昇格させること
- Web / HTTP / mailbox の**再導入**（削除後に「便利だから」で戻さない）
- **MCP server に、会話以外の能力を持たせること。** file 読み書き・任意 path 指定・
  subprocess 実行・shell 経由の呼び出しを tool にも実装にも持ち込まない（理由は premise 5）
- **MCP server が `Config::discover` を呼ぶこと。** 現行実装は `TMUX` 不在時に
  `tmux display-message` を、tmux option 読み出しに `tmux show-option` を
  subprocess 起動する。MCP からこれを呼ぶと上記の禁止に抵触する
- **接続先 socket を tool 引数や任意の環境変数から受け取ること。**
  `AGENT_TALK_RPC_SOCKET` を production 設定で受け取らない。
  test override を設ける場合も `cfg(test)` か test harness 内に閉じる
- 内部コマンド（hooks / wrapper / lifecycle 系）の**物理削除**。help から隠すまでが本決定の範囲

**接続先の positive 定義**（検査可能な形にするため、禁止の裏返しではなく許可を列挙する）

MCP server が接続してよいのは、**forward された `TMUX` と `XDG_RUNTIME_DIR`（欠落時は
daemon と同じ HOME fallback）から、daemon と同一の規則で導出した agent-talkd の UDS ただ1つ**。
導出に使う入力は形式検証し、接続後に peer UID の一致を確認する。
TCP、他の UDS、shell、subprocess、tool 引数由来の path は一切なし。

## Rejected alternatives

| 案 | 却下理由 |
|---|---|
| **Web サーバを agent の窓口にする** | agent が endpoint を知る必要があり、`--help` 相当の学習コストが CLI と同じだけ残る。加えて network 露出が復活し、直前に中止した Port-3 の議論を蒸し返す |
| **CLI を維持したまま skill で使い方を教える** | user が明示的に否定（「追加のスキル無し」「毎回 /agent-talk は時間の無駄」） |
| **CLI を 5 動詞へ縮小して agent に使わせる** | 縮小しても agent は使い方を読む必要があり、`--help` 発行の根本原因が残る。MCP なら schema が自動で context に入るため学習が不要 |
| **常駐プロセスを廃し MCP server に状態を持たせる** | MCP server は session ごとに spawn / 終了するため、tmux 監視・queue・journal・跨セッション配達を持てない。技術的に不成立 |
| **Web 画面で agent 同士の会話を閲覧できるようにする**（`client/` の維持） | user の UX 評価により却下。原文: 「エージェント同士の会話の流れを後から全て見張って評価するみたいな需要はパブリックのプロジェクトとしてはありかもしれませんが、出先からそれを読んでも対処も何もないし、スクロールして流れを遡れるみたいな事も出来ないので、UXは最悪だろうなというか、これで何すればええねんと思います」。**読めても行動できない画面は機能ではない** |
| **外部 mailbox を互換機能として残す** | 唯一の利用者である agent-terrace を閉じるため、残しても producer / consumer が存在しない孤児 API になる（`agent-terrace/src/lib.rs:506,603` が `mailbox-list-v1` と `--from` の唯一の呼び出し元であることを確認済み）。将来の専用アプリは SSH 経由であり mailbox を使わない |

## Non-goals

- モバイルからの操作体験そのものの改善。SSH を別口とする専用アプリの領域であり、本 repo の範囲外
- 削除した機能の代替提供。会話ログの閲覧手段も、外部からの投稿手段も、本 repo では持たない
- doorbell を MCP へ移すこと。MCP はサーバから agent のターンを開始できないため構造的に不能

## Premises

崩れた場合、本決定を再開する。

1. ~~codex の workspace-write sandbox 下で stdio MCP server が UDS へ接続できるか~~
   → **検証済み・成立**（codex による実機確認、2026-07-31）。
   codex-cli 0.146.0 では local stdio MCP process は agent の exec sandbox 内ではなく
   **orchestrator の直接の子**として spawn される。証拠: (a) sandbox 経由の
   `agent-talk who` は Read-only file system (os error 30) で失敗、(b) 同一 turn の
   stdio MCP から `/proc/net/unix` を読むと agent-talkd の listener が見えるが、
   sandbox 側からは 0 件で namespace が実際に異なる、(c) tag `rust-v0.146.0` の
   `codex-rs/rmcp-client/src/stdio_server_launcher.rs` が local server を
   orchestrator の子として直接 spawn すると明記（remote MCP のみ executor API 経由）。
   ただし**実 RPC を通した確認はまだ無い**ため、統合テストで hello / list_peers の
   実接続を mandatory とする
2. 3 ランタイムとも stdio MCP server をサポートする（claude / codex は設定実績あり、
   cursor は未確認）
3. ~~MCP server は spawn 時の環境変数を継承し、`TMUX_PANE` から pane を導出できる~~
   → **既定では偽。ランタイムによって異なる**（2026-07-31、稼働中プロセスの
   `/proc/<pid>/environ` を直接読んで実測）。

   | runtime | MCP プロセスの環境変数 | `TMUX` / `TMUX_PANE` |
   |---|---|---|
   | codex 0.146.0 | **7 個のみ**（HOME / LANG / LOGNAME / PATH / SHELL / TERM / USER） | **無し** |
   | claude | 74 個（親の環境を継承） | **有り**（`TMUX_PANE=%38` 等） |
   | cursor | 未測定 | 未確認 |

   codex は `create_env_for_mcp_server` が `env_clear()` した上で DEFAULT_ENV_VARS と
   設定の `env_vars` だけを渡す（`codex-rs/rmcp-client/src/utils.rs`, tag `rust-v0.146.0`）。
   実測値はこの実装と完全に一致する。

   **書き換え後の premise 3**: 3 ランタイムとも、設定で `TMUX` / `TMUX_PANE` と、
   runtime root を決める `XDG_RUNTIME_DIR`（または fallback 用の `HOME`）を forward できる。
   codex は `env_vars = ["TMUX", "TMUX_PANE", "XDG_RUNTIME_DIR"]` の指定が**必須**
   （`HOME` は既定7変数に含まれるため fallback は成立する）、
   claude は既定で継承済み、cursor は Phase 3 前の mandatory gate で測定する。
4. ~~user はモバイル会話経路（外部 mailbox）を当面必要としている~~
   → **偽。撤回**（2026-08-01、user の UX 評価による）。
   会話ログを外から読めても行動できず、遡ることもできないため価値がない。
   モバイル体験は SSH 経由の専用アプリで別途解決する
5. **MCP server は agent の exec sandbox の外で動く**（premise 1 の裏返し）。
   したがって MCP server は agent 自身の shell より広い権限を持つ。
   会話以外の能力（file / path / subprocess / 任意 socket）を1つでも持たせると、
   **agent が自分の sandbox を迂回する経路になる**。tool surface を会話に限定することが
   この premise 下での必須条件であり、forbidden effects に明記した

## 段階（各 phase は独立に deliver 可能・前 phase を壊さない）

| Phase | 内容 | 壊すもの |
|---|---|---|
| **1** | `agent-talk-mcp` bin 追加（4 tool）**＋ [0002](0002-message-retention-ack.md) の daemon 側変更**（ack RPC、`read` 非破壊化、state / journal / checkpoint）。既存 CLI・skill・dispatcher は並存 | 既存 CLI の**コマンド面と send / queue / lifecycle 挙動**は維持。**保持と再読の挙動は [0002](0002-message-retention-ack.md) のとおり意図的に変わる** |
| **2** | 3 ランタイムの MCP 設定、doorbell 文言変更、`agent-talk` skill 削除 | skill 経由の運用 |
| **3** | CLI の会話動詞（who/send/read/reply）を削除、残る内部コマンドを help から隠す、`agent-talk-peer` dispatcher 退役、`--skill` 削除 | 旧 CLI 利用者 |
| **4** | **Web / HTTP 面の一括削除**（`client/`、`build.rs`、HTTP listener、`/v1/*`、`http_socket`、CI frontend job、`DESIGN.md`、hyper 系依存）と**外部 mailbox の削除**（`--from`、`mailbox-list-v1`、`reply`、`allowed_sources`、mailbox の state / journal / retention）、`gc` / `watch` の物理削除 | mailbox 経由の外部投稿、Web 閲覧 |

**Phase 1 は「純増」ではない。** `ack_message` は daemon 側の保持規則の変更なしには実装できず、
[0002](0002-message-retention-ack.md) が「MCP と同時に導入」と定めているため、両者は同一 Phase になる。
リスク低減の範囲も限定する。**既存 CLI のコマンド面・`send` / queue / lifecycle の挙動は維持**するが、
**保持と再読の挙動は [0002](0002-message-retention-ack.md) のとおり意図的に変わる**
（`read` が checkpoint 後も再読可能になり、not-found 文言も変わる）。
したがって「既存の統合テストが無改変で緑」は成立しない。
旧い保持挙動を符号化したテストは [0002](0002-message-retention-ack.md) に合わせて更新し、
**それ以外の既存テストが無改変で緑であること**を non-regression 証拠とする。

内部コマンドは Phase 3 で**隠すだけ**にする。agent が CLI を一切見なくなれば
「15 個から選ばされる」問題は解消しており、物理削除は UX 要件ではなく別の cleanup。

**Phase 3 に進む前の mandatory gate**: cursor の MCP 動作と環境変数 forward を実測する。
成立すれば Phase 3 で dispatcher を全面退役させる。
成立しなければ **cursor についてのみ Phase 3 を延期し dispatcher を残す**
（claude / codex は予定どおり進める）。cursor を製品対象外とする判断も可だが、
その場合は本記録を更新すること。

## Verification

**Mandatory**

- 実 tmux（隔離 socket）で MCP tool 経由の `send_message` → doorbell 着弾 →
  `read_message` → 相手からの `send_message` による返信、が往復すること
- **MCP server から daemon UDS への実 RPC 接続**（premise 1 は実 RPC 未確認のため、
  `hello` / `list_peers` の実接続を統合テストで通すこと）
- busy 中の送信が queue され、turn-end で doorbell が鳴ること
- `tools/list` の schema 全体を直列化し、`skill` / `from` / `pane` が出現しないこと
- MCP server が file / path / subprocess / 任意 socket の tool を持たないこと
  （premise 5。schema snapshot と source scan で固定）
- **`TMUX` / `TMUX_PANE` の未 forward で MCP が fail closed になること**
- **`TMUX` / `TMUX_PANE` が不正形式（空・書式違反）でも fail closed になること**
- **接続先 UDS の peer UID が自分の effective UID と一致しない場合に拒否すること**
  （実 UID を用意できない環境では、credential 判定関数の単体テストを mandatory の実行形とする）
- runtime root の4経路: (a) `XDG_RUNTIME_DIR` ありで runtime socket へ接続、
  (b) `XDG_RUNTIME_DIR` なし + `HOME` ありで fallback path へ接続、
  (c) `XDG_RUNTIME_DIR` が不正（相対 path 等）で fail closed、
  (d) `XDG_RUNTIME_DIR` も `HOME` も無しで fail closed
  （初期化または tool call が確実に失敗し、誤った pane へ配達しない）
- **明示 forward 時に、正しい tmux server / pane で hello → list_peers → send → read が通ること**
- 別 pane 文字列・未登録 pane を daemon が拒否する既存境界が変わらないこと
- **MCP プロセスツリーに tmux 等の subprocess が生じないこと**
  （source scan で `Command` 不使用を固定、または実プロセスツリーで確認）
- 既定で TCP listener を持たないこと（`TcpListener` の不在を source scan で固定）
- Phase 4 後、**HTTP 面が一切残らないこと**（`hyper` 系依存・`/v1` route・`http_socket` の
  不在を source scan と `Cargo.toml` で固定。socket が作られないことを実 tmux で確認）
- Phase 4 後、**mailbox 面が一切残らないこと**（`--from` / `mailbox-list-v1` / `reply` が
  受理されず、journal に mailbox record 種別が残らないこと）
- Phase 4 の削除後も、agent 同士の会話（send → doorbell → read）と hooks の
  lifecycle が無傷であること
- Phase 3 で会話動詞を削除した後も、hooks（register / busy / idle / turn-end）と
  `run` wrapper と TPM の `ensure-daemon` が動作すること

**Optional**

（cursor の実測は Phase 3 の mandatory gate へ移動。上記「段階」を参照）
- MCP server 経由の同時接続数・レイテンシ測定（実測で問題が出るまで不要）

## Supersedes / updates

| path | provenance | current role | 扱い |
|---|---|---|---|
| `AGENTS.md:7-8`（no network service） | agent-origin | **binding instruction** | **維持**。本決定と一致する。変更不要 |
| `docs/design.md`（no TCP / same-UID / transport bridge 別 scope） | agent-origin | descriptive state | 維持。Port-3 gate 前提の記述のみ更新 |
| `README.md`（CLI 一覧・`--skill`・HTTP API・mailbox の説明） | agent-origin | descriptive state | Phase 3-4 で exact-conformance update |
| `src/help.rs`（22 コマンドの help 表） | agent-origin | implementation artifact | Phase 3 で会話動詞を削除、Phase 4 で mailbox 系を削除 |
| `client/`, `DESIGN.md`, `build.rs` | agent-origin | implementation artifact | **Phase 4 で削除** |
| `docs/decisions/` の先行記録（read-only observation API / status API と embedded client） | agent-origin | decision evidence | **superseded**。Web 閲覧面を製品方向とする前提が撤回されたため |
| Port-3a / Port-3b の計画 | agent-origin | decision evidence | **cancelled**（既出） |
| `~/.dotfiles/.../skills/agent-talk/SKILL.md` | agent-origin | binding instruction（別 repo） | Phase 2 で削除。**本 repo の権限外**、user 作業 |
| Port-3a / Port-3b の計画（planning contract） | agent-origin | decision evidence | **cancelled**。remote write の導入予定を取り消す |

`AGENTS.md` は変更しない。これは今回の縮小方針と一致しており、
**過去に実装を止めた文が、結果として正しかった**ことによる。

## Reopen triggers

- premise 1〜4 のいずれかが偽と判明した
- MCP を採っても agent が使い方を訊く事象が実測で継続した（本決定の目的が未達）
- モバイル専用アプリの要件が外部 mailbox の仕様変更を要求した
- 権限範囲外の変更が必要になった

「不安がある」「より強い hardening を思いついた」「却下済み案の再提示」は再開理由にしない。

## Readiness

| 条件 | 判定 | 根拠 |
|---|---|---|
| A. Authority closure | **PASS** | user 原文で縮小方針・skill 廃止・追加 skill 無しが明示。Web / mailbox の削除も user の縮小方針と UX 評価（Authority evidence 記載）に基づく |
| B. Conflict closure | **PASS** | `AGENTS.md` の binding instruction と一致。descriptive state の更新対象を path 単位で列挙済み |
| C. Product closure | **PASS** | 窓口 = MCP を採用。Web / HTTP / 外部 mailbox は user の UX 評価により削除確定。据え置きは無し |
| D. Risk closure | **PASS** | premise 1 は実機検証で解消。premise 3 は実測で偽と判明し、env forward contract と fail-closed 検証で所有。裏返しの premise 5（sandbox 外実行）を forbidden effects で所有 |
| E. Verification closure | **PASS** | mandatory / optional を分類。実 RPC 接続と tool surface 限定を mandatory 化 |
| F. Independent executability | **PASS** | レビュアーが再判定で PASS と判定（会話履歴なしで変更対象・非目標・権限範囲・段階・停止 gate・検証方法を 0001/0002 と repository から復元でき、blocking question なし） |

**独立レビュアー（codex, `%6`）の最終判定: A〜F すべて PASS。両 ADR は Implementation Ready。**

判定に至るまでに指摘され、反映した点:

- **premise 3 が既定で偽である blocker を検出**。codex が source から指摘し、
  こちらが `/proc/<pid>/environ` の直接読み取りで確認。claude 側は 74 変数を継承し
  `TMUX_PANE` を持つが、codex 側は 7 変数のみで持たない、というランタイム間の非対称も判明
- `Config::discover` の tmux subprocess が forbidden effect と衝突する点を検出
- 接続先を positive 形（forward 済み `TMUX` からの純粋導出 UDS のみ）へ変更
- ASCII 図の tool 名が旧称のまま残っていた機械的矛盾を修正

- tool 命名を `list_peers` / `send_message` / `read_message` に変更し、
  `resolve` を `send_message` 内部へ吸収（`reply_message` は mailbox 廃止に伴い不採用）
- 内部コマンドは物理削除せず help から隠すに留める
- premise 1 を実機検証で解消し、裏返しの premise 5 を新設
- server instructions の必要性（tool があるだけでは横展開は起きない）を追記

### 訂正記録（2026-08-01）

当初この記録は外部 mailbox と `client/` を「当面維持」の非目標として据え置いていた。
**これは誤りだった。** user は先行して
「agent-talk, terrace は完全に agent 同士が会話するだけの小さい機能に閉じてしまって」
と述べており、維持はその決定と矛盾していた。

codex は当該箇所を Authority gap として user への確認を推奨したが、こちらが
「維持＝無操作なので権限不要」として退けた。**指摘の側が正しかった。**
無操作かどうかではなく、**既に述べられた決定と矛盾しないか**が判定軸だった。

user の UX 評価（読めても行動できない、遡れない）と、agent-terrace が外部 mailbox の
唯一の利用者であるという構造的事実により、両方とも削除で確定した。


---

## 追記 (2026-08-05): GET 専用の部分的撤回

本決定の「HTTP は GET 専用 (POST は 405)」は、user 指示により **`POST /api/letters`
1 route に限り撤回**された。旧 agent-terrace の手紙投函 (置換時に受容された機能後退、
home-server README の TODO として記録) の回収である。

### Authority evidence (user 原文、2026-08-05、本 repo の会話ペイン)

> http管理画面の改修をお願いします
> - 手紙を出す機能を復活させる

(agent-terrace 置換時の受容記録: 「いつの間に……agent-terrace相当の機能は出来るように
します。一旦todoに放り込んでおいて、作業を続けてください」— 2026-08-03)

### 契約

- 手紙は既存の外部 mailbox 送信経路 (`send --from` と同一の allowlist 判定・resolve・
  journal-first 永続化・配達/requeue) をそのまま通り、新しい送信実装を持たない。
- source の許可は `@agent_talkd_allowed_sources` (既定 `mobile`) が最終判定する。
- Content-Type は `application/json` のみ・CORS header なし (cross-site simple
  request の遮断)。**process は認証を追加しない。TCP 面は `AGENT_TALK_HTTP_ADDR` を
  明示設定したときだけ開き (既定 off)、その到達範囲は operator 所有の VPN/LAN 境界が
  定める** — agent-terrace の同形式の運用が先行事例だが、現在の裁定の根拠は上記の
  user 原文である。
- **他の書き込み route は引き続き非目標** — GET 専用の原則はこの1 route を除いて有効。
