<script lang="ts">
  import { untrack } from "svelte";
  import { fetchScreen, errorReason, type ScreenCapture } from "./api";
  let {
    pane,
    terminal,
    session,
    active,
    available,
    connected,
    changed,
  }: {
    pane: string;
    terminal: string | undefined;
    session: string | null;
    active: boolean;
    available: boolean;
    connected: boolean;
    changed: boolean;
  } = $props();
  let capture = $state<ScreenCapture | null>(null);
  let reason = $state("");
  let pending = $state(false);
  let fresh = $state(false);
  let visible = $state(document.visibilityState === "visible");
  let generation = 0;
  let controller: AbortController | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const canRead = $derived(
    active && visible && available && connected && !changed && !!terminal,
  );
  const screenState = $derived(
    changed
      ? "別のセッションに変わりました。一覧から対象を開き直してください。"
      : !available
        ? "対象は退出しました。"
        : !connected
          ? "接続が切れています。"
          : !terminal
            ? "対象の端末を特定できません。"
            : reason
              ? `画面を取得できません: ${reason}。`
              : !fresh
                ? "現在の画面を確認しています…"
                : "接続中",
  );
  async function refresh() {
    if (!canRead || pending || !terminal) return;
    const id = generation;
    pending = true;
    controller = new AbortController();
    if (timer) clearTimeout(timer);
    try {
      const next = await fetchScreen(
        pane,
        terminal,
        session,
        controller.signal,
      );
      if (id !== generation || !canRead) return;
      capture = next;
      reason = "";
      fresh = true;
    } catch (error) {
      if (id !== generation || !canRead) return;
      reason = errorReason(error);
      fresh = false;
    } finally {
      if (id === generation) {
        pending = false;
        if (canRead) timer = setTimeout(() => void refresh(), 2000);
      }
    }
  }
  $effect(() => {
    if (canRead) untrack(() => void refresh());
    return () => {
      generation++;
      controller?.abort();
      if (timer) clearTimeout(timer);
      pending = false;
      fresh = false;
    };
  });
</script>

<svelte:document
  onvisibilitychange={() => {
    visible = document.visibilityState === "visible";
  }}
/>
<section class="screen-panel" hidden={!active} aria-label="現在の端末画面">
  <div class="screen-status">
    <output aria-live="polite"
      >{screenState}{capture && (!fresh || !canRead)
        ? " 以前の表示です。現在の画面ではありません。"
        : ""}</output
    >
    <button
      class="quiet-button"
      disabled={!canRead || pending}
      onclick={refresh}>画面を更新</button
    >
    {#if capture}<time datetime={new Date(capture.captured_at).toISOString()}
        >取得 {new Date(capture.captured_at).toLocaleTimeString("ja-JP")}</time
      >{/if}
    <span>端末の表示テキスト・閲覧専用</span>
  </div>
  {#if capture}
    <!-- svelte-ignore a11y_no_noninteractive_tabindex (Scrollable terminal text supports keyboard reading.) -->
    <pre
      class:stale={!fresh || !canRead}
      tabindex="0"
      aria-label="端末の表示テキスト"
      data-testid="terminal-text">{capture.text}</pre>
  {:else}<p class="panel-note">
      {pending ? "画面を取得しています…" : "取得済みの画面はありません。"}
    </p>{/if}
</section>
