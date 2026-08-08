import { fireEvent, render, screen } from "@testing-library/svelte";
import { beforeEach, expect, it, vi } from "vitest";
import MenuModal from "./MenuModal.svelte";
import { applyTheme, loadTheme, saveTheme, THEME_STORAGE_KEY } from "./theme";

beforeEach(() => {
  window.localStorage.clear();
  delete document.documentElement.dataset.theme;
});

it("falls back to system for a missing or unknown stored value", () => {
  expect(loadTheme()).toBe("system");
  window.localStorage.setItem(THEME_STORAGE_KEY, "sepia");
  expect(loadTheme()).toBe("system");
});

it("stores every choice and drives data-theme", () => {
  saveTheme("light");
  expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe("light");
  expect(document.documentElement.dataset.theme).toBe("light");
  expect(loadTheme()).toBe("light");

  saveTheme("dark");
  expect(document.documentElement.dataset.theme).toBe("dark");

  // system は属性を外して prefers-color-scheme に委ねる。key は消さず
  // "system" を明示保存する (DESIGN.md §4.2)。
  saveTheme("system");
  expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe("system");
  expect(document.documentElement.dataset.theme).toBeUndefined();
});

it("applies without saving when only the attribute should change", () => {
  applyTheme("light");
  expect(document.documentElement.dataset.theme).toBe("light");
  expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBeNull();
});

it("offers three choices, applies immediately, and stays open", async () => {
  const onclose = vi.fn();
  render(MenuModal, { onclose });

  const group = screen.getByRole("radiogroup", { name: "テーマ" });
  const options = Array.from(group.querySelectorAll("[role='radio']"));
  expect(options.map((option) => option.textContent?.trim())).toEqual([
    "墨 — ダーク",
    "生成り — ライト",
    "システムに従う",
  ]);
  // 既定 (未保存) は system。
  expect(
    screen
      .getByRole("radio", { name: "システムに従う" })
      .getAttribute("aria-checked"),
  ).toBe("true");

  await fireEvent.click(screen.getByRole("radio", { name: "生成り — ライト" }));
  expect(document.documentElement.dataset.theme).toBe("light");
  expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe("light");
  // 変化を目視確認できるようモーダルは開いたまま。
  expect(onclose).not.toHaveBeenCalled();
  expect(
    screen
      .getByRole("radio", { name: "生成り — ライト" })
      .getAttribute("aria-checked"),
  ).toBe("true");
});

it("closes on Escape, the close button, and the scrim", async () => {
  const onclose = vi.fn();
  const { container } = render(MenuModal, { onclose });

  await fireEvent.keyDown(screen.getByRole("dialog", { name: "テーマ設定" }), {
    key: "Escape",
  });
  expect(onclose).toHaveBeenCalledTimes(1);

  await fireEvent.click(screen.getByRole("button", { name: "閉じる" }));
  expect(onclose).toHaveBeenCalledTimes(2);

  await fireEvent.click(container.querySelector(".modal-scrim")!);
  expect(onclose).toHaveBeenCalledTimes(3);
});
