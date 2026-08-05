import { fireEvent, render, screen } from "@testing-library/svelte";
import { expect, it, vi } from "vitest";
import AgentLetterComposer from "./AgentLetterComposer.svelte";
import type { Agent } from "./api";

const agent: Agent = {
  name: "codex",
  state: "idle",
  pane_id: "w1:p7",
  session: "work",
  backend: "herdr",
  location: "work:1.0",
  cwd: "/tmp/work",
};

function mailboxes(names: string[]): Response {
  return new Response(JSON.stringify({ mailboxes: names }), { status: 200 });
}

it("opens the sheet, focuses the body, and posts to the shown agent", async () => {
  const fetch = vi.fn((input: RequestInfo | URL, _init?: RequestInit) => {
    if (String(input) === "/api/mailboxes")
      return Promise.resolve(mailboxes(["review", "mobile"]));
    return Promise.resolve(
      new Response(
        JSON.stringify({
          version: 1,
          id: 41,
          path: "sent",
          to: "w1:p7",
          name: "codex",
        }),
        { status: 200 },
      ),
    );
  });
  vi.stubGlobal("fetch", fetch);
  render(AgentLetterComposer, { agent });

  // launcher は宛先の agent 名を出す。
  const launcher = await screen.findByRole("button", {
    name: /codex に手紙を出す/,
  });
  await fireEvent.click(launcher);

  // sheet が開き、mailbox 確認後に本文へ focus。source は mobile を優先する。
  const body = await screen.findByRole("textbox", {
    name: "codex への手紙の本文",
  });
  expect(await screen.findByText("mobile から")).toBeTruthy();
  await vi.waitFor(() => expect(document.activeElement).toBe(body));

  await fireEvent.input(body, { target: { value: "こんにちは" } });
  await fireEvent.click(screen.getByRole("button", { name: "手紙を出す" }));

  expect(await screen.findByText("送信しました #41")).toBeTruthy();
  // POST は表示中 agent の pane に固定される (宛先の選び直しをさせない)。
  const post = fetch.mock.calls.find(([url]) => String(url) === "/api/letters");
  expect(post).toBeTruthy();
  expect(JSON.parse(String(post![1]!.body))).toEqual({
    source: "mobile",
    target: "w1:p7",
    body: "こんにちは",
  });
  // 成功時だけ draft を消す。
  expect((body as HTMLTextAreaElement).value).toBe("");
});

it("keeps the draft on failure and closes back to the launcher on Escape", async () => {
  const fetch = vi.fn((input: RequestInfo | URL) => {
    if (String(input) === "/api/mailboxes")
      return Promise.resolve(mailboxes(["mobile"]));
    return Promise.resolve(
      new Response(JSON.stringify({ error: "target_not_found" }), {
        status: 404,
      }),
    );
  });
  vi.stubGlobal("fetch", fetch);
  render(AgentLetterComposer, { agent });

  await fireEvent.click(
    await screen.findByRole("button", { name: /codex に手紙を出す/ }),
  );
  const body = await screen.findByRole("textbox", {
    name: "codex への手紙の本文",
  });
  await fireEvent.input(body, { target: { value: "残る下書き" } });
  await fireEvent.click(screen.getByRole("button", { name: "手紙を出す" }));

  expect(
    await screen.findByText("送信できませんでした (target_not_found)"),
  ).toBeTruthy();
  expect((body as HTMLTextAreaElement).value).toBe("残る下書き");

  // Escape で閉じ、focus は launcher へ戻る。
  await fireEvent.keyDown(body, { key: "Escape" });
  const launcher = await screen.findByRole("button", {
    name: /codex に手紙を出す/,
  });
  expect(document.activeElement).toBe(launcher);
});

it("reports queued acceptance distinctly from sent", async () => {
  const fetch = vi.fn((input: RequestInfo | URL) => {
    if (String(input) === "/api/mailboxes")
      return Promise.resolve(mailboxes(["mobile"]));
    return Promise.resolve(
      new Response(
        JSON.stringify({
          version: 1,
          id: 55,
          path: "queued",
          to: "w1:p7",
          name: "codex",
        }),
        { status: 200 },
      ),
    );
  });
  vi.stubGlobal("fetch", fetch);
  render(AgentLetterComposer, { agent });

  await fireEvent.click(
    await screen.findByRole("button", { name: /codex に手紙を出す/ }),
  );
  const body = await screen.findByRole("textbox", {
    name: "codex への手紙の本文",
  });
  await fireEvent.input(body, { target: { value: "あとで読んで" } });
  await fireEvent.click(screen.getByRole("button", { name: "手紙を出す" }));

  expect(await screen.findByText("受理されました (配達待ち) #55")).toBeTruthy();
});

it("disables sending with a reason when no mailbox is allowed", async () => {
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(mailboxes([])));
  render(AgentLetterComposer, { agent });

  await fireEvent.click(
    await screen.findByRole("button", { name: /codex に手紙を出す/ }),
  );
  expect(
    await screen.findByText(/許可された mailbox が無いため送信できません/),
  ).toBeTruthy();
  expect(screen.queryByRole("button", { name: "手紙を出す" })).toBeNull();
  // textarea が無くても focus は dialog に入り、閉じる/Escape が効く。
  expect(document.activeElement).toBe(
    screen.getByRole("dialog", { name: "codex への手紙" }),
  );
});

it("blocks sending while mailboxes are still loading", async () => {
  let resolveMailboxes: (value: Response) => void = () => undefined;
  const fetch = vi.fn(
    () => new Promise<Response>((resolve) => (resolveMailboxes = resolve)),
  );
  vi.stubGlobal("fetch", fetch);
  render(AgentLetterComposer, { agent });

  await fireEvent.click(
    await screen.findByRole("button", { name: /codex に手紙を出す/ }),
  );
  // 取得中は進捗を出し、送信は不可。空の「 から」を出さない。
  expect(await screen.findByText("mailbox を確認中")).toBeTruthy();
  const submit = screen.getByRole("button", {
    name: "手紙を出す",
  }) as HTMLButtonElement;
  expect(submit.disabled).toBe(true);

  resolveMailboxes(mailboxes(["mobile"]));
  expect(await screen.findByText("mobile から")).toBeTruthy();
});

it("distinguishes a mailbox fetch failure and offers retry", async () => {
  const fetch = vi
    .fn()
    .mockRejectedValueOnce(new Error("offline"))
    .mockResolvedValueOnce(mailboxes(["mobile"]));
  vi.stubGlobal("fetch", fetch);
  render(AgentLetterComposer, { agent });

  await fireEvent.click(
    await screen.findByRole("button", { name: /codex に手紙を出す/ }),
  );
  // 取得失敗は allowlist 不備と別の文言で、再試行できる。
  expect(
    await screen.findByText(/mailbox を取得できませんでした/),
  ).toBeTruthy();
  expect(
    screen.queryByText(/許可された mailbox が無いため送信できません/),
  ).toBeNull();
  await fireEvent.click(screen.getByRole("button", { name: "再試行" }));
  expect(await screen.findByText("mobile から")).toBeTruthy();
});

it("keeps a per-pane draft across sibling switches", async () => {
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(mailboxes(["mobile"])));
  // agent A で下書き → B へ切替 ({#key} による remount) → A へ戻る。
  const first = render(AgentLetterComposer, { agent });
  await fireEvent.click(
    await screen.findByRole("button", { name: /codex に手紙を出す/ }),
  );
  const body = await screen.findByRole("textbox", {
    name: "codex への手紙の本文",
  });
  await fireEvent.input(body, { target: { value: "書きかけ" } });
  first.unmount();

  const sibling = render(AgentLetterComposer, {
    agent: { ...agent, name: "claude", pane_id: "w1:p2" },
  });
  // 別 pane へ下書きを持ち越さない。
  await fireEvent.click(
    await screen.findByRole("button", { name: /claude に手紙を出す/ }),
  );
  const siblingBody = await screen.findByRole("textbox", {
    name: "claude への手紙の本文",
  });
  expect((siblingBody as HTMLTextAreaElement).value).toBe("");
  sibling.unmount();

  render(AgentLetterComposer, { agent });
  // launcher が下書きの存在を示し、開くと本文が戻る。
  const restored = await screen.findByRole("button", {
    name: /codex に手紙を出す/,
  });
  expect(restored.textContent).toContain("下書きあり");
  await fireEvent.click(restored);
  const restoredBody = await screen.findByRole("textbox", {
    name: "codex への手紙の本文",
  });
  expect((restoredBody as HTMLTextAreaElement).value).toBe("書きかけ");
});
