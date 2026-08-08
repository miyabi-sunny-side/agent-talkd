<script lang="ts">
  import type { Snippet } from "svelte";
  import Icon from "./Icon.svelte";

  let {
    title,
    onclose,
    children,
  }: { title: string; onclose: () => void; children: Snippet } = $props();

  let dialog = $state<HTMLElement | undefined>();

  $effect(() => {
    const autofocus = dialog?.querySelector<HTMLElement>("[data-autofocus]");
    (autofocus ?? dialog)?.focus();
  });

  function onkeydown(event: KeyboardEvent) {
    if (event.key === "Escape") {
      event.preventDefault();
      onclose();
      return;
    }
    if (event.key !== "Tab" || !dialog) {
      return;
    }
    const focusables = dialog.querySelectorAll<HTMLElement>(
      "button, a[href], input, select, textarea",
    );
    if (focusables.length === 0) {
      return;
    }
    const first = focusables[0];
    const last = focusables[focusables.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last?.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first?.focus();
    }
  }
</script>

<svelte:window {onkeydown} />

<button class="scrim" type="button" aria-label="閉じる" onclick={onclose}
></button>
<div
  class="modal"
  role="dialog"
  aria-modal="true"
  aria-label={title}
  tabindex="-1"
  bind:this={dialog}
>
  <div class="modal-head">
    <h2>{title}</h2>
    <button
      class="icon-btn"
      type="button"
      aria-label="閉じる"
      onclick={onclose}
    >
      <Icon name="x" />
    </button>
  </div>
  {@render children()}
</div>

<style>
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 20;
    padding: 0;
    border: none;
    background: var(--scrim);
    cursor: default;
  }
  .modal {
    position: fixed;
    top: 50%;
    left: 50%;
    z-index: 21;
    width: min(360px, calc(100vw - 32px));
    max-height: 80dvh;
    padding: 16px;
    overflow-y: auto;
    border: 1px solid var(--border);
    border-radius: 12px;
    background: var(--surface-raised);
    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.25);
    transform: translate(-50%, -50%);
  }
  .modal-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 12px;
  }
  .modal-head h2 {
    margin: 0;
    font-size: 16px;
    font-weight: 600;
    line-height: 1.3;
  }
  .icon-btn {
    display: grid;
    width: 36px;
    height: 36px;
    place-items: center;
    padding: 0;
    border: 0;
    color: var(--on-surface);
    background: transparent;
    cursor: pointer;
  }
  .icon-btn:hover {
    color: var(--accent);
  }
</style>
