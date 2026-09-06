import { render, screen, fireEvent, waitFor } from "@testing-library/svelte";
import { beforeEach, afterEach, expect, it, vi } from "vitest";
import ConversationPanel from "./ConversationPanel.svelte";
import type { Agent } from "./api";
const agent: Agent = {
  pane_id: "w:p",
  name: "codex",
  workspace: "work",
  cwd: "/work",
  harness: "codex",
  status: "working",
  session_id: "s",
  send_unavailable: null,
};
let fetcher: ReturnType<typeof vi.fn>;
beforeEach(() => {
  sessionStorage.clear();
  fetcher = vi.fn(async (path: string) =>
    path === "/api/messages"
      ? Response.json({ status: "submitted" })
      : Response.json({
          pane_id: "w:p",
          session_id: "s",
          messages: [{ id: "1", role: "assistant", text: "実際の報告" }],
          truncated: false,
        }),
  );
  vi.stubGlobal("fetch", fetcher);
});
afterEach(() => vi.unstubAllGlobals());
async function compose(text: string) {
  await fireEvent.click(screen.getByRole("button", { name: /手紙/ }));
  await fireEvent.input(screen.getByRole("textbox", { name: "本文" }), {
    target: { value: text },
  });
}
it("shows native reports and submits original text without inventing a report", async () => {
  render(ConversationPanel, { agent, available: true, connected: true });
  await screen.findByText("実際の報告");
  await compose(" 原文\n追加指示 ");
  await fireEvent.click(screen.getByRole("button", { name: "送信" }));
  await screen.findByText(/送信を受け付けました/);
  expect(screen.queryByText("処理完了")).toBeNull();
  const call = fetcher.mock.calls.find((c) => c[0] === "/api/messages");
  expect(
    JSON.parse((call as unknown as [string, RequestInit])[1].body as string)
      .body,
  ).toBe(" 原文\n追加指示 ");
});
it("preserves failed drafts and never automatically resends", async () => {
  fetcher.mockImplementation(async (path: string) =>
    path === "/api/messages"
      ? Response.json(
          { error: { code: "blocked", message: "承認待ち" } },
          { status: 409 },
        )
      : Response.json({
          pane_id: "w:p",
          session_id: "s",
          messages: [],
          truncated: false,
        }),
  );
  render(ConversationPanel, { agent, available: true, connected: true });
  await compose("下書き");
  await fireEvent.click(screen.getByRole("button", { name: "送信" }));
  await screen.findByText(/承認待ち/);
  expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe(
    "下書き",
  );
  expect(
    fetcher.mock.calls.filter((c) => c[0] === "/api/messages"),
  ).toHaveLength(1);
});
it("blocks a replacement session and keeps the previous draft", async () => {
  const { rerender } = render(ConversationPanel, {
    agent,
    available: true,
    connected: true,
  });
  await compose("古い対象への下書き");
  await rerender({
    agent: { ...agent, session_id: "new" },
    available: true,
    connected: true,
  });
  await screen.findAllByText(/別のセッション/);
  expect(
    (screen.getByRole("button", { name: "送信" }) as HTMLButtonElement)
      .disabled,
  ).toBe(true);
  expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe(
    "古い対象への下書き",
  );
});
it("does not clear edits made while sending", async () => {
  let resolve!: (value: Response) => void;
  fetcher.mockImplementation(async (path: string) =>
    path === "/api/messages"
      ? new Promise<Response>((r) => {
          resolve = r;
        })
      : Response.json({
          pane_id: "w:p",
          session_id: "s",
          messages: [],
          truncated: false,
        }),
  );
  render(ConversationPanel, { agent, available: true, connected: true });
  await compose("送る分");
  await fireEvent.click(screen.getByRole("button", { name: "送信" }));
  await fireEvent.input(screen.getByRole("textbox"), {
    target: { value: "次の下書き" },
  });
  resolve(Response.json({ status: "submitted" }));
  await screen.findByText(/送信を受け付けました/);
  await waitFor(() =>
    expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe(
      "次の下書き",
    ),
  );
});

it("restores drafts for the same session after leaving and keeps another session empty", async () => {
  const first = render(ConversationPanel, {
    agent,
    available: true,
    connected: true,
  });
  await compose("保存する下書き");
  first.unmount();
  const second = render(ConversationPanel, {
    agent: { ...agent, session_id: "other" },
    available: true,
    connected: true,
  });
  await fireEvent.click(screen.getByRole("button", { name: "手紙" }));
  expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe("");
  second.unmount();
  render(ConversationPanel, { agent, available: true, connected: true });
  await fireEvent.click(
    screen.getByRole("button", { name: "手紙・下書きあり" }),
  );
  expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe(
    "保存する下書き",
  );
});
it("keeps reports and drafts on disconnection and departure", async () => {
  const view = render(ConversationPanel, {
    agent,
    available: true,
    connected: true,
  });
  await screen.findByText("実際の報告");
  await compose("追加指示");
  await view.rerender({ agent, available: true, connected: false });
  expect(
    (screen.getByRole("button", { name: "送信" }) as HTMLButtonElement)
      .disabled,
  ).toBe(true);
  expect(screen.getByText("実際の報告")).toBeTruthy();
  await view.rerender({ agent, available: false, connected: true });
  expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe(
    "追加指示",
  );
  await screen.findAllByText(/対象は退出/);
});
it("does not submit on Enter or IME confirmation", async () => {
  render(ConversationPanel, { agent, available: true, connected: true });
  await compose("改行を含む指示");
  await fireEvent.keyDown(screen.getByRole("textbox"), {
    key: "Enter",
    isComposing: true,
  });
  await fireEvent.keyDown(screen.getByRole("textbox"), { key: "Enter" });
  expect(
    fetcher.mock.calls.filter((c) => c[0] === "/api/messages"),
  ).toHaveLength(0);
});
it("keeps a newly edited draft when an earlier unmounted sender finishes", async () => {
  let resolve!: (response: Response) => void;
  fetcher.mockImplementation(async (path: string) =>
    path === "/api/messages"
      ? new Promise<Response>((r) => {
          resolve = r;
        })
      : Response.json({
          pane_id: "w:p",
          session_id: "s",
          messages: [],
          truncated: false,
        }),
  );
  const first = render(ConversationPanel, {
    agent,
    available: true,
    connected: true,
  });
  await compose("送信分");
  await fireEvent.click(screen.getByRole("button", { name: "送信" }));
  first.unmount();
  const second = render(ConversationPanel, {
    agent,
    available: true,
    connected: true,
  });
  await compose("戻って編集した下書き");
  resolve(Response.json({ status: "submitted" }));
  await new Promise((r) => setTimeout(r, 0));
  second.unmount();
  render(ConversationPanel, { agent, available: true, connected: true });
  await fireEvent.click(screen.getByRole("button", { name: /手紙/ }));
  expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe(
    "戻って編集した下書き",
  );
});
