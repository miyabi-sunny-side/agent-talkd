export interface Agent {
  pane_id: string;
  name: string;
  workspace: string;
  cwd: string;
  harness: string;
  status: string;
  session_id: string | null;
  send_unavailable: string | null;
}
export interface Message {
  id: string;
  role: "user" | "assistant";
  text: string;
  timestamp?: string | null;
}
export interface Conversation {
  pane_id: string;
  session_id: string;
  messages: Message[];
  truncated: boolean;
}
export class ApiError extends Error {
  constructor(
    readonly code: string,
    message: string,
  ) {
    super(message);
  }
}
function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
function nullable(value: unknown): boolean {
  return value === null || typeof value === "string";
}
function invalid(): never {
  throw new ApiError("invalid_response", "応答形式が不正です");
}
async function request(path: string, init: RequestInit = {}): Promise<unknown> {
  let response: Response;
  try {
    response = await fetch(path, {
      cache: "no-store",
      ...init,
      headers: { Accept: "application/json", ...init.headers },
    });
  } catch {
    throw new ApiError("network", "接続できません");
  }
  const body: unknown = await response.json().catch(() => null);
  if (!response.ok) {
    if (
      record(body) &&
      record(body.error) &&
      typeof body.error.message === "string" &&
      typeof body.error.code === "string"
    )
      throw new ApiError(body.error.code, body.error.message);
    throw new ApiError(
      "http",
      `接続先がエラーを返しました (HTTP ${response.status})`,
    );
  }
  return body;
}
export async function fetchAgents(): Promise<Agent[]> {
  const body = await request("/api/agents");
  if (
    !record(body) ||
    !Array.isArray(body.agents) ||
    !body.agents.every(
      (a) =>
        record(a) &&
        ["pane_id", "name", "workspace", "cwd", "harness", "status"].every(
          (k) => typeof a[k] === "string",
        ) &&
        nullable(a.session_id) &&
        nullable(a.send_unavailable),
    )
  )
    invalid();
  return body.agents as Agent[];
}
export async function fetchConversation(
  pane: string,
  session: string,
): Promise<Conversation> {
  const body = await request(
    `/api/conversation?${new URLSearchParams({ pane, session })}`,
  );
  if (
    !record(body) ||
    body.pane_id !== pane ||
    body.session_id !== session ||
    typeof body.truncated !== "boolean" ||
    !Array.isArray(body.messages) ||
    !body.messages.every(
      (m) =>
        record(m) &&
        typeof m.id === "string" &&
        (m.role === "user" || m.role === "assistant") &&
        typeof m.text === "string" &&
        (m.timestamp === undefined || nullable(m.timestamp)),
    )
  )
    invalid();
  return body as unknown as Conversation;
}
export async function sendMessage(
  pane_id: string,
  session_id: string,
  body: string,
): Promise<void> {
  const result = await request("/api/messages", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ pane_id, session_id, body }),
  });
  if (!record(result) || result.status !== "submitted") invalid();
}
export function errorReason(error: unknown): string {
  return error instanceof Error ? error.message : "接続できません";
}
export function statusLabel(status: string): string {
  return (
    (
      {
        idle: "待機中",
        working: "作業中",
        done: "応答終了",
        blocked: "承認待ち",
        unknown: "状態不明",
      } as Record<string, string>
    )[status] ?? status
  );
}
const unavailableLabels: Record<string, string> = {
  unsupported: "この CLI は送信に未対応です。",
  unregistered: "対象セッションが未登録、または特定できません。",
  blocked: "対象は承認待ちのため、送信できません。",
  unknown: "対象の状態を確認できないため、送信できません。",
  unavailable: "対象を利用できないため、送信できません。",
};
export function unavailableReason(agent: Agent): string {
  if (agent.send_unavailable)
    return unavailableLabels[agent.send_unavailable] ?? agent.send_unavailable;
  if (!agent.session_id) return unavailableLabels.unregistered!;
  if (!["codex", "claude"].includes(agent.harness))
    return `${agent.harness} は送信に未対応です。`;
  if (!["idle", "working", "done"].includes(agent.status))
    return (
      unavailableLabels[agent.status] ??
      `現在の状態 (${agent.status}) では送信できません。`
    );
  return "";
}
