import { fireEvent, render, screen } from "@testing-library/svelte";
import { beforeEach, expect, it, vi } from "vitest";
import App from "./App.svelte";

// URL が画面の情報源になったので、test 間で持ち越さない。
beforeEach(() => window.history.replaceState(null, "", "/"));

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
  // 詳細ではブランド masthead を出さず、1段の detail bar に集約する
  // (スマホでヘッダーが画面の半分を占める問題の回帰防止)。
  expect(screen.queryByText("HERDR / LOCAL BROKER")).toBeNull();
  expect(screen.queryByRole("navigation", { name: "表示切り替え" })).toBeNull();
  // 主タイトルは session。pane id は可視テキストに出さず title/aria に持つ。
  expect(screen.getByText("work")).toBeTruthy();
  expect(screen.queryByText("work · w1:p4")).toBeNull();
  expect(
    screen.getByLabelText("work · w1:p4", { selector: ".detail-identity" }),
  ).toBeTruthy();
  // 詳細を開くと URL が pane を指し、reload しても戻らない。
  expect(window.location.pathname).toBe("/agent");
  expect(new URLSearchParams(window.location.search).get("pane")).toBe("w1:p4");
  // この agent へ手紙を出す導線 (ribbon) が詳細画面にある。
  expect(
    screen.getByRole("button", { name: /codex に手紙を出す/ }),
  ).toBeTruthy();
  // 一覧由来の「戻る」は history を巻き戻す (Back で詳細へ戻らない)。
  await fireEvent.click(screen.getByRole("button", { name: /agent 一覧へ/ }));
  await vi.waitFor(() => expect(window.location.pathname).toBe("/"));
  await vi.waitFor(() =>
    expect(document.activeElement).toBe(
      screen.getByRole("button", { name: "codex の Screen を表示" }),
    ),
  );
});

it("keeps Back and Forward consistent and restores focus on popstate", async () => {
  const who = {
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
  };
  const fetch = vi.fn((input: RequestInfo | URL) => {
    const url = String(input);
    if (url === "/api/who")
      return Promise.resolve(
        new Response(JSON.stringify(who), { status: 200 }),
      );
    if (url === "/api/mailboxes")
      return Promise.resolve(
        new Response(JSON.stringify({ mailboxes: ["mobile"] }), {
          status: 200,
        }),
      );
    return Promise.resolve(
      new Response(JSON.stringify({ pane_id: "w1:p4", screen: "term" }), {
        status: 200,
      }),
    );
  });
  vi.stubGlobal("fetch", fetch);
  render(App);

  const row = await screen.findByRole("button", {
    name: "codex の Screen を表示",
  });
  await fireEvent.click(row);
  expect(await screen.findByText("term")).toBeTruthy();

  // 画面内の戻る = history.back()。Back で詳細へ戻る二重 push を作らない。
  await fireEvent.click(screen.getByRole("button", { name: /agent 一覧へ/ }));
  await vi.waitFor(() => expect(window.location.pathname).toBe("/"));
  window.history.forward();
  await vi.waitFor(() => expect(window.location.pathname).toBe("/agent"));
  expect(await screen.findByText("term")).toBeTruthy();

  // browser Back で一覧へ戻った時も、見ていた row へ focus が返る。
  window.history.back();
  await vi.waitFor(() => expect(window.location.pathname).toBe("/"));
  await vi.waitFor(() =>
    expect(document.activeElement).toBe(
      screen.getByRole("button", { name: "codex の Screen を表示" }),
    ),
  );
});

it("does not carry a draft to a new occupant of the same pane", async () => {
  const occupant = (name: string) => ({
    agents: [
      {
        name,
        state: "idle",
        pane_id: "w1:p4",
        session: "work",
        location: "work:0.0",
        cwd: "/tmp/work",
        backend: "herdr",
      },
    ],
  });
  let current = "codex";
  const letters: unknown[] = [];
  const fetch = vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url === "/api/who")
      return Promise.resolve(
        new Response(JSON.stringify(occupant(current)), { status: 200 }),
      );
    if (url === "/api/mailboxes")
      return Promise.resolve(
        new Response(JSON.stringify({ mailboxes: ["mobile"] }), {
          status: 200,
        }),
      );
    if (url === "/api/letters") {
      letters.push(JSON.parse(String(init!.body)));
      return Promise.resolve(
        new Response(
          JSON.stringify({
            version: 1,
            id: 3,
            path: "sent",
            to: "w1:p4",
            name: current,
          }),
          { status: 200 },
        ),
      );
    }
    return Promise.resolve(
      new Response(JSON.stringify({ pane_id: "w1:p4", screen: "term" }), {
        status: 200,
      }),
    );
  });
  vi.stubGlobal("fetch", fetch);
  window.history.replaceState(null, "", "/agent?pane=w1%3Ap4");
  render(App);

  await fireEvent.click(
    await screen.findByRole("button", { name: /codex に手紙を出す/ }),
  );
  const body = await screen.findByRole("textbox", {
    name: "codex への手紙の本文",
  });
  await fireEvent.input(body, { target: { value: "codex 宛の下書き" } });

  // 同じ pane の占有者が gemini に入れ替わる (registry poll が拾う)。
  current = "gemini";
  document.dispatchEvent(new Event("visibilitychange"));
  const newTab = await screen.findByRole("button", {
    name: /gemini に手紙を出す/,
  });
  // 旧 draft は新しい agent に引き継がれない。
  expect(newTab.textContent).not.toContain("下書きあり");
  await fireEvent.click(newTab);
  const newBody = await screen.findByRole("textbox", {
    name: "gemini への手紙の本文",
  });
  expect((newBody as HTMLTextAreaElement).value).toBe("");
  expect(letters).toHaveLength(0);
});

it("polls the registry only while visible and ignores a stale response", async () => {
  let resolveFirst: (value: Response) => void = () => undefined;
  const who = (name: string) =>
    new Response(
      JSON.stringify({
        agents: [
          {
            name,
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
    );
  const fetch = vi
    .fn()
    .mockReturnValueOnce(
      new Promise<Response>((resolve) => (resolveFirst = resolve)),
    )
    .mockResolvedValue(who("gemini"));
  vi.stubGlobal("fetch", fetch);
  render(App);
  await vi.waitFor(() => expect(fetch).toHaveBeenCalledTimes(1));

  // 初回応答が遅れている間に visible 復帰の refresh が走る。
  document.dispatchEvent(new Event("visibilitychange"));
  expect(await screen.findByText("gemini")).toBeTruthy();
  // 遅れて届いた古い snapshot は新しい表示を上書きしない。
  resolveFirst(who("codex"));
  await Promise.resolve();
  expect(screen.queryByText("codex")).toBeNull();
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

  // 1 click で codex の screen へ行き来できる。タブ切替は replaceState なので
  // 履歴は増えず、Back は一覧へ戻る (DESIGN.md §2)。
  const historyLength = window.history.length;
  await fireEvent.click(buttons[1]!);
  expect(await screen.findByText("on w2:p4")).toBeTruthy();
  expect(new URLSearchParams(window.location.search).get("pane")).toBe("w2:p4");
  expect(window.history.length).toBe(historyLength);
  const after = screen
    .getByRole("navigation", { name: "同一 session の agent 切り替え" })
    .querySelectorAll("button");
  expect(after[1]?.getAttribute("aria-current")).toBe("true");
  expect(after[0]?.getAttribute("aria-current")).toBeNull();
});

it("restores the same agent from a deep link and explains a missing pane", async () => {
  const who = {
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
  };
  const fetch = vi.fn((input: RequestInfo | URL) => {
    const url = String(input);
    if (url === "/api/who")
      return Promise.resolve(
        new Response(JSON.stringify(who), { status: 200 }),
      );
    if (url === "/api/mailboxes")
      return Promise.resolve(
        new Response(JSON.stringify({ mailboxes: ["mobile"] }), {
          status: 200,
        }),
      );
    return Promise.resolve(
      new Response(JSON.stringify({ pane_id: "w1:p4", screen: "deep" }), {
        status: 200,
      }),
    );
  });
  vi.stubGlobal("fetch", fetch);

  // reload 相当: URL に pane を持った状態で mount する。
  window.history.replaceState(null, "", "/agent?pane=w1%3Ap4");
  const first = render(App);
  expect(await screen.findByText("deep")).toBeTruthy();
  first.unmount();

  // 不在 pane は silent redirect せず、説明と一覧導線を出す。
  window.history.replaceState(null, "", "/agent?pane=w9%3Ap9");
  render(App);
  expect(
    await screen.findByText("この agent は見つかりません。", { exact: false }),
  ).toBeTruthy();
  expect(window.location.pathname).toBe("/agent");
  expect(screen.getByRole("button", { name: "一覧へ" })).toBeTruthy();
});
