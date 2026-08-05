<script lang="ts">
  import { onMount } from "svelte";
  import { fetchScreen, type Agent } from "./api";

  let { agent }: { agent: Agent } = $props();
  let phase = $state<"loading" | "error" | "ready">("loading");
  let terminal = $state("");
  let requestNumber = 0;
  let controller: AbortController | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;

  function stopTimer(): void {
    if (timer !== undefined) clearTimeout(timer);
    timer = undefined;
  }

  function schedule(): void {
    stopTimer();
    if (document.visibilityState === "visible") {
      timer = setTimeout(() => void refresh(false), 2_000);
    }
  }

  async function refresh(showLoading = true): Promise<void> {
    const current = ++requestNumber;
    controller?.abort();
    controller = new AbortController();
    if (showLoading && terminal === "") phase = "loading";
    try {
      const capture = await fetchScreen(agent.pane_id, controller.signal);
      if (current !== requestNumber) return;
      terminal = capture.screen;
      phase = "ready";
    } catch (error) {
      if (current !== requestNumber || controller.signal.aborted) return;
      phase = "error";
    } finally {
      if (current === requestNumber) schedule();
    }
  }

  onMount(() => {
    void refresh();
    const onVisibility = (): void => {
      stopTimer();
      if (document.visibilityState === "visible") void refresh(false);
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      requestNumber += 1;
      controller?.abort();
      stopTimer();
      document.removeEventListener("visibilitychange", onVisibility);
    };
  });
</script>

<section
  class="screen-panel"
  aria-label={`${agent.name} screen`}
  aria-busy={phase === "loading"}
>
  <!-- 詳細画面は terminal が主役。見出しは detail-bar が担い、ここは
       更新状態の 1 行だけに抑える。 -->
  <div class="screen-toolbar">
    <span class="screen-status" aria-live="polite" aria-atomic="true">
      {#if phase === "loading"}画面を取得中{:else if phase === "error"}画面を取得できませんでした{:else}2秒ごとに更新{/if}
    </span>
    <button type="button" class="quiet-button" onclick={() => refresh(false)}
      >更新</button
    >
  </div>

  {#if phase === "error" && terminal === ""}
    <div class="panel-state error" role="alert">
      <p>pane が終了したか、一時的に capture できません。</p>
      <button type="button" onclick={() => refresh()}>再試行</button>
    </div>
  {:else if phase === "loading" && terminal === ""}
    <div class="panel-state" aria-hidden="true">
      <span class="brush-loader"></span>
    </div>
  {:else}
    <div
      class="terminal-frame"
      role="log"
      aria-label={`${agent.name} terminal`}
    >
      <pre class:dimmed={phase === "error"}>{terminal ||
          "（画面は空です）"}</pre>
    </div>
  {/if}
</section>
