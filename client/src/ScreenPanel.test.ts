import { render, screen, fireEvent, waitFor } from "@testing-library/svelte";
import { afterEach, expect, it, vi } from "vitest";
import ScreenPanel from "./ScreenPanel.svelte";
afterEach(() => vi.unstubAllGlobals());
const props = {
  pane: "w1:p3",
  terminal: "terminal-3",
  session: "original",
  active: true,
  available: true,
  connected: true,
  changed: false,
};
function response(text = "one  two\n  indented") {
  return Response.json({
    pane_id: props.pane,
    terminal_id: props.terminal,
    session_id: props.session,
    text,
    captured_at: 1788700000000,
    format: "text",
  });
}
it("fetches only when visible and preserves stale text with an explicit state", async () => {
  const fetcher = vi.fn().mockImplementation(async () => response());
  vi.stubGlobal("fetch", fetcher);
  const view = render(ScreenPanel, { ...props, active: false });
  expect(fetcher).not.toHaveBeenCalled();
  await view.rerender(props);
  const terminal = await screen.findByTestId("terminal-text");
  expect(terminal.textContent).toBe("one  two\n  indented");
  await view.rerender({ ...props, connected: false });
  await screen.findByText(/接続が切れています.*以前の表示/);
  expect(terminal.textContent).toBe("one  two\n  indented");
  await view.rerender({ ...props, available: false });
  await screen.findByText(/対象は退出しました.*以前の表示/);
  await view.rerender({ ...props, changed: true });
  await screen.findByText(/別のセッション.*以前の表示/);
});
it("ignores a delayed response after hiding and refreshes when reopened", async () => {
  let resolve!: (value: Response) => void;
  const fetcher = vi
    .fn()
    .mockImplementationOnce(
      () =>
        new Promise<Response>((r) => {
          resolve = r;
        }),
    )
    .mockImplementation(async () => response("new display"));
  vi.stubGlobal("fetch", fetcher);
  const view = render(ScreenPanel, props);
  await waitFor(() => expect(fetcher).toHaveBeenCalledTimes(1));
  await view.rerender({ ...props, active: false });
  resolve(response("old display"));
  await new Promise((r) => setTimeout(r, 0));
  expect(screen.queryByText("old display")).toBeNull();
  await view.rerender(props);
  await screen.findByText("new display");
  fetcher.mockRejectedValue(new Error("offline"));
  await fireEvent.click(screen.getByRole("button", { name: "画面を更新" }));
  await screen.findByText(/画面を取得できません.*以前の表示/);
  expect(screen.getByText("new display")).toBeTruthy();
});

it("reads an unregistered or unsupported terminal without a native session", async () => {
  const fetcher = vi.fn(async (_path: string) =>
    Response.json({
      pane_id: props.pane,
      terminal_id: props.terminal,
      session_id: null,
      text: "waiting for setup",
      format: "text",
      captured_at: 1788700000000,
    }),
  );
  vi.stubGlobal("fetch", fetcher);
  render(ScreenPanel, { ...props, session: null });
  await screen.findByText("waiting for setup");
  const query = new URL(
    fetcher.mock.calls[0]![0] as unknown as string,
    "http://localhost",
  ).searchParams;
  expect(query.get("terminal")).toBe(props.terminal);
  expect(query.has("session")).toBe(false);
});
