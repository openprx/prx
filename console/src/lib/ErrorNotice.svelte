<script>
  import { t } from './i18n';

  // `error`: an ApiError (or any Error). The gateway's own sentence is
  // shown as is; a 403 at `supervised` autonomy adds a `reason`, and a
  // refused removal lists what still uses the object.
  let { error } = $props();

  const message = $derived(error instanceof Error ? error.message : String(error ?? ''));
  const reason = $derived(typeof error?.reason === 'string' ? error.reason : '');
  const referrers = $derived(Array.isArray(error?.body?.referrers) ? error.body.referrers : []);
</script>

{#if error}
  <div
    role="alert"
    class="space-y-1 rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"
  >
    <p>{message}</p>
    {#if reason}
      <p class="font-medium">{t('manage.refusedBecause')}</p>
      <p>{reason}</p>
    {/if}
    {#if referrers.length > 0}
      <p class="font-medium">{t('manage.usedBy')}</p>
      <ul class="list-inside list-disc">
        {#each referrers as referrer}
          <li class="font-mono text-xs">{referrer}</li>
        {/each}
      </ul>
    {/if}
  </div>
{/if}
