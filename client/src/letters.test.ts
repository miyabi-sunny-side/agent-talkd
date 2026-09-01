import { describe, expect, it } from "vitest";
import { groupLetters, nextCursor, workspaceOf } from "./letters";
import type { Agent, MailboxEvent } from "./api";

function event(
  overrides: Partial<MailboxEvent> & { id: number },
): MailboxEvent {
  return {
    created_at: "2026-09-01T09:00:00+09:00",
    mailbox: "mobile",
    source_label: "mobile",
    direction: "out",
    body: "本文",
    skill: null,
    target_name: "claude",
    target_pane: "w1:p1",
    reply_to: null,
    ...overrides,
  };
}

function agent(overrides: Partial<Agent> & { pane_id: string }): Agent {
  return {
    name: "claude",
    state: "idle",
    session: "knowledge",
    location: "0",
    cwd: "/home",
    backend: "herdr",
    ...overrides,
  };
}

describe("workspaceOf", () => {
  it("herdr 形式の pane から workspace prefix を取り出す", () => {
    expect(workspaceOf("w2:p3")).toBe("w2");
  });

  it("区切りの無い旧形式 pane は全体を workspace として扱う", () => {
    expect(workspaceOf("%1")).toBe("%1");
  });
});

describe("groupLetters", () => {
  it("workspace 単位でまとめ、group も group 内も id 降順に並べる", () => {
    const events = [
      event({ id: 1, target_pane: "w1:p1" }),
      event({ id: 2, target_pane: "w2:p1", target_name: "codex" }),
      event({ id: 3, target_pane: "w1:p2", direction: "in", reply_to: 1 }),
    ];
    const groups = groupLetters(events, []);
    expect(groups.map((group) => group.key)).toEqual(["w1", "w2"]);
    expect(groups[0].events.map((item) => item.id)).toEqual([3, 1]);
    expect(groups[1].events.map((item) => item.id)).toEqual([2]);
  });

  it("live agent がいる workspace は session label を見出しにする", () => {
    const groups = groupLetters(
      [event({ id: 1, target_pane: "w2:p3" })],
      [agent({ pane_id: "w2:p1", session: "knowledge" })],
    );
    expect(groups[0].label).toBe("knowledge");
  });

  it("live agent が居ない workspace は workspace id へ fallback する", () => {
    const groups = groupLetters(
      [event({ id: 1, target_pane: "w9:p1" })],
      [agent({ pane_id: "w2:p1", session: "knowledge" })],
    );
    expect(groups[0].label).toBe("w9");
  });

  it("空の入力は空の group 一覧を返す", () => {
    expect(groupLetters([], [])).toEqual([]);
  });
});

describe("nextCursor", () => {
  it("表示順に関係なく最大 id を cursor にする", () => {
    const events = [event({ id: 7 }), event({ id: 3 }), event({ id: 5 })];
    expect(nextCursor(events)).toBe(7);
  });

  it("event が無ければ undefined を返す", () => {
    expect(nextCursor([])).toBeUndefined();
  });
});
