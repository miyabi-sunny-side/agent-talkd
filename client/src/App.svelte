<script lang="ts">
  import { onMount, tick } from "svelte";
  import { fetchAgents, type Agent } from "./api";
  import { currentRoute, navigate, onPopstate, type Route } from "./router";
  import AgentLetterComposer from "./AgentLetterComposer.svelte";
  import LettersPanel from "./LettersPanel.svelte";
  import ScreenPanel from "./ScreenPanel.svelte";
  import ThemeModal from "./ThemeModal.svelte";

  const REGISTRY_INTERVAL_MS = 5_000;
  const AGENT_NAME_ORDER = ["claude", "codex", "grok"];

  type SessionGroup = {
    key: string;
    session: string;
    backend: string;
    agents: Agent[];
  };

  let agents = $state<Agent[]>([]);
  let registryPhase = $state<"loading" | "error" | "ready">("loading");
  let message = $state("");
  // URL が唯一の画面情報源 (DESIGN.md §2)。view を別 state に持たない。
  let route = $state<Route>(currentRoute());
  let menuOpen = $state(false);
  let themeOpen = $state(false);
  let menuButton = $state<HTMLButtonElement | undefined>();
  let pollTimer: ReturnType<typeof setTimeout> | undefined;

  const liveAgent = $derived.by(() => {
    if (route.view !== "agent") return null;
    const pane = route.pane;
    return agents.find((agent) => agent.pane_id === pane) ?? null;
  });
  // pane が registry から消えても、見ていた agent の header は「退出」表示で
  // 残す (DESIGN.md §7.5)。route が変われば捨てる。
  let lastSeenAgent = $state<Agent | null>(null);
  $effect(() => {
    if (route.view !== "agent") {
      lastSeenAgent = null;
    } else if (liveAgent !== null) {
      lastSeenAgent = liveAgent;
    }
  });
  const displayAgent = $derived(
    liveAgent ??
      (route.view === "agent" && lastSeenAgent?.pane_id === route.pane
        ? lastSeenAgent
        : null),
  );
  const departed = $derived(displayAgent !== null && liveAgent === null);
  const siblings = $derived(
    displayAgent === null
      ? []
      : sortAgents(
          agents.filter(
            (candidate) =>
              candidate.backend === displayAgent.backend &&
              candidate.session === displayAgent.session,
          ),
        ),
  );
  // 退出中でも選択 agent をタブに残す (DESIGN.md §7.4 / §7.5)。
  const tabs = $derived.by(() => {
    if (displayAgent === null) return [] as Agent[];
    if (!departed) return siblings.length > 0 ? siblings : [displayAgent];
    const live = siblings.filter(
      (candidate) => candidate.pane_id !== displayAgent.pane_id,
    );
    return sortAgents([...live, displayAgent]);
  });
  const sessionGroups = $derived(groupBySession(agents));

  $effect(() => {
    document.title =
      route.view === "agent"
        ? `agent talk · ${displayAgent?.name ?? "agent"}`
        : route.view === "letters"
          ? "agent talk · letters"
          : "agent talk · observer";
  });

  // 世代番号で single-flight にする。遅れて届いた古い応答が新しい snapshot を
  // 上書きしないため (DESIGN.md §9)。
  let registryGeneration = 0;

  function agentNameRank(name: string): number {
    const index = AGENT_NAME_ORDER.indexOf(name);
    return index === -1 ? AGENT_NAME_ORDER.length : index;
  }

  function sortAgents(list: Agent[]): Agent[] {
    return [...list].sort((left, right) => {
      const byName = agentNameRank(left.name) - agentNameRank(right.name);
      if (byName !== 0) return byName;
      return left.pane_id.localeCompare(right.pane_id);
    });
  }

  function groupBySession(list: Agent[]): SessionGroup[] {
    const map = new Map<string, SessionGroup>();
    for (const agent of list) {
      const key = `${agent.backend}\0${agent.session}`;
      const existing = map.get(key);
      if (existing) {
        existing.agents.push(agent);
      } else {
        map.set(key, {
          key,
          session: agent.session,
          backend: agent.backend,
          agents: [agent],
        });
      }
    }
    return Array.from(map.values()).map((group) => ({
      ...group,
      agents: sortAgents(group.agents),
    }));
  }

  function agentAriaLabel(agent: Agent): string {
    return `${agent.name} (${agent.state}) の Screen を表示`;
  }

  function tabAriaLabel(agent: Agent, isDeparted: boolean): string {
    if (isDeparted) return `${agent.name} (退出)`;
    return `${agent.name} (${agent.state})`;
  }

  async function refresh(initial = false): Promise<void> {
    const current = ++registryGeneration;
    if (initial) {
      registryPhase = "loading";
      message = "agent registry を確認中";
    }
    try {
      const found = await fetchAgents();
      if (current !== registryGeneration) return;
      agents = found;
      registryPhase = "ready";
      message =
        found.length === 0
          ? "登録中の agent はありません"
          : `${found.length} agent`;
    } catch {
      if (current !== registryGeneration) return;
      // poll 失敗は表示中の内容を消さない (DESIGN.md §9)。
      if (registryPhase !== "ready") registryPhase = "error";
      message = "agent registry を取得できませんでした";
    }
  }

  function stopPoll(): void {
    if (pollTimer !== undefined) clearTimeout(pollTimer);
    pollTimer = undefined;
  }

  /// visible の間だけ cadence を1本だけ走らせる。hidden では止める。
  function schedulePoll(): void {
    stopPoll();
    if (document.visibilityState !== "visible") return;
    pollTimer = setTimeout(() => {
      void refresh().finally(schedulePoll);
    }, REGISTRY_INTERVAL_MS);
  }

  // 一覧から入った画面かどうか。true なら「戻る」は history.back() で
  // 履歴を巻き戻す (Back で詳細へ戻ってしまう二重 push を作らない)。
  let cameFromRegistry = $state(false);

  function openScreen(agent: Agent): void {
    navigate({ view: "agent", pane: agent.pane_id });
    route = { view: "agent", pane: agent.pane_id };
    cameFromRegistry = true;
    closeMenu();
  }

  async function focusAgentButton(paneId: string | null): Promise<void> {
    await tick();
    if (paneId === null) return;
    const target = Array.from(
      document.querySelectorAll<HTMLButtonElement>(".agent-btn"),
    ).find((button) => button.dataset.pane === paneId);
    target?.focus();
  }

  async function backToRegistry(): Promise<void> {
    if (route.view === "registry") return; // 既に一覧。重複 push しない。
    const paneId = route.view === "agent" ? route.pane : null;
    closeMenu();
    if (cameFromRegistry) {
      // 一覧由来: 履歴を1つ戻す。popstate 側が route と focus を担う。
      window.history.back();
      return;
    }
    // deep-link で直接開いた画面: 戻る先が無いので `/` へ置き換える。
    navigate({ view: "registry" }, "replace");
    route = { view: "registry" };
    await focusAgentButton(paneId);
  }

  function openLetters(): void {
    if (route.view === "letters") return; // 重複 push しない。
    closeMenu();
    navigate({ view: "letters" });
    route = { view: "letters" };
    cameFromRegistry = false;
  }

  // 同一 session のタブ切替は replaceState — Back はタブ履歴を遡らず一覧へ
  // 戻る (DESIGN.md §2)。
  function switchAgentTab(sibling: Agent): void {
    navigate({ view: "agent", pane: sibling.pane_id }, "replace");
    route = { view: "agent", pane: sibling.pane_id };
  }

  function toggleMenu(): void {
    menuOpen = !menuOpen;
  }

  function closeMenu(restoreFocus = false): void {
    menuOpen = false;
    if (restoreFocus) menuButton?.focus();
  }

  function openTheme(): void {
    closeMenu(false);
    themeOpen = true;
  }

  function closeTheme(): void {
    themeOpen = false;
    menuButton?.focus();
  }

  function onWindowKeydown(event: KeyboardEvent): void {
    if (menuOpen && event.key === "Escape") {
      // letter dock など後続の window Escape ハンドラに奪わせない。
      event.preventDefault();
      event.stopImmediatePropagation();
      closeMenu(true);
    }
  }

  onMount(() => {
    void refresh(true);
    schedulePoll();
    const unsubscribe = onPopstate((next) => {
      const leaving = route.view === "agent" ? route.pane : null;
      route = next;
      // browser Back で一覧へ戻った時も、見ていた button へ focus を返す。
      if (next.view === "registry") {
        cameFromRegistry = false;
        void focusAgentButton(leaving);
      }
    });
    const onVisibility = (): void => {
      // hidden では timer を止め、visible 復帰で即時 refresh してから
      // cadence を1本だけ張り直す。
      stopPoll();
      if (document.visibilityState === "visible") {
        void refresh().finally(schedulePoll);
      }
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      unsubscribe();
      stopPoll();
      document.removeEventListener("visibilitychange", onVisibility);
    };
  });
</script>

<svelte:window onkeydown={onWindowKeydown} />

<main class:detail-view={route.view === "agent"}>
  {#if route.view === "agent"}
    {#if displayAgent}
      <header class="detail-chrome">
        <div class="detail-bar-primary">
          <button
            class="brand-link"
            type="button"
            onclick={backToRegistry}
            aria-label="agent talk — 一覧へ戻る"
          >
            agent <i>talk</i>
          </button>
          <strong
            class="detail-session"
            title={`${displayAgent.session} · ${displayAgent.pane_id}`}
            aria-label={`${displayAgent.session} · ${displayAgent.pane_id}`}
            >{displayAgent.session}</strong
          >
          <div class="menu-wrapper">
            <button
              bind:this={menuButton}
              type="button"
              class="icon-button menu-button"
              aria-label="メニュー"
              aria-expanded={menuOpen}
              onclick={toggleMenu}
            >
              <svg viewBox="0 0 24 24" aria-hidden="true"
                ><path d="M4 7h16M4 12h16M4 17h16" /></svg
              >
            </button>
            {#if menuOpen}
              <button
                class="menu-overlay"
                type="button"
                tabindex="-1"
                aria-label="メニューを閉じる"
                onclick={() => closeMenu(true)}
              ></button>
              <nav class="menu-dropdown" aria-label="メニュー">
                <button class="menu-item" type="button" onclick={openLetters}
                  >Letters</button
                >
                <button class="menu-item" type="button" onclick={openTheme}
                  >テーマ設定</button
                >
              </nav>
            {/if}
          </div>
        </div>
        <nav class="agent-tabs" aria-label="同一 session の agent 切り替え">
          {#each tabs as sibling (sibling.pane_id)}
            {@const isActive = sibling.pane_id === displayAgent.pane_id}
            {@const isDeparted =
              isActive && departed
                ? true
                : !agents.some((agent) => agent.pane_id === sibling.pane_id)}
            <button
              type="button"
              class:active={isActive}
              class:departed={isDeparted}
              class:idle={!isDeparted && sibling.state === "idle"}
              class:busy={!isDeparted && sibling.state === "busy"}
              aria-current={isActive ? "true" : undefined}
              aria-label={tabAriaLabel(sibling, isDeparted)}
              title={`${sibling.session} · ${sibling.pane_id}`}
              onclick={() => {
                if (!isActive) switchAgentTab(sibling);
              }}
            >
              {sibling.name}
            </button>
          {/each}
        </nav>
      </header>
      <!-- pane の占有者が入れ替わったら instance ごと差し替える
           (旧 draft を新しい agent へ見せない・誤送信しない)。 -->
      {#key `${displayAgent.pane_id} ${displayAgent.name}`}
        <ScreenPanel agent={displayAgent} />
        <AgentLetterComposer agent={displayAgent} />
      {/key}
    {:else if registryPhase === "loading"}
      <header class="detail-chrome">
        <div class="detail-bar-primary">
          <button
            class="brand-link"
            type="button"
            onclick={backToRegistry}
            aria-label="agent talk — 一覧へ戻る"
          >
            agent <i>talk</i>
          </button>
        </div>
      </header>
      <div class="state-card loading" aria-busy="true">
        <span class="brush-loader" aria-hidden="true"></span>
        <p>接続を確かめています</p>
      </div>
    {:else if registryPhase === "error"}
      <header class="detail-chrome">
        <div class="detail-bar-primary">
          <button
            class="brand-link"
            type="button"
            onclick={backToRegistry}
            aria-label="agent talk — 一覧へ戻る"
          >
            agent <i>talk</i>
          </button>
        </div>
      </header>
      <div class="state-card error" role="alert">
        <span class="error-mark" aria-hidden="true">!</span>
        <div>
          <strong>一覧を読み込めません</strong>
          <p>daemon の状態を確認して、もう一度お試しください。</p>
        </div>
        <button type="button" onclick={() => refresh(true)}>再試行</button>
      </div>
    {:else}
      <!-- deep-link の pane が snapshot に無い。URL は保ち、silent redirect
           しない (DESIGN.md §2)。 -->
      <header class="detail-chrome">
        <div class="detail-bar-primary">
          <button
            class="brand-link"
            type="button"
            onclick={backToRegistry}
            aria-label="agent talk — 一覧へ戻る"
          >
            agent <i>talk</i>
          </button>
        </div>
      </header>
      <div class="state-card empty not-found">
        <span aria-hidden="true">○</span>
        <p>
          この agent は見つかりません。<br />退出したか、pane が変わりました。
        </p>
        <button type="button" class="quiet-button" onclick={backToRegistry}
          >一覧へ</button
        >
      </div>
    {/if}
  {:else}
    <header class="app-header">
      <button
        class="brand-link"
        type="button"
        onclick={backToRegistry}
        aria-label="agent registry を表示"
      >
        agent <i>talk</i>
      </button>
      <div class="menu-wrapper">
        <button
          bind:this={menuButton}
          type="button"
          class="icon-button menu-button"
          aria-label="メニュー"
          aria-expanded={menuOpen}
          onclick={toggleMenu}
        >
          <svg viewBox="0 0 24 24" aria-hidden="true"
            ><path d="M4 7h16M4 12h16M4 17h16" /></svg
          >
        </button>
        {#if menuOpen}
          <button
            class="menu-overlay"
            type="button"
            tabindex="-1"
            aria-label="メニューを閉じる"
            onclick={() => closeMenu(true)}
          ></button>
          <nav class="menu-dropdown" aria-label="メニュー">
            <button
              class="menu-item"
              type="button"
              class:active={route.view === "registry"}
              onclick={backToRegistry}>Agents</button
            >
            <button
              class="menu-item"
              type="button"
              class:active={route.view === "letters"}
              onclick={openLetters}>Letters</button
            >
            <button class="menu-item" type="button" onclick={openTheme}
              >テーマ設定</button
            >
          </nav>
        {/if}
      </div>
    </header>

    {#if route.view === "registry"}
      <section class="registry" aria-labelledby="registry-heading">
        <div class="registry-summary">
          <h1 id="registry-heading">稼働中の agent</h1>
          <output
            aria-live="polite"
            aria-atomic="true"
            class:failed={registryPhase === "error"}>{message}</output
          >
        </div>

        {#if registryPhase === "loading"}
          <div class="state-card loading" aria-busy="true">
            <span class="brush-loader" aria-hidden="true"></span>
            <p>接続を確かめています</p>
          </div>
        {:else if registryPhase === "error"}
          <div class="state-card error" role="alert">
            <span class="error-mark" aria-hidden="true">!</span>
            <div>
              <strong>一覧を読み込めません</strong>
              <p>daemon の状態を確認して、もう一度お試しください。</p>
            </div>
            <button type="button" onclick={() => refresh(true)}>再試行</button>
          </div>
        {:else if agents.length === 0}
          <div class="state-card empty">
            <span aria-hidden="true">○</span>
            <p>静かな待合です。<br />agent が登録されると、ここに現れます。</p>
          </div>
        {:else}
          <ul class="session-list" aria-label="agent status list">
            {#each sessionGroups as group, index (group.key)}
              <li class="session-card" style={`--index:${index}`}>
                <h2 class="session-title">{group.session}</h2>
                <div
                  class="agent-buttons"
                  role="group"
                  aria-label={group.session}
                >
                  {#each group.agents as agent (agent.pane_id)}
                    <button
                      type="button"
                      class="agent-btn"
                      class:idle={agent.state === "idle"}
                      class:busy={agent.state === "busy"}
                      data-pane={agent.pane_id}
                      aria-label={agentAriaLabel(agent)}
                      onclick={() => openScreen(agent)}
                      onkeydown={(event) => {
                        // jsdom は button の native Enter 起動を合成しないため明示する。
                        if (event.key === "Enter") {
                          event.preventDefault();
                          openScreen(agent);
                        }
                      }}
                    >
                      {agent.name}
                    </button>
                  {/each}
                </div>
              </li>
            {/each}
          </ul>
        {/if}
      </section>
    {:else}
      <button class="back-button" type="button" onclick={backToRegistry}
        >← agent 一覧へ</button
      >
      <LettersPanel />
    {/if}

    <footer class="site-footer">
      <span>OBSERVE + LETTERS</span><span>herdr</span>
    </footer>
  {/if}
</main>

{#if themeOpen}
  <ThemeModal onclose={closeTheme} />
{/if}
