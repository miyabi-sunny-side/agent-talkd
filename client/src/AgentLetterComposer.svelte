<script lang="ts" module>
  // pane ごとの未送信下書き。{#key pane_id} の remount (タブ切替) を跨いで
  // 保持し、切替でデータを失わせない。key に agent 名を含め、pane の占有者が
  // 入れ替わった場合は旧 draft を新しい agent へ見せない (DESIGN.md §7.6)。
  // page 再読み込みでは消える (memory のみ)。
  const drafts = new Map<string, string>();
</script>

<script lang="ts">
  import { onMount, tick } from "svelte";
  import { fetchMailboxes, sendLetter, type Agent } from "./api";

  let { agent }: { agent: Agent } = $props();
  // 初期値の捕捉でよい: この component は {#key pane_id} で pane ごとに
  // remount され、instance の生存中に agent が別 pane を指すことはない。
  // svelte-ignore state_referenced_locally
  const draftKey = `${agent.pane_id} ${agent.name}`;
  let expanded = $state(false);
  let source = $state("");
  let sourcePhase = $state<"loading" | "ready" | "empty" | "error">("loading");
  // svelte-ignore state_referenced_locally
  let draft = $state(drafts.get(draftKey) ?? "");
  let sending = $state(false);
  let status = $state("");
  let failed = $state(false);
  let tab = $state<HTMLButtonElement | undefined>();
  let panel = $state<HTMLDivElement | undefined>();
  let textarea = $state<HTMLTextAreaElement | undefined>();

  $effect(() => {
    // 下書きの保存。成功送信で clear された場合は保持からも消す。
    if (draft.trim() === "") {
      drafts.delete(draftKey);
    } else {
      drafts.set(draftKey, draft);
    }
  });

  $effect(() => {
    // panel を開いたまま mailbox の確認が終わったら、focus を本文へ進める
    // (開いた時点では textarea が disabled で focus を受けられない)。
    if (
      expanded &&
      sourcePhase === "ready" &&
      document.activeElement === panel
    ) {
      textarea?.focus();
    }
  });

  async function loadSources(): Promise<void> {
    sourcePhase = "loading";
    try {
      const mailboxes = await fetchMailboxes();
      source = mailboxes.includes("mobile") ? "mobile" : (mailboxes[0] ?? "");
      sourcePhase = source === "" ? "empty" : "ready";
    } catch {
      // 取得失敗は「許可が無い」とは別の状態。設定不備へ誤誘導しない。
      source = "";
      sourcePhase = "error";
    }
  }

  onMount(() => {
    void loadSources();
  });

  async function openPanel(): Promise<void> {
    expanded = true;
    await tick();
    if (sourcePhase === "ready") {
      textarea?.focus();
    } else {
      // textarea に focus できない状態 (loading / empty / error) では
      // panel 自身が focus を受け、Escape / 閉じる が効く。
      panel?.focus();
    }
  }

  async function closePanel(): Promise<void> {
    expanded = false;
    await tick();
    tab?.focus();
  }

  function togglePanel(): void {
    if (expanded) void closePanel();
    else void openPanel();
  }

  function onWindowKeydown(event: KeyboardEvent): void {
    if (event.key === "Escape" && expanded && !event.defaultPrevented) {
      event.stopPropagation();
      void closePanel();
    }
  }

  async function submit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    if (sending || sourcePhase !== "ready" || draft.trim() === "") return;
    sending = true;
    failed = false;
    status = "送信中";
    try {
      const accepted = await sendLetter(source, agent.pane_id, draft);
      status =
        accepted.path === "sent"
          ? `送信しました #${accepted.id}`
          : `受理されました (配達待ち) #${accepted.id}`;
      draft = "";
    } catch (error) {
      // 失敗時は draft を保持し、書き直しではなく再送信で復帰できるようにする。
      failed = true;
      status =
        error instanceof Error
          ? `送信できませんでした (${error.message})`
          : "送信できませんでした";
    } finally {
      sending = false;
    }
  }
</script>

<svelte:window onkeydown={onWindowKeydown} />

<footer class="letter-dock" class:expanded>
  <div class="letter-dock-tab">
    <button
      bind:this={tab}
      type="button"
      class="letter-tab"
      aria-expanded={expanded}
      aria-controls="letter-dock-panel"
      aria-label={`${agent.name} に手紙を出す`}
      onclick={togglePanel}
    >
      <svg viewBox="0 0 24 24" aria-hidden="true"
        ><path d="M4 6h16v12H4z" /><path d="m4 7 8 6 8-6" /></svg
      >
      手紙
      {#if draft.trim() !== ""}<span class="draft-badge">下書きあり</span>{/if}
      <svg class="dock-chevron" viewBox="0 0 24 24" aria-hidden="true"
        ><path d="m7 15 5-5 5 5" /></svg
      >
    </button>
  </div>

  <div
    bind:this={panel}
    id="letter-dock-panel"
    class="letter-dock-panel"
    role="region"
    tabindex="-1"
    aria-label={`${agent.name} への手紙`}
    aria-hidden={!expanded}
    inert={!expanded}
  >
    <form class="composer-form" onsubmit={submit}>
      <div class="composer-head">
        <strong>{agent.name} へ手紙を出す</strong>
        <span class="composer-source">
          {#if sourcePhase === "loading"}mailbox を確認中{:else if sourcePhase === "ready"}{source}
            から{/if}
        </span>
        <button
          type="button"
          class="icon-button"
          aria-label="手紙を閉じる"
          onclick={closePanel}
        >
          <svg viewBox="0 0 24 24" aria-hidden="true"
            ><path d="m6 6 12 12M18 6 6 18" /></svg
          >
        </button>
      </div>
      {#if sourcePhase === "empty"}
        <p class="composer-blocked" role="alert">
          許可された mailbox が無いため送信できません。daemon の
          AGENT_TALK_ALLOWED_SOURCES を確認してください。
        </p>
      {:else if sourcePhase === "error"}
        <p class="composer-blocked" role="alert">
          mailbox を取得できませんでした。
          <button type="button" class="quiet-button" onclick={loadSources}
            >再試行</button
          >
        </p>
      {:else}
        <textarea
          bind:this={textarea}
          bind:value={draft}
          rows="3"
          disabled={sourcePhase === "loading"}
          placeholder={`${agent.name} への本文`}
          aria-label={`${agent.name} への手紙の本文`}></textarea>
        <div class="compose-actions">
          <button
            type="submit"
            disabled={sending || sourcePhase !== "ready" || draft.trim() === ""}
            >{sending ? "送信中…" : "手紙を出す"}</button
          >
          <span class="compose-status" class:failed aria-live="polite"
            >{status}</span
          >
        </div>
      {/if}
    </form>
  </div>
</footer>
