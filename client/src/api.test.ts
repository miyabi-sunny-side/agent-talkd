import { describe, expect, it, vi } from "vitest";
import { fetchAgents } from "./api";

describe("fetchAgents", () => {
  it("returns a validated agent list", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        new Response(
          JSON.stringify({
            agents: [
              {
                name: "codex",
                state: "idle",
                pane_id: "%1",
                session: "dev",
                location: "dev:0.1",
                cwd: "/tmp/project with spaces",
              },
            ],
          }),
          { status: 200 },
        ),
      ),
    );
    await expect(fetchAgents()).resolves.toEqual([
      expect.objectContaining({
        name: "codex",
        cwd: "/tmp/project with spaces",
      }),
    ]);
  });

  it("rejects malformed and unsuccessful responses", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(new Response("{}", { status: 200 })),
    );
    await expect(fetchAgents()).rejects.toThrow("invalid response");
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(new Response("", { status: 503 })),
    );
    await expect(fetchAgents()).rejects.toThrow("503");
  });
});
