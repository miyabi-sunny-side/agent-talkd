import { fireEvent, render, screen } from "@testing-library/svelte";
import { beforeEach, expect, it, vi } from "vitest";
import ThemeModal from "./ThemeModal.svelte";
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

it("offers three icon choices, applies immediately, and stays open", async () => {
  const onclose = vi.fn();
  render(ThemeModal, { onclose });

  const group = screen.getByRole("radiogroup", { name: "テーマ" });
  const options = Array.from(group.querySelectorAll("[role='radio']"));
  expect(options.map((option) => option.textContent?.trim())).toEqual([
    "自動",
    "ライト",
    "ダーク",
  ]);
  // 既定 (未保存) は system。
  expect(
    screen.getByRole("radio", { name: "自動" }).getAttribute("aria-checked"),
  ).toBe("true");

  await fireEvent.click(screen.getByRole("radio", { name: "ライト" }));
  expect(document.documentElement.dataset.theme).toBe("light");
  expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe("light");
  // 変化を目視確認できるようモーダルは開いたまま。
  expect(onclose).not.toHaveBeenCalled();
  expect(
    screen.getByRole("radio", { name: "ライト" }).getAttribute("aria-checked"),
  ).toBe("true");
});

it("closes on Escape, the close button, and the scrim", async () => {
  const onclose = vi.fn();
  const { container } = render(ThemeModal, { onclose });

  await fireEvent.keyDown(window, { key: "Escape" });
  expect(onclose).toHaveBeenCalledTimes(1);

  onclose.mockClear();
  const closeButtons = screen.getAllByRole("button", { name: "閉じる" });
  // icon × (modal head) を押す。scrim も同名なので最後ではなく head 側。
  await fireEvent.click(closeButtons[closeButtons.length - 1]!);
  expect(onclose).toHaveBeenCalledTimes(1);

  onclose.mockClear();
  await fireEvent.click(container.querySelector(".scrim")!);
  expect(onclose).toHaveBeenCalledTimes(1);
});
