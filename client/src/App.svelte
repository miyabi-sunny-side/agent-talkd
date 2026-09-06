<script lang="ts">
  import { onMount, tick } from "svelte";
  import {
    fetchAgents,
    errorReason,
    unavailableReason,
    statusLabel,
    type Agent,
  } from "./api";
  import { currentRoute, navigate, onPopstate, type Route } from "./router";
  import ConversationPanel from "./ConversationPanel.svelte";
  import ThemeModal from "./ThemeModal.svelte";
  let agents = $state<Agent[]>([]);
  let phase = $state<"loading" | "ready" | "error">("loading");
  let reason = $state("");
  let route = $state<Route>(currentRoute());
  let lastSeen = $state<Agent | null>(null);
  let menuOpen = $state(false);
  let themeOpen = $state(false);
  let menuButton: HTMLButtonElement;
  let fromRegistry = false;
  let generation = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let disposed = false;
  const live = $derived(
    route.view === "agent"
      ? (agents.find(
          (a) => route.view === "agent" && a.pane_id === route.pane,
        ) ?? null)
      : null,
  );
  $effect(() => {
    if (live) lastSeen = live;
    else if (route.view !== "agent") lastSeen = null;
  });
  const selected = $derived(
    live ??
      (route.view === "agent" && lastSeen?.pane_id === route.pane
        ? lastSeen
        : null),
  );
  const tabs = $derived(
    selected
      ? [
          ...agents.filter((a) => a.workspace === selected.workspace),
          ...(!live ? [selected] : []),
        ]
      : [],
  );
  const groups = $derived.by(() => {
    const groups = new Map<string, Agent[]>();
    for (const agent of agents)
      groups.set(agent.workspace, [
        ...(groups.get(agent.workspace) ?? []),
        agent,
      ]);
    return [...groups];
  });
  $effect(() => {
    document.title =
      route.view === "agent"
        ? `agent talk · ${selected?.name ?? "agent"}`
        : "agent talk · agents";
  });
  async function refresh() {
    const id = ++generation;
    try {
      const found = await fetchAgents();
      if (disposed || id !== generation) return;
      agents = found;
      phase = "ready";
      reason = "";
    } catch (error) {
      if (disposed || id !== generation) return;
      if (phase !== "ready") phase = "error";
      reason = errorReason(error);
    }
  }
  function schedule() {
    if (timer) clearTimeout(timer);
    if (!disposed && document.visibilityState === "visible")
      timer = setTimeout(() => {
        void refresh().finally(schedule);
      }, 5000);
  }
  function openAgent(agent: Agent, replace = false) {
    navigate(
      { view: "agent", pane: agent.pane_id },
      replace ? "replace" : "push",
    );
    route = { view: "agent", pane: agent.pane_id };
    if (!replace) fromRegistry = true;
    menuOpen = false;
  }
  async function focusAgent(pane: string | null) {
    await tick();
    Array.from(document.querySelectorAll<HTMLButtonElement>(".agent-btn"))
      .find((b) => b.dataset.pane === pane)
      ?.focus();
  }
  async function home() {
    menuOpen = false;
    if (route.view === "registry") return;
    const pane = route.pane;
    if (fromRegistry) {
      window.history.back();
      return;
    }
    navigate({ view: "registry" }, "replace");
    route = { view: "registry" };
    await focusAgent(pane);
  }
  function closeMenu() {
    menuOpen = false;
    menuButton?.focus();
  }
  async function toggleMenu() {
    menuOpen = !menuOpen;
    await tick();
    if (menuOpen)
      document.querySelector<HTMLButtonElement>(".menu-item")?.focus();
  }
  function openTheme() {
    menuOpen = false;
    themeOpen = true;
  }
  function closeTheme() {
    themeOpen = false;
    menuButton?.focus();
  }
  onMount(() => {
    void refresh();
    schedule();
    const unsubscribe = onPopstate((next) => {
      const pane = route.view === "agent" ? route.pane : null;
      route = next;
      if (next.view === "registry") {
        fromRegistry = false;
        void focusAgent(pane);
      }
    });
    const visibility = () => {
      if (timer) clearTimeout(timer);
      if (document.visibilityState === "visible")
        void refresh().finally(schedule);
    };
    document.addEventListener("visibilitychange", visibility);
    return () => {
      disposed = true;
      generation++;
      if (timer) clearTimeout(timer);
      unsubscribe();
      document.removeEventListener("visibilitychange", visibility);
    };
  });
</script>

<svelte:window
  onkeydown={(e) => {
    if (menuOpen && e.key === "Escape") {
      e.preventDefault();
      e.stopImmediatePropagation();
      closeMenu();
    }
  }}
/>
{#snippet menu()}
  <div class="menu-wrapper">
    <button
      bind:this={menuButton}
      type="button"
      class="icon-button menu-button"
      aria-label="メニュー"
      aria-expanded={menuOpen}
      onclick={toggleMenu}
      ><svg viewBox="0 0 24 24" aria-hidden="true"
        ><path d="M4 7h16M4 12h16M4 17h16" /></svg
      ></button
    >
    {#if menuOpen}<button
        class="menu-overlay"
        type="button"
        tabindex="-1"
        aria-label="メニューを閉じる"
        onclick={closeMenu}
      ></button>
      <nav class="menu-dropdown" aria-label="メニュー">
        <button class="menu-item" onclick={home}>エージェント一覧</button
        ><button class="menu-item" onclick={openTheme}>テーマ設定</button>
      </nav>{/if}
  </div>
{/snippet}
{#if route.view === "agent"}
  <main class="detail-view">
    <header class="detail-chrome">
      <div class="detail-bar-primary">
        <button
          class="brand-link"
          onclick={home}
          aria-label="agent talk — 一覧へ戻る">agent <i>talk</i></button
        >
        <h1 class="detail-session" title={selected?.workspace}>
          {selected?.workspace ?? "会話・報告"}
        </h1>
        {@render menu()}
      </div>
      <nav class="agent-tabs" aria-label="同じ作業領域のエージェント">
        {#each tabs as agent (agent.pane_id)}<button
            class:active={selected?.pane_id === agent.pane_id}
            class:idle={agent.status === "idle"}
            class:busy={agent.status === "working"}
            aria-current={selected?.pane_id === agent.pane_id
              ? "true"
              : undefined}
            aria-label={`${agent.name} (${!live && selected?.pane_id === agent.pane_id ? "退出" : agent.status})`}
            onclick={() => {
              if (selected?.pane_id !== agent.pane_id) openAgent(agent, true);
            }}>{agent.name}</button
          >{/each}
      </nav>
    </header>
    {#if selected}
      {#key selected.pane_id}<ConversationPanel
          agent={selected}
          available={!!live}
          connected={!reason}
        />{/key}
    {:else if phase === "loading"}<div class="state-card" aria-busy="true">
        <span class="brush-loader"></span>
        <p>接続を確かめています</p>
      </div>
    {:else if phase === "error"}<div class="state-card error" role="alert">
        <p>一覧を読み込めません。{reason}</p>
        <button class="quiet-button" onclick={refresh}>再試行</button>
      </div>
    {:else}<div class="state-card">
        <p>この agent は見つかりません。退出したか、対象が変わりました。</p>
        <button class="quiet-button" onclick={home}>一覧へ</button>
      </div>{/if}
  </main>
{:else}
  <header class="app-header">
    <button class="brand-link" onclick={home} aria-label="エージェント一覧"
      >agent <i>talk</i></button
    >{@render menu()}
  </header>
  <main>
    <section class="registry" aria-labelledby="registry-heading">
      <div class="registry-summary">
        <h1 id="registry-heading">稼働中の agent</h1>
        <output aria-live="polite" class:failed={!!reason}
          >{phase === "loading" ? "確認中" : `${agents.length} agent`}</output
        >
      </div>
      {#if reason}<div class="history-error" role="alert">
          <p>一覧を更新できません。{reason}</p>
          <button class="quiet-button" onclick={refresh}>再試行</button>
        </div>{/if}
      {#if phase === "loading"}<div class="state-card" aria-busy="true">
          <span class="brush-loader"></span>
          <p>接続を確かめています</p>
        </div>
      {:else if phase === "ready" && agents.length === 0}<div
          class="state-card"
        >
          <p>
            稼働中の agent はありません。<br />Herdr
            の対象が見つかると、ここに現れます。
          </p>
        </div>
      {:else}<ul class="session-list" aria-label="エージェント一覧">
          {#each groups as [workspace, members], index (workspace)}<li
              class="session-card"
              style={`--index:${index}`}
            >
              <h2 class="session-title">{workspace}</h2>
              <div class="agent-buttons">
                {#each members as agent (agent.pane_id)}<button
                    class="agent-btn"
                    class:idle={agent.status === "idle"}
                    class:busy={agent.status === "working"}
                    data-pane={agent.pane_id}
                    aria-label={`${agent.name} (${agent.status}) の会話を表示`}
                    onclick={() => openAgent(agent)}
                    ><span>{agent.name}</span><small
                      >{agent.harness} · {statusLabel(agent.status)}</small
                    >{#if unavailableReason(agent)}<small
                        >{unavailableReason(agent)}</small
                      >{/if}</button
                  >{/each}
              </div>
            </li>{/each}
        </ul>{/if}
    </section>
  </main>
{/if}
{#if themeOpen}<ThemeModal onclose={closeTheme} />{/if}
