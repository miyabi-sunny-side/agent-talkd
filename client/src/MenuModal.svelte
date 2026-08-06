<script lang="ts">
  import { onMount } from "svelte";
  import { loadTheme, saveTheme, type ThemeChoice } from "./theme";

  let { onclose }: { onclose: () => void } = $props();
  let choice = $state<ThemeChoice>(loadTheme());
  let dialog = $state<HTMLDivElement | undefined>();

  const options: { value: ThemeChoice; label: string }[] = [
    { value: "dark", label: "墨 — ダーク" },
    { value: "light", label: "生成り — ライト" },
    { value: "system", label: "システムに従う" },
  ];

  function choose(value: ThemeChoice): void {
    // 即適用・即保存。モーダルは開いたままにして変化を目視確認できるようにする
    // (DESIGN.md §4.4)。
    choice = value;
    saveTheme(value);
  }

  function onKeydown(event: KeyboardEvent): void {
    if (event.key === "Escape") {
      event.stopPropagation();
      onclose();
      return;
    }
    if (event.key !== "Tab") return;
    // 開いている間は背後を操作させない (focus はモーダル内で循環)。
    const focusables = Array.from(
      dialog?.querySelectorAll<HTMLElement>("button") ?? [],
    );
    if (focusables.length === 0) return;
    const first = focusables[0]!;
    const last = focusables.at(-1)!;
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  }

  onMount(() => {
    dialog?.querySelector<HTMLElement>("[aria-checked='true']")?.focus();
  });
</script>

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="modal-scrim" onclick={onclose}></div>
<div
  bind:this={dialog}
  class="menu-modal"
  role="dialog"
  aria-modal="true"
  aria-label="メニュー"
  tabindex="-1"
  onkeydown={onKeydown}
>
  <div class="modal-head">
    <strong>メニュー</strong>
    <button
      type="button"
      class="icon-button"
      aria-label="閉じる"
      onclick={onclose}
    >
      <svg viewBox="0 0 24 24" aria-hidden="true"
        ><path d="m6 6 12 12M18 6 6 18" /></svg
      >
    </button>
  </div>
  <div role="radiogroup" aria-label="テーマ" class="modal-options">
    <span class="modal-section-label">テーマ</span>
    {#each options as option (option.value)}
      <button
        type="button"
        role="radio"
        class="modal-option"
        aria-checked={choice === option.value}
        class:selected={choice === option.value}
        onclick={() => choose(option.value)}
      >
        {option.label}
      </button>
    {/each}
  </div>
</div>
