# 会話履歴の取得責務を Herdr に集約するための契約

状態: **契約と不足機能の調査完了、実移譲は未実装**。2026-09-07、agent-talk
0.14.8 / Herdr 0.8.2（protocol 20、schema_version 1）で確認した。
現在の会話表示、HOME の必須条件、native JSONL reader は維持している。
この文書は Herdr に未提供のコマンドがあると仮定した実装仕様ではない。

利用者が求める到達点は、agent-talk を Herdr への窓口にし、native 履歴の所在解決・
解析・ページ取得を Herdr 側の一つの所有者へ移すこと。Codex / Claude Code は
引き続き原本の保存を所有する。別 DB、複製ログ、agent-talk 専用 helper、独自
セッション管理は作らない。従来の「agent-talk が native transcript を読む」という
構成は現状の説明であり、この変更を禁止する規則ではない。

## 確認した提供範囲

installed binary のバージョン、CLI help、API schema と同版の公式文書を照合した。
再確認には次のコマンドを使う。

```sh
herdr --version
herdr agent read --help
herdr api schema --json
```

| 現在の操作 | 提供するもの | 会話取得に不足するもの |
| --- | --- | --- |
| `agent list` / `agent get` | pane / terminal と `agent_session {source, agent, kind, value}`。kind は `id` / `path` | native ID を履歴へ解決した結果、本文、ページ |
| `agent read` / `pane read` | visible / recent / recent-unwrapped / detection の端末テキスト | user / assistant の区別、過去の報告、native 履歴のカーソル |
| `api snapshot` | live workspace / tab / pane / agent と session identity | native 会話本文 |
| native agent session restore | integration が登録した ID を使う `codex resume <id>` / `claude --resume <id>` による再起動 | 既存プロセスを変えずに会話だけを読む操作 |

schema の全 request method と success response に native 会話ページの取得操作は
ない。`ReadSource` にも transcript はない。端末の保存履歴も会話履歴とは別で、
有効化しても今回の代替にはならない。CLI の resume を agent-talk が起動することは
セッションへの作用を持つため、読み取り API の代用にはしない。

一次情報:

- [Herdr 0.8.2 Socket API](https://github.com/herdrdev/herdr/blob/v0.8.2/docs/next/website/src/content/docs/socket-api.mdx)
- [Herdr 0.8.2 session state and restore](https://github.com/herdrdev/herdr/blob/v0.8.2/docs/next/website/src/content/docs/session-state.mdx)
- [Herdr stable documentation index](https://herdr.dev/llms.txt)（調査時の stable は 0.8.2）

## 移譲先と不足機能

移譲先の候補は [Herdr 本体](https://github.com/herdrdev/herdr) の native session
integration と read-only API / CLI。Herdr が知る生きた宛先と native reference を
起点に、そのホストの原本を読む。ハーネス固有の保存場所・設定・フォーマットを
知る取得処理をここに集約する。ハーネスが利用可能な read-only API を持つ場合は、
Herdr の取得実装がその採否を決める。agent-talk にハーネス別の分岐を残さない。

必要なのは単なる `transcript_path` の追加ではない。path だけなら app の探索は
減らせても JSONL 解析・ページングの独自所有が残る。任意 path をブラウザから
渡す API も設けない。Herdr 側は登録された native reference と元セッションを
照合し、その reference の履歴だけを返す。

現時点では管理対象の Herdr checkout と、その変更・配布を担当する範囲が未確定。
本 repo 内だけで不足機能を実装して移譲済みとは扱えない。以下は依存先へ渡す
受入契約であり、コマンド名・wire schema・対応最低バージョンは未確定である。

## 窓口に必要な受入契約

| 項目 | 必要な挙動 |
| --- | --- |
| 対象 | pane と期待する terminal / native session / process の実体を指定・照合する。入替は明示エラーにし、他の会話を返さない。app 固有の hash を Herdr が理解する前提にはしない |
| 操作 | read-only の latest / before / after。before と after は排他。既存 CLI への入力、resume、新セッション作成を伴わない |
| 本文 | 同じ native 会話の user / assistant テキストを時系列順で返す。原文の改行・空白を保つ。tool 内部応答、推論、sidechain、重複 event は表示対象から外す。Codex の環境注入は metadata で区別し、人間の同じ文字列を消さない |
| 識別子 | セッションに結び付く安定した message ID、role、text、あれば timestamp。app が response を既存の `Conversation` へ写せること |
| ページ | opaque cursor、older / next cursor、要求方向の続きの有無。追記で重複・取りこぼしを起こさず、書きかけ末尾は次回へ残す |
| 上限 | 原本読み取りは現在の 2 MiB / 500 メッセージ相当以下に制限。巨大・壊れた記録を省略しても前後へ進める。CLI 応答は app の bounded stdout 内に収まるよう JSON の膨張も考慮する |
| 変化・失敗 | cursor の別履歴使用、置換・縮小等の非互換な変化を拒否。未登録、未対応、原本未出現、読取失敗を空の成功履歴と区別する。省略と書きかけ末尾をそれぞれ明示する |
| 保存 | native 原本だけを読む。永続 cache、複製ログ、別会話 DB は不要。探索・解析の制限は取得側が所有する |

既存 HTTP 契約は [README](../README.md#http-api) の `Conversation` と cursor error を
維持できるように変換する。Herdr の cursor は app が内部を解釈せず渡す。
移行で旧 cursor が使えない場合は `cursor_changed` 相当を返し、ブラウザの既存本文を
保持したまま直近へ戻れることを検証する。端末表示への自動 fallback はしない。

## 依存提供後に除去・変更する処理

| 現在の所有箇所 | 移行時の変更 |
| --- | --- |
| `src/config.rs` の `Config.home` / `HOME is required` | app 独自の HOME 読取りと必須条件を削除。OS や子プロセスの HOME は変更しない |
| `src/daemon.rs` の `Console.home` と履歴用 `spawn_blocking` | Herdr CLI への async read と response 変換へ置換 |
| `src/history.rs` の固定 root、`resolve_path`、`find_session` | app から削除。native reference の解決は Herdr が所有 |
| 同ファイルの file identity / cursor proof / bounded record read / `parse_message` | app から削除。取得契約の振る舞いを Herdr 側で保証 |
| `src/daemon.rs` の cursor 検査 | app 所有の hex / inode 形式の解釈を除去し、query 排他・入力長など HTTP の検査を残す |
| `src/herdr.rs` | 提供された CLI のみを argv で呼び、失敗・response identity・サイズを検査。独自 RPC を復活させない |
| `tests/remote.rs` と history unit tests | app は Herdr CLI fixture で変換と失敗時の表示契約を検証。native 解析ケースは移譲先の回帰検証へ引き継ぐ |
| README / AGENTS / 内部設計 | 配布済み Herdr の契約と最低版を記載し、現状の HOME / native reader 説明を置換 |

app の宛先選択、HTTP 入力検証、送信前照合、原文の一回送信、画面の前後照合、
ブラウザの Markdown 表示・ページ操作・読み位置保持は引き続き app の仕事。
公開範囲や外部のアクセス制御を変更する機能ではない。

## 対応範囲を決めるための判断材料

推奨する次の範囲は、Herdr 本体で上記取得 API / CLI を実装・配布し、その版を使う
agent-talk adapter へ一度で移行すること。Herdr 側の担当 repo / 配布経路を確定して
着手する。今回追加したものは契約文書だけで、Herdr への機能追加・app reader の削除・
HOME 依存解消はまだ完了していない。

Herdr 本体への追加を今回の範囲に含めない場合は、現 native reader を暫定維持する。
別名 helper、app 内の二重 adapter、専用保存基盤を足す作業には振り替えない。
既存会話を recent screen に置き換える案では、古い報告、role、ページ継続を失うため、
利用者の明示的な機能変更判断なしに進めない。

移行時は、隔離した同一性・ページング・失敗ケースを検証する。
専用の Codex / Claude セッションで原文受領と返答も確認する。
古い会話・新着・再接続・読み位置は Chromium + Playwright で確かめる。
app が HOME なしで起動し、取得先の CLI が
その環境で接続・原本を解決できる構成も実測する。公開 HTTPS の配布確認まで、
依存が未配布なのに実移譲が済んだとは報告しない。
