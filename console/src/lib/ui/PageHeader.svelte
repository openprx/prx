<script>
  let { title, description = '' } = $props();
</script>

{#if description}
  <div class="ui-page-header">
    <h2 class="text-2xl font-semibold">{title}</h2>
    <p class="ui-page-description">{description}</p>
  </div>
{:else}
  <h2 class="text-2xl font-semibold">{title}</h2>
{/if}
