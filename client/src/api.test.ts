import { describe, expect, it, vi } from "vitest";
import {
  fetchAgents,
  fetchMailbox,
  fetchMailboxes,
  fetchScreen,
  sendLetter,
} from "./api";

function json(value: unknown, status = 200): Response {
  return new Response(JSON.stringify(value), { status });
}

describe("HTTP API", () => {
  it("validates agents and encodes pane ids", async () => {
    const fetch = vi
      .fn()
      .mockResolvedValueOnce(
        json({
          agents: [
            {
              name: "codex",
              state: "idle",
              pane_id: "w1:p1",
              session: "dev",
              location: "dev:0.1",
              cwd: "/tmp/project with spaces",
              backend: "herdr",
            },
          ],
        }),
      )
      .mockResolvedValueOnce(json({ pane_id: "w1:p1", screen: "safe text" }));
    vi.stubGlobal("fetch", fetch);

    await expect(fetchAgents()).resolves.toHaveLength(1);
    await expect(fetchScreen("w1:p1")).resolves.toEqual({
      pane_id: "w1:p1",
      screen: "safe text",
    });
    expect(fetch.mock.calls[1]?.[0]).toBe("/api/agents/w1%3Ap1/screen");
  });

  it("builds incremental mailbox queries and validates event fields", async () => {
    const fetch = vi
      .fn()
      .mockResolvedValueOnce(json({ mailboxes: ["mobile"] }))
      .mockResolvedValueOnce(
        json({
          version: 1,
          mailbox: "mobile",
          events: [
            {
              id: 12,
              created_at: "2026-07-21T11:00:00Z",
              mailbox: "mobile",
              source_label: "mobile",
              direction: "in",
              body: "依頼",
              skill: null,
              target_name: "claude",
              target_pane: "w1:p1",
              reply_to: null,
            },
          ],
        }),
      );
    vi.stubGlobal("fetch", fetch);

    await expect(fetchMailboxes()).resolves.toEqual(["mobile"]);
    await expect(
      fetchMailbox("mobile", { after: 9, limit: 100 }),
    ).resolves.toMatchObject({
      version: 1,
      mailbox: "mobile",
    });
    expect(fetch.mock.calls[1]?.[0]).toBe(
      "/api/mailbox/mobile?after=9&limit=100",
    );
  });

  it("rejects unsuccessful, mismatched, and malformed responses", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({}, 503)));
    await expect(fetchAgents()).rejects.toThrow("503");

    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(json({ pane_id: "w1:p2", screen: "" })),
    );
    await expect(fetchScreen("w1:p1")).rejects.toThrow("invalid response");

    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({ mailboxes: [7] })));
    await expect(fetchMailboxes()).rejects.toThrow("invalid response");

    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        json({
          version: 1,
          mailbox: "mobile",
          events: [{ id: 1, direction: "sideways" }],
        }),
      ),
    );
    await expect(fetchMailbox("mobile")).rejects.toThrow("invalid response");
  });
});

describe("letters", () => {
  it("posts a letter as JSON and validates the accepted response", async () => {
    const fetch = vi
      .fn()
      .mockResolvedValueOnce(
        json({ version: 1, id: 9, path: "sent", to: "w1:p1", name: "claude" }),
      );
    vi.stubGlobal("fetch", fetch);

    await expect(sendLetter("mobile", "w1:p1", "hello")).resolves.toEqual({
      version: 1,
      id: 9,
      path: "sent",
      to: "w1:p1",
      name: "claude",
    });
    const [url, init] = fetch.mock.calls[0] as [string, RequestInit];
    expect(url).toBe("/api/letters");
    expect(init.method).toBe("POST");
    expect((init.headers as Record<string, string>)["Content-Type"]).toBe(
      "application/json",
    );
    expect(JSON.parse(String(init.body))).toEqual({
      source: "mobile",
      target: "w1:p1",
      body: "hello",
    });
  });

  it("surfaces the server error code on rejection", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValueOnce(json({ error: "source_not_allowed" }, 403)),
    );
    await expect(sendLetter("mobile", "w1:p1", "hello")).rejects.toThrow(
      "source_not_allowed",
    );
  });
});
