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
              pane_id: "w1:p2",
              session: "work",
              location: "work:1.0",
              cwd: "/tmp/a b",
              backend: "herdr",
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
                  pane_id: "w1:p4",
                  session: "work",
                  location: "work:0.0",
                  cwd: "/tmp/work",
                  backend: "herdr",
                },
              ],
            }),
            { status: 200 },
          )
        : new Response(
            JSON.stringify({ pane_id: "w1:p4", screen: "terminal output" }),
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
  // 詳細ではブランド masthead を出さず、compact な detail bar に集約する
  // (スマホでヘッダーが画面の半分を占める問題の回帰防止)。
  expect(screen.queryByText("HERDR / LOCAL BROKER")).toBeNull();
  expect(screen.queryByRole("navigation", { name: "表示切り替え" })).toBeNull();
  expect(screen.getByText("work · w1:p4")).toBeTruthy();
  // この agent へ手紙を出す導線が詳細画面にある。
  expect(
    screen.getByRole("button", { name: /codex に手紙を出す/ }),
  ).toBeTruthy();
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

it("shows session badges and hops between same-backend siblings only", async () => {
  const who = {
    agents: [
      {
        name: "claude",
        state: "idle",
        pane_id: "w2:p1",
        session: "knowledge",
        location: "knowledge:1.1",
        cwd: "/tmp/knowledge",
        backend: "herdr",
      },
      {
        name: "codex",
        state: "idle",
        pane_id: "w2:p4",
        session: "knowledge",
        location: "knowledge:1.4",
        cwd: "/tmp/knowledge",
        backend: "herdr",
      },
    ],
  };
  const fetch = vi.fn((input: RequestInfo | URL) => {
    const url = String(input);
    if (url === "/api/who")
      return Promise.resolve(
        new Response(JSON.stringify(who), { status: 200 }),
      );
    const pane = decodeURIComponent(
      url.replace("/api/agents/", "").replace("/screen", ""),
    );
    return Promise.resolve(
      new Response(JSON.stringify({ pane_id: pane, screen: `on ${pane}` }), {
        status: 200,
      }),
    );
  });
  vi.stubGlobal("fetch", fetch);
  render(App);

  // 一覧に session (workspace label) の badge が見える。
  const badges = await screen.findAllByText("knowledge");
  expect(badges.length).toBeGreaterThanOrEqual(2);

  await fireEvent.click(
    screen.getByRole("button", { name: "claude の Screen を表示" }),
  );
  const switcher = await screen.findByRole("navigation", {
    name: "同一 session の agent 切り替え",
  });
  const buttons = switcher.querySelectorAll("button");
  // 兄弟は同一 session の claude / codex だけ。
  expect(
    Array.from(buttons).map((button) => button.textContent?.trim()),
  ).toEqual(["claude", "codex"]);
  expect(buttons[0]?.getAttribute("aria-current")).toBe("true");

  // 1 click で codex の screen へ行き来できる。
  await fireEvent.click(buttons[1]!);
  expect(await screen.findByText("on w2:p4")).toBeTruthy();
  const after = screen
    .getByRole("navigation", { name: "同一 session の agent 切り替え" })
    .querySelectorAll("button");
  expect(after[1]?.getAttribute("aria-current")).toBe("true");
  expect(after[0]?.getAttribute("aria-current")).toBeNull();
});
