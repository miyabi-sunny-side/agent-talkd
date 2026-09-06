import { afterEach, expect, it, vi } from "vitest";
import { fetchAgents, fetchConversation, sendMessage } from "./api";
afterEach(() => vi.unstubAllGlobals());
it("loads registered remote targets", async () => {
  const agent = {
    pane_id: "w:p",
    name: "codex",
    workspace: "work",
    cwd: "/work",
    harness: "codex",
    status: "busy",
    session_id: "s",
    send_unavailable: null,
  };
  const fetcher = vi.fn().mockResolvedValue(Response.json({ agents: [agent] }));
  vi.stubGlobal("fetch", fetcher);
  expect(await fetchAgents()).toEqual([agent]);
  expect(fetcher.mock.calls[0][0]).toBe("/api/agents");
});
it("rejects a conversation from a replaced session", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValue(
      Response.json({
        pane_id: "w:p",
        session_id: "other",
        messages: [],
        truncated: false,
      }),
    ),
  );
  await expect(fetchConversation("w:p", "original")).rejects.toThrow(
    "応答形式",
  );
});
it("submits original text with the exact target session", async () => {
  const fetcher = vi
    .fn()
    .mockResolvedValue(Response.json({ status: "submitted" }));
  vi.stubGlobal("fetch", fetcher);
  await sendMessage("pane/α", "s", "  原文\n次の行  ");
  expect(JSON.parse(fetcher.mock.calls[0][1].body)).toEqual({
    pane_id: "pane/α",
    session_id: "s",
    body: "  原文\n次の行  ",
  });
});
it("surfaces structured errors without retrying a submission", async () => {
  const fetcher = vi
    .fn()
    .mockResolvedValue(
      Response.json(
        { error: { code: "blocked", message: "承認待ちです" } },
        { status: 409 },
      ),
    );
  vi.stubGlobal("fetch", fetcher);
  await expect(sendMessage("p", "s", "本文")).rejects.toThrow("承認待ちです");
  expect(fetcher).toHaveBeenCalledTimes(1);
});
