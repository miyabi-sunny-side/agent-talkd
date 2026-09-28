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
          older_cursor: null,
          next_cursor: "cursor-1",
          has_more: false,
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
          older_cursor: null,
          next_cursor: "cursor-1",
          has_more: false,
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
          older_cursor: null,
          next_cursor: "cursor-1",
          has_more: false,
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
          older_cursor: null,
          next_cursor: "cursor-1",
          has_more: false,
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
it("changes bounded pages only on explicit navigation and requests newer records from that page", async () => {
  fetcher.mockImplementation(async (path: string) => {
    const params = new URL(path, "http://localhost").searchParams;
    const old = params.has("before");
    return Response.json({
      pane_id: "w:p",
      session_id: "s",
      messages: [
        {
          id: old ? "1" : "2",
          role: "assistant",
          text: old ? "古い報告" : "新しい報告",
        },
      ],
      truncated: false,
      older_cursor: old ? null : "before",
      next_cursor: old ? "old-end" : "new-end",
      has_more: old ? false : true,
    });
  });
  render(ConversationPanel, { agent, available: true, connected: true });
  await screen.findByText("新しい報告");
  await fireEvent.click(screen.getByRole("button", { name: "古い会話" }));
  await screen.findByText("古い報告");
  expect(screen.queryByText("新しい報告")).toBeNull();
  await waitFor(() =>
    expect(
      (screen.getByRole("button", { name: "新しい会話" }) as HTMLButtonElement)
        .disabled,
    ).toBe(false),
  );
  const visibility = vi
    .spyOn(document, "visibilityState", "get")
    .mockReturnValue("visible");
  await fireEvent(document, new Event("visibilitychange"));
  await screen.findByText("新しい会話があります。過去の会話を表示しています。");
  expect(screen.getByText("古い報告")).toBeTruthy();
  visibility.mockRestore();
  await fireEvent.click(screen.getByRole("button", { name: "新しい会話" }));
  await screen.findByText("新しい報告");
  expect(
    fetcher.mock.calls.some(([path]) => String(path).includes("after=old-end")),
  ).toBe(true);
});

const imagePath = "/home/u/.cache/agent-talk/images/image-abc.png";
function history() {
  return Response.json({
    pane_id: "w:p",
    session_id: "s",
    messages: [],
    truncated: false,
    older_cursor: null,
    next_cursor: "cursor-1",
    has_more: false,
  });
}
function uploads() {
  return fetcher.mock.calls.filter((c) =>
    String(c[0]).startsWith("/api/images"),
  );
}
function png(name = "screen.png", size = 8) {
  return new File([new Uint8Array(size)], name, { type: "image/png" });
}
async function pick(files: File[]) {
  const input = document.querySelector<HTMLInputElement>("input[type=file]")!;
  await fireEvent.change(input, { target: { files } });
}
function body() {
  return (screen.getByRole("textbox", { name: "本文" }) as HTMLTextAreaElement)
    .value;
}
it("uploads a picked image, inserts its path on its own line and sends only on 送信", async () => {
  fetcher.mockImplementation(async (path: string) =>
    path.startsWith("/api/images")
      ? Response.json({ path: imagePath })
      : path === "/api/messages"
        ? Response.json({ status: "submitted" })
        : history(),
  );
  render(ConversationPanel, { agent, available: true, connected: true });
  await compose("一行目\n二行目");
  const file = png();
  await pick([file]);
  await waitFor(() => expect(body()).toBe(`一行目\n二行目\n${imagePath}\n`));
  const [path, init] = uploads()[0] as unknown as [string, RequestInit];
  expect(new URL(path, "http://x").searchParams.get("pane")).toBe("w:p");
  expect(new URL(path, "http://x").searchParams.get("session")).toBe("s");
  expect(init.body).toBe(file);
  expect((init.headers as Record<string, string>)["Content-Type"]).toBe(
    "image/png",
  );
  expect(
    fetcher.mock.calls.filter((c) => c[0] === "/api/messages"),
  ).toHaveLength(0);
  await screen.findByText(/24 時間後に自動で削除/);
  await fireEvent.input(screen.getByRole("textbox"), {
    target: { value: `${body()}上の画像を元に調査してください` },
  });
  await fireEvent.click(screen.getByRole("button", { name: "送信" }));
  await screen.findByText(/送信を受け付けました/);
  const sent = fetcher.mock.calls.find((c) => c[0] === "/api/messages");
  expect(
    JSON.parse((sent as unknown as [string, RequestInit])[1].body as string)
      .body,
  ).toBe(`一行目\n二行目\n${imagePath}\n上の画像を元に調査してください`);
});
it("leaves the draft untouched when the picker is cancelled or the file is rejected", async () => {
  render(ConversationPanel, { agent, available: true, connected: true });
  await compose("そのまま");
  await pick([]);
  await pick([new File(["x"], "photo.heic", { type: "image/heic" })]);
  await screen.findByText(/HEIC などは JPEG/);
  await pick([png("big.png", 20 * 1024 * 1024 + 1)]);
  await screen.findByText(/20 MiB まで/);
  await pick([new File(["x"], "note.txt", { type: "text/plain" })]);
  await screen.findByText(/PNG・JPEG・WebP/);
  expect(uploads()).toHaveLength(0);
  expect(body()).toBe("そのまま");
});
it("keeps the draft on upload failure and retries the same file", async () => {
  let fail = true;
  fetcher.mockImplementation(async (path: string) =>
    path.startsWith("/api/images")
      ? fail
        ? Response.json(
            {
              error: {
                code: "image_not_saved",
                message: "画像を保存できませんでした",
              },
            },
            { status: 507 },
          )
        : Response.json({ path: imagePath })
      : history(),
  );
  render(ConversationPanel, { agent, available: true, connected: true });
  await compose("下書き");
  await pick([png()]);
  await screen.findByText(/画像を保存できませんでした/);
  expect(body()).toBe("下書き");
  fail = false;
  await fireEvent.click(screen.getByRole("button", { name: "画像を再試行" }));
  await waitFor(() => expect(body()).toBe(`下書き\n${imagePath}\n`));
  expect(uploads()).toHaveLength(2);
  expect((uploads()[1] as unknown as [string, RequestInit])[1].body).toBe(
    (uploads()[0] as unknown as [string, RequestInit])[1].body,
  );
});
it("keeps edits made during upload and blocks sending until it finishes", async () => {
  let resolve!: (value: Response) => void;
  fetcher.mockImplementation(async (path: string) =>
    path.startsWith("/api/images")
      ? new Promise<Response>((r) => {
          resolve = r;
        })
      : history(),
  );
  render(ConversationPanel, { agent, available: true, connected: true });
  await compose("前");
  await pick([png()]);
  await screen.findByText(/画像をアップロードしています/);
  const send = screen.getByRole("button", {
    name: /送信/,
  }) as HTMLButtonElement;
  expect(send.disabled).toBe(true);
  await fireEvent.input(screen.getByRole("textbox"), {
    target: { value: "前\n通信中に書いた説明" },
  });
  resolve(Response.json({ path: imagePath }));
  await waitFor(() =>
    expect(body()).toBe(`前\n通信中に書いた説明\n${imagePath}\n`),
  );
  await waitFor(() => expect(send.disabled).toBe(false));
});
it("delivers a late upload only to the draft of the session it started from", async () => {
  let resolve!: (value: Response) => void;
  fetcher.mockImplementation(async (path: string) =>
    path.startsWith("/api/images")
      ? new Promise<Response>((r) => {
          resolve = r;
        })
      : history(),
  );
  const first = render(ConversationPanel, {
    agent,
    available: true,
    connected: true,
  });
  await compose("元の宛先");
  await pick([png()]);
  first.unmount();
  const other = render(ConversationPanel, {
    agent: { ...agent, pane_id: "w:q" },
    available: true,
    connected: true,
  });
  await compose("別の宛先");
  resolve(Response.json({ path: imagePath }));
  await new Promise((r) => setTimeout(r, 0));
  expect(body()).toBe("別の宛先");
  other.unmount();
  render(ConversationPanel, { agent, available: true, connected: true });
  await fireEvent.click(
    screen.getByRole("button", { name: "手紙・下書きあり" }),
  );
  expect(body()).toBe(`元の宛先\n${imagePath}\n`);
});
it("delivers a late upload to the same session opened again", async () => {
  let resolve!: (value: Response) => void;
  fetcher.mockImplementation(async (path: string) =>
    path.startsWith("/api/images")
      ? new Promise<Response>((r) => {
          resolve = r;
        })
      : history(),
  );
  const first = render(ConversationPanel, {
    agent,
    available: true,
    connected: true,
  });
  await compose("戻る前");
  await pick([png()]);
  first.unmount();
  render(ConversationPanel, { agent, available: true, connected: true });
  await compose("戻って編集");
  resolve(Response.json({ path: imagePath }));
  await waitFor(() => expect(body()).toBe(`戻って編集\n${imagePath}\n`));
});
