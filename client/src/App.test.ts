import { fireEvent, render, screen } from "@testing-library/svelte";
import { expect, it, vi } from "vitest";
import App from "./App.svelte";

it("renders loading then agent status", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValue(
      new Response(
        JSON.stringify({
          agents: [
            {
              name: "claude",
              state: "busy",
              pane_id: "%2",
              session: "work",
              location: "work:1.0",
              cwd: "/tmp/a b",
            },
          ],
        }),
        { status: 200 },
      ),
    ),
  );
  render(App);
  expect(screen.getByText("接続を確かめています")).toBeTruthy();
  expect(await screen.findByText("claude")).toBeTruthy();
  expect(screen.getByText("busy")).toBeTruthy();
  expect(
    screen.getByRole("button", { name: "claude の Screen を表示" }),
  ).toBeTruthy();
});

it("opens a screen from the keyboard and restores focus when returning", async () => {
  const fetch = vi.fn((path: string) =>
    Promise.resolve(
      path === "/api/who"
        ? new Response(
            JSON.stringify({
              agents: [
                {
                  name: "codex",
                  state: "idle",
                  pane_id: "%4",
                  session: "work",
                  location: "work:0.0",
                  cwd: "/tmp/work",
                },
              ],
            }),
            { status: 200 },
          )
        : new Response(
            JSON.stringify({ pane_id: "%4", screen: "terminal output" }),
            {
              status: 200,
            },
          ),
    ),
  );
  vi.stubGlobal("fetch", fetch);
  render(App);
  const row = await screen.findByRole("button", {
    name: "codex の Screen を表示",
  });
  row.focus();
  await fireEvent.keyDown(row, { key: "Enter" });
  expect(await screen.findByText("terminal output")).toBeTruthy();
  await fireEvent.click(screen.getByRole("button", { name: /agent 一覧へ/ }));
  expect(document.activeElement).toBe(
    screen.getByRole("button", { name: "codex の Screen を表示" }),
  );
});

it("offers retry after an error and supports the empty state", async () => {
  const fetch = vi
    .fn()
    .mockRejectedValueOnce(new Error("offline"))
    .mockResolvedValueOnce(new Response('{"agents":[]}', { status: 200 }));
  vi.stubGlobal("fetch", fetch);
  render(App);
  const retry = await screen.findByRole("button", { name: "再試行" });
  await fireEvent.click(retry);
  expect(
    await screen.findByText("静かな待合です。", { exact: false }),
  ).toBeTruthy();
  expect(fetch).toHaveBeenCalledTimes(2);
});
