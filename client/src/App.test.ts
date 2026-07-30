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
