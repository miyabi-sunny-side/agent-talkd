import type { Agent, MailboxEvent } from "./api";

export interface LetterGroup {
  /** workspace id (旧形式 pane はその pane id)。表示順・同一性の key。 */
  key: string;
  /** live agent の session label。居なければ key と同じ。 */
  label: string;
  /** id 降順 (新着が上)。 */
  events: MailboxEvent[];
}

/** herdr の pane id (`w2:p3`) から workspace prefix を取り出す。 */
export function workspaceOf(pane: string): string {
  const separator = pane.indexOf(":");
  return separator === -1 ? pane : pane.slice(0, separator);
}

/**
 * mailbox events を workspace (space) 単位にまとめる。group は最新 event の
 * id 降順、group 内も id 降順。label は live registry から引き、居なければ
 * workspace id のまま。
 */
export function groupLetters(
  events: MailboxEvent[],
  agents: Agent[],
): LetterGroup[] {
  const groups = new Map<string, MailboxEvent[]>();
  for (const event of [...events].sort((left, right) => right.id - left.id)) {
    const key = workspaceOf(event.target_pane);
    const bucket = groups.get(key);
    if (bucket === undefined) {
      groups.set(key, [event]);
    } else {
      bucket.push(event);
    }
  }
  const labels = new Map<string, string>();
  for (const agent of agents) {
    const key = workspaceOf(agent.pane_id);
    if (!labels.has(key)) labels.set(key, agent.session);
  }
  return [...groups.entries()].map(([key, grouped]) => ({
    key,
    label: labels.get(key) ?? key,
    events: grouped,
  }));
}

/** 追記 fetch の cursor。表示順に依存せず最大 id を使う。 */
export function nextCursor(events: MailboxEvent[]): number | undefined {
  let max: number | undefined;
  for (const event of events) {
    if (max === undefined || event.id > max) max = event.id;
  }
  return max;
}
