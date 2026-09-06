import { render, screen, fireEvent } from "@testing-library/svelte";
import { beforeEach, afterEach, expect, it, vi } from "vitest";
import App from "./App.svelte";
const agent = {
  pane_id: "w:p",
  name: "codex",
  workspace: "work",
  cwd: "/work",
  harness: "codex",
  status: "idle",
  session_id: "s",
  send_unavailable: null,
};
beforeEach(() => {
  window.history.replaceState(null, "", "/");
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string) =>
      path === "/api/agents"
        ? Response.json({ agents: [agent] })
        : Response.json({
            pane_id: "w:p",
            session_id: "s",
            messages: [],
            truncated: false,
            older_cursor: null,
            next_cursor: "cursor-1",
            has_more: false,
          }),
    ),
  );
});
afterEach(() => vi.unstubAllGlobals());
it("opens a target and renders its native conversation", async () => {
  render(App);
  await fireEvent.click(
    await screen.findByRole("button", { name: /codex.*idle/ }),
  );
  await screen.findByRole("region", { name: "会話・報告" });
  expect(window.location.search).toBe("?pane=w%3Ap");
  expect(screen.queryByText("Letters")).toBeNull();
});
it("keeps a missing deep-link and explains its absence", async () => {
  window.history.replaceState(null, "", "/agent?pane=missing");
  render(App);
  await screen.findByText(/この agent は見つかりません/);
  expect(window.location.search).toBe("?pane=missing");
});
