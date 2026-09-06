import { expect, it } from "vitest";
import { appendMessages } from "./conversation";
const message = (id: string, text = id) => ({
  id,
  text,
  role: "assistant" as const,
});
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
