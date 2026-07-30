<script lang="ts">
  import { onMount } from "svelte";
  import { fetchAgents, type Agent } from "./api";

  let agents = $state<Agent[]>([]);
  let phase = $state<"loading" | "error" | "ready">("loading");
  let message = $state("");

  async function refresh(): Promise<void> {
    phase = "loading";
    message = "agent registry を確認中";
    try {
      agents = await fetchAgents();
      phase = "ready";
      message =
        agents.length === 0
          ? "登録中の agent はありません"
          : `${agents.length} agent`;
    } catch {
      phase = "error";
      message = "agent registry を取得できませんでした";
    }
  }

  onMount(refresh);
</script>

<svelte:head><title>agent talk · registry</title></svelte:head>

<main>
  <header class="masthead">
    <div>
      <span class="eyebrow">TMUX / LOCAL BROKER</span>
      <h1>agent <i>talk</i></h1>
    </div>
    <div class="seal" aria-hidden="true">話</div>
  </header>

  <section class="registry" aria-labelledby="registry-heading">
    <div class="section-heading">
      <h2 id="registry-heading"><span>一</span> 稼働中の agent</h2>
      <output
        aria-live="polite"
        aria-atomic="true"
        class:failed={phase === "error"}>{message}</output
      >
    </div>

    {#if phase === "loading"}
      <div class="state-card loading" aria-busy="true">
        <span class="brush-loader" aria-hidden="true"></span>
        <p>接続を確かめています</p>
      </div>
    {:else if phase === "error"}
      <div class="state-card error" role="alert">
        <span class="error-mark" aria-hidden="true">!</span>
        <div>
          <strong>一覧を読み込めません</strong>
          <p>daemon の状態を確認して、もう一度お試しください。</p>
        </div>
        <button type="button" onclick={refresh}>再試行</button>
      </div>
    {:else if agents.length === 0}
      <div class="state-card empty">
        <span aria-hidden="true">○</span>
        <p>静かな待合です。<br />agent が登録されると、ここに現れます。</p>
      </div>
    {:else}
      <ul class="agent-list" aria-label="agent status list">
        {#each agents as agent, index (agent.pane_id)}
          <li style={`--index:${index}`}>
            <div
              class:idle={agent.state === "idle"}
              class:busy={agent.state === "busy"}
              class="state-stroke"
              aria-hidden="true"
            ></div>
            <div class="identity">
              <strong>{agent.name}</strong>
              <span title={agent.cwd}>{agent.cwd}</span>
            </div>
            <div class="coordinates">
              <span>{agent.location}</span><code>{agent.pane_id}</code>
            </div>
            <span
              class:idle={agent.state === "idle"}
              class:busy={agent.state === "busy"}
              class="status"
            >
              <i aria-hidden="true"></i>{agent.state}
            </span>
          </li>
        {/each}
      </ul>
    {/if}
  </section>

  <footer><span>READ ONLY</span><span>同一 tmux server</span></footer>
</main>
