// テーマ3択 (墨 dark / 生成り light / システム追従) の保存と適用
// (DESIGN.md §4)。
//
// - localStorage key は `agent-talkd:theme`。"system" も明示保存する。
// - 明示選択時は `<html data-theme="dark|light">`、system 時は属性を外して
//   CSS の prefers-color-scheme に委ねる。
// - 初期 paint の flash 防止は client/index.html の同期 inline script が担い、
//   本 module は起動後の適用・切替を担う (規則は両者で同一に保つこと)。

export type ThemeChoice = "dark" | "light" | "system";

export const THEME_STORAGE_KEY = "agent-talkd:theme";

export function loadTheme(): ThemeChoice {
  try {
    const stored = window.localStorage.getItem(THEME_STORAGE_KEY);
    if (stored === "dark" || stored === "light" || stored === "system") {
      return stored;
    }
  } catch {
    // storage 不能 (private mode 等) は system 扱い。
  }
  return "system";
}

export function applyTheme(choice: ThemeChoice): void {
  if (choice === "system") {
    delete document.documentElement.dataset.theme;
  } else {
    document.documentElement.dataset.theme = choice;
  }
}

export function saveTheme(choice: ThemeChoice): void {
  try {
    window.localStorage.setItem(THEME_STORAGE_KEY, choice);
  } catch {
    // 保存できなくても現 session の適用は続ける。
  }
  applyTheme(choice);
}
