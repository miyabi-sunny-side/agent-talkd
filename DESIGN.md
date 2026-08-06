---
name: agent-talkd observation & letters UI
version: 1
description: >
  Project design authority for the agent-talkd web client. Self-contained:
  everything needed to implement and verify the UI lives in this file.
---

# agent-talkd — Web UI Design Authority

この文書が本 repository の UI 設計の正である。共有テンプレート (Sumi / Kinari)
は bootstrap 入力として参照済みで、必要な規則はすべて本書へ複製・適合済み。
以後テンプレート側の変更に自動追従しない。
(theme 3択・Kinari token・モーダル規約は copy-then-own で本書が正。)

## 1. 目的と範囲

herdr 上で稼働する対話 agent の観測 (registry / terminal screen) と、許可された
mailbox からの手紙送信 (letters) を行う、同一ホスト・同一 UID 向けの小さな
操作面。視覚言語は「墨と和紙」— 墨色の地、和紙色の文字、柿色の印。
縦書き風 masthead・章番号 (一)・brush loader・角印「話」は製品の identity で
あり、registry / letters 画面で維持する。

過去の「read-only 専用」「composition 禁止」「Port-3 gate」記述は廃止済み。
手紙送信は正式機能である。

## 2. 画面と URL (router contract)

SPA は History API による手書き router を持つ (`client/src/router.ts`)。
外部 router 依存は追加しない。daemon は未知 GET path を SPA entry へ
fallback するため、サーバ変更は不要。

| URL | 画面 |
|---|---|
| `/` | Registry (agent 一覧) |
| `/letters` | Letters (mailbox timeline + inline compose) |
| `/agent?pane=<id>` | Agent detail (terminal screen + letter dock) |

規則:

- pane id は opaque (`%`, `/`, Unicode を含み得る)。必ず URLSearchParams で
  query に載せる。path segment にしない。
- 一覧→詳細、任意画面→Letters は `pushState`。
- 同一 session 内の agent タブ切替は `replaceState` (Back はタブ履歴を
  遡らず一覧へ戻る)。
- `popstate` で再 fetch なしに view を復元する。reload は同じ画面を復元する。
- 未知 path は Registry を描画し `replaceState` で `/` へ正規化する。
- deep-link (`/agent?pane=...`) は agents fetch 完了前に not-found 判定しない。
  fetch 成功後も pane が不在なら silent redirect せず、URL を保ったまま
  「この agent は見つかりません」の説明 + 「一覧へ」導線 (quiet-button) を出す。
  fetch 失敗は registry と同型のエラー + 再試行。
- `document.title` は view に追従する (`agent talk · observer` /
  `agent talk · <agent name>` / `agent talk · letters`)。

選択中 agent は「URL の pane + 最新 registry snapshot」から導出する。
view/selectedAgent を URL と別に持つ state にしない (単一情報源)。

## 3. Domain model

- **agent**: name (claude/codex/grok 等) / state (idle|busy) / pane_id (opaque) /
  session (= workspace label) / location / cwd / backend (herdr)。
- **session (workspace)**: 同一 session に複数 agent が同居する。詳細画面の
  主タイトルは session であり、agent はその中のタブである。
- **letter**: mailbox (source) → 対象 pane への本文送信。結果は
  sent (即配達) | queued (配達待ち)。mailbox 一覧は許可制で、空 (empty) と
  取得失敗 (error) は別状態。
- **状態色**: idle = teal 系、busy = ochre 系。row stroke・status dot・status
  文字にのみ使う。文字 (idle/busy) が常に色と併記され、色だけに依存しない。
  danger は失敗表示専用で agent 状態には使わない。

## 4. Theme — 3択制

### 4.1 方針と理由

従来は Sumi (dark) 単一テーマだった。屋外・明所での閲覧と OS 設定追従の期待に
応えるため、**墨 (dark) / 生成り (light) / システム追従** の3択制へ移行する。
Washi (e-paper) は実 e-paper 用途専用であり、通常 screen の選択肢に出さない。

再評価条件: e-paper クライアントを正式サポートする時、または Kinari で
terminal 固定 dark (後述) が実用上の問題になった時に選択肢構成を見直す。

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
  理由: masthead・角印・brush loader と一体の確立した identity であり、
  テンプレートは bootstrap 入力に過ぎないため。
- **Kinari の accent**: テンプレート既定 #9a6a00 (金) ではなく柿の hue を
  保った焦柿 #a84a17 を採る。理由: brand hue の連続性。cream 地で文字用途
  4.5:1 以上を実測で満たすこと (満たさない場合は hue を保ち明度のみ調整)。
- **terminal は両テーマで dark 固定**: capture 対象の herdr pane は dark TTY
  であり、反転は capture の忠実性を損なうため。
- Kinari 固有規則 (テンプレート由来): accent-subtle の控えめ装飾は tint ≤12%
  とし意味は text/shape でも伝える。focus ring は cream 地で ≥3:1 を実測。

### 4.4 テーマ切替 UI

ハンバーガー (menu) ボタン → メニューモーダル内「テーマ」section →
full-width ボタン3択 (radio semantics, `aria-checked`)。UI 呼称:
**「墨 — ダーク」「生成り — ライト」「システムに従う」**。
クリックで即適用 + 即保存し、モーダルは開いたままにする (変化を目視確認
できる)。閉じるのは共通3経路 (§8 モーダル規約)。

## 5. Typography

- 本文: `"Hiragino Kaku Gothic ProN", "Yu Gothic UI", "Noto Sans CJK JP",
  "Noto Sans JP", system-ui, sans-serif`
- mono (meta/status/terminal): `"SFMono-Regular", Consolas, monospace`
- masthead title `clamp(29px, 6vw, 48px)` / 見出し 15px / 本文 13px /
  meta・status 10px mono letter-spacing 0.08em / terminal 12px (狭幅 11px)

## 6. Layout と spacing

- `main`: `width: min(1020px, 100%)`、中央寄せ。
  padding `28px clamp(16px,5vw,56px) 18px`。
- 詳細画面 (`/agent`) は chrome を最小化: `padding-top: max(10px,
  env(safe-area-inset-top))`、下端は letter dock の tab (44px) が terminal
  最終行を隠さないだけの padding-bottom を確保。
- 横 scroll をページに出さない (溢れる要素は自身の overflow container 内で
  scroll する)。最小対応幅 320px。

## 7. Components

### 7.1 Masthead (Registry / Letters のみ)

eyebrow `HERDR / LOCAL BROKER`、title `agent talk` (talk = accent italic)、
nav (Agents / Letters / menu ボタン)、角印「話」(狭幅では非表示)。
詳細画面では表示しない (terminal が主役)。

### 7.2 Menu ボタンとメニューモーダル

- menu ボタン: 44×44px の quiet icon button (三本線 SVG)。masthead nav 右端と
  詳細ヘッダー右端の両方に置く。`aria-haspopup="dialog"`、
  `aria-label="メニュー"`。
- メニューモーダル: §8 の共通規約に従う。menu 系レイアウト = caption-muted の
  section label (`テーマ`) + full-width ボタン縦積み。現時点の項目はテーマ3択
  のみ。

### 7.3 Registry (一覧)

現行仕様を維持: 章番号 `一` + 見出し、`aria-live` の件数 output、
agent row (4px state stroke / name + session badge / cwd (title に全文) /
location + pane id / status dot + 文字 / →)。row 全体が button で Enter 起動。
loading = brush loader、empty = ○ + 静かな文言、error = 再試行付き alert。
row の ink-in animation は初回描画のみで、poll による再描画で再生させない。

### 7.4 詳細ヘッダー (1段)

**1行構成・実質1段**。旧 sibling-switcher 行と screen-toolbar 行は削除する。

```
[← 戻る 44px] [session名 ●state] [agentタブ (横scroll)] [≡ 44px]
```

- grid: `auto minmax(72px, max-content) minmax(0,1fr) auto`、gap 8–12px。
- 高さ: 内容 44px + padding-block 4px + border-bottom 1px = **53px**。
  390px 幅で bounding box ≤56px を守る。
- 主タイトルは **session (workspace label)** 13–14px semibold、溢れは
  ellipsis。直下ではなく同一行内に status dot (7px) + state 文字 (10px mono、
  idle/busy token 色) を添える。
- **pane id は視覚表示しない**。identity block の `title` と `aria-label` に
  `session · pane_id` として残す。
- agent タブ: 同一 backend+session の agent が2つ以上ある時のみ表示。
  pill (radius 999px、active = on-surface 地 + surface 文字反転、
  `aria-current="true"`)。行内で `overflow-x: auto`・折返しなし・
  scrollbar 非表示。タブの tap 領域は高さ ≥36px。切替は replaceState (§2)。
- 戻るボタン: 44×44px、`aria-label="agent 一覧へ戻る"`。戻り先は `/`
  (registry の該当 row へ focus 復帰)。

### 7.5 Screen (terminal)

- 2秒間隔の visible-only poll を**継続**する (hidden で停止、復帰で即時
  refresh、unmount で abort)。「2秒ごとに更新」表示と手動更新ボタンは
  **UI から削除** (reload は router が担う)。
- terminal: `role="log"`、mono、`--terminal-bg` 地、上辺 2px accent。
  自身の中で scroll し、ページを横に広げない。
- 更新失敗: 既存 capture がある時は差し替えず dim (opacity 0.62)。初回失敗は
  terminal 領域内に説明 + 再試行ボタン (role=alert)。
- registry 側で pane が消えた場合: header の state 文字を muted の「退出」に
  し、タブは最新 snapshot の兄弟のみへ更新。terminal は上記失敗系に従う。

### 7.6 Letter dock (ribbon composer) — 詳細画面下部

agent-terrace の letter-dock 構造を踏襲する。全幅 launcher バーは廃止。

- **dock**: 画面下端に固定。tab 行の上辺に viewport 全幅の 1px `--border`
  線を敷き、tab はその線から生える。
- **tab (launcher)**: 右寄せ (右 inset `max(10px, safe-area-right)`)。
  `min-width: 108px`・`height: 44px`・`border-radius: 9px 9px 0 0`・
  border は下辺なし・地は `--surface-raised`・mono 12px。内容 = 封筒 SVG
  (17px) + `手紙` + chevron SVG (13px、開時 180° 回転)。hover / 開時は
  accent 色。`aria-expanded` + `aria-controls`。未送信 draft がある時は
  tab 内に `下書きあり` badge (busy token 色の枠)。
- **panel**: tab の下の線から下 → 上へ展開。`max-height: min(62dvh, 420px)`
  で内部 scroll。展開 motion は max-height/transform/opacity 220ms
  `cubic-bezier(0.22,1,0.36,1)` 以下。**閉時は `inert` + `aria-hidden="true"`**。
- panel 内容 (現行 composer の機能を器ごと移す。機能は削らない):
  - 見出し `{agent.name} へ手紙を出す` + source 表示 + 閉じる quiet icon
    button (SVG ×)。
  - source phase: loading (`mailbox を確認中`) / ready (`{source} から`) /
    empty (`許可された mailbox が無い` 説明, role=alert) /
    error (説明 + 再試行, role=alert)。empty と error を混同させない。
  - 宛先は表示中 pane に固定 (選択 UI なし)。
  - 送信結果は sent (`送信しました #id`) と queued (`受理されました
    (配達待ち) #id`) を区別し `aria-live` で通知。失敗時は draft を保持。
  - draft は module scope の Map に `pane_id + agent name` を key として保持
    (タブ切替の remount を跨ぐ。identity が変わった pane の draft は破棄)。
  - 開: panel へ展開し ready なら textarea へ focus (不能時は panel 自身)。
    閉: Escape / 閉じるボタンで閉じ、focus は tab へ復帰。
- skill menu は API が無いため対象外。

### 7.7 Letters 画面

現行仕様を維持: mailbox selector + IN/OUT timeline (teal/kaki は文字 IN/OUT
の補強)、poll なし・ID cursor による手動追記 fetch、inline compose
(source = 選択 mailbox、宛先 = 稼働 agent select)。この画面の compose は
dock 化の対象外。

### 7.8 Footer (Registry / Letters のみ)

`OBSERVE + LETTERS` / `herdr`。

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
| screen capture (`/api/agents/<pane>/screen`) | 2s | `/agent` 表示中 + document visible |
| registry (`/api/who`) | 5s | 全 view で document visible (App level 単一 poller) |
| letters timeline | poll なし | 手動更新のみ (ID cursor) |

registry poll の目的: 一覧の鮮度と、詳細ヘッダーの state / タブ構成の追従。
hidden で停止し、visible 復帰で即時 refresh。poll 失敗は表示中の内容を消さず、
registry 画面では aria-live output にのみ失敗を示す。

## 10. Responsive

- 320px〜: ページ横 scroll なし。
- ≤620px: masthead 2列 (角印非表示)、agent row 2行組、terminal padding 縮小。
- 390×844 / 412×915 (mobile): 詳細ヘッダー ≤56px、terminal 上端 ≤96px、
  terminal 高さ ≥55% viewport。
- ≥1020px: main は 1020px で中央固定。dock の tab は main と同じ右端基準で
  よい (viewport 右端でも可、ただし線は全幅)。

## 11. Keyboard / focus / touch

- すべての操作 (row / 戻る / タブ / menu / dock tab / 送信 / 再試行 /
  selector / モーダル) はキーボード到達可能で focus-visible ring
  (accent 2px, offset 2–3px) を持つ。
- touch target: 主要操作 44×44px 以上 (戻る・menu・dock tab)。タブ pill は
  ≥36px 高。
- 詳細 → 戻る で registry の該当 row へ focus 復帰。モーダル・dock は
  閉時に起点ボタンへ focus 復帰。
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

- **router**: `/` ⇄ `/agent?pane` (push / Back)、任意 → `/letters` (push)、
  タブ切替 = replace、未知 path → `/` (replace)、popstate = 復元のみ。
- **detail 画面**: loading → ready / error(初回) / error(継続, dim)。
  pane 消滅 → 「退出」表示。not-found → 説明 + 一覧導線。
- **composer**: closed ⇄ open。open 内で source phase
  loading → ready | empty | error(再試行)。送信 idle → sending →
  sent | queued | failed(draft 保持)。
- **theme**: dark | light | system。選択即適用・即保存・reload 後も維持。
  system は OS 設定変更へ live 追従してよい (matchMedia)。

## 15. 検証方法

- unit (vitest + jsdom): router の parse/serialize/navigate/popstate、theme
  store (保存値 ⇄ data-theme)、draft key (pane+identity)、pane 消滅時の
  header 状態。jsdom の History API で Back/replace を検証する。
- `npm run check` (svelte-check) と `npm run format:check` を green に保つ。
- browser (ui-checker): 390×844 / 412×915 / 1020×800 で、DOM・computed
  style・getBoundingClientRect・実操作により本書の数値 (ヘッダー ≤56px、
  tab 44px/108px、panel ≤min(62dvh,420px)、contrast 比、theme 永続化、
  deep-link reload、Back/Forward) を実測する。prefers-color-scheme と
  prefers-reduced-motion は DevTools emulation で両値を検証する。
