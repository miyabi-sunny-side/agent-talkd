import { fireEvent, render, screen } from "@testing-library/svelte";
import { expect, it, vi } from "vitest";
import LettersPanel from "./LettersPanel.svelte";

function json(value: unknown, status = 200): Response {
  return new Response(JSON.stringify(value), { status });
}

function agent(paneId: string, name: string) {
  return {
    name,
    state: "idle",
    pane_id: paneId,
    session: "knowledge",
    location: "knowledge:1.1",
    cwd: "/tmp/knowledge",
    backend: "herdr",
  };
}

function event(
  id: number,
  direction: "in" | "out",
  body: string,
  mailbox = "mobile",
) {
  return {
    id,
    created_at: "2026-07-21T11:00:00Z",
    mailbox,
    source_label: mailbox,
    direction,
    body,
    skill: null,
    target_name: "claude",
    target_pane: "%1",
    reply_to: null,
  };
}

it("discovers mailboxes, distinguishes directions, and requests incremental events", async () => {
  let mailboxCalls = 0;
  const fetch = vi.fn((input: RequestInfo | URL) => {
    const url = String(input);
    if (url === "/api/mailboxes")
      return Promise.resolve(json({ mailboxes: ["mobile"] }));
    if (url === "/api/who")
      return Promise.resolve(json({ agents: [agent("w2:p1", "claude")] }));
    mailboxCalls += 1;
    return Promise.resolve(
      mailboxCalls === 1
        ? json({
            version: 1,
            mailbox: "mobile",
            events: [
              event(12, "in", "incoming request"),
              event(13, "out", "outgoing answer"),
            ],
          })
        : json({ version: 1, mailbox: "mobile", events: [] }),
    );
  });
  vi.stubGlobal("fetch", fetch);
  render(LettersPanel);

  expect(await screen.findByText("incoming request")).toBeTruthy();
  expect(screen.getByText("outgoing answer")).toBeTruthy();
  expect(screen.getByText("IN")).toBeTruthy();
  expect(screen.getByText("OUT")).toBeTruthy();
  await fireEvent.click(screen.getByRole("button", { name: "更新" }));
  await vi.waitFor(() => expect(mailboxCalls).toBe(2));
  const mailboxUrls = fetch.mock.calls
    .map((call) => String(call[0]))
    .filter((url) => url.startsWith("/api/mailbox/"));
  expect(mailboxUrls.at(-1)).toBe("/api/mailbox/mobile?after=13&limit=100");
});

it("offers retry when mailbox discovery fails", async () => {
  let discoveries = 0;
  const fetch = vi.fn((input: RequestInfo | URL) => {
    const url = String(input);
    if (url === "/api/who") return Promise.resolve(json({ agents: [] }));
    if (url === "/api/mailboxes") {
      discoveries += 1;
      return discoveries === 1
        ? Promise.reject(new Error("offline"))
        : Promise.resolve(json({ mailboxes: [] }));
    }
    return Promise.resolve(json({ version: 1, mailbox: "", events: [] }));
  });
  vi.stubGlobal("fetch", fetch);
  render(LettersPanel);

  const retry = await screen.findByRole("button", { name: "再試行" });
  await fireEvent.click(retry);
  expect(await screen.findByText("mailbox はありません")).toBeTruthy();
});

it("does not let an older mailbox response overwrite a new selection", async () => {
  let resolveMobile: (value: Response) => void = () => undefined;
  const mobileResponse = new Promise<Response>((resolve) => {
    resolveMobile = resolve;
  });
  const fetch = vi.fn((input: RequestInfo | URL) => {
    const url = String(input);
    if (url === "/api/mailboxes")
      return Promise.resolve(json({ mailboxes: ["mobile", "desktop"] }));
    if (url === "/api/who") return Promise.resolve(json({ agents: [] }));
    if (url.startsWith("/api/mailbox/mobile")) return mobileResponse;
    return Promise.resolve(
      json({
        version: 1,
        mailbox: "desktop",
        events: [event(20, "in", "new desktop letter", "desktop")],
      }),
    );
  });
  vi.stubGlobal("fetch", fetch);
  render(LettersPanel);

  const mailbox = await screen.findByRole("combobox", { name: "mailbox" });
  await fireEvent.change(mailbox, { target: { value: "desktop" } });
  expect(await screen.findByText("new desktop letter")).toBeTruthy();

  resolveMobile(
    json({
      version: 1,
      mailbox: "mobile",
      events: [event(19, "in", "stale mobile letter")],
    }),
  );
  await Promise.resolve();
  await Promise.resolve();
  expect(screen.queryByText("stale mobile letter")).toBeNull();
  expect(screen.getByText("new desktop letter")).toBeTruthy();
});

it("composes a letter, posts it, and refreshes the history", async () => {
  let mailboxCalls = 0;
  const fetch = vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url === "/api/mailboxes")
      return Promise.resolve(json({ mailboxes: ["mobile"] }));
    if (url === "/api/who")
      return Promise.resolve(
        json({ agents: [agent("w2:p1", "claude"), agent("w2:p4", "codex")] }),
      );
    if (url === "/api/letters") {
      expect(init?.method).toBe("POST");
      expect(JSON.parse(String(init?.body))).toEqual({
        source: "mobile",
        target: "w2:p1",
        body: "査収ください",
      });
      return Promise.resolve(
        json({ version: 1, id: 30, path: "sent", to: "w2:p1", name: "claude" }),
      );
    }
    mailboxCalls += 1;
    return Promise.resolve(
      json({
        version: 1,
        mailbox: "mobile",
        events: mailboxCalls === 1 ? [] : [event(30, "out", "査収ください")],
      }),
    );
  });
  vi.stubGlobal("fetch", fetch);
  render(LettersPanel);

  const body = await screen.findByLabelText("手紙の本文");
  await fireEvent.input(body, { target: { value: "査収ください" } });
  await fireEvent.submit(
    screen.getByRole("button", { name: "手紙を出す" }).closest("form")!,
  );

  expect(await screen.findByText("送信しました #30 → claude")).toBeTruthy();
  // 送信後に履歴が更新され、out event が現れる。
  expect(await screen.findByText("査収ください")).toBeTruthy();
  // 本文はクリアされる。
  expect((body as HTMLTextAreaElement).value).toBe("");
});
