import { fireEvent, render, screen } from "@testing-library/svelte";
import { afterEach, expect, it, vi } from "vitest";
import ScreenPanel from "./ScreenPanel.svelte";
import type { Agent } from "./api";

const agent: Agent = {
  name: "codex",
  state: "idle",
  pane_id: "w1:p7",
  session: "work",
  backend: "herdr",
  location: "work:1.0",
  cwd: "/tmp/work",
};

function capture(screenText: string): Response {
  return new Response(
    JSON.stringify({ pane_id: "w1:p7", screen: screenText }),
    {
      status: 200,
    },
  );
}

afterEach(() => vi.useRealTimers());

it("renders captured terminal text without interpreting markup", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValue(capture("<b>literal</b>\nline 2")),
  );
  const { container } = render(ScreenPanel, { agent });

  expect(
    await screen.findByText("<b>literal</b>", { exact: false }),
  ).toBeTruthy();
  expect(container.querySelector("b")).toBeNull();
  expect(screen.getByRole("log", { name: "codex terminal" })).toBeTruthy();
});

it("polls every two seconds while visible", async () => {
  vi.useFakeTimers();
  const fetch = vi.fn().mockResolvedValue(capture("terminal"));
  vi.stubGlobal("fetch", fetch);
  render(ScreenPanel, { agent });
  await vi.waitFor(() => expect(fetch).toHaveBeenCalledTimes(1));
  await vi.advanceTimersByTimeAsync(2_000);
  await vi.waitFor(() => expect(fetch).toHaveBeenCalledTimes(2));
});

it("does not let an older refresh overwrite a newer response", async () => {
  // 1回目の応答が遅れている間に visibilitychange 由来の refresh が走り、
  // 遅れて届いた古い応答が新しい画面を上書きしない。
  let resolveFirst: (value: Response) => void = () => undefined;
  const fetch = vi
    .fn()
    .mockReturnValueOnce(
      new Promise<Response>((resolve) => (resolveFirst = resolve)),
    )
    .mockResolvedValueOnce(capture("new screen"));
  vi.stubGlobal("fetch", fetch);
  render(ScreenPanel, { agent });
  await vi.waitFor(() => expect(fetch).toHaveBeenCalledTimes(1));

  document.dispatchEvent(new Event("visibilitychange"));
  expect(await screen.findByText("new screen")).toBeTruthy();
  resolveFirst(capture("stale screen"));
  await Promise.resolve();
  expect(screen.queryByText("stale screen")).toBeNull();
});

it("shows neither a refresh button nor a poll status line", async () => {
  // 詳細は terminal が主役。更新は router (ブラウザ reload) が担う。
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(capture("terminal")));
  render(ScreenPanel, { agent });
  expect(await screen.findByText("terminal")).toBeTruthy();
  expect(screen.queryByRole("button", { name: "更新" })).toBeNull();
  expect(screen.queryByText("2秒ごとに更新")).toBeNull();
});
