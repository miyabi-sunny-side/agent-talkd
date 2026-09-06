<script lang="ts">
  import { onMount, tick, untrack } from "svelte";
  import {
    fetchConversation,
    sendMessage,
    errorReason,
    unavailableReason,
    statusLabel,
    type Agent,
    type Message,
    ApiError,
  } from "./api";
  let {
    agent,
    available,
    connected,
  }: { agent: Agent; available: boolean; connected: boolean } = $props();
  const target = untrack(() => ({
    pane: agent.pane_id,
    session: agent.session_id,
    name: agent.name,
    harness: agent.harness,
  }));
  const draftKey = `agent-talkd:draft:${JSON.stringify([target.pane, target.session])}`;
  function loadDraft() {
    try {
      return sessionStorage.getItem(draftKey) ?? "";
    } catch {
      return "";
    }
  }
  let draft = $state(loadDraft());
  let open = $state(false);
  let sending = $state(false);
  let result = $state("");
  let failed = $state(false);
  let messages = $state<Message[]>([]);
  let loaded = $state(false);
  let historyError = $state("");
  let truncated = $state(false);
  let textarea: HTMLTextAreaElement;
  let tab: HTMLButtonElement;
  let viewport: HTMLElement;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let disposed = false;
  let fetching = false;
  const changed = $derived(agent.session_id !== target.session);
  const blocked = $derived(
    changed
      ? "別のセッションに変わりました。下書きは以前の宛先に保管しています。一覧から対象を開き直してください。"
      : !available
        ? "対象は退出しました。報告と下書きは保持しています。"
        : !connected
          ? "接続が切れています。最新の対象状態を確認できるまで送信できません。"
          : unavailableReason(agent),
  );
  $effect(() => {
    try {
      if (draft) sessionStorage.setItem(draftKey, draft);
      else sessionStorage.removeItem(draftKey);
    } catch {
      /* storage unavailable: retain in memory */
    }
  });
  async function refresh() {
    if (fetching || disposed || !target.session || changed || !available)
      return;
    fetching = true;
    try {
      const response = await fetchConversation(target.pane, target.session);
      if (disposed || changed) return;
      const follow =
        !loaded ||
        (viewport &&
          viewport.scrollHeight - viewport.scrollTop - viewport.clientHeight <
            64);
      messages = response.messages;
      truncated = response.truncated;
      loaded = true;
      historyError = "";
      await tick();
      if (follow && viewport) viewport.scrollTop = viewport.scrollHeight;
    } catch (error) {
      if (!disposed) historyError = errorReason(error);
    } finally {
      fetching = false;
    }
  }
  function schedule() {
    if (timer) clearTimeout(timer);
    if (!disposed && document.visibilityState === "visible")
      timer = setTimeout(() => {
        void refresh().finally(schedule);
      }, 2000);
  }
  onMount(() => {
    void refresh();
    schedule();
    const visibility = () => {
      if (timer) clearTimeout(timer);
      if (document.visibilityState === "visible")
        void refresh().finally(schedule);
    };
    document.addEventListener("visibilitychange", visibility);
    return () => {
      disposed = true;
      if (timer) clearTimeout(timer);
      document.removeEventListener("visibilitychange", visibility);
    };
  });
  async function toggle() {
    open = !open;
    await tick();
    if (open) textarea?.focus();
    else tab?.focus();
  }
  async function close() {
    if (!open) return;
    open = false;
    await tick();
    tab?.focus();
  }
  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (sending || blocked || !target.session || !draft.trim()) return;
    const body = draft;
    sending = true;
    result = "送信中です…";
    failed = false;
    try {
      await sendMessage(target.pane, target.session, body);
      if (draft === body) {
        draft = "";
        try {
          if (sessionStorage.getItem(draftKey) === body)
            sessionStorage.removeItem(draftKey);
        } catch {
          /* retain memory */
        }
      }
      result =
        "送信を受け付けました。Herdr の入力受付を確認しました。進捗・完了は会話の報告で確認できます。";
      void refresh();
    } catch (error) {
      failed = true;
      const uncertain =
        !(error instanceof ApiError) ||
        ["network", "invalid_response", "http", "delivery_unknown"].includes(
          error.code,
        );
      result = `${errorReason(error)}。${uncertain ? "送信結果を確認できません。履歴を確認してから再送してください。" : "下書きを保持しています。"}自動再送はしません。`;
    } finally {
      sending = false;
    }
  }
</script>

<svelte:window
  onkeydown={(e) => {
    if (open && e.key === "Escape") {
      e.preventDefault();
      void close();
    }
  }}
/>
<div class="conversation-workspace">
  <!-- svelte-ignore a11y_no_noninteractive_tabindex (Scrollable conversation needs keyboard scrolling.) -->
  <section
    class="conversation-panel"
    aria-label="会話・報告"
    bind:this={viewport}
    tabindex="0"
  >
    <div class="conversation-meta">
      <span>{target.harness} · セッション {target.session ?? "未登録"}</span
      ><span>{statusLabel(agent.status)}</span>
    </div>
    <p class="conversation-cwd">{agent.cwd}</p>
    {#if blocked}<p class="availability" role="status">{blocked}</p>{/if}
    {#if historyError}<div class="history-error">
        <output class="failed" aria-live="polite"
          >会話を更新できません: {historyError}</output
        ><button class="quiet-button" onclick={refresh}>再試行</button>
      </div>{/if}
    {#if !target.session}<p class="panel-note">
        対象セッションを特定できないため、会話を表示できません。
      </p>
    {:else if !loaded && !historyError}<div
        class="panel-state"
        aria-busy="true"
      >
        <span class="brush-loader"></span>
        <p>会話を読み込んでいます</p>
      </div>
    {:else if loaded && messages.length === 0}<p class="panel-note">
        まだ会話・報告はありません。
      </p>{/if}
    {#if truncated}<p class="panel-note">直近の会話を表示しています。</p>{/if}
    <ol class="conversation-list">
      {#each messages as message (message.id)}
        <li class:user={message.role === "user"}>
          <div class="message-meta">
            <strong
              >{message.role === "user" ? "あなた" : "エージェント"}</strong
            >{#if message.timestamp}<time datetime={message.timestamp}
                >{message.timestamp}</time
              >{/if}
          </div>
          <p>{message.text}</p>
        </li>
      {/each}
    </ol>
  </section>
  <aside class="letter-dock" class:expanded={open} aria-label="手紙">
    <div class="letter-dock-tab">
      <button
        bind:this={tab}
        class="letter-tab"
        class:has-draft={!!draft}
        aria-label={draft ? "手紙・下書きあり" : "手紙"}
        aria-expanded={open}
        aria-controls="message-composer"
        onclick={toggle}
        ><svg viewBox="0 0 24 24" aria-hidden="true"
          ><rect x="3" y="5" width="18" height="14" rx="2" /><path
            d="m3 6 9 7 9-7"
          /></svg
        >手紙<svg class="dock-chevron" viewBox="0 0 24 24" aria-hidden="true"
          ><path d="m6 15 6-6 6 6" /></svg
        ></button
      >
    </div>
    <div
      class="letter-dock-panel"
      id="message-composer"
      inert={!open}
      aria-hidden={!open}
    >
      <form class="composer-form" onsubmit={submit}>
        <div class="composer-head">
          <strong>{target.name} へ手紙を出す</strong><button
            type="button"
            class="icon-button"
            aria-label="手紙を閉じる"
            onclick={close}
            ><svg viewBox="0 0 24 24" aria-hidden="true"
              ><path d="m6 6 12 12M6 18 18 6" /></svg
            ></button
          >
        </div>
        <p class="composer-source">
          {target.harness} · セッション {target.session ?? "未登録"}
        </p>
        <label for="message-body">本文</label><textarea
          id="message-body"
          bind:this={textarea}
          bind:value={draft}
          rows="3"
          placeholder="指示や追加の依頼を、そのまま送れます"></textarea>
        {#if blocked}<p class="composer-blocked">{blocked}</p>{/if}
        <div class="compose-actions">
          <output class="compose-status" class:failed aria-live="polite"
            >{result}</output
          ><button
            type="submit"
            disabled={sending || !!blocked || !draft.trim()}
            >{sending ? "送信中…" : "送信"}</button
          >
        </div>
      </form>
    </div>
  </aside>
</div>
