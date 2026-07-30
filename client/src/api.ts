export type AgentState = "idle" | "busy";

export interface Agent {
  name: string;
  state: AgentState;
  pane_id: string;
  session: string;
  location: string;
  cwd: string;
}

interface WhoResponse {
  agents: Agent[];
}

export async function fetchAgents(signal?: AbortSignal): Promise<Agent[]> {
  const response = await fetch("/v1/who", {
    headers: { Accept: "application/json" },
    signal,
  });
  if (!response.ok)
    throw new Error(`registry request failed (${response.status})`);
  const body: unknown = await response.json();
  if (!isWhoResponse(body))
    throw new Error("registry returned an invalid response");
  return body.agents;
}

function isWhoResponse(value: unknown): value is WhoResponse {
  if (!value || typeof value !== "object" || !("agents" in value)) return false;
  const agents = (value as { agents: unknown }).agents;
  return Array.isArray(agents) && agents.every(isAgent);
}

function isAgent(value: unknown): value is Agent {
  if (!value || typeof value !== "object") return false;
  const agent = value as Record<string, unknown>;
  return (
    ["name", "pane_id", "session", "location", "cwd"].every(
      (key) => typeof agent[key] === "string",
    ) &&
    (agent.state === "idle" || agent.state === "busy")
  );
}
