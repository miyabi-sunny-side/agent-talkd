export type AgentState = "idle" | "busy";

export type Backend = "herdr";

export interface Agent {
  name: string;
  state: AgentState;
  pane_id: string;
  session: string;
  location: string;
  cwd: string;
  backend: Backend;
}

export interface ScreenCapture {
  pane_id: string;
  screen: string;
}

export type MailboxDirection = "in" | "out";

export interface MailboxEvent {
  id: number;
  created_at: string;
  mailbox: string;
  source_label: string;
  direction: MailboxDirection;
  body: string;
  skill: string | null;
  target_name: string;
  target_pane: string;
  reply_to: number | null;
}

interface WhoResponse {
  agents: Agent[];
}

interface MailboxesResponse {
  mailboxes: string[];
}

export interface MailboxResponse {
  version: 1;
  mailbox: string;
  events: MailboxEvent[];
}

async function getJson(path: string, signal?: AbortSignal): Promise<unknown> {
  const response = await fetch(path, {
    headers: { Accept: "application/json" },
    signal,
  });
  if (!response.ok) throw new Error(`request failed (${response.status})`);
  return response.json() as Promise<unknown>;
}

export async function fetchAgents(signal?: AbortSignal): Promise<Agent[]> {
  const body = await getJson("/api/who", signal);
  if (!isWhoResponse(body))
    throw new Error("registry returned an invalid response");
  return body.agents;
}

export async function fetchScreen(
  paneId: string,
  signal?: AbortSignal,
): Promise<ScreenCapture> {
  const body = await getJson(
    `/api/agents/${encodeURIComponent(paneId)}/screen`,
    signal,
  );
  if (!isScreenCapture(body) || body.pane_id !== paneId)
    throw new Error("screen returned an invalid response");
  return body;
}

export async function fetchMailboxes(signal?: AbortSignal): Promise<string[]> {
  const body = await getJson("/api/mailboxes", signal);
  if (!isMailboxesResponse(body))
    throw new Error("mailboxes returned an invalid response");
  return body.mailboxes;
}

export async function fetchMailbox(
  mailbox: string,
  options: { after?: number; limit?: number; signal?: AbortSignal } = {},
): Promise<MailboxResponse> {
  const query = new URLSearchParams();
  if (options.after !== undefined) query.set("after", String(options.after));
  if (options.limit !== undefined) query.set("limit", String(options.limit));
  const suffix = query.size === 0 ? "" : `?${query}`;
  const body = await getJson(
    `/api/mailbox/${encodeURIComponent(mailbox)}${suffix}`,
    options.signal,
  );
  if (!isMailboxResponse(body) || body.mailbox !== mailbox)
    throw new Error("mailbox returned an invalid response");
  return body;
}

export interface LetterAccepted {
  version: 1;
  id: number;
  path: "sent" | "queued";
  to: string;
  name: string;
}

export async function fetchSkills(
  paneId: string,
  signal?: AbortSignal,
): Promise<string[]> {
  const body = await getJson(
    `/api/agents/${encodeURIComponent(paneId)}/skills`,
    signal,
  );
  if (!isSkillsResponse(body))
    throw new Error("skills returned an invalid response");
  return body.skills;
}

export async function sendLetter(
  source: string,
  target: string,
  body: string,
  skill: string | null = null,
): Promise<LetterAccepted> {
  const payload: {
    source: string;
    target: string;
    body: string;
    skill?: string;
  } = { source, target, body };
  if (skill) payload.skill = skill;
  const response = await fetch("/api/letters", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });
  if (!response.ok) {
    const detail = (await response.json().catch(() => null)) as {
      error?: string;
    } | null;
    throw new Error(detail?.error ?? `letter failed (${response.status})`);
  }
  const accepted = (await response.json()) as unknown;
  if (!isLetterAccepted(accepted))
    throw new Error("letters returned an invalid response");
  return accepted;
}

function isSkillsResponse(value: unknown): value is { skills: string[] } {
  return (
    isRecord(value) &&
    Array.isArray(value.skills) &&
    value.skills.every((skill) => typeof skill === "string")
  );
}

function isLetterAccepted(value: unknown): value is LetterAccepted {
  return (
    isRecord(value) &&
    value.version === 1 &&
    Number.isSafeInteger(value.id) &&
    (value.path === "sent" || value.path === "queued") &&
    typeof value.to === "string" &&
    typeof value.name === "string"
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function isWhoResponse(value: unknown): value is WhoResponse {
  return (
    isRecord(value) &&
    Array.isArray(value.agents) &&
    value.agents.every(isAgent)
  );
}

function isAgent(value: unknown): value is Agent {
  if (!isRecord(value)) return false;
  return (
    ["name", "pane_id", "session", "location", "cwd"].every(
      (key) => typeof value[key] === "string",
    ) &&
    (value.state === "idle" || value.state === "busy") &&
    value.backend === "herdr"
  );
}

function isScreenCapture(value: unknown): value is ScreenCapture {
  return (
    isRecord(value) &&
    typeof value.pane_id === "string" &&
    typeof value.screen === "string"
  );
}

function isMailboxesResponse(value: unknown): value is MailboxesResponse {
  return (
    isRecord(value) &&
    Array.isArray(value.mailboxes) &&
    value.mailboxes.every((mailbox) => typeof mailbox === "string")
  );
}

function isNullableString(value: unknown): value is string | null {
  return value === null || typeof value === "string";
}

function isNullableId(value: unknown): value is number | null {
  return value === null || (Number.isSafeInteger(value) && Number(value) >= 0);
}

function isMailboxEvent(value: unknown): value is MailboxEvent {
  if (!isRecord(value)) return false;
  return (
    Number.isSafeInteger(value.id) &&
    Number(value.id) >= 0 &&
    [
      "created_at",
      "mailbox",
      "source_label",
      "body",
      "target_name",
      "target_pane",
    ].every((key) => typeof value[key] === "string") &&
    (value.direction === "in" || value.direction === "out") &&
    isNullableString(value.skill) &&
    isNullableId(value.reply_to)
  );
}

function isMailboxResponse(value: unknown): value is MailboxResponse {
  return (
    isRecord(value) &&
    value.version === 1 &&
    typeof value.mailbox === "string" &&
    Array.isArray(value.events) &&
    value.events.every(isMailboxEvent)
  );
}
