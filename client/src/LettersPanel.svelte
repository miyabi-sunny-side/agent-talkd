<script lang="ts">
  import { onMount } from "svelte";
  import { fetchMailbox, fetchMailboxes, type MailboxEvent } from "./api";

  let mailboxes = $state<string[]>([]);
  let selected = $state("");
  let events = $state<MailboxEvent[]>([]);
  let phase = $state<"loading" | "error" | "ready">("loading");
  let message = $state("mailbox を確認中");
  let controller: AbortController | undefined;
  let generation = 0;

  function formatTime(value: string): string {
    const date = new Date(value);
    return Number.isNaN(date.valueOf())
      ? value
      : new Intl.DateTimeFormat("ja-JP", {
          month: "2-digit",
          day: "2-digit",
          hour: "2-digit",
          minute: "2-digit",
        }).format(date);
  }

  async function discover(): Promise<void> {
    const current = ++generation;
    controller?.abort();
    controller = new AbortController();
    phase = "loading";
    message = "mailbox を確認中";
    try {
      const found = await fetchMailboxes(controller.signal);
      if (current !== generation) return;
      mailboxes = found;
      selected = found.includes(selected) ? selected : (found[0] ?? "");
      events = [];
      if (selected === "") {
        phase = "ready";
        message = "mailbox はありません";
      } else {
        await loadSelected(true, current);
      }
    } catch {
      if (current !== generation || controller.signal.aborted) return;
      phase = "error";
      message = "mailbox を取得できませんでした";
    }
  }

  async function loadSelected(reset = false, expected?: number): Promise<void> {
    if (selected === "") return;
    const mailbox = selected;
    const current = expected ?? ++generation;
    if (expected === undefined) {
      controller?.abort();
      controller = new AbortController();
    }
    if (reset) {
      events = [];
      phase = "loading";
    }
    try {
      const after =
        reset || events.length === 0 ? undefined : events.at(-1)?.id;
      const result = await fetchMailbox(mailbox, {
        after,
        limit: 100,
        signal: controller?.signal,
      });
      if (current !== generation || mailbox !== selected) return;
      const previous = reset ? [] : events;
      const seen = new Set(previous.map((event) => event.id));
      events = [
        ...previous,
        ...result.events.filter((event) => !seen.has(event.id)),
      ].sort((left, right) => left.id - right.id);
      phase = "ready";
      message =
        events.length === 0
          ? "letter はありません"
          : `${events.length} letters`;
    } catch {
      if (current !== generation || controller?.signal.aborted) return;
      phase = "error";
      message = "letter を取得できませんでした";
    }
  }

  function selectMailbox(event: Event): void {
    selected = (event.currentTarget as HTMLSelectElement).value;
    generation += 1;
    controller?.abort();
    controller = new AbortController();
    events = [];
    void loadSelected(true, generation);
  }

  onMount(() => {
    void discover();
    return () => {
      generation += 1;
      controller?.abort();
    };
  });
</script>

<section
  class="letters-panel"
  aria-labelledby="letters-heading"
  aria-busy={phase === "loading"}
>
  <header class="panel-heading">
    <div>
      <span class="section-number">三</span>
      <div>
        <h2 id="letters-heading">Letters</h2>
        <p>external mailbox · read only</p>
      </div>
    </div>
    <div class="letter-actions">
      {#if mailboxes.length > 0}
        <label>
          <span>mailbox</span>
          <select value={selected} onchange={selectMailbox}>
            {#each mailboxes as mailbox}<option value={mailbox}
                >{mailbox}</option
              >{/each}
          </select>
        </label>
        <button
          type="button"
          class="quiet-button"
          onclick={() => loadSelected(false)}>更新</button
        >
      {/if}
    </div>
  </header>

  <div
    class="screen-status"
    class:failed={phase === "error"}
    aria-live="polite"
  >
    {message}
  </div>

  {#if phase === "error"}
    <div class="panel-state error" role="alert">
      <p>履歴を読み込めません。接続を確認してください。</p>
      <button
        type="button"
        onclick={mailboxes.length === 0 ? discover : () => loadSelected(false)}
        >再試行</button
      >
    </div>
  {:else if phase === "loading"}
    <div class="panel-state">
      <span class="brush-loader" aria-hidden="true"></span>
    </div>
  {:else if events.length === 0}
    <div class="panel-state empty">
      <span aria-hidden="true">○</span>
      <p>まだ letter はありません。</p>
    </div>
  {:else}
    <ol class="letter-list" aria-label={`${selected} letter history`}>
      {#each events as event (event.id)}
        <li
          class:incoming={event.direction === "in"}
          class:outgoing={event.direction === "out"}
        >
          <div class="letter-meta">
            <strong>{event.direction === "in" ? "IN" : "OUT"}</strong>
            <span>#{event.id}</span>
            <time datetime={event.created_at}
              >{formatTime(event.created_at)}</time
            >
          </div>
          <p>{event.body}</p>
          <footer>
            <span>{event.source_label}</span>
            <span
              >{event.direction === "in"
                ? `→ ${event.target_name}`
                : `← ${event.target_name}`}</span
            >
            {#if event.reply_to !== null}<span>reply #{event.reply_to}</span
              >{/if}
          </footer>
        </li>
      {/each}
    </ol>
  {/if}
</section>
