<script lang="ts" module>
  // pane ごとの未送信下書き。{#key pane_id} の remount (兄弟切替) を跨いで
  // 保持し、切替でデータを失わせない。page 再読み込みでは消える (memory のみ)。
  const drafts = new Map<string, string>();
</script>

<script lang="ts">
  import { onMount, tick } from "svelte";
  import { fetchMailboxes, sendLetter, type Agent } from "./api";

  let { agent }: { agent: Agent } = $props();
  let open = $state(false);
  let source = $state("");
  let sourcePhase = $state<"loading" | "ready" | "empty" | "error">("loading");
  // 初期値の捕捉でよい: この component は {#key pane_id} で pane ごとに
  // remount され、instance の生存中に agent が別 pane を指すことはない。
  // svelte-ignore state_referenced_locally
  let draft = $state(drafts.get(agent.pane_id) ?? "");
  let sending = $state(false);
  let status = $state("");
  let failed = $state(false);
  let launcher = $state<HTMLButtonElement | undefined>();
  let sheet = $state<HTMLDivElement | undefined>();
  let textarea = $state<HTMLTextAreaElement | undefined>();

  $effect(() => {
    // 下書きの保存。成功送信で clear された場合は保持からも消す。
    if (draft.trim() === "") {
      drafts.delete(agent.pane_id);
    } else {
      drafts.set(agent.pane_id, draft);
    }
  });

  $effect(() => {
    // sheet を開いたまま mailbox の確認が終わったら、focus を本文へ進める
    // (開いた時点では textarea が disabled で focus を受けられない)。
    if (open && sourcePhase === "ready" && document.activeElement === sheet) {
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

  async function openSheet(): Promise<void> {
    open = true;
    await tick();
    if (sourcePhase === "ready") {
      textarea?.focus();
    } else {
      // textarea に focus できない状態 (loading / empty / error) では
      // dialog 自身が focus を受け、Escape / 閉じる が効く。
      sheet?.focus();
    }
  }

  async function closeSheet(): Promise<void> {
    open = false;
    await tick();
    launcher?.focus();
  }

  function onKeydown(event: KeyboardEvent): void {
    if (event.key === "Escape") {
      event.stopPropagation();
      void closeSheet();
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

{#if open}
  <div
    bind:this={sheet}
    class="composer-sheet"
    role="dialog"
    tabindex="-1"
    aria-label={`${agent.name} への手紙`}
    onkeydown={onKeydown}
  >
    <form class="letter-compose composer-form" onsubmit={submit}>
      <div class="composer-head">
        <strong>{agent.name} へ手紙を出す</strong>
        <span class="composer-source">
          {#if sourcePhase === "loading"}mailbox を確認中{:else if sourcePhase === "ready"}{source}
            から{/if}
        </span>
        <button type="button" class="quiet-button" onclick={closeSheet}
          >閉じる</button
        >
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
          rows="4"
          disabled={sourcePhase === "loading"}
          placeholder={`${agent.name} (${agent.pane_id}) への本文`}
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
{:else}
  <div class="composer-dock">
    <button
      bind:this={launcher}
      type="button"
      class="composer-launcher"
      onclick={openSheet}
    >
      <span aria-hidden="true">✉</span>
      {agent.name} に手紙を出す{#if draft.trim() !== ""}<span
          class="draft-badge">下書きあり</span
        >{/if}
    </button>
  </div>
{/if}
