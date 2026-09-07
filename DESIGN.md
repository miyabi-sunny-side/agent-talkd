---
name: agent-talkd remote messages UI
version: 5
description: >
  Project design authority for the agent-talkd web client. Self-contained:
  everything needed to implement and verify the UI lives in this file.
---

# agent-talkd — Web UI Design Authority

この文書が本 repository の UI 設計の正である。共有テンプレート
`rust-svelte-template` (Sumi / Kinari、参照日 2026-08-08) は bootstrap 入力と
して copy-then-own 済み。以後テンプレート側の変更に自動追従しない。
theme 3択・token・モーダル/メニュー規約は本書が正。

## 1. 目的と範囲

既存の Tailscale / HTTPS 経路から Herdr 内の対象を選び、指示を原文で送り、
同じ CLI session の返答・進捗・完了報告を読むための人間向け操作面。
スマホで端末の記号・Shift/Ctrl 等を操作させず、帰宅後も同じ session で継続する。
Codex / Claude Code を対象とし、その他の CLI は実際の adapter 対応状態を示す。

視覚言語は既存の「墨と和紙」— 墨色の地、和紙色の文字、柿色の accent。
48px app header、compact summary、session 単位カード、状態 border / 文字色、
下部の手紙 dock を継承し、モバイル縦スペースを優先する。
mailbox・skill 選択・エージェント間通信・呼び鈴・ack の操作は置かない。
brush loader は loading 表示として残す。長い native CLI 履歴は直近から開き、
古い会話のページ取得と差分更新で、同じ session の原文を継続して読める。
「画面」は Herdr `pane.read` の visible text を確認する閲覧専用の補助面とする。
窓の画像や端末の完全再現ではなく、取得できた行・空白を読む表示である。

## 2. 画面と URL (router contract)

SPA は History API による手書き router を持つ (`client/src/router.ts`)。
外部 router 依存は追加しない。daemon は未知 GET path を SPA entry へ
fallback するため、サーバ変更は不要。

| URL | 画面 |
|---|---|
| `/` | Registry (agent 一覧) |
| `/letters` | 旧 URL。Registry へ `replaceState` で正規化 |
| `/agent?pane=<id>` | Agent detail (同一 CLI session の会話 / 閲覧専用画面 + 共通の手紙 dock) |

規則:

- pane id は opaque (`%`, `/`, Unicode を含み得る)。必ず URLSearchParams で
  query に載せる。path segment にしない。
- 一覧→詳細は `pushState`。
- 同一 workspace 内の agent タブ切替は `replaceState` (Back はタブ履歴を
  遡らず一覧へ戻る)。
- `popstate` で再 fetch なしに view を復元する。reload は同じ画面を復元する。
  full reload 後の会話は同じ CLI session の直近から再開し、過去の読み位置の
  永続復元は要求しない。取得中は直近を開いていることが分かる表示にする。
- 未知 path は Registry を描画し `replaceState` で `/` へ正規化する。
- deep-link (`/agent?pane=...`) は agents fetch 完了前に not-found 判定しない。
  fetch 成功後も pane が不在なら silent redirect せず、URL を保ったまま
  「この agent は見つかりません」の説明 + 「一覧へ」導線 (quiet-button) を出す。
  fetch 失敗は registry と同型のエラー + 再試行。
- `document.title` は view に追従する (`agent talk · agents` /
  `agent talk · <agent name>`)。

選択中 agent は「URL の pane + 最新 registry snapshot」から導出する。
view/selectedAgent を URL と別に持つ state にしない (単一情報源)。
送信時に固定した CLI session identity は宛先検証用に保持し、registry 更新で
暗黙に別 identity へ差し替えない。

## 3. Domain model

- **agent**: Herdr の name / state / opaque pane_id / workspace label / location /
  cwd / runtime。対応状態と送信できない理由は adapter の実データから表示する。
- **workspace**: 複数 agent をまとめる Herdr の作業領域。詳細主タイトルとタブの
  grouping に使用する。CLI session identity とは区別する。
- **CLI session identity**: 実際の対話セッションを識別する値。送信宛先と履歴と
  draft は pane + この identity に紐付ける。同じ pane の再利用でも別 session へ
  下書きを移したり、自動再送したりしない。
- **message / report**: 対象 CLI の実履歴の user / assistant 本文。役割・時刻
  (取得できた場合) を付けて原文を表示し、AI による要約や別の通信台帳を作らない。
- **送信結果**: API 受理・対象への入力・エージェントの処理・完了は別の事実。
  adapter が確認できた段階だけ表示し、HTTP 成功から処理完了を推測しない。
  進捗・完了の根拠は実際の assistant 報告であり、idle への遷移だけではない。
- **宛先の短い表示**: Herdr の明示名 / タブ名を優先し、runtime 名だけでは区別
  できない対象には作業ディレクトリの末尾等の実データを添える。同名なら親 directory
  等で区別を補う。省略した作業パスは title 等から確認できる。opaque pane / session
  ID は通常の見出し・タブ・フォーム・accessible name に出さず、内部照合に保持する。
- **状態色**: idle = `--idle`、busy = `--busy`。一覧ボタンの border と詳細タブの
  文字色で補強する。accessible name に name と状態を含め、送信可否の理由は
  可視テキストでも説明する。danger は失敗表示専用。

## 4. Theme — 3択制

### 4.1 方針と理由

従来は Sumi (dark) 単一テーマだった。屋外・明所での閲覧と OS 設定追従の期待に
応えるため、**墨 (dark) / 生成り (light) / システム追従** の3択制へ移行する。
Washi (e-paper) は実 e-paper 用途専用であり、通常 screen の選択肢に出さない。

再評価条件: e-paper クライアントを正式サポートする時、または通常 screen の
用途が変化した時に選択肢構成を見直す。

### 4.2 保存と機構

- localStorage key: `agent-talkd:theme`、値は `"dark" | "light" | "system"`。
  「システム」選択時も `"system"` を明示保存する (key は消さない)。
  key 不在・不正値は system と同じ扱い。
- 明示選択時は `document.documentElement` に `data-theme="dark|light"` を設定。
  system 時は `data-theme` 属性を外し、CSS の
  `@media (prefers-color-scheme: ...)` に委ねる。
- token は CSS custom properties で二組定義する:
  既定 `:root` = Sumi、`:root[data-theme="light"]` と
  `@media (prefers-color-scheme: light){ :root:not([data-theme="dark"]) }` =
  Kinari (light block は二箇所同値で重複定義してよい)。
  `color-scheme` プロパティも theme と共に切り替える。
- **theme flash 禁止**: `client/index.html` の `<head>` 冒頭に同期 inline
  script を置き、bundle 読込前に localStorage を読んで `data-theme` を設定する
  (try/catch で localStorage 不能時は system 扱い)。

### 4.3 Token (semantic layer)

実装は下記 semantic 名を正とする (旧 `--ink`/`--paper`/`--kaki` 等は
この名前へ改名する)。

| token | Sumi (dark, 既定) | Kinari (light) |
|---|---|---|
| `--surface` | `#171714` | `#faf6ef` |
| `--surface-raised` | `#1d1d19` | `#fffdf8` |
| `--on-surface` | `#e9e4d8` | `#3a2f28` |
| `--muted` | `#a8a294` | `#6f6257` |
| `--border` | `#3a3933` | `#e3d9c9` |
| `--accent` (柿) | `#d66f3d` | `#a84a17` |
| `--accent-subtle` | `rgba(214,111,61,0.13)` | `rgba(168,74,23,0.10)` |
| `--idle` | `#62b7a5` | `#1f7a66` |
| `--busy` | `#c9a24e` | `#8a6a1c` |
| `--danger` | `#e68269` | `#9c2b1d` |
| `--danger-subtle` | `rgba(230,130,105,0.12)` | `#f9e9e4` |
| `--link` | `#9ec5dd` | `#14506e` |
| `--scrim` | `rgba(0,0,0,0.55)` | `rgba(58,47,40,0.4)` |
| `--terminal-bg` | `#0d0e0c` | `#0d0e0c` (固定) |
| `--terminal-fg` | `#eee9dd` | `#eee9dd` (固定) |

決定と理由:

- **Sumi の解釈**: 共有テンプレートの中立 gray + 金 (#191919/#e0a800) では
  なく、実装済みの墨・和紙・柿 palette を本 Project の正準 Sumi とする。
  理由: 既存画面・brush loader と一体の確立した identity であり、
  テンプレートは bootstrap 入力に過ぎないため。
- **Kinari の accent**: テンプレート既定 #9a6a00 (金) ではなく柿の hue を
  保った焦柿 #a84a17 を採る。理由: brand hue の連続性。cream 地で文字用途
  4.5:1 以上を実測で満たすこと (満たさない場合は hue を保ち明度のみ調整)。
- terminal token は既存資産として保持する。会話・報告本文には通常の
  surface / on-surface を使い、両テーマに追従させる。
- Kinari 固有規則 (テンプレート由来): accent-subtle の控えめ装飾は tint ≤12%
  とし意味は text/shape でも伝える。focus ring は cream 地で ≥3:1 を実測。

### 4.4 テーマ切替 UI

ハンバーガー → dropdown「テーマ設定」→ テンプレート移植の ThemeModal
(Modal + Icon)。3択 radio (icon 付き): **自動 / ライト / ダーク**
(値は `system` / `light` / `dark`)。クリックで即適用 + 即保存し、モーダルは
開いたまま。閉じるのは scrim / × / Escape (§8)。storage key は
`agent-talkd:theme` のまま。

## 5. Typography

- 本文: `"Hiragino Kaku Gothic ProN", "Yu Gothic UI", "Noto Sans CJK JP",
  "Noto Sans JP", system-ui, sans-serif`
- mono (meta/status/terminal): `"SFMono-Regular", Consolas, monospace`
- brand 15px / 見出し 13–15px / 本文 13px /
  meta・status 10px mono letter-spacing 0.08em / terminal 12px (狭幅 11px)

## 6. Layout と spacing

- 一覧の **app header** は `main` の sibling として viewport
  full-bleed (48px sticky)。左右 gutter は詳細 1 段目と同一 token
  (`--chrome-inline-start` / `--chrome-inline-end` =
  `max(12px, safe-area-left)` / `max(8px, safe-area-right)`)。
- 一覧の **本文 `main`**: `width: min(1020px, 100%)`、中央寄せ。
  横 padding `clamp(16px, 5vw, 56px)`。`min-height: calc(100dvh - 48px)`。
  chrome の端位置と本文列の幅は独立 (header を本文 max-width に閉じ込めない)。
- 詳細 (`/agent`): `main` は full-bleed。chrome 2 段 48+40 + hairline 1 =
  outer **89px** と letter dock を除く領域を会話・報告に与える。dock 展開時も
  本文末尾と送信結果を覆わず、会話と composer がそれぞれ scroll できる。
- 横 scroll をページに出さない。最小対応幅 320px。カード内 agent ボタンは wrap。

## 7. Components

### 7.1 App header (Registry)

テンプレート Header を copy-then-own した 48px sticky bar。`main` 外に置き
viewport 全幅。左右 gutter は詳細 `.detail-bar-primary` と共通 token。

- 左: brand `agent talk` (`talk` = accent italic)。SPA navigate 用 button。
  full page reload の `<a href="/">` は使わない。
- 右: ハンバーガー (44×44)。`aria-haspopup="menu"` / `aria-expanded` /
  `aria-label="メニュー"`。
- eyebrow / 角印 / 大型 wordmark は置かない。

### 7.2 Menu (template dropdown) とテーマモーダル

- 右寄せ dropdown (overlay + panel)。項目: Agents / テーマ設定。
- 「テーマ設定」はテーマモーダル (§8) を開く。ラベルは §4.4 のとおり。
- Escape / overlay で閉じ、起点 menu ボタンへ focus 復帰。

### 7.3 Registry (一覧)

- compact summary (≤40px): 見出し「稼働中の agent」+ aria-live 件数。章番号なし。
- agents を backend+session で group し session カード1枚に agent ボタンを並べる
  (優先: claude → codex → grok → 他)。ボタンは registry 実データから動的生成。
- ボタン: 可視は name、状態は 2px border 色、aria-label に state、
  min-height 44px。カードに作業先を表示し、詳細では cwd / runtime を読める。
  pane_id は通常非表示。送信不能でも詳細から理由と取得済み報告を読める。
- loading / empty / error は従来どおり。

### 7.4 詳細ヘッダー (2段)

```
[agent talk (home)]     [session 名 …]     [≡ menu]
[ agent tab · agent tab · … (横 scroll)              ]
```

- 1 段目 48px: brand (`aria-label="agent talk — 一覧へ戻る"`) + session 名
  (Herdr workspace label)。
  session の title/aria-label に省略前の workspace label。opaque ID は含めない。
- 2 段目 40px: 同一 workspace の agent タブを常時表示。active は下線、状態は
  文字色 (idle/busy/退出)。切替は replaceState。tap ≥36px。

### 7.5 会話・報告

- 詳細の主面は選択中の CLI session の履歴。user は「あなた」、assistant は
  「エージェント」と可視ラベルで区別し、本文を通常フォント・テーマ追従で表示する。
  assistant の報告は段落・リスト・強調・インラインコード・コードブロック・リンク・
  表を安全な Markdown として表示する。user の原文は改行と記号を保つ。コード内の
  改行・空白・記号は保持し、コードと表の横 scroll は各領域内に限定する。
  raw HTML は実行せず文字列として扱い、script / event handler や危険な URL scheme
  を実行可能にしない。外部埋め込みは行わない。本文は長い文字列を折り返し、
  ページ取得上限・時系列・読み位置の契約は Markdown 化後も維持する。
- 原文の時系列を維持し、Web から送った内容と端末上の内容を同じ履歴で読む。
  ローカル送信結果を履歴に重複追加しない。実履歴に未反映の間は送信状態欄で示す。
- 初回 loading / 履歴なし / 取得失敗 + 再試行を別表示にする。更新失敗時は取得済み
  本文を保持し「接続が切れています」等の理由を示す。報告がないことを完了としない。
- 更新時に全文を live announcement せず、接続・送信状態を `aria-live="polite"`
  で通知する。読んでいる途中の scroll 位置を更新で末尾へ飛ばさない。
- 初回は直近ページの末尾を表示する。先頭に「古い会話」quiet-button を
  置き、前のページがある場合だけ操作可能にする。取得中はボタン付近で状態を示し
  重複取得を防ぐ。過去取得に失敗しても現在の本文を残し、同じ場所で再試行できる。
  最古まで到達したら「会話の先頭です」と示し、追加取得を止める。
- 過去の履歴は「古い会話」「新しい会話」で明示的に表示ページを切り替える。
  各ページは最大 500 件 / 2MiB とし、ページ単位で表示することを操作付近で説明する。
  古いページを開いたらその末尾を表示し、続きを遡る操作は上端に置く。
  「新しい会話」は次のページへ進み、間の会話を飛ばさずに辿れるようにする。
- 新着差分でも、過去を読んでいる間は同じ本文・読み位置を保持する。
  末尾を追従しているときだけ新着に追従する。本文を重複表示せず、時系列を保つ。
  古いページの閲覧中は新着の有無だけ更新し、本文を最新ページへ入れ替えない。
- 自動追記後の表示・保持は最大 1000 件 / 4MiB とする。上限に達したら既存表示を
  保って「表示上限に達しました。最新へ戻ると続きを読めます」と説明する。
  ページ切替または「最新へ」の明示操作で旧表示を解放する。背景の差分更新で
  読んでいる本文を追い出したり、無断で別ページへ切り替えたりしない。
- 詳細 chrome 直下に高さ 44px の操作バーを確保し、本文の scroll から独立させる。
  共通の「会話 / 画面」選択をここに置く。選択状態は可視の下線と ARIA で示し、
  keyboard で切り替えられる。両操作は高さ 44px を確保する。
  状態と「最新へ戻る」quiet-button はこのバーに置く。状態のないときは
  「会話・報告」と表示する。長い状態・エラー文は高さ 44px 以内の内部 scroll で
  全文へ到達できるようにし、状態変化で本文の開始位置を動かさない。
- 最新から離れている間は、操作バーの「最新へ戻る」を表示する。
  新着があればその旨を短く併記し、上限に達しても新着本文を無制限に
  蓄積しない。押すと同じ session の直近を取得して末尾へ移動する。失敗したら
  元の本文・位置を保って再試行できる。新着なしでも最新へ戻れる。
- 新しい操作は既存の quiet-button、本文・muted・状態色の recipe を再利用する。
  本文や手紙 dock を覆わず、320px 幅でも折り返して到達できること。keyboard
  操作と focus-visible を備え、touch target は 44px 以上。非同期取得で focus を
  奪わず、操作元が消えるページ切替では会話領域へ focus を移して操作を継続する。
- 初期面は会話とし、閲覧専用の「画面」は §7.10 に従う。端末入力・キー送信 UI は
  置かない。承認待ちを含め、スマホで端末キーを再現する操作を要求しない。

### 7.6 手紙 dock — 詳細画面下部

既存の ribbon composer を継承し、宛先は表示中の pane + CLI session に固定する。
会話 / 画面の両面で同じ dock・draft・送信状態を共有し、切替で再初期化しない。

- **tab**: 右寄せ、右 inset `max(10px, safe-area-right)`、`min-width: 108px`、
  `height: 44px`、上角 9px、地は `--surface-raised`。上辺に全幅 1px border。
  封筒 SVG + `手紙` + chevron、`aria-expanded` / `aria-controls` を持つ。
  draft があれば accent 枠と accessible name の「下書きあり」で示す。
- **panel**: 通常は `max-height: min(62dvh, 420px)`、内部 scroll。実際の可視領域が
  小さいときは §10 の高さ制約を優先し、送信・閉じるまで必ず到達できる。閉時は `inert` と
  `aria-hidden="true"`。開くと本文へ focus、閉じる / Escape で tab へ戻す。
- 内容は `{短い宛先表示} へ手紙を出す`、必要な場合の runtime / 作業先、本文 textarea、
  送信ボタン、状態説明。mailbox source・skill picker・独立の授権ゲートは置かない。
- textarea は通常の日本語入力と改行を扱う。明示した「送信」ボタンで原文を送り、
  Enter / IME 確定だけでは送信しない。送信ボタンは min-height 44px。
- 送信中は重複 submit を防ぐ。宛先を切り替えた後に前の送信が完了しても、
  新しい宛先の draft を消さない。本文を後から編集した場合も編集分を消さない。
- draft は pane + CLI session identity ごとに保持し、dock 開閉・タブ切替・
  再読込で復元する。明確な送信成功時だけ送った本文を消す。エラー、終了、再起動、
  通信切断、結果不明では保持する。session が変わった場合は旧 draft を保管して
  再利用を止め、別 session になった説明を表示する。
- 成功は確認できた範囲で「送信を受け付けました」または「対象へ入力しました」と
  表示し、処理完了の印を付けない。結果不明時はその旨を説明し自動再送しない。

### 7.7 対象・接続の状態

| 状態 | 可視説明と操作 |
|---|---|
| 未登録 / session 未特定 | 対象 session を特定できない理由を示し送信不可。draft 保持 |
| unsupported | runtime 名と未対応を示し送信不可。閲覧可能なら履歴を表示 |
| blocked | 承認待ち等の取得済み理由を示し送信不可。キー操作や迂回送信を促さない |
| 終了 / 不在 | 退出を示し、取得済み報告と draft を保持。一覧へ戻れる |
| 再起動 / identity 変更 | 別 session になったことを示し旧宛先への送信を止める。新しい対象は明示的に開き直す |
| 接続切断 / 更新失敗 | 最新状態を確認できない説明と再試行。既存表示・draft を保持 |
| busy | 稼働中と表示。追加指示の可否は adapter の capability に従う |

送信不可の理由はボタン付近に常時読めるテキストで置き、disabled の色だけにしない。
API が返す理由と一致させ、未知の状態を送信可能と推測しない。

### 7.8 Footer

Registry / 詳細のいずれにも site footer は置かない。

### 7.9 App icon (アプリマーク)

installable web app のホーム画面アイコンと favicon に用いるマーク。画面内には
置かない (§7.1 の「eyebrow / 角印 / 大型 wordmark は置かない」は継続する)。

**意匠**: 墨地に白抜きで置いた鉤括弧の対 —「 (和紙色) と 」 (柿色)。対角に配置し、
間の墨地は空けたままにする。文字を使わず多角形2枚だけで構成し、ラスタライズ環境の
フォントに依存しない。

**原本と座標系**: SVG 1枚を原本とし `viewBox="0 0 512 512"`。以下は 512 単位系。

- 地: `0 0 512 512` を `#171714` (Sumi の `--surface`) で塗る。透過部を作らない。
  両テーマで同一の意匠とし、light 用の別版は作らない。
- 「: `#e9e4d8` (Sumi の `--on-surface`)。外角 (96,96)、横画は右へ x=288、
  縦画は下へ y=332。
- 」: `#d66f3d` (Sumi の `--accent`)。「 を中心 (256,256) で 180° 回転した位置
  (外角 (416,416))。
- 画の太さは角で 52、自由端で 46。長辺側で細らせ、端は画に直交して断つ
  (斜めに切らない)。外側の2辺 (「の上辺と左辺) は直線で、角は丸めない。
- 二つの括弧を接触させない。最短間隔 76 以上。
- 地に対する contrast は両画とも 3:1 以上 (§12)。

**版**:

| 版 | purpose | 図形の外接箱 | 用途 |
|---|---|---|---|
| 通常 | `any` | 96–416 (一辺 320 = 全体の 62.5%) | favicon / iOS home screen / 既定 |
| maskable | `maskable` | 116–396 (一辺 280) | Android adaptive icon |

maskable 版は通常版の描画を中心 (256,256) 基準で `scale(0.875)` した1枚とする。
地は 512 全面のままとし、意匠そのものは変えない。

**安全域**: maskable 版は直径 80% (中心 (256,256)・半径 204.8) の円の外へ、地以外の
画素を1つも出さない。通常版はこの制約を負わない (最外角は中心から 226 で円外)。

**保証するサイズ**: 32 / 48 / 192 / 512px。幾何 (画の太さ 52–46、括弧間の最短間隙
76) が正であり、各サイズの画素値はそこから導かれる従属値とする。画素で検査する
ときは次の2語で測る。

- **被覆幅**: 画に直交する走査線上で、その画のインク色に対する被覆率 (地色を 0、
  インク色を 1 とした線形の推定値) を合計した値。
- **可視幅**: 同じ走査線上で被覆率が 0 を超える画素の本数。

- 48px: 各画は被覆幅 4.3px 以上、可視幅 5 画素以上。
- 32px: 各画は被覆幅 2.8px 以上、可視幅 4 画素以上。
- 全サイズ: 二つの括弧が被覆率 0.5 以上の画素で連結しないこと。
- いずれのサイズでも鉤括弧の対として判別できること。

**書き出し**: 原本 SVG から 192px と 512px の PNG を生成し、全画素を不透過にする
(alpha を持たせない)。

**manifest の色**: `background_color` / `theme_color` はいずれも `#171714` とし、
`client/index.html` の `<meta name="theme-color">` と同値に保つ。

### 7.10 閲覧専用の「画面」

- Herdr `pane.read` が返す現在の visible text を情報源とし、「端末テキスト・閲覧専用」
  と明示する。会話履歴から画面を作らず、常駐記録や端末入力を追加しない。
- 既存 terminal token と mono font を使う `pre` 相当で行・空白を保持する。折り返しで
  行配置を変えず、上下左右の scroll は画面領域の内側に閉じる。選択・コピー可能。
  ブラウザで raw HTML / 制御シーケンスを実行しない。テーマ切替でも terminal token
  は §4.3 の固定色を維持し、周囲の chrome と状態説明は通常テーマに従う。
- 取得成功時刻を可視表示し、自動更新の開始時刻で上書きしない。loading / 空の画面 /
  初回失敗 + 再試行を区別する。更新失敗・切断・終了後は取得済み表示と成功時刻を
  残し、「古い表示」と理由を併記する。切替後の再取得前も現在確認済みとは表示しない。
- 画面閲覧は会話履歴の取得成功・送信可否と独立する。native session 未登録 / unsupported / blocked でも、
  Herdr の端末実体を特定できれば閲覧できる。API は pane + terminal identity を必須、
  native CLI session identity を任意とし、native 登録を閲覧の必須条件にしない。
- pane + terminal identity と、指定した場合の CLI session identity を固定して取得する。
  対象変更後の遅延応答は混ぜない。固定した identity の変更では旧画面を古い表示と
  明示して停止し、新しい対象を明示的に開き直す。端末終了時も更新を止める。
  手紙の draft 保護と送信可否は引き続き §7.7 に従う。
- 会話 / 画面切替は対象・draft・会話ページ・読み位置を保持し、戻る履歴を増やさない。
  hidden の本文は focus 対象にしない。非同期更新で操作 focus や画面の scroll 位置を
  奪わず、状態だけ `aria-live="polite"` で伝える。端末全文を live announcement しない。

## 8. モーダル共通規約 (copy-then-own)

- 中央配置・`border-radius: 12px`・`padding: 16px`・地は `--surface-raised`。
- `--scrim` の全面 scrim + `box-shadow: 0 8px 32px rgba(0,0,0,0.25)`。
- 閉じる3経路: scrim click / SVG × の quiet icon button / Escape。
  閉時は開いた元のボタンへ focus 復帰。
- 内部 scroll、`max-height: 80dvh`。
- `role="dialog"` + `aria-modal="true"` + `aria-label`。開時は背後を操作させ
  ない (focus はモーダル内で循環)。
- focus-visible は accent 2px outline + offset 2px。
- accent 塗り (filled) の primary ボタンは1画面1つまで。
- メニュー系モーダルは full-width ボタン縦積み + caption-muted の
  section label。

## 9. Polling contract

| 対象 | 周期 | 条件 |
|---|---|---|
| 選択 session の会話・報告 (`/api/conversation`) | 2s | `/agent` の会話表示中 + document visible。初回は直近、その後は前方差分 |
| 選択端末のテキスト (Herdr `pane.read` 経由) | 2s | `/agent` の画面表示中 + document visible + 同じ生存 terminal identity。native 登録・送信可否は条件にしない |
| registry (`/api/agents`) | 5s | 全 view で document visible (App level 単一 poller) |

非表示の面・document hidden で取得を停止し、表示復帰で即時 refresh。
画面の取得済み応答は §7.10 の固定した identity と表示世代を照合し、
切替前の遅延応答を採用しない。
古い宛先の遅延レスポンスを新しい宛先の履歴へ混ぜない。poll 失敗で取得済み内容を消さず、可視状態と aria-live で
失敗を示す。切断・再接続中も取得済みの本文と読み位置を保持し、初回 loading に
戻さない。同じ session への復帰は前方差分から再開する。cursor が無効になり
直近の再取得が必要な場合も、本文を残したまま説明と「最新へ」の操作を示す。
過去ページは §7.5 の明示操作で取得する。native CLI 履歴を情報源とし、独立した
会話の保存基盤は追加しない。

## 10. Responsive

- 320px〜: ページ横 scroll なし。agent ボタンは wrap。
- 390×844 / 412×915 (mobile): app/detail 1 段目 48px、detail 2 段目 40px、
  dock 閉時の会話・報告領域は viewport の 55% 以上。dock 展開中はこの最低値を
  課さず、本文入力・送信・状態表示が縦 scroll で到達できること。
- 320×440 / 390×440 と横向きの低い viewport でも、dock の閉じる・textarea・
  送信・状態説明へ到達できる。`visualViewport` がある場合はその height / offset と
  resize / scroll に追従し、無い場合は `dvh` に fallback する。safe-area の左右・下
  inset を含めて可視領域に収める。入力欄の最小高さで送信を押し出さない。
  panel の padding と border を高さに含め、内部 scroll の末尾で送信ボタン全体が
  可視領域内に入ること。状態説明が長くても閉じる・送信が到達不能にならない。
- ≥1020px: 一覧の本文 main は 1020px で中央固定。app header は viewport 全幅
  のまま (詳細 1 段目と brand/menu の端を揃える)。詳細は viewport 全幅を使い、
  本文は折り返して表示。dock の tab は右寄せ、上辺の線は全幅。

## 11. Keyboard / focus / touch

- すべての操作 (agent ボタン / brand home / タブ / menu / dock tab / 送信 /
  再試行 / モーダル) はキーボード到達可能で focus-visible ring
  (accent 2px, offset 2–3px) を持つ。
- touch target: 主要操作 44×44px 以上 (agent ボタン・menu・dock tab)。
  タブは ≥36px 高。
- 詳細 → brand home で registry の該当 agent ボタンへ focus 復帰。
  モーダル・dock・dropdown は閉時に起点へ focus 復帰。
- 非同期状態変化 (件数 / 送信結果) は `aria-live` で通知する。

## 12. Contrast (実測要件)

両テーマで computed style から実測して満たす:

- 本文 (`--on-surface` / `--surface`): ≥7:1 目標、最低 4.5:1。
- meta・muted 文字: ≥4.5:1。
- state 文字 (idle/busy/danger) と accent 文字: ≥4.5:1。
- focus ring・状態 dot 等の非文字 UI: ≥3:1。
- 満たさない token は hue を保って明度のみ調整し、本表を更新する。

## 13. Motion / reduced motion

- 展開系 (dock panel, modal) ≤220ms、装飾系 (hover, chevron, row ink-in)
  ≤180ms。Kinari の装飾 motion は ≤150ms。
- `prefers-reduced-motion: reduce` で全 animation / transition を実質無効化
  (0.01ms)。brush loader・ink-in・dock 展開・chevron 回転が対象。

## 14. State transitions

- **router**: `/` ⇄ `/agent?pane` (push / Back)、タブ切替 = replace、
  旧 `/letters` と未知 path → `/` (replace)、popstate = 復元。
- **detail**: loading → ready / empty / error。継続 fetch の error は本文保持。
  対象消滅や identity 変更で送信を止め、説明を示す。
- **history**: 直近追従 ⇄ 過去閲覧。後方取得中 / 後方取得失敗は本文・位置保持。
  過去閲覧中の新着は最新への導線で知らせる。表示上限でのページ切替と直近への
  移動は明示操作で行う。切断 → 復帰は取得済み表示を保ち、identity を混ぜない。
- **content view**: 会話 ⇄ 画面。切替で対象・draft・会話の読み位置を維持。
  画面は loading → ready / empty / error、継続失敗・切断・終了で stale。
  identity 変更で旧表示の更新を停止する。
- **composer**: closed ⇄ open。送信 idle → sending → accepted / delivered /
  failed / result-unknown。状態名は API が確かめた事実に合わせる。
  accepted / delivered を agent の処理完了へ自動遷移させない。
- **theme**: dark | light | system。選択即適用・即保存・reload 後も維持。
  system は OS 設定変更へ live 追従してよい (matchMedia)。

## 15. 検証方法

- 実行可能な router / draft / session identity の振る舞いを対象に検証する。
  原文送信、送信中の対象切替・本文編集、遅延レスポンス、再起動後の誤送信防止、
  失敗時の draft 保持を確認する。本文にある単語の存在をテストにしない。
- `npm run check` (svelte-check) と `npm run format:check` を green に保つ。
- Chromium + Playwright による browser 実測: 320px 幅 / 390×844 / 412×915 /
  1020×800、両テーマで横 overflow なし、header 48px、detail chrome 89px、
  dock tab 44px / 108px、通常 panel ≤min(62dvh,420px)、本文の可読性・contrast 比を測る。
  320×440 / 390×440 と横向きでも送信・閉じるの全 bounding box が可視領域内に
  到達し、クリックできることを確認する。visualViewport 追従の確認と実機 IME の
  確認は区別し、実スマホを未確認ならその制約を報告する。
- 実操作で deep-link reload、Back/Forward、テーマ保存、IME 改行、focus 復帰、
  disabled 理由、loading / empty / error、draft 復元と再起動時の隔離を確認する。
  prefers-color-scheme / reduced-motion も emulation で確認する。
- 長履歴で複数の過去ページへ到達できること、境界の重複・欠落がないこと、表示と
  保持が上記の上限内に収まることを確認する。過去閲覧中の新着で同一メッセージの
  viewport 内の縦座標を測り、変化が 2px 以内であることを確認する。
  上限到達時は明示操作による切替のみ許可し、「最新へ」で直近末尾に戻れること、
  取得失敗・切断・復帰で既存本文を消さないこと、focus と狭幅の操作性を確認する。
- Markdown の通常表示・長いコードと表・raw HTML / 危険リンクを DOM と実操作で
  確認する。会話 / 画面の両方から同じ draft を送れ、切替で会話の読み位置が変わらず、
  opaque ID が通常表示されないことを確認する。画面タブの非表示中は画面取得が増えず、
  復帰で即時取得すること、取得時刻・更新失敗・切断・終了・identity 変更時の古い表示を
  確認する。専用 Herdr pane の実際の行・空白と Web 表示を比較し、画像との違いを
  記録する。利用者の作業 pane にテスト入力を送らない。
- 実ブラウザ経路で Codex と Claude Code の対象確認 → 原文指示 → 実 session 受信
  → assistant 報告表示を確認する。追加指示と接続切断・終了時の表示も確認し、
  送信成功だけで実 session の処理・報告検証を代替しない。
