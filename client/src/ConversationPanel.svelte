<script lang="ts">
  import { onMount, tick, untrack } from "svelte";
  import ScreenPanel from "./ScreenPanel.svelte";
  import Markdown from "./Markdown.svelte";
  import { appendMessages } from "./conversation";
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
    terminal: agent.terminal_id,
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
  let view = $state<"conversation" | "screen">("conversation");
  let sending = $state(false);
  let result = $state("");
  let failed = $state(false);
  let messages = $state<Message[]>([]);
  let loaded = $state(false);
  let historyError = $state("");
  let truncated = $state(false);
  let olderCursor = $state<string | null>(null);
  let nextCursor = $state("");
  let pollCursor = "";
  let detached = $state(false);
  let limited = $state(false);
  let retryMode: "poll" | "latest" | "older" | "newer" = "poll";
  let unread = $state(false);
  let atBottom = $state(true);
  let paging = $state(false);
  let cursorChanged = $state(false);
  let textarea: HTMLTextAreaElement;
  let tab: HTMLButtonElement;
  let viewport: HTMLElement;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let disposed = false;
  let fetching = false;
  let queued: "latest" | "older" | "newer" | undefined;
  const changed = $derived(agent.session_id !== target.session);
  const screenChanged = $derived(
    agent.terminal_id !== target.terminal ||
      (target.session !== null && changed),
  );
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
  function trackScroll() {
    atBottom =
      viewport.scrollHeight - viewport.scrollTop - viewport.clientHeight < 64;
  }
  async function refresh(mode: "poll" | "latest" | "older" | "newer" = "poll") {
    if (fetching) {
      if (mode !== "poll") queued = mode;
      return;
    }
    if (
      disposed ||
      view !== "conversation" ||
      !target.session ||
      changed ||
      !available ||
      (cursorChanged && mode !== "latest")
    )
      return;
    fetching = true;
    paging = mode !== "poll";
    const origin = document.activeElement;
    const cursor =
      mode === "older" && olderCursor
        ? { before: olderCursor }
        : mode === "newer"
          ? { after: nextCursor }
          : mode === "poll" && pollCursor
            ? { after: pollCursor }
            : {};
    try {
      const response = await fetchConversation(
        target.pane,
        target.session,
        cursor,
      );
      if (disposed || changed || view !== "conversation") return;
      // Read the position after the request: the person may have scrolled while it was in flight.
      const follow =
        !loaded ||
        mode === "latest" ||
        (mode === "poll" && atBottom && !detached);
      const previousTop = viewport?.scrollTop ?? 0;
      const anchor = Array.from(
        viewport?.querySelectorAll<HTMLElement>("[data-message-id]") ?? [],
      ).find(
        (element) =>
          element.getBoundingClientRect().bottom >=
          viewport.getBoundingClientRect().top,
      );
      const anchorY = anchor?.getBoundingClientRect().top;
      if (mode !== "poll" || !loaded) {
        limited = false;
        messages = response.messages;
        olderCursor = response.older_cursor;
        nextCursor = response.next_cursor;
        detached = mode === "older" || (mode === "newer" && response.has_more);
        if (!detached) pollCursor = response.next_cursor;
        unread = false;
        truncated = response.truncated;
      } else {
        pollCursor = response.next_cursor;
        if (response.messages.length) {
          unread = true;
          if (!detached) {
            const appended = appendMessages(messages, response.messages);
            if (appended) {
              messages = appended;
              nextCursor = response.next_cursor;
            } else {
              detached = true;
              limited = true;
            }
          }
        } else if (!detached) nextCursor = response.next_cursor;
        truncated ||= response.truncated;
      }
      loaded = true;
      if (mode !== "poll" || retryMode === "poll") historyError = "";
      cursorChanged = false;
      await tick();
      if (viewport) {
        if ((follow && !detached) || mode === "older")
          viewport.scrollTop = viewport.scrollHeight;
        else if (mode === "newer") viewport.scrollTop = 0;
        else if (anchor?.isConnected && anchorY !== undefined)
          viewport.scrollTop += anchor.getBoundingClientRect().top - anchorY;
        else viewport.scrollTop = previousTop;
        trackScroll();
      }
      if (atBottom && !detached) unread = false;
      await tick();
      if (
        mode !== "poll" &&
        origin instanceof HTMLButtonElement &&
        !origin.isConnected &&
        document.activeElement === document.body
      )
        viewport?.focus({ preventScroll: true });
    } catch (error) {
      if (!disposed) {
        retryMode = mode;
        cursorChanged =
          error instanceof ApiError && error.code === "cursor_changed";
        historyError = cursorChanged
          ? "履歴ファイルが変わりました。表示済みの会話を保持しています。最新へ戻って読み直してください。"
          : errorReason(error);
      }
    } finally {
      fetching = false;
      paging = false;
      if (queued) {
        const next = queued;
        queued = undefined;
        void refresh(next);
      }
    }
  }
  function schedule() {
    if (timer) clearTimeout(timer);
    if (
      !disposed &&
      view === "conversation" &&
      document.visibilityState === "visible"
    )
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
  async function switchView(next: "conversation" | "screen") {
    view = next;
    if (timer) clearTimeout(timer);
    await tick();
    if (view === "conversation") void refresh().finally(schedule);
  }
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
  <div class="history-controls">
    <nav class="view-tabs" aria-label="表示内容">
      <button
        class:active={view === "conversation"}
        aria-pressed={view === "conversation"}
        onclick={() => switchView("conversation")}>会話</button
      >
      <button
        class:active={view === "screen"}
        aria-pressed={view === "screen"}
        onclick={() => switchView("screen")}>画面</button
      >
    </nav>
    {#if view === "conversation"}
      <output aria-live="polite"
        >{historyError
          ? `会話を更新できません: ${historyError}`
          : paging
            ? "会話のページを読み込んでいます…"
            : limited
              ? "表示上限に達しました。新しい会話、または最新へ戻ると続きを読めます。"
              : detached
                ? unread
                  ? "新しい会話があります。過去の会話を表示しています。"
                  : "過去の会話を表示しています。"
                : unread
                  ? "新しい会話があります。"
                  : "会話・報告"}</output
      >
      {#if historyError && !cursorChanged}<button
          class="quiet-button"
          onclick={() => refresh(retryMode)}>再試行</button
        >{/if}
      {#if loaded && (!atBottom || detached || unread || cursorChanged)}<button
          class="quiet-button"
          disabled={paging}
          onclick={() => refresh("latest")}>最新へ戻る</button
        >{/if}
    {:else}<span class="view-caption">閲覧専用</span>{/if}
  </div>
  <!-- svelte-ignore a11y_no_noninteractive_tabindex (Scrollable conversation needs keyboard scrolling.) -->
  <section
    class="conversation-panel"
    hidden={view !== "conversation"}
    aria-label="会話・報告"
    bind:this={viewport}
    onscroll={trackScroll}
    tabindex="0"
  >
    <div class="conversation-meta">
      <span>{target.name} · {target.harness}</span><span
        >{statusLabel(agent.status)}</span
      >
    </div>
    <p class="conversation-cwd">{agent.cwd}</p>
    {#if blocked}<p class="availability" role="status">{blocked}</p>{/if}
    {#if !target.session}<p class="panel-note">
        対象セッションを特定できないため、会話を表示できません。
      </p>
    {:else if !loaded && !historyError}<div
        class="panel-state"
        aria-busy="true"
      >
        <span class="brush-loader"></span>
        <p>直近の会話を読み込んでいます</p>
      </div>
    {:else if loaded && messages.length === 0}<p class="panel-note">
        この範囲に表示できる会話・報告はありません。
      </p>{/if}
    {#if loaded}<p class="conversation-cwd">
        {olderCursor
          ? "古い会話はページごとに表示します。"
          : "会話の先頭です。"}
      </p>{/if}
    {#if olderCursor}<button
        class="quiet-button history-page"
        disabled={paging || cursorChanged}
        onclick={() => refresh("older")}>古い会話</button
      >{/if}
    {#if truncated}<p class="panel-note">
        大きすぎる記録や読み取れない記録の一部を省いています。
      </p>{/if}
    <ol class="conversation-list">
      {#each messages as message (message.id)}
        <li data-message-id={message.id} class:user={message.role === "user"}>
          <div class="message-meta">
            <strong
              >{message.role === "user" ? "あなた" : "エージェント"}</strong
            >{#if message.timestamp}<time datetime={message.timestamp}
                >{message.timestamp}</time
              >{/if}
          </div>
          {#if message.role === "assistant"}<Markdown
              text={message.text}
            />{:else}<p>{message.text}</p>{/if}
        </li>
      {/each}
    </ol>
    {#if detached}<button
        class="quiet-button history-page"
        disabled={paging || cursorChanged}
        onclick={() => refresh("newer")}>新しい会話</button
      >{/if}
  </section>
  <ScreenPanel
    pane={target.pane}
    terminal={target.terminal}
    session={target.session}
    active={view === "screen"}
    {available}
    {connected}
    changed={screenChanged}
  />
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
