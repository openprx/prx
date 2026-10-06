<script>
  let { value = $bindable(''), label, disabled = false } = $props();
</script>
<label class="block">
  <span>{label}</span>
  <input type="password" bind:value {disabled} autocomplete="new-password" spellcheck="false"
    class="mt-1 block w-full rounded-lg border border-gray-400 bg-transparent px-3 py-2" />
</label>
