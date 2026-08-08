<script lang="ts">
  import Icon from "./Icon.svelte";
  import Modal from "./Modal.svelte";
  import { loadTheme, saveTheme, type ThemeChoice } from "./theme";

  let { onclose }: { onclose: () => void } = $props();

  let choice = $state<ThemeChoice>(loadTheme());

  const options: { value: ThemeChoice; label: string; icon: string }[] = [
    { value: "system", label: "自動", icon: "monitor" },
    { value: "light", label: "ライト", icon: "sun" },
    { value: "dark", label: "ダーク", icon: "moon" },
  ];

  // 選択してもモーダルは閉じない: テーマの変化をその場で目視確認させる。
  function choose(value: ThemeChoice) {
    choice = value;
    saveTheme(value);
  }
</script>

<Modal title="テーマ設定" {onclose}>
  <div class="options" role="radiogroup" aria-label="テーマ">
    {#each options as option (option.value)}
      <button
        class="option"
        class:selected={choice === option.value}
        type="button"
        role="radio"
        aria-checked={choice === option.value}
        data-autofocus={choice === option.value ? "" : undefined}
        onclick={() => choose(option.value)}
      >
        <Icon name={option.icon} />
        <span>{option.label}</span>
      </button>
    {/each}
  </div>
</Modal>

<style>
  .options {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .option {
    display: flex;
    min-height: 44px;
    align-items: center;
    gap: 8px;
    padding: 8px 12px;
    border: 1px solid var(--border);
    border-radius: 8px;
    color: var(--on-surface);
    background: var(--surface-raised);
    font-size: 14px;
    font-weight: 500;
    cursor: pointer;
  }
  .option:hover {
    background: var(--accent-subtle);
  }
  .option.selected {
    border-color: var(--accent);
    background: var(--accent-subtle);
  }
</style>
