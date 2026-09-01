---
name: agent-talkd observation & letters UI
version: 2
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

herdr 上で稼働する対話 agent の観測 (registry / terminal screen) と、許可された
mailbox からの手紙送信 (letters) を行う、同一ホスト・同一 UID 向けの小さな
操作面。視覚言語は「墨と和紙」— 墨色の地、和紙色の文字、柿色の accent。
chrome はテンプレートの静かな 48px app header + compact summary を土台にし、
session 単位カード・状態 border / 文字色・letter dock を agent-talk の差別化と
する。モバイル縦スペースを優先する。

過去の「read-only 専用」「composition 禁止」「Port-3 gate」、および
eyebrow `HERDR / LOCAL BROKER`・角印「話」・縦書き masthead を identity と
する記述は廃止済み。brush loader は loading 表示として残す。
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

- **agent**: name (タブ名由来。custom タブ名が無ければ runtime 検出名
  claude/codex/grok 等) / state (idle|busy) / pane_id (opaque) /
  session (= workspace label) / location / cwd / backend (herdr)。
- **session (workspace)**: 同一 session に複数 agent が同居する。詳細画面の
  主タイトルは session であり、agent はその中のタブである。
- **letter**: mailbox (source) → 対象 pane への本文送信。結果は
  sent (即配達) | queued (配達待ち)。mailbox 一覧は許可制で、空 (empty) と
  取得失敗 (error) は別状態。
- **状態色**: idle = teal 系 (`--idle`)、busy = ochre 系 (`--busy`)。
  registry の agent ボタンは **border 色**、詳細の agent タブは **文字色** で
  視覚表現する。色だけに依存しないため、各 button / tab の `aria-label` に
  `name (idle|busy|退出)` を必ず含める。danger は失敗表示専用。

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

- 一覧 / Letters の **app header** は `main` の sibling として viewport
  full-bleed (48px sticky)。左右 gutter は詳細 1 段目と同一 token
  (`--chrome-inline-start` / `--chrome-inline-end` =
  `max(12px, safe-area-left)` / `max(8px, safe-area-right)`)。
- 一覧 / Letters の **本文 `main`**: `width: min(1020px, 100%)`、中央寄せ。
  横 padding `clamp(16px, 5vw, 56px)`。`min-height: calc(100dvh - 48px)`。
  chrome の端位置と本文列の幅は独立 (header を本文 max-width に閉じ込めない)。
- 詳細 (`/agent`): `main` は full-bleed。chrome 2 段 48+40 + hairline 1 =
  outer **89px** + letter dock tab を除いた高さを terminal に与える。
- 横 scroll をページに出さない。最小対応幅 320px。カード内 agent ボタンは wrap。

## 7. Components

### 7.1 App header (Registry / Letters)

テンプレート Header を copy-then-own した 48px sticky bar。`main` 外に置き
viewport 全幅。左右 gutter は詳細 `.detail-bar-primary` と共通 token。

- 左: brand `agent talk` (`talk` = accent italic)。SPA navigate 用 button。
  full page reload の `<a href="/">` は使わない。
- 右: ハンバーガー (44×44)。`aria-haspopup="menu"` / `aria-expanded` /
  `aria-label="メニュー"`。
- eyebrow / 角印 / 大型 wordmark は置かない。

### 7.2 Menu (template dropdown) とテーマモーダル

- 右寄せ dropdown (overlay + panel)。項目: Agents / Letters / テーマ設定
  (詳細では Letters + テーマ設定)。
- 「テーマ設定」はテーマモーダル (§8) を開く。ラベルは §4.4 のとおり。
- Escape / overlay で閉じ、起点 menu ボタンへ focus 復帰。

### 7.3 Registry (一覧)

- compact summary (≤40px): 見出し「稼働中の agent」+ aria-live 件数。章番号なし。
- agents を backend+session で group し session カード1枚に agent ボタンを並べる
  (優先: claude → codex → grok → 他)。ボタンは registry 実データから動的生成。
- ボタン: 可視は name のみ、状態は 2px border 色、aria-label に state、
  min-height 44px。location / pane_id / cwd は非表示。
- loading / empty / error は従来どおり。

### 7.4 詳細ヘッダー (2段)

```
[agent talk (home)]     [session 名 …]     [≡ menu]
[ agent tab · agent tab · … (横 scroll)              ]
```

- 1 段目 48px: brand (`aria-label="agent talk — 一覧へ戻る"`) + session 名。
  session の title/aria-label に `session · pane_id`。pane id は非表示。
- 2 段目 40px: 同一 session の agent タブを常時表示。active は下線、状態は
  文字色 (idle/busy/退出)。切替は replaceState。tap ≥36px。

### 7.5 Screen (terminal)

- 2秒間隔の visible-only poll を継続。手動更新 UI なし。
- terminal: role=log、mono、固定 dark。詳細では viewport 全幅。accent border は
  付けず、左右・下辺の暗色 border のみ。
- 失敗・退出時の扱いは従来どおり (dim / 再試行 / タブ更新)。

### 7.6 Letter dock (ribbon composer) — 詳細画面下部

agent-terrace の letter-dock 構造を踏襲する。全幅 launcher バーは廃止。

- **dock**: 画面下端に固定。tab 行の上辺に viewport 全幅の 1px `--border`
  線を敷き、tab はその線から生える。
- **tab (launcher)**: 右寄せ (右 inset `max(10px, safe-area-right)`)。
  `min-width: 108px`・`height: 44px`・`border-radius: 9px 9px 0 0`・
  border は下辺なし・地は `--surface-raised`・mono 12px。内容 = 封筒 SVG
  (17px) + `手紙` + chevron SVG (13px、開時 180° 回転)。hover / 開時は
  accent 色。`aria-expanded` + `aria-controls`。未送信 draft (body または
  skill) がある時は tab の寸法・可視内容を変えず、枠を `--accent` にする。
  色だけに依存せず accessible name には `下書きあり` を加える。
- **panel**: tab の下の線から下 → 上へ展開。`max-height: min(62dvh, 420px)`
  で内部 scroll。展開 motion は max-height/transform/opacity 220ms
  `cubic-bezier(0.22,1,0.36,1)` 以下。**閉時は `inert` + `aria-hidden="true"`**。
- panel 内容:
  - 見出し `{agent.name} へ手紙を出す` + source 表示 + 閉じる quiet icon
    button。
  - source phase: loading / ready / empty / error (混同させない)。
  - 宛先は表示中 pane に固定。
  - **actions 行**: 左 skill ボタン + 右送信ボタン (min-height 44px)。
  - skill: 既定「なし」(trigger 表示も「なし」)。actions 直下に in-flow の
    menu (`role=menu` / menuitemradio) を展開。開時は現選択へ focus、
    Arrow/Home/End、Escape/選択後は trigger へ復帰。候補は
    `GET /api/agents/{pane}/skills` (installed ∩ allowlist、skill_syntax の
    無い runtime は空)。0 件でも「なし」のみ。draft に skill も保持。
  - 送信は `POST /api/letters` に optional `skill`。失敗時は body+skill 保持、
    成功時のみ clear。
  - 送信結果 sent / queued を `aria-live` で区別。
  - 開: panel 展開 + focus。閉: Escape / 閉じる → tab 復帰。skill popup 中の
    Escape は popup のみ閉じる。

### 7.7 Letters 画面

取得契約は現行のまま。mailbox selector と IN/OUT timeline を保つ
(teal/kaki は文字 IN/OUT の補強)。fetch は poll なし・ID cursor による
手動追記のまま。inline compose も現行どおり (source = 選択 mailbox、
宛先 = 稼働 agent select)。この画面の compose は dock 化の対象外。

**表示順と grouping** — client 側の表示変換のみで行い、取得配列・cursor・
サーバ契約は変えない。

- letter を workspace (herdr の space) で group 化する。key は `target_pane`
  の `:` より前の prefix (`w2:p3` → `w2`)。`:` を含まない旧形式 pane は pane id
  全体を key とする。
- 並びは **新着が上**: group は「group 内の最大 id」の降順、group 内の letter は
  id 降順。読むために末尾まで scroll させない。
- **group 見出しは sticky な 1 行の非対話 `h3`** とする。§7.3 の session
  カードは Letters に流用しない。理由は 2 つ。カードの padding + gap が
  group ごとに縦を約 36px 奪う (モバイル縦スペース優先 §1)。そして全幅
  hairline を既に持つ letter list と枠が二重になる。
  - `position: sticky; top: 48px` (app header の直下)。地は `--surface`
    (不透過)、下辺のみ 1px `--border`、高さ 32px 以上。letter はその下を潜る。
  - 内容は 3 要素まで: label / workspace id / 件数。
    - **label**: live registry (`/api/who`) に同 prefix の agent がいればその
      `session` (workspace label)、いなければ workspace id。13px・weight 600・
      letter-spacing 0.04em・1 行 ellipsis。
    - **workspace id**: label と異なるときだけ併記 (同じ文字列を 2 度出さない)。
      10px mono `--muted`。
    - **件数**: 行末 (`margin-left: auto`) に `N 通`。10px mono `--muted`。
    - **相手 agent 名は見出しに置かない**。1 workspace には複数 agent が同居
      し得るため (§3) 見出しの単一名は誤導になる。宛先の所有者は各 letter の
      footer (`→ / ← {target_name}`) 1 箇所に保つ。pane id は §7.3 と同じく
      非表示。
- letter item の意匠 (4px 方向バー・IN/OUT・#id・時刻・本文・footer) は現行
  のまま。group ごとの `ol.letter-list` は上辺 border を持たない (見出しの下辺
  と二重にしない)。group 由来の追加 indent は与えず、見出しの text 左端は
  letter 本文の左端 (list 左端から 20px) に揃える。
- group の折りたたみ・並べ替え・group 単位 fetch は置かない。tab 停止も増やさ
  ない (selector → 更新 → 宛先 → 本文 → 送信)。見出しに entrance animation は
  付けない (更新のたびに再生させないため)。
- loading / empty / error / status 行 (`N letters`) は現行のまま。group は
  events から導出するので空 group は構造上発生しない。
- a11y: 各 group は `h3` + `ol[aria-labelledby]`。外側 list の
  `${mailbox} letter history` は保つ。

### 7.8 Footer

Registry / Letters / 詳細のいずれにも site footer は置かない。

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

- 320px〜: ページ横 scroll なし。agent ボタンは wrap。
- 390×844 / 412×915 (mobile): app/detail 1 段目 48px、detail 2 段目 40px、
  terminal 高さ ≥55% viewport。
- ≥1020px: 一覧の本文 main は 1020px で中央固定。app header は viewport 全幅
  のまま (詳細 1 段目と brand/menu の端を揃える)。詳細 terminal は viewport
  全幅。dock の tab は右寄せ、上辺の線は全幅。

## 11. Keyboard / focus / touch

- すべての操作 (agent ボタン / brand home / タブ / menu / dock tab / 送信 /
  再試行 / selector / モーダル) はキーボード到達可能で focus-visible ring
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
  style・getBoundingClientRect・実操作により本書の数値 (app header 48px、
  detail chrome 89px (48+40+border 1)、dock tab 44px/108px、panel ≤min(62dvh,420px)、
  contrast 比、theme 永続化、deep-link reload、Back/Forward、eyebrow 不在、
  session カード、pane 座標非表示) を実測する。prefers-color-scheme と
  prefers-reduced-motion は DevTools emulation で両値を検証する。
- Letters の grouping (§7.7) は browser で実測する。対象は id の降順、
  workspace ごとの分割、見出しの sticky 位置 (top 48px)、見出し高さ 32px 以上、
  件数の一致。unit は純関数 (events → groups) を対象にする。
