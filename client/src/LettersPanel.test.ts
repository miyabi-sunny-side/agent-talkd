import { fireEvent, render, screen } from "@testing-library/svelte";
import { expect, it, vi } from "vitest";
import LettersPanel from "./LettersPanel.svelte";

function json(value: unknown): Response {
  return new Response(JSON.stringify(value), { status: 200 });
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
  const fetch = vi
    .fn()
    .mockResolvedValueOnce(json({ mailboxes: ["mobile"] }))
    .mockResolvedValueOnce(
      json({
        version: 1,
        mailbox: "mobile",
        events: [
          event(12, "in", "incoming request"),
          event(13, "out", "outgoing answer"),
        ],
      }),
    )
    .mockResolvedValueOnce(json({ version: 1, mailbox: "mobile", events: [] }));
  vi.stubGlobal("fetch", fetch);
  render(LettersPanel);

  expect(await screen.findByText("incoming request")).toBeTruthy();
  expect(screen.getByText("outgoing answer")).toBeTruthy();
  expect(screen.getByText("IN")).toBeTruthy();
  expect(screen.getByText("OUT")).toBeTruthy();
  await fireEvent.click(screen.getByRole("button", { name: "更新" }));
  await vi.waitFor(() => expect(fetch).toHaveBeenCalledTimes(3));
  expect(fetch.mock.calls[2]?.[0]).toBe(
    "/api/mailbox/mobile?after=13&limit=100",
  );
});

it("offers retry when mailbox discovery fails", async () => {
  const fetch = vi
    .fn()
    .mockRejectedValueOnce(new Error("offline"))
    .mockResolvedValueOnce(json({ mailboxes: [] }));
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
  const fetch = vi
    .fn()
    .mockResolvedValueOnce(json({ mailboxes: ["mobile", "desktop"] }))
    .mockReturnValueOnce(mobileResponse)
    .mockResolvedValueOnce(
      json({
        version: 1,
        mailbox: "desktop",
        events: [event(20, "in", "new desktop letter", "desktop")],
      }),
    );
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
