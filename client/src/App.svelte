<script lang="ts">
  import { onMount, tick } from "svelte";
  import { fetchAgents, type Agent } from "./api";
  import LettersPanel from "./LettersPanel.svelte";
  import ScreenPanel from "./ScreenPanel.svelte";

  let agents = $state<Agent[]>([]);
  let phase = $state<"loading" | "error" | "ready">("loading");
  let message = $state("");
  let view = $state<"registry" | "screen" | "letters">("registry");
  let selectedAgent = $state<Agent | null>(null);
  // 同一 session (workspace label) の兄弟 agent (claude ⇄ codex の行き来)。
  const siblings = $derived(
    selectedAgent === null
      ? []
      : agents.filter(
          (candidate) =>
            candidate.backend === selectedAgent!.backend &&
            candidate.session === selectedAgent!.session,
        ),
  );

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

  function openScreen(agent: Agent): void {
    selectedAgent = agent;
    view = "screen";
  }

  async function backToRegistry(): Promise<void> {
    const paneId = selectedAgent?.pane_id;
    view = "registry";
    selectedAgent = null;
    await tick();
    if (paneId) {
      const target = Array.from(
        document.querySelectorAll<HTMLButtonElement>(".agent-row"),
      ).find((button) => button.dataset.pane === paneId);
      target?.focus();
    }
  }

  onMount(refresh);
</script>

<svelte:head><title>agent talk · observer</title></svelte:head>

<main class:detail-view={view !== "registry"}>
  <header class="masthead">
    <button
      class="wordmark"
      type="button"
      onclick={backToRegistry}
      aria-label="agent registry を表示"
    >
      <span class="eyebrow">HERDR / LOCAL BROKER</span>
      <span class="title">agent <i>talk</i></span>
    </button>
    <nav aria-label="表示切り替え">
      <button
        class:active={view === "registry"}
        type="button"
        onclick={backToRegistry}>Agents</button
      >
      <button
        class:active={view === "letters"}
        type="button"
        onclick={() => (view = "letters")}>Letters</button
      >
    </nav>
    <div class="seal" aria-hidden="true">話</div>
  </header>

  {#if view === "registry"}
    <section class="registry" aria-labelledby="registry-heading">
      <div class="section-heading">
        <h1 id="registry-heading"><span>一</span> 稼働中の agent</h1>
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
              <button
                type="button"
                class="agent-row"
                data-pane={agent.pane_id}
                aria-label={`${agent.name} の Screen を表示`}
                onclick={() => openScreen(agent)}
                onkeydown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    openScreen(agent);
                  }
                }}
              >
                <span
                  class:idle={agent.state === "idle"}
                  class:busy={agent.state === "busy"}
                  class="state-stroke"
                  aria-hidden="true"
                ></span>
                <span class="identity"
                  ><span class="identity-name"
                    ><strong>{agent.name}</strong><span class="session-badge"
                      >{agent.session}</span
                    ></span
                  ><span title={agent.cwd}>{agent.cwd}</span></span
                >
                <span class="coordinates"
                  ><span>{agent.location}</span><code>{agent.pane_id}</code
                  ></span
                >
                <span
                  class:idle={agent.state === "idle"}
                  class:busy={agent.state === "busy"}
                  class="status"><i aria-hidden="true"></i>{agent.state}</span
                >
                <span class="row-arrow" aria-hidden="true">→</span>
              </button>
            </li>
          {/each}
        </ul>
      {/if}
    </section>
  {:else if view === "screen" && selectedAgent}
    <button class="back-button" type="button" onclick={backToRegistry}
      >← agent 一覧へ</button
    >
    {#if siblings.length > 1}
      <nav class="sibling-switcher" aria-label="同一 session の agent 切り替え">
        <span class="switcher-scope">{selectedAgent.session}</span>
        {#each siblings as sibling (sibling.pane_id)}
          <button
            type="button"
            class:active={sibling.pane_id === selectedAgent.pane_id}
            aria-current={sibling.pane_id === selectedAgent.pane_id
              ? "true"
              : undefined}
            onclick={() => (selectedAgent = sibling)}
          >
            {sibling.name}
          </button>
        {/each}
      </nav>
    {/if}
    {#key selectedAgent.pane_id}<ScreenPanel agent={selectedAgent} />{/key}
  {:else}
    <button class="back-button" type="button" onclick={backToRegistry}
      >← agent 一覧へ</button
    >
    <LettersPanel />
  {/if}

  <footer class="site-footer">
    <span>OBSERVE + LETTERS</span><span>herdr</span>
  </footer>
</main>
