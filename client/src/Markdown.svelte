<script lang="ts">
  import { renderMarkdown } from "./markdown";

  let { text }: { text: string } = $props();
  const html = $derived(renderMarkdown(text));
</script>

<div class="markdown-body">{@html html}</div>
