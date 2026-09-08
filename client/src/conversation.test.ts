import { expect, it, vi } from "vitest";
import { appendMessages, formatTimestamp } from "./conversation";
const message = (id: string, text = id) => ({
  id,
  text,
  role: "assistant" as const,
});
it.each([
  ["Asia/Tokyo", "2026/09/08 03:30", "2026/03/08 18:59", "2026/03/08 19:00"],
  [
    "America/Los_Angeles",
    "2026/09/07 11:30",
    "2026/03/08 01:59",
    "2026/03/08 03:00",
  ],
])(
  "formats native timestamps in the browser timezone (%s)",
  (zone, local, before, after) => {
    vi.stubEnv("TZ", zone);
    try {
      for (const timestamp of [
        "2026-09-07T18:30:00Z",
        "2026-09-08T03:30:00+09:00",
        "2026-09-07T11:30:00-07:00",
      ])
        expect(formatTimestamp(timestamp)).toBe(local);
      expect(formatTimestamp("2026-03-08T09:59:00Z")).toBe(before);
      expect(formatTimestamp("2026-03-08T10:00:00Z")).toBe(after);
      for (const timestamp of [
        undefined,
        null,
        "",
        "not-a-date",
        "2026-99-99T25:00:00Z",
      ])
        expect(formatTimestamp(timestamp)).toBe("");
    } finally {
      vi.unstubAllEnvs();
    }
  },
);
it("appends native messages once, without replacing earlier reports", () => {
  expect(appendMessages([message("1")], [message("1"), message("2")])).toEqual([
    message("1"),
    message("2"),
  ]);
});
it("keeps the reading window unchanged when count or text budget is exceeded", () => {
  expect(
    appendMessages(
      Array.from({ length: 1000 }, (_, i) => message(String(i))),
      [message("next")],
    ),
  ).toBeNull();
  expect(
    appendMessages([message("1", "a".repeat(2 * 1024 * 1024))], [message("2")]),
  ).toBeNull();
});
